//! Off-audio-thread disposal of retired runtime graphs.
//!
//! Swapping a new [`RuntimeProject`] into the audio callback must not drop the
//! previous one *there*: its `Drop` frees every track's render buffers,
//! releases the last `Arc<ClipAudioSource>` (munmap / free), and destroys C++
//! VST3 processor handles — all heap / OS work forbidden on the realtime
//! thread (see `tasks/native/audio-system-spec.md` §1, §3 and the Phase A
//! audit finding A.2.1).
//!
//! Instead the callback hands the retired graph to a background "graveyard"
//! thread through a bounded, lock-free channel. Once the channel exists a
//! `try_send` performs no allocation, so the realtime thread only does an
//! atomic enqueue; the actual destructor runs on the background thread.

use std::sync::OnceLock;

use crossbeam_channel::{bounded, Sender};

use crate::audio_file::AudioFileBuffer;
use crate::runtime::RuntimeProject;

/// Bounded so a pathological load storm cannot grow memory without limit. This
/// is far more than the handful of retired graphs ever expected to be in
/// flight; if it somehow fills, [`retire`] falls back to dropping in place.
const GRAVEYARD_CAPACITY: usize = 64;

fn sender() -> &'static Sender<RuntimeProject> {
    static GY: OnceLock<Sender<RuntimeProject>> = OnceLock::new();
    GY.get_or_init(|| {
        let (tx, rx) = bounded::<RuntimeProject>(GRAVEYARD_CAPACITY);
        // Dedicated thread: blocks on recv (non-realtime) and drops each
        // retired graph here, well away from the audio callback. Exits when
        // the last sender is gone — but the sender is a process-lifetime
        // static, so in practice it lives for the whole run.
        let _ = std::thread::Builder::new()
            .name("daux-graveyard".to_string())
            .spawn(move || {
                while let Ok(old) = rx.recv() {
                    drop(old);
                }
            });
        tx
    })
}

fn audio_file_sender() -> &'static Sender<Box<AudioFileBuffer>> {
    static GY: OnceLock<Sender<Box<AudioFileBuffer>>> = OnceLock::new();
    GY.get_or_init(|| {
        let (tx, rx) = bounded::<Box<AudioFileBuffer>>(GRAVEYARD_CAPACITY);
        let _ = std::thread::Builder::new()
            .name("daux-audition-graveyard".to_string())
            .spawn(move || {
                while let Ok(old) = rx.recv() {
                    drop(old);
                }
            });
        tx
    })
}

/// Initialise the graveyard channel and drop-thread from a non-realtime
/// thread.
///
/// Call this from the control thread (e.g. project load) *before* a
/// `LoadProject` command can reach the callback, so the callback's first
/// [`retire`] is a cheap atomic enqueue rather than a channel allocation plus
/// thread spawn.
pub fn prime() {
    let _ = sender();
    let _ = audio_file_sender();
    let _ = ara_sender();
    let _ = midi_sender();
}

/// Hand a retired runtime graph to the background dropper.
///
/// Realtime-safe: a bounded-channel `try_send` performs no allocation. If the
/// channel is full (pathological — implies the background thread is wedged),
/// the value is dropped in place as a last resort rather than leaked.
#[inline]
pub fn retire(old: RuntimeProject) {
    if let Err(err) = sender().try_send(old) {
        drop(err.into_inner());
    }
}

/// Dispose a decoded audition source away from the realtime callback.
#[inline]
pub fn retire_audio_file(old: Box<AudioFileBuffer>) {
    if let Err(err) = audio_file_sender().try_send(old) {
        drop(err.into_inner());
    }
}

fn midi_sender() -> &'static Sender<Box<crate::runtime::RuntimeMidiData>> {
    static GY: OnceLock<Sender<Box<crate::runtime::RuntimeMidiData>>> = OnceLock::new();
    GY.get_or_init(|| {
        let (tx, rx) = bounded::<Box<crate::runtime::RuntimeMidiData>>(GRAVEYARD_CAPACITY);
        let _ = std::thread::Builder::new()
            .name("daux-midi-graveyard".to_string())
            .spawn(move || {
                while let Ok(old) = rx.recv() {
                    drop(old);
                }
            });
        tx
    })
}

/// Dispose a replaced MIDI schedule away from the realtime callback: its event
/// lists are heap memory the callback must not free.
#[inline]
pub fn retire_midi(old: Box<crate::runtime::RuntimeMidiData>) {
    if let Err(err) = midi_sender().try_send(old) {
        drop(err.into_inner());
    }
}

fn ara_sender() -> &'static Sender<Vec<crate::runtime::RuntimeAraRenderer>> {
    static GY: OnceLock<Sender<Vec<crate::runtime::RuntimeAraRenderer>>> = OnceLock::new();
    GY.get_or_init(|| {
        let (tx, rx) = bounded::<Vec<crate::runtime::RuntimeAraRenderer>>(GRAVEYARD_CAPACITY);
        let _ = std::thread::Builder::new()
            .name("daux-ara-graveyard".to_string())
            .spawn(move || {
                while let Ok(old) = rx.recv() {
                    drop(old);
                }
            });
        tx
    })
}

/// Dispose a retired ARA renderer list away from the realtime callback.
///
/// Dropping the last handle to a bound ARA instance destroys a C++ VST3
/// processor, which is exactly the OS work `retire` exists to keep off the audio
/// thread.
#[inline]
pub fn retire_ara_renderers(old: Vec<crate::runtime::RuntimeAraRenderer>) {
    if old.is_empty() {
        return;
    }
    if let Err(err) = ara_sender().try_send(old) {
        drop(err.into_inner());
    }
}
