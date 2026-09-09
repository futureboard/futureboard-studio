//! DAUx WASAPI Exclusive backend — Windows only.
//!
//! Uses raw Win32 WASAPI COM APIs to open a device in exclusive mode with
//! event-driven buffer filling and MMCSS "Pro Audio" thread priority.
//!
//! # Thread model
//!
//! A dedicated audio thread is spawned.  The thread:
//!   1. Calls `CoInitializeEx(COINIT_MULTITHREADED)` for COM.
//!   2. Sets MMCSS "Pro Audio" priority via `AvSetMmThreadCharacteristicsW`.
//!   3. Negotiates an exclusive-mode format with `IsFormatSupported`.
//!   4. Opens WASAPI device in exclusive, event-driven mode.
//!   5. Runs `WaitForMultipleObjects([buf_event, stop_event])` render loop.
//!   6. Calls `CoUninitialize` on exit.
//!
//! # Error behaviour
//!
//! All WASAPI failures are returned as `Err(String)` via the info channel —
//! never panicked, never silently swallowed.  The engine layer is responsible
//! for deciding whether to retry with a fallback backend.

#![allow(non_snake_case, clippy::too_many_arguments)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

use crossbeam_channel::{bounded, Receiver, Sender};
use windows::core::GUID;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eMultimedia, eRender, IAudioClient, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_NOPERSIST, DEVICE_STATE_ACTIVE, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    WAVEFORMATEXTENSIBLE_0,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects};

use crate::backend::render::{drain_commands, fill_output_f32, LocalAudioState};
use crate::backend::DauxDeviceConfig;
use crate::command::EngineCommand;
use crate::dsp::dither::{DitheredOutput, OutputDither};
use crate::engine::SharedState;
use crate::error::SphereAudioError;
use crate::runtime::RuntimeProject;

// ── Raw extern for MMCSS (avrt.lib) ──────────────────────────────────────────

#[link(name = "avrt")]
extern "system" {
    fn AvSetMmThreadCharacteristicsW(task_name: *const u16, task_index: *mut u32) -> isize;
    fn AvRevertMmThreadCharacteristics(handle: isize) -> i32;
}

// ── WASAPI HRESULT error codes ────────────────────────────────────────────────

const E_AUDCLNT_DEVICE_IN_USE: i32 = 0x88890004u32 as i32;
const E_AUDCLNT_UNSUPPORTED_FORMAT: i32 = 0x88890008u32 as i32;
const E_AUDCLNT_EXCLUSIVE_MODE_NOT_ALLOWED: i32 = 0x8889000Eu32 as i32;
const E_AUDCLNT_DEVICE_INVALIDATED: i32 = 0x88890014u32 as i32;
const E_AUDCLNT_BUFFER_SIZE_NOT_ALIGNED: i32 = 0x88890019u32 as i32;

/// Map a WASAPI HRESULT to a human-readable error string.
fn classify_hresult(code: i32, context: &str) -> String {
    let detail = match code {
        E_AUDCLNT_DEVICE_IN_USE => {
            "Device is in use by another application (close other audio software)".to_string()
        }
        E_AUDCLNT_UNSUPPORTED_FORMAT => "Unsupported audio format for exclusive mode".to_string(),
        E_AUDCLNT_EXCLUSIVE_MODE_NOT_ALLOWED => {
            "Exclusive mode is not allowed — enable it in Windows Sound > Advanced".to_string()
        }
        E_AUDCLNT_DEVICE_INVALIDATED => "Audio device was disconnected or invalidated".to_string(),
        E_AUDCLNT_BUFFER_SIZE_NOT_ALIGNED => {
            "Buffer size is not aligned for this device. Try 256 or 512 samples.".to_string()
        }
        _ => format!("HRESULT 0x{:08X}", code as u32),
    };
    format!("WASAPI Exclusive {context}: {detail}")
}

// ── Exclusive-mode sample words ──────────────────────────────────────────────

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

// ksmedia.h subformat GUIDs.
const KSDATAFORMAT_SUBTYPE_PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID =
    GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

// KSAUDIO_SPEAKER_* layouts for the common channel counts.
const SPEAKER_MONO: u32 = 0x4; // FRONT_CENTER
const SPEAKER_STEREO: u32 = 0x3; // FRONT_LEFT | FRONT_RIGHT
const SPEAKER_QUAD: u32 = 0x33;
const SPEAKER_5POINT1: u32 = 0x3F;
const SPEAKER_7POINT1_SURROUND: u32 = 0x63F;

/// A channel mask for a device that did not give us one. Zero ("any layout")
/// is legal but some drivers reject it in exclusive mode, so name the standard
/// layout when the channel count has one.
fn default_channel_mask(channels: u16) -> u32 {
    match channels {
        1 => SPEAKER_MONO,
        2 => SPEAKER_STEREO,
        4 => SPEAKER_QUAD,
        6 => SPEAKER_5POINT1,
        8 => SPEAKER_7POINT1_SURROUND,
        // No standard layout: the low `channels` bits, which is what WASAPI
        // itself does for an unlabelled multichannel endpoint.
        n if n < 32 => (1u32 << n) - 1,
        _ => 0,
    }
}

