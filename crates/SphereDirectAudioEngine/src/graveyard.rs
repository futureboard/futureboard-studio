//! Off-audio-thread disposal of retired runtime graphs.
//!
//! Swapping a new [`RuntimeProject`] into the audio callback must not drop the
//! previous one *there*: its `Drop` frees every track's render buffers,
//! releases the last `Arc<ClipAudioSource>` (munmap / free), and destroys C++
//! VST3 processor handles — all heap / OS work forbidden on the realtime
//! thread (see `tasks/native/audio-system-spec.md` §1, §3 and the Phase A
//! audit finding A.2.1).
//!
//! Instead the callback hands the retired value to a background "graveyard"
//! thread through a bounded, lock-free channel. Once the channel exists a
//! `try_send` performs no allocation, so the realtime thread only does an
//! atomic enqueue; the actual destructor runs on the background thread.
//!
//! Every kind of graveyard (graphs, audition sources, MIDI schedules, tempo
//! changes, ARA renderer lists) is created together, by [`prime`], on a control
//! thread: at engine creation, before every device open and with every command
//! that carries something the callback will retire. The `retire_*` functions
//! never create a channel or a thread — they run on the audio thread. Should
//! one ever run before [`prime`] (or after its worker failed to start), the
//! retired value is leaked on purpose and counted, rather than allocating a
//! channel, spawning a thread or running a destructor on the callback.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use crossbeam_channel::{bounded, Sender};

use crate::audio_file::AudioFileBuffer;
use crate::command::{InsertParamChange, TempoMapChange};
use crate::runtime::{RuntimeAraRenderer, RuntimeMidiData, RuntimeProject};

/// Bounded so a pathological load storm cannot grow memory without limit. This
/// is far more than the handful of retired values ever expected to be in
/// flight; if it somehow fills, the value is leaked and counted (see [`leak`]).
const GRAVEYARD_CAPACITY: usize = 64;

/// One channel (and one dropper thread) per kind, so a slow VST3 teardown in a
/// retired graph never holds up the disposal of a tempo change.
struct Graveyards {
    project: Sender<Box<RuntimeProject>>,
    audio_file: Sender<Box<AudioFileBuffer>>,
    midi: Sender<Box<RuntimeMidiData>>,
    tempo: Sender<Box<TempoMapChange>>,
    time_signature: Sender<Box<crate::time_signature_map::RuntimeTimeSignatureMapSnapshot>>,
    insert_param: Sender<Box<InsertParamChange>>,
    #[allow(clippy::box_collection)] // matches EngineCommand::SetBridgeEditorActive
    bridge_editor: Sender<Box<std::collections::HashSet<String>>>,
    ara: Sender<Vec<RuntimeAraRenderer>>,
}

static GRAVEYARDS: OnceLock<Graveyards> = OnceLock::new();

/// Retired values the audio thread had to leak because no graveyard could take
/// them. Zero in a healthy process.
static LEAKED: AtomicU64 = AtomicU64::new(0);
/// The part of [`LEAKED`] already reported by [`prime`].
static LEAKS_REPORTED: AtomicU64 = AtomicU64::new(0);

/// A bounded channel whose receiving end drops every value it gets on a
/// dedicated, non-realtime thread. Control thread only.
fn spawn_worker<T: Send + 'static>(name: &str) -> Sender<T> {
    let (tx, rx) = bounded::<T>(GRAVEYARD_CAPACITY);
    #[cfg(test)]
    test_hooks::note_worker_spawn();
    // Exits when the last sender is gone — but the sender is a
    // process-lifetime static, so in practice it lives for the whole run.
    let spawned = std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            while let Ok(old) = rx.recv() {
                drop(old);
            }
        });
    if let Err(error) = spawned {
        // The receiver went down with the closure, so every send on this
        // channel now fails and the callback leaks (and counts) instead.
        eprintln!("[graveyard] could not start {name}: {error}; retired values will be leaked");
    }
    tx
}

