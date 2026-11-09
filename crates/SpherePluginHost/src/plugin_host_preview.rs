//! VST3 MIDI preview + local audio output for the external PluginHost process.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use DirectAudio::vst3_processor::{
    PluginMidiOutEvent, Vst3MidiEvent, Vst3MidiEventKind, Vst3PluginState, Vst3RuntimeProcessor,
};

use crate::audio_bridge::{SharedMidiEvent, MAX_CHANNELS, MIDI_RING_CAP};

fn forensic_trace_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        std::env::var_os("FUTUREBOARD_FORENSIC_TRACE").is_some()
            || std::env::var_os("FUTUREBOARD_MIDI_VERBOSE").is_some()
    })
}

const PREVIEW_TAIL_BLOCKS: u32 = 8;
const CC_SUSTAIN: u16 = 64;
const CC_ALL_SOUND_OFF: u16 = 120;
const CC_ALL_NOTES_OFF: u16 = 123;

pub type SharedPluginHostPreview = Arc<Mutex<PluginHostPreviewEngine>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PreviewNoteKey {
    channel: u8,
    pitch: u8,
}

/// Per-instance MIDI/note state shared between the IPC thread (which queues
/// events) and the audio producer (which takes them per block). Behind
/// [`VoiceShared::events`], held only to push or to hand the queue over.
#[derive(Debug, Default)]
struct VoiceMidiState {
    pending_events: Vec<Vst3MidiEvent>,
    active_notes: Vec<PreviewNoteKey>,
    tail_blocks: u32,
}

impl VoiceMidiState {
    fn has_activity(&self) -> bool {
        !self.pending_events.is_empty() || !self.active_notes.is_empty() || self.tail_blocks > 0
    }

    fn preview_note_on(&mut self, channel: u8, pitch: u8, velocity: u8) {
        let key = PreviewNoteKey {
            channel: channel.min(15),
            pitch: pitch.min(127),
        };
        if self.active_notes.contains(&key) {
            self.pending_events
                .push(Vst3MidiEvent::note_off(0, key.channel, key.pitch, 0.0));
        }
        let vel = velocity.clamp(1, 127) as f32 / 127.0;
        self.pending_events
            .push(Vst3MidiEvent::note_on(0, key.channel, key.pitch, vel));
        if !self.active_notes.contains(&key) {
            self.active_notes.push(key);
        }
        self.tail_blocks = PREVIEW_TAIL_BLOCKS;
    }

    fn preview_note_off(&mut self, channel: u8, pitch: u8) {
        let key = PreviewNoteKey {
            channel: channel.min(15),
            pitch: pitch.min(127),
        };
        self.pending_events
            .push(Vst3MidiEvent::note_off(0, key.channel, key.pitch, 0.0));
        self.active_notes.retain(|n| *n != key);
        self.tail_blocks = PREVIEW_TAIL_BLOCKS;
    }

    fn preview_control_change(&mut self, channel: u8, controller: u8, value: u8) {
        // `128`/`129` are the VST3 channel-pressure/pitch-bend controllers;
        // clamping them to 127 would send CC 127 (Poly Mode On) instead.
        let controller = controller.min(129);
        let value = value.min(127);
        let normalized = if controller == 129 {
            // Expand the 7-bit value so centre (64) is the unbent 8192.
            f32::from(u16::from(value) << 7) / 16_383.0
        } else {
            f32::from(value) / 127.0
        };
        self.pending_events.push(Vst3MidiEvent::control_change(
            0,
            channel.min(15),
            u16::from(controller),
            normalized,
        ));
        self.tail_blocks = PREVIEW_TAIL_BLOCKS;
    }

    fn panic(&mut self) {
        let drained: Vec<PreviewNoteKey> = self.active_notes.drain(..).collect();
        for key in drained {
            self.pending_events
                .push(Vst3MidiEvent::note_off(0, key.channel, key.pitch, 0.0));
        }
        let ch = 0u8;
        self.pending_events
            .push(Vst3MidiEvent::control_change(0, ch, CC_SUSTAIN, 0.0));
        self.pending_events
            .push(Vst3MidiEvent::control_change(0, ch, CC_ALL_NOTES_OFF, 0.0));
        self.pending_events
            .push(Vst3MidiEvent::control_change(0, ch, CC_ALL_SOUND_OFF, 0.0));
        self.tail_blocks = PREVIEW_TAIL_BLOCKS;
    }
}

/// Translate one engine-pushed shared-memory MIDI event into the bridge's
/// event form. Raw status byte: high nibble = message, low nibble = channel.
/// `None` for a message a VST3 voice has no use for.
fn shared_event_to_vst3(ev: &SharedMidiEvent, instance_id: &str) -> Option<Vst3MidiEvent> {
    let channel = ev.status & 0x0F;
    match ev.status & 0xF0 {
        // Note-on with velocity 0 is a note-off (running-status idiom).
        0x90 if ev.data2 > 0 => {
            if forensic_trace_enabled() {
                eprintln!(
                    "[plugin-host-midi-consume] note_on instance={instance_id} pitch={} offset={}",
                    ev.data1, ev.sample_offset
                );
            }
            let vel = ev.data2.clamp(1, 127) as f32 / 127.0;
            Some(Vst3MidiEvent::note_on(
                ev.sample_offset,
                channel,
                ev.data1.min(127),
                vel,
            ))
        }
        0x80 | 0x90 => {
            if forensic_trace_enabled() {
                eprintln!(
                    "[plugin-host-midi-consume] note_off instance={instance_id} pitch={} offset={}",
                    ev.data1, ev.sample_offset
                );
            }
            Some(Vst3MidiEvent::note_off(
                ev.sample_offset,
                channel,
                ev.data1.min(127),
                0.0,
            ))
        }
        0xB0 => Some(Vst3MidiEvent::control_change(
            ev.sample_offset,
            channel,
            ev.data1 as u16,
            ev.data2 as f32 / 127.0,
        )),
        // Raw channel pressure / pitch bend map back to the VST3
        // `kAfterTouch` (128) / `kPitchBend` (129) controllers, which the
        // processor resolves through the plugin's IMidiMapping.
        0xD0 => Some(Vst3MidiEvent::control_change(
            ev.sample_offset,
            channel,
            128,
            ev.data1.min(127) as f32 / 127.0,
        )),
        // A program change maps back to VST3 `kCtrlProgramChange` (130),
        // the program as n/127, which each bridge turns into its own
        // form: a mapped or program-change parameter, or raw `0xC0`.
        0xC0 => Some(Vst3MidiEvent::control_change(
            ev.sample_offset,
            channel,
            130,
            ev.data1.min(127) as f32 / 127.0,
        )),
        0xE0 => {
            let bend = u16::from(ev.data1 & 0x7F) | (u16::from(ev.data2 & 0x7F) << 7);
            Some(Vst3MidiEvent::control_change(
                ev.sample_offset,
                channel,
                129,
                f32::from(bend) / 16_383.0,
            ))
        }
        _ => None,
    }
}

