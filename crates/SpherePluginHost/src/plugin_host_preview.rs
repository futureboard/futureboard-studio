//! VST3 MIDI preview + local audio output for the external PluginHost process.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use DirectAudio::vst3_processor::{Vst3MidiEvent, Vst3PluginState, Vst3RuntimeProcessor};

use crate::audio_bridge::{SharedMidiEvent, MAX_CHANNELS};

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
/// events) and the audio producer (which drains them per block). Its mutex is
/// only ever held for queue push/drain or one block `process()` — never across
/// plugin load, editor attach, or any other long host operation.
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

    /// Apply one engine-pushed shared-memory MIDI event. Raw status byte: high
    /// nibble = message, low nibble = channel.
    fn apply_shared(&mut self, ev: &SharedMidiEvent, instance_id: &str) {
        let channel = ev.status & 0x0F;
        let kind = ev.status & 0xF0;
        match kind {
            // Note-on with velocity 0 is a note-off (running-status idiom).
            0x90 if ev.data2 > 0 => {
                let vel = ev.data2.clamp(1, 127) as f32 / 127.0;
                if forensic_trace_enabled() {
                    eprintln!(
                        "[plugin-host-midi-consume] note_on instance={instance_id} pitch={} offset={}",
                        ev.data1, ev.sample_offset
                    );
                }
                self.pending_events.push(Vst3MidiEvent::note_on(
                    ev.sample_offset,
                    channel,
                    ev.data1.min(127),
                    vel,
                ));
                let key = PreviewNoteKey {
                    channel: channel.min(15),
                    pitch: ev.data1.min(127),
                };
                if !self.active_notes.contains(&key) {
                    self.active_notes.push(key);
                }
                self.tail_blocks = PREVIEW_TAIL_BLOCKS;
            }
            0x80 | 0x90 => {
                if forensic_trace_enabled() {
                    eprintln!(
                        "[plugin-host-midi-consume] note_off instance={instance_id} pitch={} offset={}",
                        ev.data1, ev.sample_offset
                    );
                }
                self.pending_events.push(Vst3MidiEvent::note_off(
                    ev.sample_offset,
                    channel,
                    ev.data1.min(127),
                    0.0,
                ));
                let key = PreviewNoteKey {
                    channel: channel.min(15),
                    pitch: ev.data1.min(127),
                };
                self.active_notes.retain(|n| *n != key);
                self.tail_blocks = PREVIEW_TAIL_BLOCKS;
            }
            0xB0 => {
                self.pending_events.push(Vst3MidiEvent::control_change(
                    ev.sample_offset,
                    channel,
                    ev.data1 as u16,
                    ev.data2 as f32 / 127.0,
                ));
                self.tail_blocks = PREVIEW_TAIL_BLOCKS;
            }
            // Raw channel pressure / pitch bend map back to the VST3
            // `kAfterTouch` (128) / `kPitchBend` (129) controllers, which the
            // processor resolves through the plugin's IMidiMapping.
            0xD0 => {
                self.pending_events.push(Vst3MidiEvent::control_change(
                    ev.sample_offset,
                    channel,
                    128,
                    ev.data1.min(127) as f32 / 127.0,
                ));
                self.tail_blocks = PREVIEW_TAIL_BLOCKS;
            }
            0xE0 => {
                let bend = u16::from(ev.data1 & 0x7F) | (u16::from(ev.data2 & 0x7F) << 7);
                self.pending_events.push(Vst3MidiEvent::control_change(
                    ev.sample_offset,
                    channel,
                    129,
                    f32::from(bend) / 16_383.0,
                ));
                self.tail_blocks = PREVIEW_TAIL_BLOCKS;
            }
            _ => {}
        }
    }
}

/// One entry of the published block-path snapshot: a shallow processor handle
/// (refcounted over the same C++ instance) plus the shared per-voice MIDI state.
#[derive(Debug, Clone)]
struct BridgeVoice {
    instance_id: String,
    processor: Vst3RuntimeProcessor,
    midi: Arc<Mutex<VoiceMidiState>>,
}