/// The sample word an exclusive-mode stream hands the hardware.
///
/// Exclusive mode bypasses the Windows audio engine completely: no mixer, no
/// resampler, no APO chain, no format conversion. Whatever word is negotiated
/// is what the driver receives, so the preference order below is simply "how
/// little has to happen to the engine's f32 mix to produce it".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DeviceWord {
    /// 32-bit float — the mix reaches the driver bit-for-bit, over-full-scale
    /// samples included.
    F32,
    /// 24 valid bits left-justified in a 32-bit container. What most audio
    /// interfaces actually run.
    I24In32,
    /// A 32-bit container declared fully valid. Written identically to
    /// `I24In32` (a converter resolves 24 bits at best); only the declared
    /// `wValidBitsPerSample` differs, and some drivers accept only this
    /// spelling.
    I32,
    /// 24-bit packed, three bytes per sample.
    I24,
    /// 16-bit integer.
    I16,
}

impl DeviceWord {
    /// Probe order: least conversion first.
    const PREFERENCE: &'static [DeviceWord] = &[
        DeviceWord::F32,
        DeviceWord::I24In32,
        DeviceWord::I32,
        DeviceWord::I24,
        DeviceWord::I16,
    ];

    fn bytes(self) -> usize {
        match self {
            DeviceWord::F32 | DeviceWord::I24In32 | DeviceWord::I32 => 4,
            DeviceWord::I24 => 3,
            DeviceWord::I16 => 2,
        }
    }

    fn container_bits(self) -> u16 {
        (self.bytes() * 8) as u16
    }

    fn valid_bits(self) -> u16 {
        match self {
            DeviceWord::F32 | DeviceWord::I32 => 32,
            DeviceWord::I24In32 | DeviceWord::I24 => 24,
            DeviceWord::I16 => 16,
        }
    }

    fn is_float(self) -> bool {
        matches!(self, DeviceWord::F32)
    }

    /// Whether this word can also be offered as a plain 18-byte `WAVEFORMATEX`.
    /// That form cannot express a channel mask or a valid-bit count, so it is
    /// only legal up to stereo with every container bit valid — but it is the
    /// only form some older drivers accept.
    fn fits_plain_waveformatex(self, channels: u16) -> bool {
        channels <= 2 && self.valid_bits() == self.container_bits()
    }

    fn label(self) -> &'static str {
        match self {
            DeviceWord::F32 => "32-bit float",
            DeviceWord::I24In32 => "24-bit in 32-bit",
            DeviceWord::I32 => "32-bit integer",
            DeviceWord::I24 => "24-bit packed",
            DeviceWord::I16 => "16-bit integer",
        }
    }
}

/// Build a candidate exclusive-mode format.
///
/// Always returns a `WAVEFORMATEXTENSIBLE`; `extensible == false` just sets
/// `cbSize` to 0 and a legacy format tag, so the same value can be passed as a
/// plain `WAVEFORMATEX` — WASAPI reads exactly `cbSize` extra bytes.
fn build_wave_format(
    word: DeviceWord,
    channels: u16,
    sample_rate: u32,
    channel_mask: u32,
    extensible: bool,
) -> WAVEFORMATEXTENSIBLE {
    let container_bits = word.container_bits();
    let block_align = channels * (container_bits / 8);
    let legacy_tag = if word.is_float() {
        WAVE_FORMAT_IEEE_FLOAT
    } else {
        WAVE_FORMAT_PCM
    };
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: if extensible {
                WAVE_FORMAT_EXTENSIBLE
            } else {
                legacy_tag
            },
            nChannels: channels,
            nSamplesPerSec: sample_rate,
            nAvgBytesPerSec: sample_rate * block_align as u32,
            nBlockAlign: block_align,
            wBitsPerSample: container_bits,
            cbSize: if extensible { 22 } else { 0 },
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: word.valid_bits(),
        },
        dwChannelMask: channel_mask,
        SubFormat: if word.is_float() {
            KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
        } else {
            KSDATAFORMAT_SUBTYPE_PCM
        },
    }
}