/// Events one voice can take into a single block: a full engine ring plus as
/// many preview events again. Preallocated when the voice is created, so the
/// producer never grows it; what does not fit waits (in the ring, or in the
/// preview queue) for the next block.
const VOICE_BLOCK_EVENT_CAPACITY: usize = MIDI_RING_CAP * 2;
/// One bit per channel and key.
const HELD_NOTE_WORDS: usize = 16 * 128 / 64;

/// What one plug-in instance shares between the host's main thread (IPC,
/// editor windows, state) and its audio producer.
///
/// Two locks, and the producer only ever *tries* either, so the main thread
/// never holds up audio and audio never holds up the main thread:
/// - `events` is the preview MIDI queue the main thread pushes to. The
///   producer copies from it into its own buffer when it is free; when the
///   main thread happens to hold it, those events wait one block.
/// - `render` is the producer's own: it holds it across `process()` with the
///   events of that block, in a buffer preallocated here. The main thread
///   takes it only inside [`VoiceShared::suspend`], to wait out one block in
///   flight.
///
/// Engine MIDI (the instance's shared-memory ring) goes straight from the
/// ring into `render`'s buffer, never through `events`; the notes it leaves
/// sounding are tracked in `held_notes` atomics so a panic can release them.
///
/// A control call that must not overlap `process()` — `setState`, the
/// editor's `removed()` — suspends the voice instead of holding a lock the
/// producer waits on: the producer skips it (its input passes through) for
/// as long as the call takes, and every other voice keeps playing.
#[derive(Debug)]
struct VoiceShared {
    events: Mutex<VoiceMidiState>,
    render: Mutex<Vec<Vst3MidiEvent>>,
    suspended: AtomicBool,
    /// Set once on unload and never cleared, unlike `suspended`.
    retired: AtomicBool,
    held_notes: [AtomicU64; HELD_NOTE_WORDS],
}

impl Default for VoiceShared {
    fn default() -> Self {
        Self {
            events: Mutex::new(VoiceMidiState::default()),
            render: Mutex::new(Vec::with_capacity(VOICE_BLOCK_EVENT_CAPACITY)),
            suspended: AtomicBool::new(false),
            retired: AtomicBool::new(false),
            held_notes: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl VoiceShared {
    /// Takes the voice out of processing until the guard drops; returns once
    /// the block in flight, if any, has finished.
    fn suspend(&self) -> VoiceSuspended<'_> {
        self.stop_processing();
        VoiceSuspended(self)
    }

    /// Takes the voice out of processing for good (it is being unloaded).
    /// The producer may still hold it in its block plan until it adopts one
    /// without it; until then it skips it like a suspended voice.
    fn retire(&self) {
        self.retired.store(true, Ordering::SeqCst);
        self.stop_processing();
    }

    fn stop_processing(&self) {
        self.suspended.store(true, Ordering::SeqCst);
        // A producer that took `render` before the flag was set is inside
        // `process()`: this waits for it. One that takes it after this lock is
        // released sees the flag (the lock orders the store before its load).
        drop(self.render.lock());
    }

    /// Producer: remember which engine notes are sounding. Wait-free.
    fn track_engine_note(&self, event: &Vst3MidiEvent) {
        let index = usize::from(event.channel.min(15)) * 128 + usize::from(event.pitch.min(127));
        let (word, bit) = (index / 64, 1u64 << (index % 64));
        if event.kind == Vst3MidiEventKind::NoteOn as u8 {
            self.held_notes[word].fetch_or(bit, Ordering::Relaxed);
        } else if event.kind == Vst3MidiEventKind::NoteOff as u8 {
            self.held_notes[word].fetch_and(!bit, Ordering::Relaxed);
        }
    }

    /// Main thread: release every sounding note — the preview notes queued
    /// here and the engine notes the producer saw — then sustain and the
    /// all-notes/all-sound controllers.
    fn panic(&self) {
        let mut state = self.events.lock();
        for (word_index, word) in self.held_notes.iter().enumerate() {
            let mut bits = word.swap(0, Ordering::Relaxed);
            while bits != 0 {
                let index = word_index * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                state.pending_events.push(Vst3MidiEvent::note_off(
                    0,
                    (index / 128) as u8,
                    (index % 128) as u8,
                    0.0,
                ));
            }
        }
        state.panic();
    }
}

struct VoiceSuspended<'a>(&'a VoiceShared);

impl Drop for VoiceSuspended<'_> {
    fn drop(&mut self) {
        self.0.suspended.store(false, Ordering::Release);
    }
}

/// A source of engine MIDI for one block: the instance's shared-memory ring
/// (`try_pop`), or nothing.
pub type EngineMidiSource<'a> = &'a mut dyn FnMut() -> Option<SharedMidiEvent>;

/// Producer: move engine MIDI from the instance's ring into the voice's block
/// buffer, up to the buffer's preallocated capacity; the rest stays in the
/// ring for the next block. Returns how many ring events were taken.
fn take_engine_midi(
    voice: &VoiceShared,
    block: &mut Vec<Vst3MidiEvent>,
    engine_midi: EngineMidiSource<'_>,
    instance_id: &str,
) -> u32 {
    let mut taken = 0;
    while block.len() < block.capacity() {
        let Some(ev) = engine_midi() else {
            break;
        };
        taken += 1;
        if let Some(event) = shared_event_to_vst3(&ev, instance_id) {
            voice.track_engine_note(&event);
            block.push(event);
        }
    }
    taken
}

/// Starts a voice's block: the producer's lock holding the block's events,
/// or `None` when the voice is suspended, not `ready`, or another renderer
/// has it this block — never a wait, and never an allocation. Engine MIDI is
/// taken into the block buffer even when the voice will not render; it stays
/// there, in order, for the next block that does. Also returns how many
/// engine events were taken.
fn begin_voice_block<'a>(
    voice: &'a VoiceShared,
    engine_midi: EngineMidiSource<'_>,
    instance_id: &str,
    ready: bool,
) -> (Option<parking_lot::MutexGuard<'a, Vec<Vst3MidiEvent>>>, u32) {
    let Some(mut block) = voice.render.try_lock() else {
        return (None, 0);
    };
    let taken = take_engine_midi(voice, &mut block, engine_midi, instance_id);
    if !ready || voice.suspended.load(Ordering::Acquire) || voice.retired.load(Ordering::Acquire) {
        return (None, taken);
    }
    // Preview events from the main thread. Copied into the preallocated
    // buffer (no allocation here: the queue only grows on the main thread).
    // If the main thread is pushing right now they wait one block.
    if let Some(mut state) = voice.events.try_lock() {
        let room = block.capacity() - block.len();
        let moved = state.pending_events.len().min(room);
        block.extend(state.pending_events.drain(..moved));
        if block.is_empty() && state.active_notes.is_empty() {
            state.tail_blocks = state.tail_blocks.saturating_sub(1);
        } else {
            state.tail_blocks = PREVIEW_TAIL_BLOCKS;
        }
    }
    (Some(block), taken)
}