/// Render one voice block. The voice mutex is held across `process()`: it both
/// hands the drained events to the plugin atomically and serializes `process()`
/// between the bridge producer and the legacy debug CPAL preview path. Every
/// holder is bounded (an event push or one block render) — never plugin load or
/// editor attach, so this cannot starve block production the way the engine
/// mutex did.
fn render_voice(
    processor: &Vst3RuntimeProcessor,
    midi: &Mutex<VoiceMidiState>,
    in_l: &[f32],
    in_r: &[f32],
    out_l: &mut [f32],
    out_r: &mut [f32],
    transport: DirectAudio::vst3_processor::RuntimeTransportContext,
) {
    let mut state = midi.lock();
    let mut processor = processor.clone();
    // Real transport ProcessContext immediately before process() — same thread,
    // no race. The clone shares the same C++ processor via Arc.
    processor.set_process_context(&transport);
    let _ =
        processor.process_stereo_block_with_midi(in_l, in_r, out_l, out_r, &state.pending_events);
    // Borrowed and cleared rather than `mem::take`n: taking handed the buffer
    // to this frame and dropped it at the end of the block, so the next MIDI
    // push allocated a fresh `Vec` — a malloc/free pair per block on the audio
    // producer thread for as long as any note is held. Clearing keeps the
    // capacity the voice already grew to.
    let had_events = !state.pending_events.is_empty();
    state.pending_events.clear();
    if !had_events && state.active_notes.is_empty() {
        state.tail_blocks = state.tail_blocks.saturating_sub(1);
    } else {
        state.tail_blocks = PREVIEW_TAIL_BLOCKS;
    }
}

fn render_voice_interleaved(
    processor: &Vst3RuntimeProcessor,
    midi: &Mutex<VoiceMidiState>,
    in_l: &[f32],
    in_r: &[f32],
    out_interleaved: &mut [f32],
    output_channels: usize,
    transport: DirectAudio::vst3_processor::RuntimeTransportContext,
) -> usize {
    let mut state = midi.lock();
    let mut processor = processor.clone();
    processor.set_process_context(&transport);
    let channels = output_channels.clamp(1, MAX_CHANNELS);
    let got_channels = processor
        .process_main_output_block_with_midi(
            in_l,
            in_r,
            out_interleaved,
            channels,
            &state.pending_events,
        )
        .unwrap_or(0);
    // See `render_voice`: clear, never `mem::take` — the taken buffer was
    // dropped every block and reallocated by the next MIDI push.
    let had_events = !state.pending_events.is_empty();
    state.pending_events.clear();
    if !had_events && state.active_notes.is_empty() {
        state.tail_blocks = state.tail_blocks.saturating_sub(1);
    } else {
        state.tail_blocks = PREVIEW_TAIL_BLOCKS;
    }
    got_channels.min(channels)
}

/// Block-path handle for the audio producer thread. Replaces taking the whole
/// `PluginHostPreviewEngine` mutex per block: the voice list is an `Arc`
/// snapshot republished by the engine on load/unload only, and the flags are
/// atomics. The IPC thread can hold the engine mutex across `LoadPlugin` /
/// `IPlugView::attached` for seconds without ever stalling block production.
#[derive(Debug)]
pub struct BridgeAudioShared {
    /// Swapped wholesale on load/unload; the mutex is held only to clone or
    /// replace the `Arc` (nanoseconds), never across plugin or editor work.
    voices: Mutex<Arc<Vec<BridgeVoice>>>,
    dsp_ready: AtomicBool,
    continuous_mode: AtomicBool,
    /// Bumped on every publish; the producer echoes it after releasing its
    /// previous snapshot so unload can hand the final processor release (VST3
    /// terminate) back to the IPC thread. Bounded wait, never required for
    /// correctness.
    generation: AtomicU64,
    observed_generation: AtomicU64,
}

impl BridgeAudioShared {
    fn new() -> Self {
        Self {
            voices: Mutex::new(Arc::new(Vec::new())),
            dsp_ready: AtomicBool::new(false),
            continuous_mode: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            observed_generation: AtomicU64::new(0),
        }
    }

    fn snapshot(&self) -> Arc<Vec<BridgeVoice>> {
        self.voices.lock().clone()
    }

    fn publish(&self, voices: Vec<BridgeVoice>) {
        *self.voices.lock() = Arc::new(voices);
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Called by the producer once per loop iteration, after it has dropped any
    /// snapshot it held for the block.
    pub fn mark_snapshot_observed(&self) {
        self.observed_generation
            .store(self.generation.load(Ordering::Acquire), Ordering::Release);
    }

    /// Bounded wait until the producer has observed the latest publish (and so
    /// released any retired voice's processor clone). Times out silently — the
    /// worst case is the final release happening on the producer thread.
    fn wait_snapshot_observed(&self, timeout: Duration) {
        let target = self.generation.load(Ordering::Acquire);
        let deadline = Instant::now() + timeout;
        while self.observed_generation.load(Ordering::Acquire) < target {
            if Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_micros(100));
        }
    }

    pub fn dsp_ready(&self) -> bool {
        self.dsp_ready.load(Ordering::Acquire)
    }

    pub fn continuous_mode(&self) -> bool {
        self.continuous_mode.load(Ordering::Acquire)
    }

    pub fn has_loaded_instances(&self) -> bool {
        !self.snapshot().is_empty()
    }