/// Create every graveyard channel and its drop-thread, on the calling
/// (non-realtime) thread. Idempotent: after the first call it is one atomic
/// load.
///
/// Call it before anything the callback could retire can reach the callback —
/// see the module docs for where the engine does. Also reports, off the audio
/// thread, any value the callback had to leak since the last call.
pub fn prime() {
    GRAVEYARDS.get_or_init(|| Graveyards {
        project: spawn_worker("daux-graveyard"),
        audio_file: spawn_worker("daux-audition-graveyard"),
        midi: spawn_worker("daux-midi-graveyard"),
        tempo: spawn_worker("daux-tempo-graveyard"),
        time_signature: spawn_worker("daux-time-sig-graveyard"),
        insert_param: spawn_worker("daux-param-graveyard"),
        bridge_editor: spawn_worker("daux-editor-graveyard"),
        ara: spawn_worker("daux-ara-graveyard"),
    });
    let leaked = LEAKED.load(Ordering::Relaxed);
    let reported = LEAKS_REPORTED.swap(leaked, Ordering::Relaxed);
    if leaked > reported {
        eprintln!(
            "[graveyard] the audio thread leaked {} retired value(s) it had nowhere to send ({leaked} in total)",
            leaked - reported
        );
    }
}

/// Whether [`prime`] has run.
#[allow(dead_code)] // read by tests and diagnostics
pub fn is_primed() -> bool {
    GRAVEYARDS.get().is_some()
}

/// Retired values the audio thread leaked since the process started.
#[allow(dead_code)] // read by tests and diagnostics
pub fn leaked_count() -> u64 {
    LEAKED.load(Ordering::Relaxed)
}

/// The graveyards as the audio thread sees them: one atomic load, never an
/// initialisation.
#[inline]
fn graveyards() -> Option<&'static Graveyards> {
    GRAVEYARDS.get()
}

/// Enqueue `old` on `sender`, or leak it if there is no channel or the channel
/// cannot take it. Realtime-safe either way.
#[inline]
fn send_or_leak<T>(sender: Option<&Sender<T>>, old: T) {
    match sender {
        Some(sender) => {
            if let Err(err) = sender.try_send(old) {
                leak(err.into_inner());
            }
        }
        None => leak(old),
    }
}

/// Last resort on the audio thread: no graveyard (not primed, or its worker
/// failed to start) or a full one (the worker is wedged). Dropping here would
/// run the destructor — frees, munmaps, VST3 teardown — on the callback, and
/// creating a channel would allocate and spawn a thread, so the value is
/// forgotten instead and counted for [`prime`] to report.
#[cold]
#[inline(never)]
fn leak<T>(old: T) {
    std::mem::forget(old);
    LEAKED.fetch_add(1, Ordering::Relaxed);
    #[cfg(test)]
    test_hooks::note_leak();
}

/// Hand a retired runtime graph to the background dropper, still in the box
/// it arrived in (moving it out would free the box on the audio thread).
///
/// Realtime-safe: a bounded-channel `try_send` performs no allocation.
#[inline]
pub fn retire(old: Box<RuntimeProject>) {
    send_or_leak(graveyards().map(|g| &g.project), old);
}

/// Dispose a decoded audition source away from the realtime callback.
#[inline]
pub fn retire_audio_file(old: Box<AudioFileBuffer>) {
    send_or_leak(graveyards().map(|g| &g.audio_file), old);
}

/// Dispose a replaced MIDI schedule away from the realtime callback: its event
/// lists are heap memory the callback must not free.
#[inline]
pub fn retire_midi(old: Box<RuntimeMidiData>) {
    send_or_leak(graveyards().map(|g| &g.midi), old);
}

/// Dispose an applied tempo change — it now holds the replaced tempo maps and
/// MIDI schedule — away from the realtime callback.
#[inline]
pub fn retire_tempo_change(old: Box<TempoMapChange>) {
    send_or_leak(graveyards().map(|g| &g.tempo), old);
}

/// Dispose an applied time-signature map away from the realtime callback.
#[inline]
pub fn retire_time_signature_map(
    old: Box<crate::time_signature_map::RuntimeTimeSignatureMapSnapshot>,
) {
    send_or_leak(graveyards().map(|g| &g.time_signature), old);
}