/// Render one voice block (legacy debug CPAL preview path). A voice it cannot
/// take this block is left silent.
fn render_voice(
    processor: &Vst3RuntimeProcessor,
    voice: &VoiceShared,
    in_l: &[f32],
    in_r: &[f32],
    out_l: &mut [f32],
    out_r: &mut [f32],
    transport: DirectAudio::vst3_processor::RuntimeTransportContext,
) {
    let (Some(mut block), _) = begin_voice_block(voice, &mut || None, "", true) else {
        out_l.fill(0.0);
        out_r.fill(0.0);
        return;
    };
    let mut processor = processor.clone();
    // Real transport ProcessContext immediately before process() — same thread,
    // no race. The clone shares the same C++ processor via Arc.
    processor.set_process_context(&transport);
    let _ = processor.process_stereo_block_with_midi(in_l, in_r, out_l, out_r, &block);
    // Cleared, not dropped: the buffer keeps its preallocated capacity.
    block.clear();
}

/// What one bridge block of a VST3 voice did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VoiceBlockResult {
    /// Output channels produced; `0` when the voice did not render (not
    /// ready, suspended, failed), and the caller passes its input through.
    pub channels: usize,
    /// Engine MIDI events taken from the ring this block.
    pub midi_in: u32,
    /// Plug-in MIDI output events written to `midi_out`.
    pub midi_out: usize,
}

/// One VST3 voice as the audio producer holds it: a shallow processor handle
/// (refcounted over the same C++ instance) plus the voice's shared state.
/// Resolved by instance id on the host's main thread when the producer's
/// block plan is built, so the block path never looks a voice up by name.
///
/// The producer must not hold the last handle: plans it drops go back to the
/// main thread (see [`crate::rt_handoff`]), which is where a removed voice's
/// final release — VST3 `terminate` — runs.
#[derive(Debug, Clone)]
pub struct BridgeVoice {
    instance_id: String,
    processor: Vst3RuntimeProcessor,
    midi: Arc<VoiceShared>,
}

impl BridgeVoice {
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// Identity of this loaded instance: a reload under the same instance id
    /// is a different voice.
    pub fn identity(&self) -> usize {
        Arc::as_ptr(&self.midi) as usize
    }

    pub fn main_audio_output_channel_count(&self) -> u32 {
        self.processor.main_audio_output_channel_count().max(1) as u32
    }

    /// The plug-in's reported processing latency in samples.
    pub fn latency_samples(&self) -> i32 {
        self.processor.get_latency_samples().max(0)
    }

    /// Apply one engine-pushed parameter change (normalized VST3 ParamID
    /// value). The C++ processor queues it for the next `process()` call.
    pub fn apply_param(&self, param_id: u32, value: f32) {
        let mut processor = self.processor.clone();
        processor.set_param(param_id, value as f64);
    }

    /// Render one block of this voice into the interleaved bridge buffer,
    /// taking the block's engine MIDI from `engine_midi` and moving the MIDI
    /// the plug-in produced into `midi_out`. Allocation-free and lock-wait-free.
    #[allow(clippy::too_many_arguments)]
    pub fn render_bridge_block(
        &self,
        dsp_ready: bool,
        engine_midi: EngineMidiSource<'_>,
        frames: usize,
        in_l: &[f32],
        in_r: &[f32],
        out_interleaved: &mut [f32],
        output_channels: usize,
        transport: DirectAudio::vst3_processor::RuntimeTransportContext,
        midi_out: &mut [PluginMidiOutEvent],
    ) -> VoiceBlockResult {
        let channels = output_channels.clamp(1, MAX_CHANNELS);
        let n = frames
            .min(in_l.len())
            .min(in_r.len())
            .min(out_interleaved.len() / channels);
        out_interleaved[..n * channels].fill(0.0);
        let (block, midi_in) =
            begin_voice_block(&self.midi, engine_midi, &self.instance_id, dsp_ready);
        let Some(mut block) = block else {
            return VoiceBlockResult {
                midi_in,
                ..VoiceBlockResult::default()
            };
        };
        let mut processor = self.processor.clone();
        processor.set_process_context(&transport);
        let got_channels = processor
            .process_main_output_block_with_midi(
                &in_l[..n],
                &in_r[..n],
                &mut out_interleaved[..n * channels],
                channels,
                &block,
            )
            .unwrap_or(0);
        // Still under the voice's render lock: the bridge clears this list at
        // the start of the next process call.
        let midi = processor.take_output_midi(midi_out);
        block.clear();
        VoiceBlockResult {
            channels: got_channels.min(channels),
            midi_in,
            midi_out: midi,
        }
    }
}

/// The engine's loaded VST3 voices, for the host's main thread to resolve
/// into the audio producer's block plan, plus the block-path flags. The voice
/// list is an `Arc` snapshot republished on load/unload; the producer itself
/// never reads it (its plan carries resolved [`BridgeVoice`] handles), so the
/// IPC thread can hold the engine mutex across `LoadPlugin` /
/// `IPlugView::attached` for seconds without ever stalling block production.
#[derive(Debug)]
pub struct BridgeAudioShared {
    /// Swapped wholesale on load/unload; the mutex is held only to clone or
    /// replace the `Arc` (nanoseconds), never across plugin or editor work.
    voices: Mutex<Arc<Vec<BridgeVoice>>>,
    /// `voices.len()`, readable without the mutex.
    loaded_voices: AtomicUsize,
    dsp_ready: AtomicBool,
    continuous_mode: AtomicBool,
}

impl BridgeAudioShared {
    fn new() -> Self {
        Self {
            voices: Mutex::new(Arc::new(Vec::new())),
            loaded_voices: AtomicUsize::new(0),
            dsp_ready: AtomicBool::new(false),
            continuous_mode: AtomicBool::new(false),
        }
    }

    fn snapshot(&self) -> Arc<Vec<BridgeVoice>> {
        self.voices.lock().clone()
    }

    fn publish(&self, voices: Vec<BridgeVoice>) {
        let count = voices.len();
        *self.voices.lock() = Arc::new(voices);
        self.loaded_voices.store(count, Ordering::Release);
    }

    pub fn dsp_ready(&self) -> bool {
        self.dsp_ready.load(Ordering::Acquire)
    }

    pub fn continuous_mode(&self) -> bool {
        self.continuous_mode.load(Ordering::Acquire)
    }

    pub fn has_loaded_instances(&self) -> bool {
        self.loaded_voices.load(Ordering::Acquire) > 0
    }