    pub fn loaded_instance_ids(&self) -> Vec<String> {
        self.snapshot()
            .iter()
            .map(|v| v.instance_id.clone())
            .collect()
    }

    /// Apply one engine-pushed MIDI event to the voice owning `instance_id`.
    ///
    /// Each insert has its own shared region (and MIDI ring), so events are
    /// routed only to the matching voice — never broadcast. With two VSTi
    /// loaded, notes pushed to one instance must not sound on the other.
    /// Events for an instance that is not loaded (yet, or anymore) are dropped.
    pub fn apply_shared_midi(&self, instance_id: &str, ev: SharedMidiEvent) {
        for voice in self.snapshot().iter() {
            if voice.instance_id == instance_id {
                voice.midi.lock().apply_shared(&ev, &voice.instance_id);
                return;
            }
        }
    }

    /// The reported processing latency (samples) of the voice owning
    /// `instance_id`, or `None` if it is not loaded. Used by the host to publish
    /// `latency_samples` into the shared region for the engine's PDC/reporting.
    pub fn voice_latency_samples(&self, instance_id: &str) -> Option<i32> {
        self.snapshot()
            .iter()
            .find(|v| v.instance_id == instance_id)
            .map(|v| v.processor.get_latency_samples().max(0))
    }

    pub fn main_audio_output_channel_count_for_instance(&self, instance_id: &str) -> Option<u32> {
        self.snapshot()
            .iter()
            .find(|v| v.instance_id == instance_id)
            .map(|v| v.processor.main_audio_output_channel_count().max(1) as u32)
    }

    /// Apply one engine-pushed parameter change (normalized VST3 ParamID value)
    /// to the voice owning `instance_id`. The C++ processor queues it for the
    /// next `process()` call; routed to the matching voice only.
    pub fn apply_shared_param(&self, instance_id: &str, param_id: u32, value: f32) {
        for voice in self.snapshot().iter() {
            if voice.instance_id == instance_id {
                let mut processor = voice.processor.clone();
                processor.set_param(param_id, value as f64);
                return;
            }
        }
    }