/// Dispose replaced plug-in parameters and DSP state away from the realtime callback.
#[inline]
pub fn retire_insert_param_change(old: Box<InsertParamChange>) {
    send_or_leak(graveyards().map(|g| &g.insert_param), old);
}

/// Dispose a replaced open-editor set away from the realtime callback.
#[inline]
#[allow(clippy::box_collection)] // matches EngineCommand::SetBridgeEditorActive
pub fn retire_bridge_editor_active(old: Box<std::collections::HashSet<String>>) {
    send_or_leak(graveyards().map(|g| &g.bridge_editor), old);
}

/// Dispose a retired ARA renderer list away from the realtime callback.
///
/// Dropping the last handle to a bound ARA instance destroys a C++ VST3
/// processor, which is exactly the OS work `retire` exists to keep off the audio
/// thread.
#[inline]
pub fn retire_ara_renderers(old: Vec<RuntimeAraRenderer>) {
    if old.is_empty() {
        return;
    }
    send_or_leak(graveyards().map(|g| &g.ara), old);
}

/// Per-thread counters, so a test can tell what *its* thread did while other
/// tests run in parallel against the same process-wide graveyards.
#[cfg(test)]
pub(crate) mod test_hooks {
    use std::cell::Cell;

    thread_local! {
        // `const` initialisers with no destructor: touching them never
        // allocates, so the counted realtime paths stay allocation-free.
        static WORKER_SPAWNS: Cell<u64> = const { Cell::new(0) };
        static LEAKS: Cell<u64> = const { Cell::new(0) };
    }

    pub(super) fn note_worker_spawn() {
        let _ = WORKER_SPAWNS.try_with(|n| n.set(n.get() + 1));
    }

    pub(super) fn note_leak() {
        let _ = LEAKS.try_with(|n| n.set(n.get() + 1));
    }

    /// Graveyard drop-threads this thread has spawned.
    pub(crate) fn worker_spawns_on_this_thread() -> u64 {
        WORKER_SPAWNS.with(Cell::get)
    }

    /// Retired values this thread had to leak.
    pub(crate) fn leaks_on_this_thread() -> u64 {
        LEAKS.with(Cell::get)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A primed graveyard takes a retired value with an atomic enqueue: no
    /// allocation, no free, no thread, no leak.
    #[test]
    fn retiring_into_a_primed_graveyard_is_an_enqueue() {
        prime();
        assert!(is_primed());
        let change = Box::new(TempoMapChange {
            metronome_map: crate::tempo_map::RuntimeTempoMapSnapshot::static_tempo(90.0),
            map: crate::tempo_map::RuntimeTempoMapSnapshot::static_tempo(90.0),
            midi: None,
            sample_rate: 0,
        });
        let spawns = test_hooks::worker_spawns_on_this_thread();
        let leaks = test_hooks::leaks_on_this_thread();
        let (_, heap) = crate::test_alloc::count_heap_events(|| retire_tempo_change(change));
        assert_eq!(heap, 0, "retiring allocated or freed");
        assert_eq!(test_hooks::worker_spawns_on_this_thread(), spawns);
        assert_eq!(test_hooks::leaks_on_this_thread(), leaks);
    }

    /// With nowhere to send it, a retired value is leaked and counted — never
    /// dropped (freed) on the calling thread.
    #[test]
    fn a_value_with_nowhere_to_go_is_leaked_and_counted() {
        let (tx, rx) = bounded::<Box<u64>>(1);
        tx.try_send(Box::new(1)).unwrap();
        let total = leaked_count();
        let leaks = test_hooks::leaks_on_this_thread();
        let (full, missing) = (Box::new(2u64), Box::new(3u64));
        let (_, heap) = crate::test_alloc::count_heap_events(|| {
            // A full channel, then no channel at all.
            send_or_leak(Some(&tx), full);
            send_or_leak(None, missing);
        });
        assert_eq!(heap, 0, "a leaked value was freed");
        assert_eq!(test_hooks::leaks_on_this_thread(), leaks + 2);
        assert!(leaked_count() >= total + 2);
        drop(rx);
    }
}