    pub fn loaded_instance_ids(&self) -> Vec<String> {
        self.snapshot()
            .iter()
            .map(|v| v.instance_id.clone())
            .collect()
    }

    /// The voice loaded as `instance_id`, for building the producer's block
    /// plan. Main thread only (it takes the snapshot mutex and compares
    /// names); the block path uses the handle it returns.
    pub fn voice_for(&self, instance_id: &str) -> Option<BridgeVoice> {
        self.snapshot()
            .iter()
            .find(|voice| voice.instance_id == instance_id)
            .cloned()
    }
}

#[derive(Debug)]
struct PreviewInstance {
    processor: Vst3RuntimeProcessor,
    midi: Arc<VoiceShared>,
}

#[derive(Debug)]
pub struct PluginHostPreviewEngine {
    sample_rate: u32,
    block_size: u32,
    instances: HashMap<String, PreviewInstance>,
    /// Block-path snapshot + flags shared with the audio producer thread.
    /// `dsp_ready` / `continuous_mode` live in its atomics (single source).
    bridge: Arc<BridgeAudioShared>,
}

impl PluginHostPreviewEngine {
    pub fn shared(sample_rate: u32, block_size: u32) -> SharedPluginHostPreview {
        Arc::new(Mutex::new(Self::new(sample_rate, block_size)))
    }

    pub fn new(sample_rate: u32, block_size: u32) -> Self {
        Self {
            sample_rate: sample_rate.max(44_100),
            block_size: block_size.clamp(64, 2048),
            instances: HashMap::new(),
            bridge: Arc::new(BridgeAudioShared::new()),
        }
    }

    /// Handle for the audio producer thread: block-path snapshot + flags,
    /// readable without the engine mutex.
    pub fn bridge_shared(&self) -> Arc<BridgeAudioShared> {
        self.bridge.clone()
    }

    /// Republish the block-path voice snapshot. Called on load/unload only.
    fn publish_bridge_snapshot(&self) {
        let voices: Vec<BridgeVoice> = self
            .instances
            .iter()
            .map(|(id, instance)| BridgeVoice {
                instance_id: id.clone(),
                processor: instance.processor.clone(),
                midi: instance.midi.clone(),
            })
            .collect();
        self.bridge.publish(voices);
    }