    /// Render one block for a single insert instance (serial FX chain path)
    /// into caller-provided output buffers. Allocation-free: the producer
    /// thread reuses stack buffers every block instead of allocating two `Vec`s
    /// per callback, which used to cause latency spikes on the producer and
    /// occasional missed blocks (audible as VSTi stutter / dropped notes).
    #[allow(clippy::too_many_arguments)]
    pub fn render_single_voice(
        &self,
        instance_id: &str,
        frames: usize,
        in_l: &[f32],
        in_r: &[f32],
        out_l: &mut [f32],
        out_r: &mut [f32],
        transport: DirectAudio::vst3_processor::RuntimeTransportContext,
    ) {
        let n = frames.min(out_l.len()).min(out_r.len());
        out_l[..n].fill(0.0);
        out_r[..n].fill(0.0);
        if !self.dsp_ready() {
            return;
        }
        let voices = self.snapshot();
        if let Some(voice) = voices.iter().find(|v| v.instance_id == instance_id) {
            render_voice(
                &voice.processor,
                &voice.midi,
                in_l,
                in_r,
                &mut out_l[..n],
                &mut out_r[..n],
                transport,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render_single_voice_interleaved(
        &self,
        instance_id: &str,
        frames: usize,
        in_l: &[f32],
        in_r: &[f32],
        out_interleaved: &mut [f32],
        output_channels: usize,
        transport: DirectAudio::vst3_processor::RuntimeTransportContext,
    ) -> usize {
        let channels = output_channels.clamp(1, MAX_CHANNELS);
        let n = frames
            .min(in_l.len())
            .min(in_r.len())
            .min(out_interleaved.len() / channels);
        out_interleaved[..n * channels].fill(0.0);
        if !self.dsp_ready() {
            return 0;
        }
        let voices = self.snapshot();
        if let Some(voice) = voices.iter().find(|v| v.instance_id == instance_id) {
            render_voice_interleaved(
                &voice.processor,
                &voice.midi,
                &in_l[..n],
                &in_r[..n],
                &mut out_interleaved[..n * channels],
                channels,
                transport,
            )
        } else {
            0
        }
    }

    /// Render one block of all loaded voices without touching the engine mutex.
    pub fn render_block_with_input(
        &self,
        frames: usize,
        in_l: &[f32],
        in_r: &[f32],
    ) -> (Vec<f32>, Vec<f32>) {
        let mut mix_l = vec![0.0f32; frames];
        let mut mix_r = vec![0.0f32; frames];
        let voices = self.snapshot();
        if voices.is_empty() || !self.dsp_ready() {
            return (mix_l, mix_r);
        }
        let mut out_l = vec![0.0f32; frames];
        let mut out_r = vec![0.0f32; frames];
        // Legacy debug mixer path (CPAL preview): no engine transport available,
        // so use defaults. The shared-bridge path supplies real transport.
        let transport = DirectAudio::vst3_processor::RuntimeTransportContext::default();
        for voice in voices.iter() {
            render_voice(
                &voice.processor,
                &voice.midi,
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

#[derive(Debug)]
struct PreviewInstance {
    processor: Vst3RuntimeProcessor,
    midi: Arc<Mutex<VoiceMidiState>>,
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

    /// Restore a previously captured VST3 state. Holds the voice MIDI mutex
    /// across `setState` so it is serialized against the audio producer's
    /// `process()` (the same mutex `render_voice` holds per block) — the
    /// engine bypasses the missed block via the freshness guard, which is the
    /// correct trade for a glitch-free state swap.
    pub fn set_instance_state(&self, plugin_instance_id: &str, state: &Vst3PluginState) -> bool {
        let Some(instance) = self.instances.get(plugin_instance_id) else {
            eprintln!("[plugin-host-state] set_state instance={plugin_instance_id} loaded=false");
            return false;
        };
        let _voice_guard = instance.midi.lock();
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
                midi: Arc::new(Mutex::new(VoiceMidiState::default())),
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
                midi: Arc::new(Mutex::new(VoiceMidiState::default())),
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
            let mut midi = instance.midi.lock();
            instance.processor.view_detach();
            midi.panic();
        }
        if self.instances.is_empty() {
            self.set_continuous_mode(false);
        }
        if retired.is_some() {
            // Publish the snapshot without this voice, then give the producer a
            // bounded window to drop its block snapshot so the final processor
            // release (VST3 terminate) happens on this thread, not mid-block on
            // the audio producer.
            self.publish_bridge_snapshot();
            self.bridge
                .wait_snapshot_observed(Duration::from_millis(10));
        }
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
            // Control path only: IPlugView::removed() can touch the same plugin
            // internals as process(), so serialize it with the per-voice render
            // guard instead of racing the audio producer.
            let _voice_guard = instance.midi.lock();
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
            let mut midi = instance.midi.lock();
            instance.processor.view_detach();
            midi.panic();
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
        instance.midi.lock().preview_note_off(channel, pitch);
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
            .lock()
            .preview_control_change(channel, controller, value);
    }

    pub fn preview_all_notes_off(&mut self, plugin_instance_id: &str) {
        eprintln!("[plugin-host-midi] preview all_notes_off instance={plugin_instance_id}");
        let Some(instance) = self.instances.get(plugin_instance_id) else {
            return;
        };
        instance.midi.lock().panic();
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
            .any(|i| i.midi.lock().has_activity())
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
    use DirectAudio::vst3_processor::Vst3MidiEventKind;

    fn shared(status: u8, data1: u8, data2: u8) -> SharedMidiEvent {
        SharedMidiEvent {
            sample_offset: 7,
            status,
            data1,
            data2,
            _pad: 0,
        }
    }

    /// Raw pitch bend / channel pressure from the engine ring used to be
    /// dropped here, so a bridged VST3 never saw a keyboard's bend wheel.
    #[test]
    fn shared_pitch_bend_and_pressure_reach_the_vst3_controllers() {
        let mut state = VoiceMidiState::default();
        state.apply_shared(&shared(0xE3, 0x7F, 0x7F), "insert-1");
        state.apply_shared(&shared(0xE3, 0x00, 0x40), "insert-1");
        state.apply_shared(&shared(0xD3, 100, 0), "insert-1");

        let events: Vec<(u8, u8, u8, f32)> = state
            .pending_events
            .iter()
            .map(|ev| (ev.kind, ev.channel, ev.pitch, ev.velocity))
            .collect();
        let cc = Vst3MidiEventKind::ControlChange as u8;
        assert_eq!(events.len(), 3);
        assert_eq!(events[0], (cc, 3, 129, 1.0));
        assert_eq!((events[1].0, events[1].1, events[1].2), (cc, 3, 129));
        assert!((events[1].3 - 0.5).abs() < 1.0e-3, "centre stays unbent");
        assert_eq!((events[2].0, events[2].1, events[2].2), (cc, 3, 128));
        assert!(state.pending_events.iter().all(|ev| ev.sample_offset == 7));
    }

    /// The IPC preview fallback carries a 7-bit bend on controller 129; it
    /// must stay a bend (not CC 127) and centre on the unbent value.
    #[test]
    fn preview_control_change_keeps_pitch_bend_controller() {
        let mut state = VoiceMidiState::default();
        state.preview_control_change(0, 129, 64);
        let ev = state.pending_events[0];
        assert_eq!(ev.pitch, 129);
        assert!((ev.velocity - 0.5).abs() < 1.0e-3);
    }
}
