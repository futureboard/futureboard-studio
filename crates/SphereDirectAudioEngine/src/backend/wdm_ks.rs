//! DAUx WDM-KS backend — Windows only.
//!
//! WDM-KS is not a COM audio API like WASAPI: clients enumerate KS filter
//! device interfaces, open the filter with `CreateFileW`, query KS properties
//! via `DeviceIoControl`, then create/connect a streaming pin.
//!
//! # Render path (WaveRT)
//!
//! Modern Windows audio endpoints expose **WaveRT** miniports — the same
//! kernel layer WASAPI sits on.  A WaveRT render pin hands user mode a *mapped
//! cyclic buffer* plus a hardware play-position register; the client fills the
//! buffer ahead of the play cursor and the DMA engine reads it directly.  This
//! backend drives that model end to end:
//!
//!   1. Enumerate `KSCATEGORY_AUDIO` device interfaces with SetupAPI.
//!   2. Open the KS filter and probe its pins (dataflow / communication /
//!      dataranges) to pick a render-capable pin.
//!   3. On a dedicated MMCSS "Pro Audio" thread, instantiate the pin with a
//!      negotiated `WAVEFORMATEX` via `KsCreatePin`.
//!   4. Allocate the WaveRT cyclic buffer (`KSPROPERTY_RTAUDIO_BUFFER[_WITH_
//!      NOTIFICATION]`), register a notification event when supported, and run
//!      `KSSTATE_ACQUIRE → PAUSE → RUN`.
//!   5. Render the shared engine block (`fill_output_f32`) into the mapped
//!      buffer between the play cursor and our write cursor, event-driven.
//!
//! Devices whose render pin is *not* WaveRT (legacy WaveCyclic only) return a
//! precise error rather than silently falling back — WaveCyclic
//! `IOCTL_KS_WRITE_STREAM` packet streaming is a separate, future slice.
//!
//! Realtime discipline: the render loop preallocates its scratch, touches only
//! atomics/arithmetic, and never allocates, locks, or logs in steady state.

#![allow(non_snake_case)]
#![allow(clippy::too_many_arguments)]

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

use crossbeam_channel::{bounded, Receiver, Sender};
use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, GENERIC_READ, GENERIC_WRITE, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Threading::{
    CreateEventW, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};
use windows::Win32::System::IO::DeviceIoControl;

use crate::backend::open_handshake::{reap_device_thread, wait_for_device_thread, OpenHandshake};
use crate::backend::render::LocalAudioState;
use crate::backend::DauxDeviceConfig;
use crate::command::EngineCommand;
use crate::engine::SharedState;
use crate::error::SphereAudioError;
use crate::runtime::RuntimeProject;

// ── MMCSS (avrt.lib) — same hardening as the WASAPI exclusive backend ─────────

#[link(name = "avrt")]
extern "system" {
    fn AvSetMmThreadCharacteristicsW(task_name: *const u16, task_index: *mut u32) -> isize;
    fn AvRevertMmThreadCharacteristics(handle: isize) -> i32;
}

// ── KsCreatePin (ksuser.dll) — instantiates a pin on an open filter handle ────

#[link(name = "ksuser")]
extern "system" {
    /// `DWORD KsCreatePin(HANDLE FilterHandle, PKSPIN_CONNECT Connect,
    ///                    ACCESS_MASK DesiredAccess, PHANDLE ConnectionHandle)`
    /// Returns `ERROR_SUCCESS` (0) on success, otherwise a Win32 error code.
    /// `Connect` points at a `KSPIN_CONNECT` immediately followed in memory by
    /// the pin's `KSDATAFORMAT`.
    fn KsCreatePin(
        filter_handle: HANDLE,
        connect: *const c_void,
        desired_access: u32,
        connection_handle: *mut HANDLE,
    ) -> u32;
}

// ── GUIDs ─────────────────────────────────────────────────────────────────────

// ks.h: KSCATEGORY_AUDIO = {6994AD04-93EF-11D0-A3CC-00A0C9223196}
const KSCATEGORY_AUDIO: GUID = GUID::from_u128(0x6994ad04_93ef_11d0_a3cc_00a0c9223196);
// ks.h: KSCATEGORY_RENDER = {65E8773E-8F56-11D0-A3B9-00A0C9223196}
const KSCATEGORY_RENDER: GUID = GUID::from_u128(0x65e8773e_8f56_11d0_a3b9_00a0c9223196);
// ks.h: KSPROPSETID_Pin = {8C134960-51AD-11CF-878A-00AA003EEF17}
const KSPROPSETID_PIN: GUID = GUID::from_u128(0x8c134960_51ad_11cf_878a_00aa003eef17);
// ks.h: KSINTERFACESETID_Standard / KSMEDIUMSETID_Standard share this value:
//       {1A8766A0-62CE-11CF-A5D6-28DB04C10000}
const KSINTERFACESETID_STANDARD: GUID = GUID::from_u128(0x1a8766a0_62ce_11cf_a5d6_28db04c10000);
const KSMEDIUMSETID_STANDARD: GUID = GUID::from_u128(0x1a8766a0_62ce_11cf_a5d6_28db04c10000);
// ks.h: KSPROPSETID_Connection = {1D58C920-AC9B-11CF-A5D6-28DB04C10000}
const KSPROPSETID_CONNECTION: GUID = GUID::from_u128(0x1d58c920_ac9b_11cf_a5d6_28db04c10000);
// ksmedia.h: KSPROPSETID_RtAudio = {A855A48C-2F78-4729-9051-1968746B9EEF}
const KSPROPSETID_RTAUDIO: GUID = GUID::from_u128(0xa855a48c_2f78_4729_9051_1968746b9eef);
// ksmedia.h: KSDATAFORMAT_SPECIFIER_WAVEFORMATEX = {05589F81-C356-11CE-BF01-00AA0055595A}
const KSDATAFORMAT_SPECIFIER_WAVEFORMATEX: GUID =
    GUID::from_u128(0x05589f81_c356_11ce_bf01_00aa0055595a);

// ksmedia.h / ks.h data-format GUIDs used to recognize render-capable PCM pins.
const KSDATAFORMAT_TYPE_AUDIO: GUID = GUID::from_u128(0x73647561_0000_0010_8000_00aa00389b71);
const KSDATAFORMAT_SUBTYPE_PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID =
    GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

// ── IOCTL + property constants ────────────────────────────────────────────────

// ks.h: CTL_CODE(FILE_DEVICE_KS, 0x000, METHOD_NEITHER, FILE_ANY_ACCESS)
const IOCTL_KS_PROPERTY: u32 = 0x002f0003;
const KSPROPERTY_TYPE_GET: u32 = 0x00000001;
const KSPROPERTY_TYPE_SET: u32 = 0x00000002;

const KSPROPERTY_PIN_CTYPES: u32 = 1;
const KSPROPERTY_PIN_DATAFLOW: u32 = 2;
const KSPROPERTY_PIN_DATARANGES: u32 = 3;
const KSPROPERTY_PIN_COMMUNICATION: u32 = 7;

const KSINTERFACE_STANDARD_STREAMING: u32 = 0;
const KSMEDIUM_STANDARD_DEVIO: u32 = 0;

// KSPROPERTY_CONNECTION
const KSPROPERTY_CONNECTION_STATE: u32 = 0;
// KSSTATE
const KSSTATE_STOP: u32 = 0;
const KSSTATE_ACQUIRE: u32 = 1;
const KSSTATE_PAUSE: u32 = 2;
const KSSTATE_RUN: u32 = 3;

// KSPROPERTY_RTAUDIO
const KSPROPERTY_RTAUDIO_BUFFER: u32 = 1;
const KSPROPERTY_RTAUDIO_HWLATENCY: u32 = 2;
const KSPROPERTY_RTAUDIO_BUFFER_WITH_NOTIFICATION: u32 = 5;
const KSPROPERTY_RTAUDIO_REGISTER_NOTIFICATION_EVENT: u32 = 6;
const KSPROPERTY_RTAUDIO_GETPOSITIONS: u32 = 8;

const KSPRIORITY_NORMAL: u32 = 1;