    /// Stage 1: follow the main engine's sample rate / block size (the engine
    /// owns them). Returns the clamped values actually adopted. Existing loaded
    /// instances keep their current sample rate until reloaded — re-prepare is a
    /// later stage once the shared audio transport drives `process()`.
    pub fn configure(&mut self, sample_rate: u32, max_block_size: u32) -> (u32, u32) {
        self.sample_rate = sample_rate.max(44_100);
        self.block_size = max_block_size.clamp(64, 2048);
        (self.sample_rate, self.block_size)
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    pub fn dsp_ready(&self) -> bool {
        self.bridge.dsp_ready()
    }

    pub fn set_dsp_ready(&mut self, ready: bool) {
        self.bridge.dsp_ready.store(ready, Ordering::Release);
    }

    pub fn set_continuous_mode(&mut self, enabled: bool) {
        self.bridge
            .continuous_mode
            .store(enabled, Ordering::Release);
    }

    pub fn continuous_mode(&self) -> bool {
        self.bridge.continuous_mode()
    }

    pub fn has_instance(&self, plugin_instance_id: &str) -> bool {
        self.instances.contains_key(plugin_instance_id)
    }

    /// Clone the live processor handle for editor/UI work **outside** the preview
    /// mutex. `Vst3RuntimeProcessor` is a shallow clone over the same C++
    /// instance — safe for `embed_*` calls while the audio producer holds the
    /// lock for `render_block`.
    pub fn clone_processor_for(&self, plugin_instance_id: &str) -> Option<Vst3RuntimeProcessor> {
        self.instances
            .get(plugin_instance_id)
            .map(|instance| instance.processor.clone())
    }

    pub fn loaded_instance_ids(&self) -> Vec<String> {
        self.instances.keys().cloned().collect()
    }

    pub fn main_audio_output_channel_count_for_instance(
        &self,
        plugin_instance_id: &str,
    ) -> Option<u32> {
        self.instances
            .get(plugin_instance_id)
            .map(|instance| instance.processor.main_audio_output_channel_count())
            .map(|channels| channels.max(1) as u32)
    }

    /// Real per-bus output channel counts (bus-by-bus order) for the instance,
    /// so the host can model one mixer strip per plugin output bus instead of
    /// pairing flat channels. Empty when unknown.
    pub fn output_bus_channel_counts_for_instance(
        &self,
        plugin_instance_id: &str,
    ) -> Option<Vec<u32>> {
        self.instances.get(plugin_instance_id).map(|instance| {
            instance
                .processor
                .output_bus_channel_counts()
                .into_iter()
                .map(|c| c as u32)
                .collect()
        })
    }

    /// Capture the instance's VST3 state for project persistence. Runs
    /// `getState` without the voice mutex — VST3 allows state capture while
    /// processing (it is how every host saves projects during playback).
    pub fn get_instance_state(&self, plugin_instance_id: &str) -> Option<Vst3PluginState> {
        self.instances
            .get(plugin_instance_id)
            .and_then(|instance| instance.processor.get_state())
    }

    /// Enumerate VST3 parameters for automation picker / metadata cache.
    pub fn list_parameters_for_instance(
        &self,
        plugin_instance_id: &str,
    ) -> Option<Vec<DirectAudio::vst3_processor::Vst3ParameterDescriptor>> {
        self.instances
            .get(plugin_instance_id)
            .and_then(|instance| instance.processor.list_parameters())
    }

    /// Restore a previously captured VST3 state. `setState` must not overlap
    /// `process()`, so the voice is suspended for it: the producer skips this
    /// voice (its input passes through) until the state is in, rather than
    /// waiting behind a call that can take a large sampler seconds — which
    /// stalled every voice in this host, not just this one.
    pub fn set_instance_state(&self, plugin_instance_id: &str, state: &Vst3PluginState) -> bool {
        let Some(instance) = self.instances.get(plugin_instance_id) else {
            eprintln!("[plugin-host-state] set_state instance={plugin_instance_id} loaded=false");
            return false;
        };
        let _suspended = instance.midi.suspend();
        let ok = instance.processor.set_state(state);
        eprintln!(
            "[plugin-host-state] set_state instance={plugin_instance_id} component_bytes={} controller_bytes={} ok={ok}",
            state.component.len(),
            state.controller.len()
        );
        ok
    }

    pub fn log_unified_runtime(track_id: &str, insert_id: &str, plugin_instance_id: &str) {
        Self::verify_unified_runtime(
            track_id,
            insert_id,
            plugin_instance_id,
            plugin_instance_id,
            plugin_instance_id,
            plugin_instance_id,
            plugin_instance_id,
            plugin_instance_id,
        );
    }

    /// Strict instance identity check (spec Part 6).
    #[allow(clippy::too_many_arguments)]
    pub fn verify_unified_runtime(
        track_id: &str,
        insert_id: &str,
        plugin_instance_id: &str,
        editor_instance: &str,
        dsp_instance: &str,
        midi_playback_instance: &str,
        preview_instance: &str,
        shared_audio_instance: &str,
    ) {
        let unified = editor_instance == plugin_instance_id
            && dsp_instance == plugin_instance_id
            && midi_playback_instance == plugin_instance_id
            && preview_instance == plugin_instance_id
            && shared_audio_instance == plugin_instance_id;
        eprintln!(
            "[plugin-runtime-id] track_id={track_id} insert_id={insert_id} plugin_instance_id={plugin_instance_id}"
        );
        eprintln!("[plugin-runtime-id] editor_instance={editor_instance}");
        eprintln!("[plugin-runtime-id] dsp_instance={dsp_instance}");
        eprintln!("[plugin-runtime-id] midi_playback_instance={midi_playback_instance}");
        eprintln!("[plugin-runtime-id] preview_instance={preview_instance}");
        eprintln!("[plugin-runtime-id] shared_audio_instance={shared_audio_instance}");
        eprintln!("[plugin-runtime-id] unified={unified}");
        if !unified {
            eprintln!(
                "[plugin-runtime-id] ERROR duplicate runtime refused \
                 expected={plugin_instance_id} editor={editor_instance} dsp={dsp_instance} \
                 midi_playback={midi_playback_instance} preview={preview_instance} \
                 shared_audio={shared_audio_instance}"
            );
        }
    }

    pub fn log_host_registry(&self) {
        eprintln!("[plugin-host-registry] instances={}", self.instances.len());
        for (id, instance) in &self.instances {
            let editor = instance.processor.view_is_attached();
            let dsp = instance.processor.is_ready();
            eprintln!("[plugin-host-registry] instance={id} loaded=true editor={editor} dsp={dsp}");
        }
    }

    pub fn load_instance(
        &mut self,
        plugin_instance_id: &str,
        plugin_path: &str,
        class_id: &str,
        sample_rate: u32,
        max_block_size: u32,
        module_format: DirectAudio::PluginModuleFormat,
    ) -> bool {
        self.sample_rate = sample_rate.max(44_100);
        self.block_size = max_block_size.clamp(64, 2048);
        eprintln!("[plugin-host-registry] load begin instance={plugin_instance_id}");
        if self.instances.contains_key(plugin_instance_id) {
            eprintln!(
                "[plugin-host] LoadPlugin instance={plugin_instance_id} already_loaded=true reuse=true"
            );
            eprintln!(
                "[plugin-host-registry] already_loaded instance={plugin_instance_id} reuse=true"
            );
            eprintln!(
                "[plugin-host-vst3] create skipped reason=instance_exists instance={plugin_instance_id}"
            );
            return true;
        }
        eprintln!(
            "[plugin-host-vst3] create entered instance={plugin_instance_id} path={plugin_path}"
        );
        let Some(processor) = Vst3RuntimeProcessor::new_with_format(
            plugin_path,
            class_id,
            self.sample_rate,
            module_format,
        ) else {
            eprintln!(
                "[plugin-host-midi] preview processor create failed instance={plugin_instance_id}"
            );
            return false;
        };
        processor.embed_set_instance_label(plugin_instance_id);
        self.instances.insert(
            plugin_instance_id.to_string(),
            PreviewInstance {
                processor,
                midi: Arc::new(VoiceShared::default()),
            },
        );
        self.publish_bridge_snapshot();
        eprintln!(
            "[plugin-host-midi] preview processor loaded instance={plugin_instance_id} dsp_output={}",
            if self.dsp_ready() { "ready" } else { "pending" }
        );
        eprintln!("[plugin-host-registry] loaded instance={plugin_instance_id}");
        self.set_continuous_mode(true);
        self.log_host_registry();
        true
    }

    pub fn insert_loaded_instance(
        &mut self,
        plugin_instance_id: &str,
        processor: Vst3RuntimeProcessor,
    ) -> bool {
        if self.instances.contains_key(plugin_instance_id) {
            eprintln!(
                "[plugin-host-vst3] insert skipped reason=instance_exists instance={plugin_instance_id}"
            );
            return false;
        }
        processor.embed_set_instance_label(plugin_instance_id);
        self.instances.insert(
            plugin_instance_id.to_string(),
            PreviewInstance {
                processor,
                midi: Arc::new(VoiceShared::default()),
            },
        );
        self.publish_bridge_snapshot();
        self.set_continuous_mode(true);
        eprintln!("[plugin-host-registry] loaded instance={plugin_instance_id}");
        self.log_host_registry();
        true
    }

    pub fn unload_instance(&mut self, plugin_instance_id: &str) {
        eprintln!("[plugin-host-registry] unload instance={plugin_instance_id}");
        let retired = self.instances.remove(plugin_instance_id);
        if let Some(instance) = &retired {
            // Out of processing for good: from here the producer skips the
            // voice (its input passes through) until it adopts a block plan
            // without it.
            instance.midi.retire();
            instance.processor.view_detach();
            instance.midi.panic();
        }
        if self.instances.is_empty() {
            self.set_continuous_mode(false);
        }
        if retired.is_some() {
            self.publish_bridge_snapshot();
        }
        // No wait on the producer here. It may still hold this voice in its
        // current block plan, but it never drops a plan: it hands the one it
        // replaces back to the host's main thread, which frees it (see
        // `rt_handoff`). So whichever handle goes last — this one or the
        // plan's — the final release (VST3 `terminate`) runs on the main
        // thread, however long the producer's block takes.
        drop(retired);
        eprintln!("[plugin-host-registry] instances={}", self.instances.len());
    }

    pub fn default_editor_size(&self) -> (u32, u32) {
        (880, 600)
    }

    pub fn embed_editor_for_instance(
        &self,
        plugin_instance_id: &str,
        parent_hwnd: u64,
        width: i32,
        height: i32,
    ) -> Option<u64> {
        let Some(instance) = self.instances.get(plugin_instance_id) else {
            eprintln!("[plugin-host-registry] get found=false instance={plugin_instance_id}");
            eprintln!(
                "[plugin-editor] open ERROR instance not loaded instance={plugin_instance_id} uses_runtime_instance=false"
            );
            return None;
        };
        eprintln!("[plugin-host-registry] get found=true instance={plugin_instance_id}");
        eprintln!("[plugin-editor] open instance={plugin_instance_id} uses_runtime_instance=true");
        eprintln!("[plugin-editor] createView from existing controller (reuse loaded runtime)");
        eprintln!("[plugin-editor] no_duplicate_component_created=true");
        instance
            .processor
            .embed_set_instance_label(plugin_instance_id);
        instance
            .processor
            .view_attach(parent_hwnd, (width, height))
            .map(|_| parent_hwnd)
    }

    pub fn embed_resize_for_instance(&self, plugin_instance_id: &str, width: i32, height: i32) {
        if let Some(instance) = self.instances.get(plugin_instance_id) {
            eprintln!(
                "[plugin-bridge] ResizeEditor instance={plugin_instance_id} width={width} height={height}"
            );
            instance.processor.view_set_size(width, height);
            let host_hwnd = instance.processor.handle_value();
            eprintln!("[plugin-host-layout] host_hwnd=0x{host_hwnd:x}");
            eprintln!("[plugin-host-layout] host_client=({width},{height})");
            if let Some((child_w, child_h)) = instance.processor.view_size() {
                eprintln!("[plugin-host-layout] plugin_child_count=1");
                eprintln!("[plugin-host-layout] child=plugin_view client=({child_w},{child_h})");
                let child_matches = child_w == width && child_h == height;
                eprintln!("[plugin-host-layout] child_matches_host={child_matches}");
            } else {
                eprintln!("[plugin-host-layout] plugin_child_count=0");
                eprintln!("[plugin-host-layout] child_matches_host=false");
            }
        }
    }

    /// Kept as a no-op: the plug-in's view is a child of a window the main app
    /// owns, so it is carried along by its parent. Nothing on this side has a
    /// shell left to keep glued to anything.
    pub fn embed_refresh_for_instance(&self, _plugin_instance_id: &str) {}

    /// Detach editor UI only — processor stays loaded and active.
    pub fn editor_detach_for_instance(&mut self, plugin_instance_id: &str) {
        if let Some(instance) = self.instances.get(plugin_instance_id) {
            // IPlugView::removed() can touch the same plugin internals as
            // process(), so it must not overlap it. Suspended rather than
            // locked: a slow `removed()` silences this voice only, instead of
            // holding the producer — and every voice it serves — behind it.
            let _suspended = instance.midi.suspend();
            instance.processor.view_detach();
        }
        eprintln!(
            "[PluginHost] editor closed id={plugin_instance_id} instance_still_active={}",
            self.has_instance(plugin_instance_id)
        );
    }

    /// Full detach + MIDI panic (unload / crash paths).
    pub fn embed_detach_for_instance(&mut self, plugin_instance_id: &str) {
        if let Some(instance) = self.instances.get(plugin_instance_id) {
            let _suspended = instance.midi.suspend();
            instance.processor.view_detach();
            instance.midi.panic();
        }
    }

    pub fn editor_content_size_for_instance(&self, plugin_instance_id: &str) -> (u32, u32) {
        if let Some(instance) = self.instances.get(plugin_instance_id) {
            if let Some((w, h)) = instance.processor.view_size() {
                return (w.max(1) as u32, h.max(1) as u32);
            }
        }
        self.default_editor_size()
    }

    pub fn take_pending_editor_resize_for_instance(
        &self,
        plugin_instance_id: &str,
    ) -> Option<(u32, u32)> {
        self.instances
            .get(plugin_instance_id)
            .and_then(|instance| instance.processor.take_pending_shell_resize())
            .map(|(w, h)| (w.max(1) as u32, h.max(1) as u32))
    }

    pub fn poll_pending_editor_resizes(&self) -> Vec<(String, u32, u32)> {
        let mut out = Vec::new();
        for (id, instance) in &self.instances {
            if let Some((w, h)) = instance.processor.take_pending_shell_resize() {
                out.push((id.clone(), w.max(1) as u32, h.max(1) as u32));
            }
            // The view-host path: the plug-in asking to resize its own editor
            // (a zoom control, or Kontakt restoring its size after attach).
            // The bridge only records the request; it is granted here, within
            // the view's own size contract, and reported so the studio window
            // follows. Nothing read this before, so every such request was
            // lost and the editor kept whatever size the host last forced.
            // Granted as asked: running the plug-in's own request through its
            // size contract snapped a fixed-size view (whose contract is "stay
            // at getSize") straight back to its old size, undoing a zoom.
            if let Some((w, h)) = instance.processor.view_take_resize_request() {
                if w > 0 && h > 0 {
                    instance.processor.view_set_size(w, h);
                    out.push((id.clone(), w as u32, h as u32));
                }
            }
        }
        out
    }

    pub fn preview_note_on(
        &mut self,
        plugin_instance_id: &str,
        channel: u8,
        pitch: u8,
        velocity: u8,
    ) {
        // Gated like the shared-memory consume path above. These two lines ran
        // per previewed note — per piano-roll click in the studio — and the
        // host's stderr is a pipe: back-pressure on it is felt as a stall by
        // whoever is writing to the host's stdin, which is the UI thread.
        if forensic_trace_enabled() {
            eprintln!(
                "[plugin-host-midi-consume] preview note_on instance={plugin_instance_id} pitch={pitch}"
            );
        }
        let Some(instance) = self.instances.get(plugin_instance_id) else {
            // A dropped note is a real fault, so it is reported regardless.
            eprintln!(
                "[plugin-host-midi] preview note_on dropped instance={plugin_instance_id} reason=unknown_instance"
            );
            return;
        };
        instance
            .midi
            .events
            .lock()
            .preview_note_on(channel, pitch, velocity);
        if forensic_trace_enabled() {
            eprintln!("[plugin-host-midi] queued note_on to VSTi");
        }
    }

    pub fn preview_note_off(&mut self, plugin_instance_id: &str, channel: u8, pitch: u8) {
        if forensic_trace_enabled() {
            eprintln!(
                "[plugin-host-midi-consume] preview note_off instance={plugin_instance_id} pitch={pitch}"
            );
        }
        let Some(instance) = self.instances.get(plugin_instance_id) else {
            return;
        };
        instance.midi.events.lock().preview_note_off(channel, pitch);
    }

    pub fn preview_control_change(
        &mut self,
        plugin_instance_id: &str,
        channel: u8,
        controller: u8,
        value: u8,
    ) {
        let Some(instance) = self.instances.get(plugin_instance_id) else {
            return;
        };
        instance
            .midi
            .events
            .lock()
            .preview_control_change(channel, controller, value);
    }

    pub fn preview_all_notes_off(&mut self, plugin_instance_id: &str) {
        eprintln!("[plugin-host-midi] preview all_notes_off instance={plugin_instance_id}");
        let Some(instance) = self.instances.get(plugin_instance_id) else {
            return;
        };
        instance.midi.panic();
    }

    pub fn midi_panic(&mut self, plugin_instance_id: &str) {
        eprintln!("[plugin-host-midi] midi_panic instance={plugin_instance_id}");
        self.preview_all_notes_off(plugin_instance_id);
    }

    /// Stage 3: render one block of all preview instruments interleaved-stereo
    /// into `out` (length `frames * 2`) and return the per-channel peak. Used by
    /// the host's shared-memory bridge service to fill `audio_out`.
    pub fn render_into_interleaved(&mut self, out: &mut [f32], frames: usize) -> (f32, f32) {
        let (mix_l, mix_r) = self.render_block(frames);
        let mut peak_l = 0.0f32;
        let mut peak_r = 0.0f32;
        for i in 0..frames {
            let l = mix_l.get(i).copied().unwrap_or(0.0);
            let r = mix_r.get(i).copied().unwrap_or(0.0);
            if let Some(slot) = out.get_mut(i * 2) {
                *slot = l;
            }
            if let Some(slot) = out.get_mut(i * 2 + 1) {
                *slot = r;
            }
            peak_l = peak_l.max(l.abs());
            peak_r = peak_r.max(r.abs());
        }
        (peak_l, peak_r)
    }

    pub fn has_active_preview(&self) -> bool {
        self.instances
            .values()
            .any(|i| i.midi.events.lock().has_activity())
    }

    pub fn has_loaded_instances(&self) -> bool {
        !self.instances.is_empty()
    }

    pub fn render_block(&mut self, frames: usize) -> (Vec<f32>, Vec<f32>) {
        let scratch_l = vec![0.0f32; frames];
        let scratch_r = vec![0.0f32; frames];
        self.render_block_with_input(frames, &scratch_l, &scratch_r)
    }

    pub fn render_block_with_input(
        &mut self,
        frames: usize,
        in_l: &[f32],
        in_r: &[f32],
    ) -> (Vec<f32>, Vec<f32>) {
        let mut mix_l = vec![0.0f32; frames];
        let mut mix_r = vec![0.0f32; frames];
        if self.instances.is_empty() || !self.dsp_ready() {
            return (mix_l, mix_r);
        }
        let mut out_l = vec![0.0f32; frames];
        let mut out_r = vec![0.0f32; frames];
        // Legacy in-engine debug mixer: no shared-bridge transport here.
        let transport = DirectAudio::vst3_processor::RuntimeTransportContext::default();
        for instance in self.instances.values() {
            render_voice(
                &instance.processor,
                &instance.midi,
                in_l,
                in_r,
                &mut out_l,
                &mut out_r,
                transport,
            );
            for i in 0..frames {
                mix_l[i] += out_l[i];
                mix_r[i] += out_r[i];
            }
        }
        (mix_l, mix_r)
    }
}

pub fn try_start_preview_output(shared: &SharedPluginHostPreview) -> bool {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let host = cpal::default_host();
    let device = match host.default_output_device() {
        Some(device) => device,
        None => {
            eprintln!(
                "[plugin-host-midi] preview received but dsp_output=pending reason=no_output_device"
            );
            return false;
        }
    };
    let config = match device.default_output_config() {
        Ok(config) => config,
        Err(error) => {
            eprintln!(
                "[plugin-host-midi] preview received but dsp_output=pending reason=config_error {error}"
            );
            return false;
        }
    };
    let channels = config.channels() as usize;
    let sample_rate = config.sample_rate().0;
    let shared_cb = shared.clone();
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => device.build_output_stream(
            &config.into(),
            move |data: &mut [f32], _| {
                let ch = channels.max(1);
                let frames = data.len() / ch;
                let (mix_l, mix_r) = shared_cb.lock().render_block(frames);
                for (i, frame) in data.chunks_mut(ch).enumerate() {
                    let l = mix_l.get(i).copied().unwrap_or(0.0);
                    let r = mix_r.get(i).copied().unwrap_or(0.0);
                    if ch == 1 {
                        frame[0] = (l + r) * 0.5;
                    } else {
                        frame[0] = l;
                        if ch > 1 {
                            frame[1] = r;
                        }
                        for sample in frame.iter_mut().skip(2) {
                            *sample = 0.0;
                        }
                    }
                }
            },
            |error| eprintln!("[plugin-host-midi] preview output stream error={error}"),
            None,
        ),
        cpal::SampleFormat::I16 => device.build_output_stream(
            &config.into(),
            move |data: &mut [i16], _| {
                let ch = channels.max(1);
                let frames = data.len() / ch;
                let (mix_l, mix_r) = shared_cb.lock().render_block(frames);
                for (i, frame) in data.chunks_mut(ch).enumerate() {
                    let l = mix_l.get(i).copied().unwrap_or(0.0);
                    let r = mix_r.get(i).copied().unwrap_or(0.0);
                    let mono = if ch == 1 { (l + r) * 0.5 } else { l };
                    let sample = if ch == 1 { mono } else { l };
                    frame[0] = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                    if ch > 1 {
                        frame[1] = (r.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                    }
                }
            },
            |error| eprintln!("[plugin-host-midi] preview output stream error={error}"),
            None,
        ),
        _ => {
            eprintln!(
                "[plugin-host-midi] preview received but dsp_output=pending reason=unsupported_sample_format"
            );
            return false;
        }
    };
    let stream = match stream {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!(
                "[plugin-host-midi] preview received but dsp_output=pending reason=stream_error {error}"
            );
            return false;
        }
    };
    if let Err(error) = stream.play() {
        eprintln!(
            "[plugin-host-midi] preview received but dsp_output=pending reason=play_error {error}"
        );
        return false;
    }
    shared.lock().set_dsp_ready(true);
    eprintln!("[plugin-host-midi] preview dsp_output=ready sr={sample_rate}");
    std::mem::forget(stream);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_alloc::count_heap_events;
    use std::collections::VecDeque;