/// Write one block of engine `f32` into the driver's buffer in `word`.
///
/// # Safety
///
/// `dst` must point at `src.len() * word.bytes()` writable bytes — the buffer
/// `IAudioRenderClient::GetBuffer` just handed back for exactly this many
/// frames.
#[inline]
unsafe fn write_device_samples(
    dst: *mut u8,
    src: &[f32],
    word: DeviceWord,
    dither: &mut OutputDither,
) {
    match word {
        DeviceWord::F32 => {
            // Bit-exact: the driver gets the mix as the graph produced it.
            std::ptr::copy_nonoverlapping(
                src.as_ptr().cast::<u8>(),
                dst,
                std::mem::size_of_val(src),
            )
        }
        // Both spellings carry the same payload: 24 valid bits left-justified
        // in the 32-bit word, low byte zero. `i32::dithered_from_f32` produces
        // exactly that, and clips — the only bound in the whole output path,
        // and only because an integer word cannot hold anything else.
        DeviceWord::I24In32 | DeviceWord::I32 => {
            for (i, &v) in src.iter().enumerate() {
                let bytes = i32::dithered_from_f32(v, dither).to_le_bytes();
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst.add(i * 4), 4);
            }
        }
        DeviceWord::I24 => {
            for (i, &v) in src.iter().enumerate() {
                // The same 24-bit value, right-shifted out of its container.
                let bytes = (i32::dithered_from_f32(v, dither) >> 8).to_le_bytes();
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst.add(i * 3), 3);
            }
        }
        DeviceWord::I16 => {
            for (i, &v) in src.iter().enumerate() {
                let bytes = i16::dithered_from_f32(v, dither).to_le_bytes();
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst.add(i * 2), 2);
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────

/// Handle to a running WASAPI Exclusive stream.
///
/// Dropping this handle signals the audio thread to stop immediately —
/// it sets `stop_flag` AND signals `stop_event` so the thread wakes from
/// `WaitForMultipleObjects` without waiting for the 2-second buffer timeout.
pub struct WasapiExclusiveHandle {
    pub cmd_tx: Sender<EngineCommand>,
    pub sample_rate: u32,
    pub buffer_size: u32,
    pub device_name: String,
    stop_flag: Arc<AtomicBool>,
    /// Manual-reset event signaled to wake the audio thread on shutdown.
    stop_event: HANDLE,
    thread: Option<thread::JoinHandle<()>>,
}

// Safety: HANDLE (isize) is safe to send across threads for a kernel event object.
unsafe impl Send for WasapiExclusiveHandle {}

impl Drop for WasapiExclusiveHandle {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        unsafe {
            let _ = SetEvent(self.stop_event);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        unsafe {
            let _ = CloseHandle(self.stop_event);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────

pub fn open(
    config: &DauxDeviceConfig,
    shared: Arc<SharedState>,
    initial_runtime: RuntimeProject,
    glitch_counter: Arc<AtomicU64>,
) -> Result<WasapiExclusiveHandle, SphereAudioError> {
    let output_device_id = config.output_device_id.clone();
    let requested_sr = config.sample_rate;
    let buf_frames = config
        .buffer_size
        .unwrap_or(if config.safe_mode { 512 } else { 256 });

    let (tx, rx) = bounded::<EngineCommand>(512);
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop2 = Arc::clone(&stop_flag);
    let glitch2 = Arc::clone(&glitch_counter);

    let stop_event: HANDLE = unsafe {
        CreateEventW(None, false, false, None)
            .map_err(|e| SphereAudioError::StreamOpenFailed(format!("CreateEventW(stop): {e}")))?
    };
    let stop_event_usize = stop_event.0 as usize;

    let (info_tx, info_rx) = std::sync::mpsc::channel::<Result<(u32, u32, String), String>>();

    let t = thread::Builder::new()
        .name("daux-wasapi-excl".into())
        .spawn(move || unsafe {
            let stop_ev = HANDLE(stop_event_usize as *mut _);
            wasapi_thread(
                output_device_id,
                requested_sr,
                buf_frames,
                rx,
                shared,
                initial_runtime,
                glitch2,
                stop2,
                stop_ev,
                info_tx,
            );
        })
        .map_err(|e| {
            unsafe {
                let _ = CloseHandle(stop_event);
            }
            SphereAudioError::StreamOpenFailed(e.to_string())
        })?;

    let (sample_rate, buffer_size, device_name) = info_rx
        .recv_timeout(std::time::Duration::from_secs(8))
        .map_err(|e| {
            // Timeout or channel disconnect (thread panicked before sending).
            SphereAudioError::StreamOpenFailed(format!("WASAPI Exclusive thread init failed: {e}"))
        })
        .and_then(|r| r.map_err(SphereAudioError::StreamOpenFailed))?;

    Ok(WasapiExclusiveHandle {
        cmd_tx: tx,
        sample_rate,
        buffer_size,
        device_name,
        stop_flag,
        stop_event,
        thread: Some(t),
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Audio thread
// ─────────────────────────────────────────────────────────────────────────────

unsafe fn wasapi_thread(
    device_id: Option<String>,
    requested_sr: Option<u32>,
    buf_frames: u32,
    cmd_rx: Receiver<EngineCommand>,
    shared: Arc<SharedState>,
    initial_runtime: RuntimeProject,
    glitch_counter: Arc<AtomicU64>,
    stop_flag: Arc<AtomicBool>,
    stop_event: HANDLE,
    info_tx: std::sync::mpsc::Sender<Result<(u32, u32, String), String>>,
) {
    // ── COM init ──────────────────────────────────────────────────────────────
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

    // ── MMCSS ─────────────────────────────────────────────────────────────────
    let task: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
    let mut task_idx = 0u32;
    let mmcss_h = AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut task_idx);
    if mmcss_h != 0 {
        eprintln!("[DAUx WASAPI Excl] MMCSS 'Pro Audio' set (index={task_idx})");
        shared.mmcss_active.store(true, Ordering::Relaxed);
    } else {
        eprintln!("[DAUx WASAPI Excl] MMCSS set failed (non-fatal)");
    }

    // ── Device enumerator ─────────────────────────────────────────────────────
    let enumerator: IMMDeviceEnumerator =
        match CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) {
            Ok(e) => e,
            Err(e) => {
                let _ = info_tx.send(Err(format!("CoCreateInstance(IMMDeviceEnumerator): {e}")));
                cleanup_mmcss(mmcss_h);
                CoUninitialize();
                return;
            }
        };

    let device: IMMDevice = match resolve_device(&enumerator, device_id.as_deref()) {
        Ok(d) => d,
        Err(e) => {
            let _ = info_tx.send(Err(e));
            cleanup_mmcss(mmcss_h);
            CoUninitialize();
            return;
        }
    };

    let device_name = get_device_friendly_name(&device);
    eprintln!("[DAUx WASAPI Excl] Opening: {device_name}");

    // ── Open exclusive stream with format negotiation ─────────────────────────
    let _stream_completed = open_exclusive_stream(
        &device,
        requested_sr,
        buf_frames,
        &shared,
        &info_tx,
        &glitch_counter,
        &stop_flag,
        stop_event,
        &cmd_rx,
        initial_runtime,
        &device_name,
        mmcss_h,
    );

    cleanup_mmcss(mmcss_h);
    shared.mmcss_active.store(false, Ordering::Relaxed);
    CoUninitialize();
}

/// Opens the WASAPI exclusive stream, negotiates format and period, runs the
/// render loop, and handles BUFFER_SIZE_NOT_ALIGNED retry.
///
/// Returns `false` if initialization failed (error already sent on `info_tx`).
/// Returns `true` when the stream ran to completion (normal shutdown).
unsafe fn open_exclusive_stream(
    device: &IMMDevice,
    requested_sr: Option<u32>,
    buf_frames: u32,
    shared: &Arc<SharedState>,
    info_tx: &std::sync::mpsc::Sender<Result<(u32, u32, String), String>>,
    glitch_counter: &Arc<AtomicU64>,
    stop_flag: &Arc<AtomicBool>,
    stop_event: HANDLE,
    cmd_rx: &Receiver<EngineCommand>,
    initial_runtime: RuntimeProject,
    device_name: &str,
    _mmcss_h: isize,
) -> bool {
    // ── IAudioClient ──────────────────────────────────────────────────────────
    let client: IAudioClient = match device.Activate(CLSCTX_ALL, None) {
        Ok(c) => c,
        Err(e) => {
            let _ = info_tx.send(Err(format!("Activate(IAudioClient): {e}")));
            return false;
        }
    };

    // ── Device format: what the hardware itself runs ──────────────────────────
    //
    // `GetMixFormat` describes the *shared-mode* engine — 32-bit float at the
    // rate the Windows mixer happens to be running. Its channel count and
    // channel mask are the device's and worth keeping, but it is not a promise
    // about exclusive mode: there the driver takes this buffer directly, and
    // plenty of interfaces accept only their native integer word.
    //
    // So read the layout out of the mix format, free it, and negotiate for
    // real — every candidate word at the wanted rate, then the same list at the
    // device's own rate. The first one `IsFormatSupported` accepts in EXCLUSIVE
    // mode is what the DAC receives, with no mixer, no resampler, no APO and no
    // format conversion anywhere in between. Before this the mix format was the
    // only thing ever offered, so an interface that wants 24-in-32 could not
    // open at all and the user was pushed back onto a shared-mode path.
    let mix_fmt = match client.GetMixFormat() {
        Ok(p) => p,
        Err(e) => {
            let _ = info_tx.send(Err(format!("GetMixFormat: {e}")));
            return false;
        }
    };
    // Safety: GetMixFormat returns a valid pointer allocated by CoTaskMemAlloc.
    if mix_fmt.is_null() {
        let _ = info_tx.send(Err("GetMixFormat returned null".into()));
        return false;
    }

    let native_sr = (*mix_fmt).nSamplesPerSec.max(1);
    let device_ch = (*mix_fmt).nChannels.max(1);
    let channel_mask = if (*mix_fmt).wFormatTag == WAVE_FORMAT_EXTENSIBLE && (*mix_fmt).cbSize >= 22
    {
        (*mix_fmt.cast::<WAVEFORMATEXTENSIBLE>()).dwChannelMask
    } else {
        default_channel_mask(device_ch)
    };
    windows::Win32::System::Com::CoTaskMemFree(Some(mix_fmt as *const _ as *const _));

    let wanted_sr = requested_sr.unwrap_or(native_sr).max(1);
    // The rate reported to the engine must always be the rate the hardware
    // actually runs at — storing the requested rate while the device runs at
    // its native one desyncs transport, tempo and pitch.
    let mut rates: Vec<u32> = vec![wanted_sr];
    if native_sr != wanted_sr {
        rates.push(native_sr);
    }

    let mut chosen: Option<(u32, DeviceWord, WAVEFORMATEXTENSIBLE)> = None;
    'negotiate: for &rate in &rates {
        for &word in DeviceWord::PREFERENCE {
            for extensible in [true, false] {
                if !extensible && !word.fits_plain_waveformatex(device_ch) {
                    continue;
                }
                let candidate = build_wave_format(word, device_ch, rate, channel_mask, extensible);
                let probe = client.IsFormatSupported(
                    AUDCLNT_SHAREMODE_EXCLUSIVE,
                    (&candidate as *const WAVEFORMATEXTENSIBLE).cast::<WAVEFORMATEX>(),
                    None,
                );
                if probe.is_ok() {
                    chosen = Some((rate, word, candidate));
                    break 'negotiate;
                }
            }
        }
    }

    let Some((sample_rate, word, wfx)) = chosen else {
        let offered = rates
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(" / ");
        let _ = info_tx.send(Err(format!(
            "WASAPI Exclusive: '{device_name}' accepted no exclusive-mode format \
             (tried 32-bit float, 24-in-32, 32-bit, 24-bit and 16-bit at {offered} Hz, \
             {device_ch} ch). Enable exclusive mode in Windows Sound > Advanced, \
             or choose a sample rate the device supports."
        )));
        return false;
    };
    let wfx_ptr = (&wfx as *const WAVEFORMATEXTENSIBLE).cast::<WAVEFORMATEX>();
    let device_ch = device_ch as usize;
    let frame_bytes = device_ch * word.bytes();
    if sample_rate != wanted_sr {
        eprintln!(
            "[DAUx WASAPI Excl] Requested {wanted_sr} Hz unsupported in exclusive mode — using device rate {sample_rate} Hz"
        );
    }
    eprintln!(
        "[DAUx WASAPI Excl] Negotiated {} @ {sample_rate} Hz, {device_ch} ch — straight to the driver",
        word.label()
    );

    // ── Query device periods ───────────────────────────────────────────────────
    // hnsMinimumDevicePeriod is the minimum exclusive-mode period.
    let mut _default_period: i64 = 0;
    let mut min_period_hns: i64 = 0;
    let _ = client.GetDevicePeriod(Some(&mut _default_period), Some(&mut min_period_hns));
    // Compute HNS period from the buffer size at the rate we will actually run at.
    let requested_hns = (buf_frames as i64 * 10_000_000i64) / sample_rate as i64;
    let hns = requested_hns.max(min_period_hns.max(1));

    // ── Initialize IAudioClient (exclusive event-driven) ──────────────────────
    //
    // Deliberately *not* AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM: that flag puts the
    // Windows resampler back into the path, which is the one thing an exclusive
    // stream exists to avoid. An unsupported format is refused above, never
    // silently converted.
    let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_NOPERSIST;

    let init_result =
        client.Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, flags, hns, hns, wfx_ptr, None);

    // Handle BUFFER_SIZE_NOT_ALIGNED: re-create client with driver-aligned period.
    let client = match init_result {
        Ok(()) => client,
        Err(e) if e.code().0 == E_AUDCLNT_BUFFER_SIZE_NOT_ALIGNED => {
            eprintln!("[DAUx WASAPI Excl] Buffer size not aligned — querying aligned size");
            // GetBufferSize() after the failed Initialize returns the aligned frame count.
            let aligned_frames = match client.GetBufferSize() {
                Ok(f) => f,
                Err(e2) => {
                    let _ = info_tx.send(Err(format!(
                        "WASAPI Exclusive: buffer not aligned; GetBufferSize failed: {e2}"
                    )));
                    return false;
                }
            };
            let aligned_hns = ((aligned_frames as i64) * 10_000_000i64) / sample_rate as i64;
            eprintln!(
                "[DAUx WASAPI Excl] Aligned buffer: {aligned_frames} frames, {aligned_hns} hns"
            );
            // Per Windows docs: release and re-create the IAudioClient before retry.
            drop(client);
            let client2: IAudioClient = match device.Activate(CLSCTX_ALL, None) {
                Ok(c) => c,
                Err(e2) => {
                    let _ = info_tx.send(Err(format!(
                        "WASAPI Exclusive: Re-Activate for alignment retry failed: {e2}"
                    )));
                    return false;
                }
            };
            if let Err(e2) = client2.Initialize(
                AUDCLNT_SHAREMODE_EXCLUSIVE,
                flags,
                aligned_hns,
                aligned_hns,
                wfx_ptr,
                None,
            ) {
                let msg = classify_hresult(e2.code().0, "Initialize (aligned retry)");
                let _ = info_tx.send(Err(msg));
                return false;
            }
            client2
        }
        Err(e) => {
            let msg = classify_hresult(e.code().0, "Initialize");
            let _ = info_tx.send(Err(msg));
            return false;
        }
    };

    // ── Actual buffer size ─────────────────────────────────────────────────────
    let actual_buf = match client.GetBufferSize() {
        Ok(f) => f,
        Err(e) => {
            let _ = info_tx.send(Err(format!("GetBufferSize: {e}")));
            return false;
        }
    };

    // ── Buffer-ready event ────────────────────────────────────────────────────
    let buf_event: HANDLE = match CreateEventW(None, false, false, None) {
        Ok(h) => h,
        Err(e) => {
            let _ = info_tx.send(Err(format!("CreateEventW(buf): {e}")));
            return false;
        }
    };

    if let Err(e) = client.SetEventHandle(buf_event) {
        let _ = info_tx.send(Err(format!("SetEventHandle: {e}")));
        let _ = CloseHandle(buf_event);
        return false;
    }

    // ── IAudioRenderClient ────────────────────────────────────────────────────
    let render: IAudioRenderClient = match client.GetService() {
        Ok(s) => s,
        Err(e) => {
            let _ = info_tx.send(Err(format!("GetService(IAudioRenderClient): {e}")));
            let _ = CloseHandle(buf_event);
            return false;
        }
    };

    if let Err(e) = client.Start() {
        let _ = info_tx.send(Err(format!("IAudioClient::Start: {e}")));
        let _ = CloseHandle(buf_event);
        return false;
    }

    shared.sample_rate.store(sample_rate, Ordering::Relaxed);
    let _ = info_tx.send(Ok((sample_rate, actual_buf, device_name.to_string())));
    eprintln!(
        "[DAUx WASAPI Excl] Stream ready: device='{}' sr={} buf={} ch={} word={} ({} bytes/frame)",
        device_name,
        sample_rate,
        actual_buf,
        device_ch,
        word.label(),
        frame_bytes
    );

    // ── Runtime ───────────────────────────────────────────────────────────────
    let mut runtime = initial_runtime;
    runtime.retarget_sample_rate(sample_rate);
    let mut local = LocalAudioState::with_monitor_capacity(sample_rate as f64, actual_buf as usize);
    let mut scratch = vec![0.0f32; actual_buf as usize * device_ch];
    // Word-length reduction for the integer device words. One xorshift for the
    // life of the stream, advanced per sample, never reseeded. Unused on the
    // float path, where samples reach the driver untouched.
    let mut dither = OutputDither::new();

    // ── Render loop ───────────────────────────────────────────────────────────
    loop {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }

        let wait_handles = [buf_event, stop_event];
        let wait = WaitForMultipleObjects(&wait_handles, false, 2000);
        if wait == WAIT_OBJECT_0 {
            // buf_event signaled — fall through to render below.
        } else {
            // Index 1 = stop_event. Timeout or other = glitch. An unexpected
            // exit while not stopping means the device went away — flag it for
            // the control thread to surface DeviceLost and recover.
            if wait.0 != 1 && !stop_flag.load(Ordering::Relaxed) {
                glitch_counter.fetch_add(1, Ordering::Relaxed);
                shared.device_lost.store(true, Ordering::Relaxed);
            }
            break;
        }
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }

        drain_commands(cmd_rx, &mut runtime, shared, &mut local, sample_rate);

        let padding = client.GetCurrentPadding().unwrap_or(actual_buf);
        let frames = actual_buf.saturating_sub(padding);
        if frames == 0 {
            continue;
        }

        let buf_ptr = match render.GetBuffer(frames) {
            Ok(p) => p,
            Err(_) => {
                glitch_counter.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };

        let out_len = frames as usize * device_ch;
        // `scratch` is preallocated to `actual_buf * device_ch` and `frames`
        // can never exceed `actual_buf` (`saturating_sub` above), so `out_len`
        // never exceeds `scratch.len()` here. Clamp defensively instead of
        // growing — growing would allocate on the audio thread.
        let total = out_len.min(scratch.len());
        let block = &mut scratch[..total];
        // `fill_output_f32` fully overwrites every sample of `block` on every
        // reachable path (see `backend/render.rs`) — no pre-zero needed.
        fill_output_f32(block, device_ch, &mut runtime, shared, &mut local);

        // Straight into the driver's own buffer in its own word. Float is a
        // memcpy; the integer words quantize with dither at the resolution the
        // converter actually resolves. Nothing between here and the DAC.
        write_device_samples(buf_ptr, block, word, &mut dither);
        if total < out_len {
            // Unreachable in practice (see above) — silence rather than leave
            // the tail of the exclusive-mode buffer holding stale samples.
            std::ptr::write_bytes(
                buf_ptr.add(total * word.bytes()),
                0,
                (out_len - total) * word.bytes(),
            );
        }

        if let Err(e) = render.ReleaseBuffer(frames, 0) {
            eprintln!("[DAUx WASAPI Excl] ReleaseBuffer: {e}");
            glitch_counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    let _ = client.Stop();
    let _ = CloseHandle(buf_event);
    eprintln!("[DAUx WASAPI Excl] Stopped: {device_name}");
    true
}

// ─────────────────────────────────────────────────────────────────────────────

unsafe fn resolve_device(
    enumerator: &IMMDeviceEnumerator,
    name: Option<&str>,
) -> Result<IMMDevice, String> {
    match name {
        None => enumerator
            .GetDefaultAudioEndpoint(eRender, eMultimedia)
            .map_err(|e| format!("GetDefaultAudioEndpoint: {e}")),
        Some(wanted) => {
            use windows::Win32::Media::Audio::IMMDeviceCollection;
            let coll: IMMDeviceCollection = enumerator
                .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
                .map_err(|e| format!("EnumAudioEndpoints: {e}"))?;
            let count = coll.GetCount().map_err(|e| format!("GetCount: {e}"))?;
            for i in 0..count {
                let dev = coll.Item(i).map_err(|e| format!("Item({i}): {e}"))?;
                if get_device_friendly_name(&dev) == wanted {
                    return Ok(dev);
                }
            }
            eprintln!("[DAUx WASAPI Excl] Device '{wanted}' not found, using default");
            enumerator
                .GetDefaultAudioEndpoint(eRender, eMultimedia)
                .map_err(|e| format!("GetDefaultAudioEndpoint (fallback): {e}"))
        }
    }
}

unsafe fn get_device_friendly_name(device: &IMMDevice) -> String {
    use windows::Win32::Devices::Properties::DEVPKEY_Device_FriendlyName;
    use windows::Win32::Foundation::PROPERTYKEY;
    use windows::Win32::System::Com::STGM_READ;
    use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;

    let store: IPropertyStore = match device.OpenPropertyStore(STGM_READ) {
        Ok(s) => s,
        Err(_) => return "Unknown Device".into(),
    };

    let key = &DEVPKEY_Device_FriendlyName as *const _ as *const PROPERTYKEY;
    let mut prop = match store.GetValue(key) {
        Ok(p) => p,
        Err(_) => return "Unknown Device".into(),
    };

    #[repr(C)]
    struct RawPropVariant {
        vt: u16,
        _pad: [u16; 3],
        pwsz: *mut u16,
    }
    let raw = &mut prop as *mut _ as *mut RawPropVariant;
    if (*raw).vt == 31 {
        let ptr = (*raw).pwsz;
        if !ptr.is_null() {
            let mut len = 0usize;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(ptr, len);
            let s = String::from_utf16_lossy(slice).to_string();
            windows::Win32::System::Com::CoTaskMemFree(Some(ptr as *const _));
            (*raw).pwsz = std::ptr::null_mut();
            (*raw).vt = 0; // VT_EMPTY
            return s;
        }
    }
    "Unknown Device".into()
}

unsafe fn cleanup_mmcss(handle: isize) {
    if handle != 0 {
        AvRevertMmThreadCharacteristics(handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read a candidate back the way WASAPI does: the first `cbSize` bytes past
    /// the 18-byte `WAVEFORMATEX` header are the extensible tail.
    fn header(fmt: &WAVEFORMATEXTENSIBLE) -> WAVEFORMATEX {
        fmt.Format
    }

    #[test]
    fn extensible_candidates_declare_container_and_valid_bits() {
        let fmt = build_wave_format(DeviceWord::I24In32, 2, 48_000, SPEAKER_STEREO, true);
        let head = header(&fmt);
        assert_eq!({ head.wFormatTag }, WAVE_FORMAT_EXTENSIBLE);
        assert_eq!({ head.cbSize }, 22, "the extensible tail must be declared");
        assert_eq!(
            { head.wBitsPerSample },
            32,
            "24-in-32 is a 32-bit container"
        );
        assert_eq!(unsafe { fmt.Samples.wValidBitsPerSample }, 24);
        assert_eq!({ fmt.SubFormat }, KSDATAFORMAT_SUBTYPE_PCM);
        assert_eq!({ head.nBlockAlign }, 8, "2 ch x 4 bytes");
        assert_eq!({ head.nAvgBytesPerSec }, 48_000 * 8);
    }

    #[test]
    fn the_float_candidate_is_offered_first_and_is_ieee_float() {
        assert_eq!(DeviceWord::PREFERENCE[0], DeviceWord::F32);
        let fmt = build_wave_format(DeviceWord::F32, 2, 96_000, SPEAKER_STEREO, true);
        assert_eq!({ fmt.SubFormat }, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
        assert_eq!(unsafe { fmt.Samples.wValidBitsPerSample }, 32);
    }

    #[test]
    fn the_plain_form_drops_the_tail_and_uses_a_legacy_tag() {
        let fmt = build_wave_format(DeviceWord::I16, 2, 44_100, SPEAKER_STEREO, false);
        let head = header(&fmt);
        assert_eq!({ head.wFormatTag }, WAVE_FORMAT_PCM);
        assert_eq!({ head.cbSize }, 0, "a plain WAVEFORMATEX has no tail");
        assert_eq!({ head.nBlockAlign }, 4);

        let float = build_wave_format(DeviceWord::F32, 2, 44_100, SPEAKER_STEREO, false);
        assert_eq!({ header(&float).wFormatTag }, WAVE_FORMAT_IEEE_FLOAT);
    }

    #[test]
    fn only_words_a_plain_waveformatex_can_describe_are_offered_in_that_form() {
        // 24-in-32 needs wValidBitsPerSample, and >2 channels needs a mask.
        assert!(!DeviceWord::I24In32.fits_plain_waveformatex(2));
        assert!(DeviceWord::I16.fits_plain_waveformatex(2));
        assert!(DeviceWord::F32.fits_plain_waveformatex(1));
        assert!(!DeviceWord::F32.fits_plain_waveformatex(6));
    }

    #[test]
    fn a_device_without_a_mask_still_gets_a_named_layout() {
        assert_eq!(default_channel_mask(2), SPEAKER_STEREO);
        assert_eq!(default_channel_mask(6), SPEAKER_5POINT1);
        // No standard layout for 3 channels: the low bits, as WASAPI does.
        assert_eq!(default_channel_mask(3), 0b111);
    }

    #[test]
    fn float_reaches_the_driver_bit_for_bit() {
        let src = [0.0f32, 0.5, -0.75, 1.9, -2.5];
        let mut buf = vec![0u8; src.len() * 4];
        let mut dither = OutputDither::new();
        unsafe { write_device_samples(buf.as_mut_ptr(), &src, DeviceWord::F32, &mut dither) };

        for (i, &expected) in src.iter().enumerate() {
            let word = f32::from_le_bytes(buf[i * 4..i * 4 + 4].try_into().unwrap());
            assert_eq!(
                word, expected,
                "sample {i}: a float device buffer must carry the mix untouched,                  over full scale included"
            );
        }
    }

    #[test]
    fn integer_words_are_left_justified_and_clip_instead_of_wrapping() {
        let src = [0.5f32, -0.5, 4.0, -4.0];
        let mut dither = OutputDither::new();

        let mut wide = vec![0u8; src.len() * 4];
        unsafe { write_device_samples(wide.as_mut_ptr(), &src, DeviceWord::I24In32, &mut dither) };
        let word = |i: usize| i32::from_le_bytes(wide[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!(word(0) & 0xFF, 0, "24 valid bits sit in the high bytes");
        assert!((word(0) - (1 << 30)).abs() <= 2 << 8, "0.5 -> {}", word(0));
        // Past full scale an integer word has nowhere to go: clip, never wrap.
        assert!(word(2) > 0 && word(3) < 0, "+4.0/-4.0 wrapped sign");
        assert!(word(2) >= i32::MAX - 255);

        let mut packed = vec![0u8; src.len() * 3];
        unsafe { write_device_samples(packed.as_mut_ptr(), &src, DeviceWord::I24, &mut dither) };
        let packed_word = |i: usize| {
            let b = &packed[i * 3..i * 3 + 3];
            // Sign-extend the 24-bit little-endian value.
            ((i32::from(b[2]) << 24) | (i32::from(b[1]) << 16) | (i32::from(b[0]) << 8)) >> 8
        };
        assert!(packed_word(0) > 0 && packed_word(1) < 0);
        assert_eq!(
            packed_word(2),
            0x7F_FFFF,
            "+4.0 must clip to 24-bit full scale"
        );
        assert_eq!(packed_word(3), -0x80_0000);

        let mut narrow = vec![0u8; src.len() * 2];
        unsafe { write_device_samples(narrow.as_mut_ptr(), &src, DeviceWord::I16, &mut dither) };
        let short = |i: usize| i16::from_le_bytes(narrow[i * 2..i * 2 + 2].try_into().unwrap());
        assert_eq!(short(2), i16::MAX);
        assert_eq!(short(3), i16::MIN);
    }
}