// mmreg.h
const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

/// WaveRT cyclic buffer depth (periods) and how many notifications fire per
/// buffer cycle.  4 periods of headroom with a wake every ~2 periods keeps the
/// fill robust to scheduling jitter without piling on latency.
const RT_BUFFER_PERIODS: u32 = 4;
const RT_NOTIFICATION_COUNT: u32 = 2;

// ── KS property structs ────────────────────────────────────────────────────────

#[repr(C)]
struct KsProperty {
    set: GUID,
    id: u32,
    flags: u32,
}

#[repr(C)]
struct KsPinProperty {
    property: KsProperty,
    pin_id: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct KsMultipleItem {
    size: u32,
    count: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct KsDataRange {
    format_size: u32,
    flags: u32,
    sample_size: u32,
    reserved: u32,
    major_format: GUID,
    sub_format: GUID,
    specifier: GUID,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct KsDataRangeAudio {
    data_range: KsDataRange,
    maximum_channels: u32,
    minimum_bits_per_sample: u32,
    maximum_bits_per_sample: u32,
    minimum_sample_frequency: u32,
    maximum_sample_frequency: u32,
}

// ── Pin connect / data format (for KsCreatePin) ────────────────────────────────

/// `KSIDENTIFIER` — used for both `KSPIN_INTERFACE` and `KSPIN_MEDIUM`.
#[repr(C)]
#[derive(Clone, Copy)]
struct KsIdentifier {
    set: GUID,
    id: u32,
    flags: u32,
}

/// `KSPRIORITY`
#[repr(C)]
#[derive(Clone, Copy)]
struct KsPriority {
    priority_class: u32,
    priority_sub_class: u32,
}

/// `KSPIN_CONNECT` (72 bytes on x64).
#[repr(C)]
struct KsPinConnect {
    interface: KsIdentifier,
    medium: KsIdentifier,
    pin_id: u32,
    pin_to_handle: *mut c_void,
    priority: KsPriority,
}

/// `KSDATAFORMAT` header (== `KSDATARANGE` prefix, 64 bytes).
#[repr(C)]
struct KsDataFormat {
    format_size: u32,
    flags: u32,
    sample_size: u32,
    reserved: u32,
    major_format: GUID,
    sub_format: GUID,
    specifier: GUID,
}

/// `WAVEFORMATEX` (18 bytes with `cbSize`).
#[repr(C)]
struct WaveFormatEx {
    format_tag: u16,
    channels: u16,
    samples_per_sec: u32,
    avg_bytes_per_sec: u32,
    block_align: u16,
    bits_per_sample: u16,
    cb_size: u16,
}

// ── WaveRT property structs ────────────────────────────────────────────────────

#[repr(C)]
struct KsRtAudioBufferPropertyWithNotification {
    property: KsProperty,
    base_address: *mut c_void,
    requested_buffer_size: u32,
    notification_count: u32,
}

#[repr(C)]
struct KsRtAudioBufferProperty {
    property: KsProperty,
    base_address: *mut c_void,
    requested_buffer_size: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct KsRtAudioBuffer {
    buffer_address: *mut c_void,
    actual_buffer_size: u32,
    call_memory_barrier: i32,
}

#[repr(C)]
struct KsRtAudioNotificationEventProperty {
    property: KsProperty,
    notification_event: HANDLE,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct KsAudioPosition {
    play_offset: u64,
    write_offset: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct KsRtAudioHwLatency {
    fifo_size: u32,
    chipset_delay: u32,
    codec_delay: u32,
}

// ── Probe model ────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct KsAudioRangeProbe {
    subtype: AudioRangeSubtype,
    maximum_channels: u32,
    minimum_bits_per_sample: u32,
    maximum_bits_per_sample: u32,
    minimum_sample_frequency: u32,
    maximum_sample_frequency: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AudioRangeSubtype {
    Pcm,
    IeeeFloat,
    Other,
}

#[derive(Clone, Debug)]
pub struct WdmKsDeviceInfo {
    pub filter_path: String,
    pub pin_id: u32,
    pub name: String,
    pub channels: u32,
    pub default_sample_rate: u32,
}

#[derive(Clone, Debug)]
struct KsPinProbe {
    pin_id: u32,
    communication: Option<u32>,
    dataflow: Option<u32>,
    data_ranges_bytes: Option<u32>,
    audio_ranges: Vec<KsAudioRangeProbe>,
    render_candidate: bool,
}

/// Sample storage format negotiated for the pin.  The engine renders f32; this
/// is what we convert *to* when copying into the WaveRT buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SampleFmt {
    F32,
    I16,
    I24,
    I32,
}

impl SampleFmt {
    fn bytes(self) -> usize {
        match self {
            SampleFmt::F32 | SampleFmt::I32 => 4,
            SampleFmt::I16 => 2,
            SampleFmt::I24 => 3,
        }
    }
    fn bits(self) -> u16 {
        match self {
            SampleFmt::I16 => 16,
            SampleFmt::I24 => 24,
            SampleFmt::F32 | SampleFmt::I32 => 32,
        }
    }
    fn is_float(self) -> bool {
        matches!(self, SampleFmt::F32)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FormatCandidate {
    fmt: SampleFmt,
    channels: u16,
    sample_rate: u32,
}

// ── Public handle ──────────────────────────────────────────────────────────────

/// Handle to a running WDM-KS WaveRT stream.
///
/// Dropping signals the render thread to stop (sets `stop_flag` AND signals
/// `stop_event` so the thread wakes immediately) and joins it.
pub struct WdmKsHandle {
    pub cmd_tx: Sender<EngineCommand>,
    pub sample_rate: u32,
    pub buffer_size: u32,
    pub device_name: String,
    stop_flag: Arc<AtomicBool>,
    stop_event: HANDLE,
    thread: Option<thread::JoinHandle<()>>,
}

// Safety: HANDLE (a kernel event) is safe to move across threads.
unsafe impl Send for WdmKsHandle {}
unsafe impl Sync for WdmKsHandle {}

impl Drop for WdmKsHandle {
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

// ── open() — control thread: select target, spawn render thread ────────────────

pub fn open(
    config: &DauxDeviceConfig,
    shared: Arc<SharedState>,
    initial_runtime: RuntimeProject,
    glitch_counter: Arc<AtomicU64>,
) -> Result<WdmKsHandle, SphereAudioError> {
    // Selection + probing is a fast control-thread operation; do it up front so
    // device-not-found / no-render-pin failures are returned synchronously.
    let target = select_render_target(config.output_device_id.as_deref())?;
    let requested_sr = config.sample_rate;
    let period_frames = config
        .buffer_size
        .unwrap_or(if config.safe_mode { 512 } else { 256 })
        .clamp(32, 4096);
    let device_name = config
        .output_device_id
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| short_device_label(&target.path));

    let (tx, rx) = bounded::<EngineCommand>(512);
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop2 = Arc::clone(&stop_flag);
    let glitch2 = Arc::clone(&glitch_counter);

    let stop_event: HANDLE = unsafe {
        CreateEventW(None, true, false, None)
            .map_err(|e| SphereAudioError::StreamOpenFailed(format!("CreateEventW(stop): {e}")))?
    };
    let stop_event_usize = stop_event.0 as usize;

    let (info_tx, info_rx) = std::sync::mpsc::channel::<Result<(u32, u32, String), String>>();
    let handshake = Arc::new(OpenHandshake::new());
    let thread_handshake = Arc::clone(&handshake);
    let thread_shared = Arc::clone(&shared);

    let thread_name = device_name.clone();
    let t = thread::Builder::new()
        .name("daux-wdm-ks".into())
        .spawn(move || unsafe {
            let stop_ev = HANDLE(stop_event_usize as *mut _);
            wdm_ks_thread(
                target,
                requested_sr,
                period_frames,
                thread_name,
                rx,
                thread_shared,
                initial_runtime,
                glitch2,
                stop2,
                stop_ev,
                info_tx,
                &thread_handshake,
            );
        })
        .map_err(|e| {
            unsafe {
                let _ = CloseHandle(stop_event);
            }
            SphereAudioError::StreamOpenFailed(e.to_string())
        })?;

    let init = wait_for_device_thread(
        info_rx,
        &handshake,
        std::time::Duration::from_secs(8),
        "WDM-KS",
    );
    let (sample_rate, buffer_size, device_name) = match init {
        Ok(info) => info,
        Err(message) => {
            // Failed or abandoned (#381): stop the thread -- a late driver
            // finds the open abandoned and exits without rendering -- and join
            // it off this thread. The stop event outlives the thread.
            stop_flag.store(true, Ordering::Relaxed);
            unsafe {
                let _ = SetEvent(stop_event);
            }
            if handshake.is_abandoned() {
                // The thread leaves MMCSS state alone from here; the next
                // stream publishes its own.
                shared.mmcss_active.store(false, Ordering::Relaxed);
            }
            reap_device_thread("daux-wdm-ks", t, move || unsafe {
                let _ = CloseHandle(HANDLE(stop_event_usize as *mut _));
            });
            return Err(SphereAudioError::StreamOpenFailed(message));
        }
    };

    Ok(WdmKsHandle {
        cmd_tx: tx,
        sample_rate,
        buffer_size,
        device_name,
        stop_flag,
        stop_event,
        thread: Some(t),
    })
}

// ── Render thread ──────────────────────────────────────────────────────────────

struct RenderTarget {
    path: String,
    pin_id: u32,
    audio_ranges: Vec<KsAudioRangeProbe>,
}

/// Live WaveRT pin context built once the pin is created and its buffer mapped.
struct RtStream {
    pin: HANDLE,
    buffer: *mut u8,
    buffer_bytes: u64,
    block_align: u64,
    channels: usize,
    fmt: SampleFmt,
    sample_rate: u32,
    period_frames: u64,
    notify_event: Option<HANDLE>,
}

unsafe fn wdm_ks_thread(
    target: RenderTarget,
    requested_sr: Option<u32>,
    period_frames: u32,
    device_name: String,
    cmd_rx: Receiver<EngineCommand>,
    shared: Arc<SharedState>,
    initial_runtime: RuntimeProject,
    glitch_counter: Arc<AtomicU64>,
    stop_flag: Arc<AtomicBool>,
    stop_event: HANDLE,
    info_tx: std::sync::mpsc::Sender<Result<(u32, u32, String), String>>,
    handshake: &OpenHandshake,
) {
    // ── MMCSS "Pro Audio" ─────────────────────────────────────────────────────
    let task: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
    let mut task_idx = 0u32;
    let mmcss_h = AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut task_idx);
    if mmcss_h != 0 {
        eprintln!("[DAUx WDM-KS] MMCSS 'Pro Audio' set (index={task_idx})");
        shared.mmcss_active.store(true, Ordering::Relaxed);
    } else {
        eprintln!("[DAUx WDM-KS] MMCSS set failed (non-fatal)");
    }
    // An abandoned open handed the shared state to whichever stream the
    // caller opened next.
    let release_mmcss = || {
        cleanup_mmcss(mmcss_h);
        if !handshake.is_abandoned() {
            shared.mmcss_active.store(false, Ordering::Relaxed);
        }
    };

    // ── Open the filter handle (owned by this thread) ─────────────────────────
    let filter = match open_filter_handle(&target.path) {
        Ok(h) => h,
        Err(e) => {
            let _ = info_tx.send(Err(e.to_string()));
            release_mmcss();
            return;
        }
    };

    // ── Instantiate the pin + map the WaveRT buffer (tries candidate formats) ─
    let stream = match open_rt_stream(filter, &target, requested_sr, period_frames) {
        Ok(s) => s,
        Err(e) => {
            let _ = info_tx.send(Err(e));
            let _ = CloseHandle(filter);
            release_mmcss();
            return;
        }
    };

    eprintln!(
        "[DAUx WDM-KS] Pin {} streaming: sr={} ch={} fmt={:?} buffer={} frames ({} B) notify={}",
        target.pin_id,
        stream.sample_rate,
        stream.channels,
        stream.fmt,
        stream.buffer_bytes / stream.block_align,
        stream.buffer_bytes,
        stream.notify_event.is_some(),
    );

    run_rt_stream(
        &stream,
        device_name,
        cmd_rx,
        &shared,
        initial_runtime,
        &glitch_counter,
        &stop_flag,
        stop_event,
        &info_tx,
        handshake,
    );

    // ── Teardown ──────────────────────────────────────────────────────────────
    let _ = set_pin_state(stream.pin, KSSTATE_PAUSE);
    let _ = set_pin_state(stream.pin, KSSTATE_ACQUIRE);
    let _ = set_pin_state(stream.pin, KSSTATE_STOP);
    if let Some(ev) = stream.notify_event {
        let _ = CloseHandle(ev);
    }
    let _ = CloseHandle(stream.pin);
    let _ = CloseHandle(filter);
    release_mmcss();
    eprintln!("[DAUx WDM-KS] Stopped");
}

/// Try the negotiated candidate formats in order; the first one that both
/// instantiates the pin (`KsCreatePin`) and maps a WaveRT buffer wins.
unsafe fn open_rt_stream(
    filter: HANDLE,
    target: &RenderTarget,
    requested_sr: Option<u32>,
    period_frames: u32,
) -> Result<RtStream, String> {
    let candidates = build_format_candidates(&target.audio_ranges, requested_sr);
    let mut last_err = String::from("no usable render format");

    for cand in candidates {
        let pin = match create_pin(filter, target.pin_id, &cand) {
            Ok(p) => p,
            Err(e) => {
                last_err = format!("KsCreatePin({cand:?}) failed: {e}");
                continue;
            }
        };

        match map_rt_buffer(pin, &cand, period_frames) {
            Ok((buffer, buffer_bytes, notify_event)) => {
                let block_align = (cand.channels as usize * cand.fmt.bytes()) as u64;
                // Round the mapped buffer down to a whole number of frames so
                // our position arithmetic never straddles a partial frame.
                let buffer_bytes = (buffer_bytes / block_align) * block_align;
                if buffer_bytes < block_align {
                    let _ = CloseHandle(pin);
                    last_err = "WaveRT buffer smaller than one frame".into();
                    continue;
                }
                return Ok(RtStream {
                    pin,
                    buffer,
                    buffer_bytes,
                    block_align,
                    channels: cand.channels as usize,
                    fmt: cand.fmt,
                    sample_rate: cand.sample_rate,
                    period_frames: period_frames as u64,
                    notify_event,
                });
            }
            Err(e) => {
                let _ = CloseHandle(pin);
                last_err = format!("WaveRT buffer for {cand:?} failed: {e}");
            }
        }
    }

    Err(format!(
        "DAUx WDM-KS pin {} is not WaveRT-streamable (last: {last_err}). \
         Legacy WaveCyclic streaming is not implemented yet.",
        target.pin_id
    ))
}

/// The event-driven render loop.  Mirrors the WASAPI exclusive backend: render
/// the shared engine block, but into the WaveRT cyclic buffer ahead of the
/// hardware play cursor.
unsafe fn run_rt_stream(
    stream: &RtStream,
    device_name: String,
    cmd_rx: Receiver<EngineCommand>,
    shared: &Arc<SharedState>,
    initial_runtime: RuntimeProject,
    glitch_counter: &Arc<AtomicU64>,
    stop_flag: &Arc<AtomicBool>,
    stop_event: HANDLE,
    info_tx: &std::sync::mpsc::Sender<Result<(u32, u32, String), String>>,
    handshake: &OpenHandshake,
) {
    let sample_rate = stream.sample_rate;
    let block_align = stream.block_align;
    let buffer_bytes = stream.buffer_bytes;
    let channels = stream.channels;
    let buffer_frames = buffer_bytes / block_align;

    // Pre-fill the whole buffer with silence before RUN so the DMA engine
    // starts on valid audio rather than whatever the driver mapped. Silence,
    // not a render: the stream is not this thread's until the handshake
    // below, and the runtime's plug-in processors are the live ones (#381).
    // Every negotiated word (float, signed PCM) is silent at all-zero bytes;
    // the render loop fills ahead of the play cursor from the first wakeup.
    std::ptr::write_bytes(stream.buffer, 0, buffer_bytes as usize);
    std::sync::atomic::fence(Ordering::SeqCst);
    let mut write_total: u64 = buffer_frames * block_align;

    // STOP → ACQUIRE → PAUSE → RUN (KS requires single-step transitions).
    for state in [KSSTATE_ACQUIRE, KSSTATE_PAUSE, KSSTATE_RUN] {
        if let Err(e) = set_pin_state(stream.pin, state) {
            let _ = info_tx.send(Err(format!("set pin state {state} failed: {e}")));
            return;
        }
    }

    // A driver slower than `open()`'s wait lands here after the open was
    // abandoned: return to the caller's teardown, which releases the pin,
    // without touching the runtime or the shared state.
    if !handshake.claim() {
        eprintln!(
            "[DAUx WDM-KS] '{device_name}' finished initialising after the open was abandoned; released without rendering"
        );
        return;
    }
    shared.sample_rate.store(sample_rate, Ordering::Relaxed);
    if info_tx
        .send(Ok((
            sample_rate,
            stream.period_frames as u32,
            device_name.clone(),
        )))
        .is_err()
    {
        // `open()` is gone (it waits for this send after a claim, so only a
        // panic there gets here): nobody owns this stream.
        return;
    }

    // Realtime runtime + scratch (period-sized; the fill loop chunks by period).
    let mut runtime = initial_runtime;
    runtime.retarget_sample_rate(sample_rate);
    runtime.reserve_track_blocks(stream.period_frames as usize);
    let local =
        LocalAudioState::with_monitor_capacity(sample_rate as f64, stream.period_frames as usize);
    let mut scratch = vec![0.0f32; stream.period_frames as usize * channels.max(1)];
    // Audioguard owns the render state: each period is rendered here, or
    // copied from the blocks the guard thread rendered ahead.
    let mut guarded = crate::audioguard::GuardedRenderer::new(
        crate::audioguard::RenderCore::new(runtime, local, cmd_rx, sample_rate),
        channels.max(1),
        Arc::clone(shared),
    );

    // Monotonic reconstruction of the wrapping hardware play offset.
    let mut play_base: u64 = 0;
    let mut last_play_off: u64 = 0;
    // Poll cadence when the driver has no notification event.
    let poll_ms = ((stream.period_frames * 1000) / sample_rate.max(1) as u64 / 2).max(1) as u32;

    loop {
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }

        // Wait for the next notification (event-driven) or poll the position.
        match stream.notify_event {
            Some(notify) => {
                let handles = [notify, stop_event];
                let wait = WaitForMultipleObjects(&handles, false, 2000);
                if wait == WAIT_OBJECT_0 {
                    // notification — fall through and fill.
                } else if wait.0 == 1 {
                    break; // stop_event
                } else {
                    // Timeout / abandoned while not stopping ⇒ device went away.
                    if !stop_flag.load(Ordering::Relaxed) {
                        glitch_counter.fetch_add(1, Ordering::Relaxed);
                        shared.device_lost.store(true, Ordering::Relaxed);
                    }
                    break;
                }
            }
            None => {
                // Polling mode: a bounded wait on stop_event doubles as the
                // poll sleep without a raw sleep() call.
                if WaitForSingleObject(stop_event, poll_ms) == WAIT_OBJECT_0 {
                    break;
                }
            }
        }
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }

        // Reconstruct a monotonic play position from the wrapping offset.
        let play_off = match get_play_offset(stream.pin) {
            Ok(off) => off.min(buffer_bytes.saturating_sub(1)),
            Err(_) => {
                glitch_counter.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        if play_off < last_play_off {
            play_base = play_base.wrapping_add(buffer_bytes);
        }
        last_play_off = play_off;
        let play_total = play_base + play_off;

        // Free space ahead of the write cursor (keep the buffer full ⇒ latency
        // is bounded by the buffer itself).
        let queued = write_total.saturating_sub(play_total);
        let free_bytes = buffer_bytes.saturating_sub(queued);
        let free_frames = free_bytes / block_align;
        if free_frames == 0 {
            continue;
        }

        render_into_ring(stream, write_total, free_frames, &mut scratch, &mut guarded);
        write_total += free_frames * block_align;
    }
}

/// Render `frames` engine frames into the WaveRT cyclic buffer starting at the
/// monotonic byte offset `start_total`, wrapping at the buffer end.  Filled in
/// period-sized chunks so transport/MIDI scheduling sees sane block sizes.
///
/// Realtime-safe: preallocated scratch, atomics + arithmetic only.
unsafe fn render_into_ring(
    stream: &RtStream,
    start_total: u64,
    frames: u64,
    scratch: &mut [f32],
    guarded: &mut crate::audioguard::GuardedRenderer,
) {
    let ch = stream.channels.max(1);
    let bps = stream.fmt.bytes() as u64;
    let buffer_bytes = stream.buffer_bytes;
    let mut written_total = start_total;
    let mut remaining = frames;

    while remaining > 0 {
        let chunk = remaining.min(stream.period_frames) as usize;
        let n = chunk * ch;
        // `fill_output_f32` fully overwrites every sample of its output slice
        // on every reachable path (see `backend/render.rs`) — no pre-zero
        // needed.
        // Drains the command queue too.
        guarded.render(&mut scratch[..n]);

        for (i, &s) in scratch[..n].iter().enumerate() {
            let byte_off = ((written_total + i as u64 * bps) % buffer_bytes) as usize;
            write_sample(stream.buffer.add(byte_off), stream.fmt, s);
        }
        written_total += n as u64 * bps;
        remaining -= chunk as u64;
    }

    // Publish the writes before the DMA engine reads them.
    std::sync::atomic::fence(Ordering::SeqCst);
}

#[inline]
unsafe fn write_sample(dst: *mut u8, fmt: SampleFmt, v: f32) {
    match fmt {
        SampleFmt::F32 => {
            let b = v.to_le_bytes();
            std::ptr::copy_nonoverlapping(b.as_ptr(), dst, 4);
        }
        SampleFmt::I16 => {
            let x = (v.clamp(-1.0, 1.0) * 32767.0) as i16;
            let b = x.to_le_bytes();
            std::ptr::copy_nonoverlapping(b.as_ptr(), dst, 2);
        }
        SampleFmt::I24 => {
            let x = (v.clamp(-1.0, 1.0) * 8_388_607.0) as i32;
            *dst = (x & 0xFF) as u8;
            *dst.add(1) = ((x >> 8) & 0xFF) as u8;
            *dst.add(2) = ((x >> 16) & 0xFF) as u8;
        }
        SampleFmt::I32 => {
            let x = (v.clamp(-1.0, 1.0) * 2_147_483_647.0) as i32;
            let b = x.to_le_bytes();
            std::ptr::copy_nonoverlapping(b.as_ptr(), dst, 4);
        }
    }
}

// ── Pin instantiation + WaveRT buffer mapping ──────────────────────────────────

/// Build `KSPIN_CONNECT` + `KSDATAFORMAT_WAVEFORMATEX` and call `KsCreatePin`.
unsafe fn create_pin(
    filter: HANDLE,
    pin_id: u32,
    cand: &FormatCandidate,
) -> Result<HANDLE, String> {
    let bits = cand.fmt.bits();
    let block_align = cand.channels * (bits / 8);
    let avg_bytes = cand.sample_rate * block_align as u32;

    let connect = KsPinConnect {
        interface: KsIdentifier {
            set: KSINTERFACESETID_STANDARD,
            id: KSINTERFACE_STANDARD_STREAMING,
            flags: 0,
        },
        medium: KsIdentifier {
            set: KSMEDIUMSETID_STANDARD,
            id: KSMEDIUM_STANDARD_DEVIO,
            flags: 0,
        },
        pin_id,
        pin_to_handle: std::ptr::null_mut(),
        priority: KsPriority {
            priority_class: KSPRIORITY_NORMAL,
            priority_sub_class: 1,
        },
    };

    let wfx = WaveFormatEx {
        format_tag: if cand.fmt.is_float() {
            WAVE_FORMAT_IEEE_FLOAT
        } else {
            WAVE_FORMAT_PCM
        },
        channels: cand.channels,
        samples_per_sec: cand.sample_rate,
        avg_bytes_per_sec: avg_bytes,
        block_align,
        bits_per_sample: bits,
        cb_size: 0,
    };

    let data_format = KsDataFormat {
        format_size: (size_of::<KsDataFormat>() + size_of::<WaveFormatEx>()) as u32,
        flags: 0,
        sample_size: block_align as u32,
        reserved: 0,
        major_format: KSDATAFORMAT_TYPE_AUDIO,
        sub_format: if cand.fmt.is_float() {
            KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
        } else {
            KSDATAFORMAT_SUBTYPE_PCM
        },
        specifier: KSDATAFORMAT_SPECIFIER_WAVEFORMATEX,
    };

    // KsCreatePin expects KSPIN_CONNECT immediately followed by the data format.
    let mut buf = vec![0u8; size_of::<KsPinConnect>() + data_format.format_size as usize];
    let connect_size = size_of::<KsPinConnect>();
    let df_size = size_of::<KsDataFormat>();
    std::ptr::copy_nonoverlapping(
        (&connect as *const KsPinConnect).cast::<u8>(),
        buf.as_mut_ptr(),
        connect_size,
    );
    std::ptr::copy_nonoverlapping(
        (&data_format as *const KsDataFormat).cast::<u8>(),
        buf.as_mut_ptr().add(connect_size),
        df_size,
    );
    std::ptr::copy_nonoverlapping(
        (&wfx as *const WaveFormatEx).cast::<u8>(),
        buf.as_mut_ptr().add(connect_size + df_size),
        size_of::<WaveFormatEx>(),
    );

    // Render (sink) pins are write-only; some KS miniports reject READ|WRITE on
    // a render pin. KS property IOCTLs (GETPOSITIONS) use FILE_ANY_ACCESS, so a
    // write-only handle still services position queries.
    let mut pin = HANDLE::default();
    let rc = KsCreatePin(filter, buf.as_ptr().cast(), GENERIC_WRITE.0, &mut pin);
    if rc != 0 {
        return Err(format!("Win32 error {rc}"));
    }
    if pin.is_invalid() {
        return Err("KsCreatePin returned an invalid handle".into());
    }
    Ok(pin)
}

/// Allocate + map the WaveRT cyclic buffer.  Tries the notification-capable
/// variant first (event-driven), then plain buffer (polling).  Returns the
/// mapped pointer, its byte size, and the notification event when registered.
unsafe fn map_rt_buffer(
    pin: HANDLE,
    cand: &FormatCandidate,
    period_frames: u32,
) -> Result<(*mut u8, u64, Option<HANDLE>), String> {
    let block_align = cand.channels as u32 * (cand.fmt.bits() / 8) as u32;
    let requested = period_frames
        .saturating_mul(block_align)
        .saturating_mul(RT_BUFFER_PERIODS);

    // ── Event-driven (BUFFER_WITH_NOTIFICATION) ───────────────────────────────
    let event: HANDLE = match CreateEventW(None, false, false, None) {
        Ok(h) => h,
        Err(e) => return Err(format!("CreateEventW(notify): {e}")),
    };

    let prop = KsRtAudioBufferPropertyWithNotification {
        property: KsProperty {
            set: KSPROPSETID_RTAUDIO,
            id: KSPROPERTY_RTAUDIO_BUFFER_WITH_NOTIFICATION,
            flags: KSPROPERTY_TYPE_GET,
        },
        base_address: std::ptr::null_mut(),
        requested_buffer_size: requested,
        notification_count: RT_NOTIFICATION_COUNT,
    };
    let mut out: KsRtAudioBuffer = zeroed();
    let notif_ok = ks_ioctl(
        pin,
        std::ptr::addr_of!(prop).cast(),
        size_of::<KsRtAudioBufferPropertyWithNotification>() as u32,
        std::ptr::addr_of_mut!(out).cast(),
        size_of::<KsRtAudioBuffer>() as u32,
    )
    .is_ok()
        && !out.buffer_address.is_null();

    if notif_ok {
        // Register our event for the driver's per-cycle notifications.
        let mut reg = KsRtAudioNotificationEventProperty {
            property: KsProperty {
                set: KSPROPSETID_RTAUDIO,
                id: KSPROPERTY_RTAUDIO_REGISTER_NOTIFICATION_EVENT,
                flags: KSPROPERTY_TYPE_GET,
            },
            notification_event: event,
        };
        let reg_ok = ks_ioctl(
            pin,
            std::ptr::addr_of!(reg).cast(),
            size_of::<KsRtAudioNotificationEventProperty>() as u32,
            std::ptr::addr_of_mut!(reg).cast(),
            size_of::<KsRtAudioNotificationEventProperty>() as u32,
        )
        .is_ok();
        if reg_ok {
            return Ok((
                out.buffer_address.cast(),
                out.actual_buffer_size as u64,
                Some(event),
            ));
        }
        // Buffer mapped but notification registration failed — fall back to
        // polling on the already-mapped buffer.
        let _ = CloseHandle(event);
        return Ok((
            out.buffer_address.cast(),
            out.actual_buffer_size as u64,
            None,
        ));
    }

    let _ = CloseHandle(event);

    // ── Polling (plain BUFFER) ────────────────────────────────────────────────
    let prop = KsRtAudioBufferProperty {
        property: KsProperty {
            set: KSPROPSETID_RTAUDIO,
            id: KSPROPERTY_RTAUDIO_BUFFER,
            flags: KSPROPERTY_TYPE_GET,
        },
        base_address: std::ptr::null_mut(),
        requested_buffer_size: requested,
    };
    let mut out: KsRtAudioBuffer = zeroed();
    ks_ioctl(
        pin,
        std::ptr::addr_of!(prop).cast(),
        size_of::<KsRtAudioBufferProperty>() as u32,
        std::ptr::addr_of_mut!(out).cast(),
        size_of::<KsRtAudioBuffer>() as u32,
    )?;
    if out.buffer_address.is_null() {
        return Err("RTAUDIO_BUFFER returned a null mapping".into());
    }
    Ok((
        out.buffer_address.cast(),
        out.actual_buffer_size as u64,
        None,
    ))
}

unsafe fn set_pin_state(pin: HANDLE, state: u32) -> Result<(), String> {
    let prop = KsProperty {
        set: KSPROPSETID_CONNECTION,
        id: KSPROPERTY_CONNECTION_STATE,
        flags: KSPROPERTY_TYPE_SET,
    };
    let mut value = state;
    ks_ioctl(
        pin,
        std::ptr::addr_of!(prop).cast(),
        size_of::<KsProperty>() as u32,
        std::ptr::addr_of_mut!(value).cast(),
        size_of::<u32>() as u32,
    )
    .map(|_| ())
}

unsafe fn get_play_offset(pin: HANDLE) -> Result<u64, String> {
    let prop = KsProperty {
        set: KSPROPSETID_RTAUDIO,
        id: KSPROPERTY_RTAUDIO_GETPOSITIONS,
        flags: KSPROPERTY_TYPE_GET,
    };
    let mut pos = KsAudioPosition::default();
    ks_ioctl(
        pin,
        std::ptr::addr_of!(prop).cast(),
        size_of::<KsProperty>() as u32,
        std::ptr::addr_of_mut!(pos).cast(),
        size_of::<KsAudioPosition>() as u32,
    )?;
    Ok(pos.play_offset)
}

#[allow(dead_code)]
unsafe fn get_hw_latency(pin: HANDLE) -> Option<KsRtAudioHwLatency> {
    let prop = KsProperty {
        set: KSPROPSETID_RTAUDIO,
        id: KSPROPERTY_RTAUDIO_HWLATENCY,
        flags: KSPROPERTY_TYPE_GET,
    };
    let mut lat = KsRtAudioHwLatency::default();
    ks_ioctl(
        pin,
        std::ptr::addr_of!(prop).cast(),
        size_of::<KsProperty>() as u32,
        std::ptr::addr_of_mut!(lat).cast(),
        size_of::<KsRtAudioHwLatency>() as u32,
    )
    .ok()
    .map(|_| lat)
}

// ── Format negotiation ─────────────────────────────────────────────────────────

/// Build an ordered list of formats to try, derived from the pin's reported
/// data ranges plus broadly-accepted defaults.  Float/16-bit stereo first.
fn build_format_candidates(
    ranges: &[KsAudioRangeProbe],
    requested_sr: Option<u32>,
) -> Vec<FormatCandidate> {
    let mut out: Vec<FormatCandidate> = Vec::new();
    let push = |c: FormatCandidate, out: &mut Vec<FormatCandidate>| {
        if !out.contains(&c) {
            out.push(c);
        }
    };

    for range in ranges {
        let channels: u16 = if range.maximum_channels >= 2 { 2 } else { 1 };
        let sr = pick_sample_rate(
            requested_sr,
            range.minimum_sample_frequency,
            range.maximum_sample_frequency,
        );
        match range.subtype {
            AudioRangeSubtype::IeeeFloat => {
                push(
                    FormatCandidate {
                        fmt: SampleFmt::F32,
                        channels,
                        sample_rate: sr,
                    },
                    &mut out,
                );
            }
            AudioRangeSubtype::Pcm => {
                let fmt =
                    pick_pcm_fmt(range.minimum_bits_per_sample, range.maximum_bits_per_sample);
                push(
                    FormatCandidate {
                        fmt,
                        channels,
                        sample_rate: sr,
                    },
                    &mut out,
                );
            }
            AudioRangeSubtype::Other => {}
        }
    }

    // Fallbacks if the pin reported no usable ranges or its preferred format is
    // rejected by KsCreatePin / RtAudio. Do not bake in a 48 kHz runtime
    // assumption; prefer the caller's requested rate, otherwise use the generic
    // selector over the device-declared range.
    let fallback_sr = pick_sample_rate(requested_sr, 0, 0);
    for sr in [requested_sr.unwrap_or(fallback_sr), fallback_sr, 44_100] {
        for fmt in [SampleFmt::I16, SampleFmt::F32, SampleFmt::I32] {
            push(
                FormatCandidate {
                    fmt,
                    channels: 2,
                    sample_rate: sr,
                },
                &mut out,
            );
        }
    }

    out.truncate(16);
    out
}

fn pick_sample_rate(requested: Option<u32>, min: u32, max: u32) -> u32 {
    let (min, max) = if min == 0 && max == 0 {
        (8_000, 192_000)
    } else {
        (min.max(1), max.max(min.max(1)))
    };
    if let Some(req) = requested.filter(|&r| r >= min && r <= max) {
        return req;
    }
    let preferred = if (44_100..=96_000).contains(&max) {
        max
    } else {
        max.min(96_000).max(min)
    };
    preferred.clamp(min, max)
}

fn pick_pcm_fmt(min_bits: u32, max_bits: u32) -> SampleFmt {
    let in_range = |b: u32| b >= min_bits.max(1) && b <= max_bits.max(min_bits.max(1));
    if in_range(16) {
        SampleFmt::I16
    } else if in_range(24) {
        SampleFmt::I24
    } else if in_range(32) {
        SampleFmt::I32
    } else {
        SampleFmt::I16
    }
}

// ── Filter + pin selection (control thread) ────────────────────────────────────

fn select_render_target(preferred_path: Option<&str>) -> Result<RenderTarget, SphereAudioError> {
    let filters = enumerate_audio_filters()?;
    if let Some(preferred) = preferred_path.filter(|s| !s.is_empty()) {
        let selected = filters
            .into_iter()
            .find(|path| path.eq_ignore_ascii_case(preferred))
            .ok_or_else(|| {
                SphereAudioError::DeviceNotFound(format!(
                    "WDM-KS filter path not found: {preferred}"
                ))
            })?;
        let pins = probe_filter_pins(&selected)?;
        return render_target_from_pins(selected, &pins);
    }

    if filters.is_empty() {
        return Err(SphereAudioError::BackendUnavailable(
            "No WDM-KS audio filter device interfaces were found".into(),
        ));
    }

    // Probe every interface; on failure, classify why.  Many drivers register
    // several KS interfaces per physical device (wave + topology, AUDIO + RENDER
    // categories), so dedupe the reasons by device label to avoid a wall of
    // identical errors in the driver-status badge.
    let mut reasons: Vec<String> = Vec::new();
    let mut had_failure = false;
    let mut all_propset_missing = true;
    for filter_path in filters {
        match probe_filter_pins(&filter_path)
            .and_then(|pins| render_target_from_pins(filter_path.clone(), &pins))
        {
            Ok(target) => return Ok(target),
            Err(error) => {
                had_failure = true;
                let text = error.to_string();
                if !is_pin_propset_missing(&text) {
                    all_propset_missing = false;
                }
                let entry = format!(
                    "{}: {}",
                    short_device_label(&filter_path),
                    concise_reason(&text)
                );
                if !reasons.contains(&entry) {
                    reasons.push(entry);
                }
            }
        }
    }

    // When *every* interface rejects the KS pin property set (0x80070492
    // ERROR_SET_NOT_FOUND), the machine simply does not expose user-mode kernel
    // streaming — typical of Intel Smart Sound (SST) / SoundWire / "Universal
    // Audio" stacks where only the Windows audio engine may stream.  Say so
    // plainly and point at the backend that does work, instead of dumping a raw
    // IOCTL error the user can't act on.
    if had_failure && all_propset_missing {
        return Err(SphereAudioError::BackendUnavailable(
            "This system does not expose user-mode WDM-KS streaming pins: every audio \
             filter rejected the KS pin property set (0x80070492 ERROR_SET_NOT_FOUND). \
             This is normal on Intel Smart Sound (SST) / SoundWire / \"Universal Audio\" \
             driver stacks, which only allow streaming through the Windows audio engine. \
             Use WASAPI Exclusive for low latency on this device."
                .into(),
        ));
    }

    Err(SphereAudioError::BackendUnavailable(format!(
        "No render-capable WDM-KS filter was found ({} device(s) probed): {}",
        reasons.len(),
        reasons.join("; ")
    )))
}

/// True when a probe error is the "KS pin property set is not present" failure
/// (Win32 1170 `ERROR_SET_NOT_FOUND`, surfaced as HRESULT `0x80070492`).
fn is_pin_propset_missing(error: &str) -> bool {
    error.contains("0x80070492") || error.contains("property set specified does not exist")
}

/// Collapse a verbose probe error into a short, badge-friendly reason.
fn concise_reason(error: &str) -> &str {
    if is_pin_propset_missing(error) {
        "no KS pin property set (0x80070492)"
    } else {
        error
    }
}

fn probe_filter_pins(filter_path: &str) -> Result<Vec<KsPinProbe>, SphereAudioError> {
    let handle = open_filter_handle(filter_path)?;
    let result = (|| {
        let pin_count = query_pin_count(handle)?;
        Ok(query_pin_probes(handle, pin_count))
    })();
    unsafe {
        let _ = CloseHandle(handle);
    }
    result
}

fn render_target_from_pins(
    filter_path: String,
    pins: &[KsPinProbe],
) -> Result<RenderTarget, SphereAudioError> {
    let chosen = choose_render_pin(pins).ok_or_else(|| {
        SphereAudioError::StreamOpenFailed(format!(
            "DAUx WDM-KS filter '{}' has no render-capable pin (pins={})",
            filter_path,
            format_pin_summary(pins)
        ))
    })?;

    Ok(RenderTarget {
        path: filter_path,
        pin_id: chosen.pin_id,
        audio_ranges: chosen.audio_ranges.clone(),
    })
}

/// Prefer a probed render candidate (most channels wins, then lowest pin id);
/// fall back to any host-data sink pin if range parsing came up empty.
fn choose_render_pin(pins: &[KsPinProbe]) -> Option<&KsPinProbe> {
    pins.iter()
        .filter(|p| p.render_candidate)
        .max_by(|a, b| {
            let am = a
                .audio_ranges
                .iter()
                .map(|r| r.maximum_channels)
                .max()
                .unwrap_or(0);
            let bm = b
                .audio_ranges
                .iter()
                .map(|r| r.maximum_channels)
                .max()
                .unwrap_or(0);
            am.cmp(&bm).then(b.pin_id.cmp(&a.pin_id))
        })
        .or_else(|| {
            pins.iter()
                .find(|p| p.dataflow == Some(1) && matches!(p.communication, Some(1 | 3)))
        })
        .or_else(|| pins.iter().find(|p| p.dataflow == Some(1)))
}

fn short_device_label(path: &str) -> String {
    // Device interface paths are long and unfriendly; surface a compact tail.
    let trimmed = path.trim_start_matches("\\\\?\\");
    match trimmed.split('#').nth(1) {
        Some(id) if !id.is_empty() => format!("WDM-KS {id}"),
        _ => "WDM-KS Device".into(),
    }
}

pub fn list_output_devices() -> Vec<WdmKsDeviceInfo> {
    let Ok(filters) = enumerate_audio_filters() else {
        return Vec::new();
    };

    filters
        .into_iter()
        .filter_map(|filter_path| {
            let pins = probe_filter_pins(&filter_path).ok()?;
            let pin = choose_render_pin(&pins)?;
            let channels = pin
                .audio_ranges
                .iter()
                .map(|range| range.maximum_channels)
                .max()
                .unwrap_or(0);
            let default_sample_rate = pin
                .audio_ranges
                .first()
                .map(|range| {
                    pick_sample_rate(
                        None,
                        range.minimum_sample_frequency,
                        range.maximum_sample_frequency,
                    )
                })
                .unwrap_or(0);
            Some(WdmKsDeviceInfo {
                name: format!("{} (pin {})", short_device_label(&filter_path), pin.pin_id),
                filter_path,
                pin_id: pin.pin_id,
                channels,
                default_sample_rate,
            })
        })
        .collect()
}

// ── Diagnostics (control thread) ───────────────────────────────────────────────

/// Enumerate every KS audio/render interface and report, per interface, whether
/// the KS pin property set (`KSPROPSETID_Pin`) is reachable and what pins it
/// exposes.  Control-thread only; surfaced by the `ks-probe` CLI command so we
/// can see real hardware topology instead of guessing.
pub fn diagnose_report() -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for (label, category) in [
        ("KSCATEGORY_AUDIO", KSCATEGORY_AUDIO),
        ("KSCATEGORY_RENDER", KSCATEGORY_RENDER),
    ] {
        let _ = writeln!(out, "== {label} ==");
        match enumerate_filter_category(label, &category) {
            Ok(paths) if paths.is_empty() => {
                let _ = writeln!(out, "  (no interfaces)");
            }
            Ok(paths) => {
                for path in paths {
                    let _ = writeln!(out, "  interface: {path}");
                    match open_filter_handle(&path) {
                        Ok(handle) => {
                            match query_pin_count(handle) {
                                Ok(n) => {
                                    let pins = query_pin_probes(handle, n);
                                    let _ =
                                        writeln!(out, "    pins={n} {}", format_pin_summary(&pins));
                                }
                                Err(e) => {
                                    let _ = writeln!(out, "    CTYPES failed: {e}");
                                }
                            }
                            unsafe {
                                let _ = CloseHandle(handle);
                            }
                        }
                        Err(e) => {
                            let _ = writeln!(out, "    open failed: {e}");
                        }
                    }
                }
            }
            Err(e) => {
                let _ = writeln!(out, "  enumerate failed: {e}");
            }
        }
    }

    let _ = writeln!(out, "== select_render_target(None) ==");
    match select_render_target(None) {
        Ok(t) => {
            let _ = writeln!(out, "  OK: pin {} on {}", t.pin_id, t.path);
        }
        Err(e) => {
            let _ = writeln!(out, "  Err: {e}");
        }
    }
    out
}

// ── SetupAPI enumeration + pin probing (control thread) ─────────────────────────

fn enumerate_audio_filters() -> Result<Vec<String>, SphereAudioError> {
    let mut paths = Vec::new();
    let mut errors = Vec::new();
    for (label, category) in [
        ("KSCATEGORY_RENDER", KSCATEGORY_RENDER),
        ("KSCATEGORY_AUDIO", KSCATEGORY_AUDIO),
    ] {
        match enumerate_filter_category(label, &category) {
            Ok(category_paths) => {
                for path in category_paths {
                    if !paths
                        .iter()
                        .any(|existing: &String| existing.eq_ignore_ascii_case(&path))
                    {
                        paths.push(path);
                    }
                }
            }
            Err(error) => errors.push(error.to_string()),
        }
    }

    if !paths.is_empty() {
        return Ok(paths);
    }

    Err(SphereAudioError::BackendUnavailable(format!(
        "No WDM-KS audio/render filter device interfaces were found ({})",
        errors.join("; ")
    )))
}

fn enumerate_filter_category(
    label: &str,
    category: &GUID,
) -> Result<Vec<String>, SphereAudioError> {
    unsafe {
        let info = SetupDiGetClassDevsW(
            Some(category),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
        .map_err(|e| {
            SphereAudioError::BackendUnavailable(format!(
                "SetupDiGetClassDevsW({label}) failed: {e}"
            ))
        })?;

        let mut paths = Vec::new();
        let mut index = 0u32;
        loop {
            let mut iface: SP_DEVICE_INTERFACE_DATA = zeroed();
            iface.cbSize = size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;

            if SetupDiEnumDeviceInterfaces(info, None, category, index, &mut iface).is_err() {
                break;
            }

            match device_interface_path(info, &mut iface) {
                Ok(path) => paths.push(path),
                Err(error) => eprintln!("[DAUx WDM-KS] skip interface {index}: {error}"),
            }
            index += 1;
        }

        let _ = SetupDiDestroyDeviceInfoList(info);
        Ok(paths)
    }
}

unsafe fn device_interface_path(
    info: windows::Win32::Devices::DeviceAndDriverInstallation::HDEVINFO,
    iface: &mut SP_DEVICE_INTERFACE_DATA,
) -> Result<String, String> {
    let mut required_size = 0u32;
    let _ = SetupDiGetDeviceInterfaceDetailW(info, iface, None, 0, Some(&mut required_size), None);
    if required_size == 0 {
        return Err(format!(
            "SetupDiGetDeviceInterfaceDetailW(size) failed: {:?}",
            GetLastError()
        ));
    }

    let mut buffer = vec![0u8; required_size as usize];
    let detail = buffer.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
    (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;

    SetupDiGetDeviceInterfaceDetailW(
        info,
        iface,
        Some(detail),
        required_size,
        Some(&mut required_size),
        None,
    )
    .map_err(|e| format!("SetupDiGetDeviceInterfaceDetailW(data) failed: {e}"))?;

    let path_ptr = (*detail).DevicePath.as_ptr();
    let mut len = 0usize;
    while *path_ptr.add(len) != 0 {
        len += 1;
    }
    Ok(String::from_utf16_lossy(std::slice::from_raw_parts(
        path_ptr, len,
    )))
}

fn open_filter_handle(path: &str) -> Result<HANDLE, SphereAudioError> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
        .map_err(|e| {
            SphereAudioError::StreamOpenFailed(format!(
                "CreateFileW(WDM-KS filter) failed for '{}': {e}",
                path
            ))
        })
    }
}

fn query_pin_count(handle: HANDLE) -> Result<u32, SphereAudioError> {
    let prop = KsProperty {
        set: KSPROPSETID_PIN,
        id: KSPROPERTY_PIN_CTYPES,
        flags: KSPROPERTY_TYPE_GET,
    };
    let mut pin_count = 0u32;
    ks_property_get(handle, &prop, &mut pin_count).map_err(|e| {
        SphereAudioError::StreamOpenFailed(format!(
            "IOCTL_KS_PROPERTY(KSPROPERTY_PIN_CTYPES) failed: {e}"
        ))
    })?;
    Ok(pin_count)
}

fn query_pin_probes(handle: HANDLE, pin_count: u32) -> Vec<KsPinProbe> {
    (0..pin_count)
        .map(|pin_id| {
            let communication = query_pin_u32(handle, pin_id, KSPROPERTY_PIN_COMMUNICATION).ok();
            let dataflow = query_pin_u32(handle, pin_id, KSPROPERTY_PIN_DATAFLOW).ok();
            let (data_ranges_bytes, audio_ranges) = query_pin_data_ranges(handle, pin_id)
                .map(|(bytes, ranges)| (Some(bytes), ranges))
                .unwrap_or((None, Vec::new()));
            let render_candidate = is_render_candidate(communication, dataflow, &audio_ranges);

            KsPinProbe {
                pin_id,
                communication,
                dataflow,
                data_ranges_bytes,
                audio_ranges,
                render_candidate,
            }
        })
        .collect()
}

fn query_pin_u32(handle: HANDLE, pin_id: u32, property_id: u32) -> Result<u32, String> {
    let prop = KsPinProperty {
        property: KsProperty {
            set: KSPROPSETID_PIN,
            id: property_id,
            flags: KSPROPERTY_TYPE_GET,
        },
        pin_id,
        reserved: 0,
    };
    let mut value = 0u32;
    ks_property_get(handle, &prop, &mut value)?;
    Ok(value)
}

fn query_pin_data_ranges(
    handle: HANDLE,
    pin_id: u32,
) -> Result<(u32, Vec<KsAudioRangeProbe>), String> {
    let prop = KsPinProperty {
        property: KsProperty {
            set: KSPROPSETID_PIN,
            id: KSPROPERTY_PIN_DATARANGES,
            flags: KSPROPERTY_TYPE_GET,
        },
        pin_id,
        reserved: 0,
    };

    let mut buffer = vec![0u8; 64 * 1024];
    let returned = ks_ioctl(
        handle,
        (&prop as *const KsPinProperty).cast(),
        size_of::<KsPinProperty>() as u32,
        buffer.as_mut_ptr().cast(),
        buffer.len() as u32,
    )?;
    let ranges = parse_audio_data_ranges(&buffer[..returned as usize]);
    Ok((returned, ranges))
}

fn parse_audio_data_ranges(buffer: &[u8]) -> Vec<KsAudioRangeProbe> {
    if buffer.len() < size_of::<KsMultipleItem>() {
        return Vec::new();
    }

    let multiple: KsMultipleItem = read_unaligned_from(buffer, 0);
    let mut ranges = Vec::new();
    let mut offset = size_of::<KsMultipleItem>();
    for _ in 0..multiple.count {
        if offset + size_of::<KsDataRange>() > buffer.len() {
            break;
        }

        let data_range: KsDataRange = read_unaligned_from(buffer, offset);
        let format_size = data_range.format_size as usize;
        if format_size < size_of::<KsDataRange>() || offset + format_size > buffer.len() {
            break;
        }

        if data_range.major_format == KSDATAFORMAT_TYPE_AUDIO
            && format_size >= size_of::<KsDataRangeAudio>()
        {
            let audio: KsDataRangeAudio = read_unaligned_from(buffer, offset);
            ranges.push(KsAudioRangeProbe {
                subtype: if audio.data_range.sub_format == KSDATAFORMAT_SUBTYPE_PCM {
                    AudioRangeSubtype::Pcm
                } else if audio.data_range.sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                    AudioRangeSubtype::IeeeFloat
                } else {
                    AudioRangeSubtype::Other
                },
                maximum_channels: audio.maximum_channels,
                minimum_bits_per_sample: audio.minimum_bits_per_sample,
                maximum_bits_per_sample: audio.maximum_bits_per_sample,
                minimum_sample_frequency: audio.minimum_sample_frequency,
                maximum_sample_frequency: audio.maximum_sample_frequency,
            });
        }

        offset += align_up(format_size, 8);
    }

    ranges
}

fn format_pin_summary(pins: &[KsPinProbe]) -> String {
    if pins.is_empty() {
        return "[]".into();
    }

    let mut summary = String::from("[");
    for (index, pin) in pins.iter().enumerate() {
        if index > 0 {
            summary.push_str(", ");
        }
        summary.push_str(&format!(
            "#{} comm={} flow={} ranges={}B audio={} render_candidate={}",
            pin.pin_id,
            pin.communication
                .map(format_pin_communication)
                .unwrap_or_else(|| "?".into()),
            pin.dataflow
                .map(format_pin_dataflow)
                .unwrap_or_else(|| "?".into()),
            pin.data_ranges_bytes
                .map(|bytes| bytes.to_string())
                .unwrap_or_else(|| "?".into()),
            format_audio_ranges(&pin.audio_ranges),
            pin.render_candidate
        ));
    }
    summary.push(']');
    summary
}

fn format_audio_ranges(ranges: &[KsAudioRangeProbe]) -> String {
    if ranges.is_empty() {
        return "[]".into();
    }

    let mut summary = String::from("[");
    for (index, range) in ranges.iter().enumerate() {
        if index > 0 {
            summary.push_str("; ");
        }
        summary.push_str(&format!(
            "{} ch<= {} bits={}..{} hz={}..{}",
            format_audio_subtype(range.subtype),
            range.maximum_channels,
            range.minimum_bits_per_sample,
            range.maximum_bits_per_sample,
            range.minimum_sample_frequency,
            range.maximum_sample_frequency
        ));
    }
    summary.push(']');
    summary
}

fn format_audio_subtype(subtype: AudioRangeSubtype) -> &'static str {
    match subtype {
        AudioRangeSubtype::Pcm => "pcm",
        AudioRangeSubtype::IeeeFloat => "float",
        AudioRangeSubtype::Other => "other",
    }
}

fn is_render_candidate(
    communication: Option<u32>,
    dataflow: Option<u32>,
    ranges: &[KsAudioRangeProbe],
) -> bool {
    let accepts_host_data = dataflow == Some(1);
    let stream_communicates = matches!(communication, Some(1 | 3));
    let supports_pcm = ranges.iter().any(|range| {
        matches!(
            range.subtype,
            AudioRangeSubtype::Pcm | AudioRangeSubtype::IeeeFloat
        )
    });
    accepts_host_data && stream_communicates && supports_pcm
}

fn align_up(value: usize, alignment: usize) -> usize {
    debug_assert!(alignment.is_power_of_two());
    (value + alignment - 1) & !(alignment - 1)
}

fn read_unaligned_from<T: Copy>(buffer: &[u8], offset: usize) -> T {
    debug_assert!(offset + size_of::<T>() <= buffer.len());
    unsafe { std::ptr::read_unaligned(buffer.as_ptr().add(offset).cast::<T>()) }
}

fn format_pin_communication(value: u32) -> String {
    match value {
        0 => "None".into(),
        1 => "Sink".into(),
        2 => "Source".into(),
        3 => "Both".into(),
        4 => "Bridge".into(),
        other => other.to_string(),
    }
}

fn format_pin_dataflow(value: u32) -> String {
    match value {
        1 => "In".into(),
        2 => "Out".into(),
        other => other.to_string(),
    }
}

// ── DeviceIoControl helpers ────────────────────────────────────────────────────

fn ks_property_get<TIn, TOut>(
    handle: HANDLE,
    input: &TIn,
    output: &mut TOut,
) -> Result<u32, String> {
    ks_ioctl(
        handle,
        (input as *const TIn).cast(),
        size_of::<TIn>() as u32,
        (output as *mut TOut).cast(),
        size_of::<TOut>() as u32,
    )
}

fn ks_ioctl(
    handle: HANDLE,
    input: *const c_void,
    input_size: u32,
    output: *mut c_void,
    output_size: u32,
) -> Result<u32, String> {
    let mut returned = 0u32;
    unsafe {
        DeviceIoControl(
            handle,
            IOCTL_KS_PROPERTY,
            Some(input),
            input_size,
            Some(output),
            output_size,
            Some(&mut returned),
            None,
        )
        .map_err(|e| format!("DeviceIoControl(IOCTL_KS_PROPERTY) failed: {e}"))?;
    }
    Ok(returned)
}

unsafe fn cleanup_mmcss(handle: isize) {
    if handle != 0 {
        AvRevertMmThreadCharacteristics(handle);
    }
}