    fn shared(status: u8, data1: u8, data2: u8) -> SharedMidiEvent {
        SharedMidiEvent {
            sample_offset: 7,
            status,
            data1,
            data2,
            _pad: 0,
        }
    }

    /// Stands in for the instance's shared-memory ring.
    fn ring(events: &[SharedMidiEvent]) -> VecDeque<SharedMidiEvent> {
        events.iter().copied().collect()
    }

    /// Raw pitch bend / channel pressure from the engine ring used to be
    /// dropped here, so a bridged VST3 never saw a keyboard's bend wheel.
    #[test]
    fn shared_pitch_bend_and_pressure_reach_the_vst3_controllers() {
        let events: Vec<(u8, u8, u8, f32, u32)> = [
            shared(0xE3, 0x7F, 0x7F),
            shared(0xE3, 0x00, 0x40),
            shared(0xD3, 100, 0),
        ]
        .iter()
        .filter_map(|ev| shared_event_to_vst3(ev, "insert-1"))
        .map(|ev| (ev.kind, ev.channel, ev.pitch, ev.velocity, ev.sample_offset))
        .collect();
        let cc = Vst3MidiEventKind::ControlChange as u8;
        assert_eq!(events.len(), 3);
        assert_eq!(events[0], (cc, 3, 129, 1.0, 7));
        assert_eq!((events[1].0, events[1].1, events[1].2), (cc, 3, 129));
        assert!((events[1].3 - 0.5).abs() < 1.0e-3, "centre stays unbent");
        assert_eq!((events[2].0, events[2].1, events[2].2), (cc, 3, 128));
        assert!(events.iter().all(|ev| ev.4 == 7));
    }

    #[test]
    fn a_suspended_voice_is_skipped_without_losing_its_events() {
        let voice = VoiceShared::default();
        voice.events.lock().preview_note_on(0, 60, 100);
        let mut engine = ring(&[shared(0x90, 64, 90)]);
        {
            let _suspended = voice.suspend();
            // The producer does not wait: it skips the voice, but takes the
            // engine's events into its own buffer.
            let (block, taken) = begin_voice_block(&voice, &mut || engine.pop_front(), "i", true);
            assert!(block.is_none());
            assert_eq!(taken, 1);
            // The main thread still queues events meanwhile.
            voice.events.lock().preview_note_off(0, 60);
        }
        let (block, taken) = begin_voice_block(&voice, &mut || engine.pop_front(), "i", true);
        let block = block.expect("resumed");
        assert_eq!(taken, 0);
        assert_eq!(
            block.len(),
            3,
            "engine and preview events queued while suspended are delivered"
        );
        assert_eq!(block[0].pitch, 64, "the engine note keeps its place");
        assert!(voice.events.lock().pending_events.is_empty());
    }

    #[test]
    fn the_event_queue_is_free_while_a_block_is_rendering() {
        let voice = VoiceShared::default();
        let (block, _) = begin_voice_block(&voice, &mut || None, "i", true);
        let block = block.expect("block");
        // `process()` would run here, holding `render` only: the main thread
        // can still queue a note.
        assert!(voice.events.try_lock().is_some());
        // A second renderer does not wait for this one either.
        assert!(begin_voice_block(&voice, &mut || None, "i", true)
            .0
            .is_none());
        drop(block);
    }

    /// #318: the producer never waits on the main thread's preview queue. If
    /// the main thread is pushing, the preview events wait one block.
    #[test]
    fn a_busy_preview_queue_does_not_hold_the_block() {
        let voice = VoiceShared::default();
        voice.events.lock().preview_note_on(0, 60, 100);
        let held = voice.events.lock();
        let mut engine = ring(&[shared(0x90, 64, 90)]);
        {
            let (block, taken) = begin_voice_block(&voice, &mut || engine.pop_front(), "i", true);
            let mut block = block.expect("rendered");
            assert_eq!(taken, 1);
            assert_eq!(block.len(), 1, "engine MIDI only");
            block.clear();
        }
        drop(held);
        let (block, _) = begin_voice_block(&voice, &mut || None, "i", true);
        assert_eq!(block.expect("rendered")[0].pitch, 60);
    }

    /// #318: dense engine MIDI reaches a VST3 voice without the producer
    /// allocating, even in a burst far larger than any before it; what does
    /// not fit this block stays in the ring for the next one.
    #[test]
    fn engine_midi_bursts_take_no_heap_and_overflow_waits_in_the_ring() {
        let voice = VoiceShared::default();
        voice.events.lock().preview_note_on(0, 1, 100);
        let burst = VOICE_BLOCK_EVENT_CAPACITY + 100;
        let mut engine: VecDeque<SharedMidiEvent> = (0..burst)
            .map(|i| shared(0x90 | (i % 16) as u8, (i % 128) as u8, 100))
            .collect();
        let (taken, events) = count_heap_events(|| {
            let (block, taken) = begin_voice_block(&voice, &mut || engine.pop_front(), "i", true);
            let mut block = block.expect("rendered");
            assert_eq!(block.len(), VOICE_BLOCK_EVENT_CAPACITY);
            block.clear();
            taken
        });
        assert_eq!(events, 0, "the block path allocated or freed");
        assert_eq!(taken as usize, VOICE_BLOCK_EVENT_CAPACITY);
        assert_eq!(engine.len(), 100, "the overflow stays in the ring");
        // The preview event that did not fit is still queued.
        assert_eq!(voice.events.lock().pending_events.len(), 1);

        let (taken, events) = count_heap_events(|| {
            let (block, taken) = begin_voice_block(&voice, &mut || engine.pop_front(), "i", true);
            let mut block = block.expect("rendered");
            assert_eq!(block.len(), 101);
            block.clear();
            taken
        });
        assert_eq!(events, 0);
        assert_eq!(taken, 100);
        assert!(voice.events.lock().pending_events.is_empty());
    }

    /// Engine notes no longer pass through the preview queue, so a panic
    /// (editor detach, unload, all-notes-off) still has to release them.
    #[test]
    fn panic_releases_the_engine_notes_still_sounding() {
        let voice = VoiceShared::default();
        let mut engine = ring(&[
            shared(0x92, 60, 100),
            shared(0x92, 64, 100),
            shared(0x82, 60, 0),
            shared(0x9F, 127, 1),
        ]);
        let (block, _) = begin_voice_block(&voice, &mut || engine.pop_front(), "i", true);
        block.expect("rendered").clear();
        voice.panic();
        let note_off = Vst3MidiEventKind::NoteOff as u8;
        let released: Vec<(u8, u8)> = voice
            .events
            .lock()
            .pending_events
            .iter()
            .filter(|ev| ev.kind == note_off)
            .map(|ev| (ev.channel, ev.pitch))
            .collect();
        assert_eq!(released, vec![(2, 64), (15, 127)]);
        assert!(voice
            .held_notes
            .iter()
            .all(|word| word.load(Ordering::Relaxed) == 0));
    }

    /// #296: a voice being unloaded stays out of processing for good, even
    /// though the producer may still hold it in its plan until it adopts the
    /// next one — and a suspend/resume pair cannot bring it back.
    #[test]
    fn a_retired_voice_is_never_rendered_again() {
        let voice = VoiceShared::default();
        voice.retire();
        assert!(begin_voice_block(&voice, &mut || None, "i", true)
            .0
            .is_none());
        drop(voice.suspend());
        assert!(begin_voice_block(&voice, &mut || None, "i", true)
            .0
            .is_none());
    }

    #[test]
    fn a_voice_that_is_not_ready_keeps_its_events() {
        let voice = VoiceShared::default();
        let mut engine = ring(&[shared(0x90, 64, 90)]);
        let (block, taken) = begin_voice_block(&voice, &mut || engine.pop_front(), "i", false);
        assert!(block.is_none());
        assert_eq!(taken, 1);
        let (block, _) = begin_voice_block(&voice, &mut || None, "i", true);
        assert_eq!(block.expect("ready").len(), 1);
    }

    #[test]
    fn preview_control_change_keeps_pitch_bend_controller() {
        let mut state = VoiceMidiState::default();
        state.preview_control_change(0, 129, 64);
        let ev = state.pending_events[0];
        assert_eq!(ev.pitch, 129);
        assert!((ev.velocity - 0.5).abs() < 1.0e-3);
    }
}
