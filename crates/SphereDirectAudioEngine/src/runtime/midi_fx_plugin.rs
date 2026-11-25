//! A hosted MIDI-effect plug-in as one stage of a track's MIDI FX chain.
//!
//! The plug-in runs in the plug-in host like any bridged instance, under its
//! own instance id; [`PluginStage`] sits in the chain where the user put it
//! and exchanges MIDI with it through the instance's bridge sink. The
//! exchange is the bridge's one-block handshake: each block reads the answer
//! to the previous request (its MIDI output), then sends this block's input
//! and requests the next. So a plug-in stage's output is one bridge period
//! late, which the track's delay compensation accounts for
//! (`RuntimeMidiFxPlugins::latency_samples`). The plug-in's audio output is
//! read (that is what marks the block consumed) and discarded.

use sphere_midi_fx::{emit, Context, EventBuffer, Kind, MidiEvent, MidiFx, NoteTracker};

use crate::plugin_bridge::{BridgeMidiOut, SharedPluginBridgeSink, MAX_BRIDGE_BLOCK_FRAMES};
use crate::vst3_processor::{RuntimeTransportContext, Vst3MidiEvent};

use super::{push_vst3_midi_event_to_sink, VST3_CTRL_PROGRAM_CHANGE};

pub use crate::types::MIDI_FX_PLUGIN_DEVICE;

/// MIDI a plug-in stage takes from its host per block.
const OUTPUT_CAPACITY: usize = sphere_midi_fx::MAX_EVENTS;

/// The plug-in stages of a track, in chain order, and the bridge sink each
/// one's instance is registered under (`None` until the plug-in is loaded).
///
/// Lives on the runtime track, outside the chain's lock, so
/// `resolve_bridge_sinks` can fill it from the audio thread's command drain
/// without locking; each block hands the sinks to the stages.
#[derive(Debug, Clone, Default)]
pub struct RuntimeMidiFxPlugins {
    pub instance_ids: Vec<String>,
    pub sinks: Vec<Option<SharedPluginBridgeSink>>,
}

impl RuntimeMidiFxPlugins {
    /// The stages of `specs` (the chain's slots in order). Control thread.
    pub fn from_specs(specs: &[crate::types::EngineMidiFxSnapshot]) -> Self {
        let instance_ids: Vec<String> = specs
            .iter()
            .filter(|spec| spec.device == MIDI_FX_PLUGIN_DEVICE)
            .filter_map(|spec| spec.plugin_instance_id.clone())
            .collect();
        let sinks = vec![None; instance_ids.len()];
        Self {
            instance_ids,
            sinks,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.instance_ids.is_empty()
    }

    /// Re-cache each stage's sink from the engine's registry. No allocation:
    /// `sinks` is sized at build.
    pub fn resolve(
        &mut self,
        registry: &std::collections::HashMap<String, SharedPluginBridgeSink>,
    ) {
        for (id, sink) in self.instance_ids.iter().zip(self.sinks.iter_mut()) {
            *sink = registry.get(id).cloned();
        }
    }

    /// What the stages add to the track's latency: each loaded plug-in's
    /// reported latency plus the one-period handshake its MIDI waits for.
    /// Counted whether or not a stage is bypassed, so toggling bypass does
    /// not replan compensation.
    pub fn latency_samples(&self, bridge_period_frames: u32) -> u32 {
        self.sinks.iter().flatten().fold(0u32, |total, sink| {
            total.saturating_add(crate::plugin_bridge::bridged_insert_latency_samples(
                sink.reported_latency_samples(),
                bridge_period_frames,
            ))
        })
    }
}

/// One plug-in stage of a chain. See the module docs.
pub(crate) struct PluginStage {
    sink: Option<SharedPluginBridgeSink>,
    transport: RuntimeTransportContext,
    scratch_l: Box<[f32]>,
    scratch_r: Box<[f32]>,
    raw: Box<[BridgeMidiOut]>,
    /// Input not sent yet: the host still owned the previous request.
    backlog: Box<EventBuffer>,
    /// Notes this stage has sent on, so a release can end them.
    sounding: NoteTracker,
    /// A request was published and its answer not read yet.
    awaiting: bool,
    /// The next answer belongs to a request from before a release: its MIDI
    /// is dropped, not played.
    discard_next: bool,
    /// Tell the plug-in to end its notes with the next request.
    reset_plugin: bool,
}

impl PluginStage {
    /// Control thread (allocates its buffers).
    pub(crate) fn new() -> Self {
        Self {
            sink: None,
            transport: RuntimeTransportContext::default(),
            scratch_l: vec![0.0; MAX_BRIDGE_BLOCK_FRAMES].into_boxed_slice(),
            scratch_r: vec![0.0; MAX_BRIDGE_BLOCK_FRAMES].into_boxed_slice(),
            raw: vec![BridgeMidiOut::default(); OUTPUT_CAPACITY].into_boxed_slice(),
            backlog: Box::default(),
            sounding: NoteTracker::default(),
            awaiting: false,
            discard_next: false,
            reset_plugin: false,
        }
    }

    /// Before each block: the stage's current sink and the transport. Only
    /// clones the sink when it changed (a reference-count bump, no
    /// allocation).
    pub(crate) fn prepare(
        &mut self,
        sink: Option<&SharedPluginBridgeSink>,
        transport: &RuntimeTransportContext,
    ) {
        let same = match (&self.sink, sink) {
            (Some(current), Some(next)) => std::sync::Arc::ptr_eq(current, next),
            (None, None) => true,
            _ => false,
        };
        if !same {
            self.sink = sink.cloned();
            self.awaiting = false;
            self.discard_next = false;
        }
        self.transport = *transport;
    }
}

impl MidiFx for PluginStage {
    // The plug-in's parameters are its own (its editor, its state).
    fn param(&self, _index: usize) -> f32 {
        0.0
    }

    fn set_param(&mut self, _index: usize, _value: f32) {}

    fn process(&mut self, ctx: &Context, input: &EventBuffer, out: &mut EventBuffer) {
        out.clear();
        let Some(sink) = self.sink.as_deref() else {
            // Not loaded (yet): nothing comes out of a plug-in that is not
            // running, and its input is not kept for later.
            self.backlog.clear();
            return;
        };
        let frames = (ctx.frames as usize).min(self.scratch_l.len());
        if frames == 0 {
            return;
        }
        for event in input.as_slice() {
            self.backlog.push(*event);
        }

        let got = sink.read_output(
            &mut self.scratch_l[..frames],
            &mut self.scratch_r[..frames],
            frames,
        );
        if got > 0 {
            self.awaiting = false;
            let count = sink.pop_output_midi(&mut self.raw);
            if std::mem::take(&mut self.discard_next) {
                // The answer to a request from before a release.
            } else {
                let last = frames as u32 - 1;
                for raw in &self.raw[..count] {
                    if let Some(event) = from_raw(raw, last) {
                        emit(event, out, &mut self.sounding);
                    }
                }
            }
        }

        // The host owns the input until it answers; a missed answer keeps
        // this block's input for the next request.
        if got > 0 || !sink.request_in_flight() {
            if std::mem::take(&mut self.reset_plugin) {
                for channel in 0..16u8 {
                    // All Notes Off.
                    sink.push_midi(0xB0 | channel, 123, 0, 0);
                }
            }
            let last = frames as u32 - 1;
            for event in self.backlog.as_slice() {
                let event = to_vst3(event, last);
                push_vst3_midi_event_to_sink(sink, &event, "midi-fx", false);
            }
            self.backlog.clear();
            sink.set_transport(&self.transport);
            sink.request_block(frames as u32);
            self.awaiting = true;
        }
    }

    fn release(&mut self, offset: u32, out: &mut EventBuffer) {
        self.sounding.release_all(offset, out);
        self.backlog.clear();
        self.reset_plugin = true;
        // An answer already requested was made from the notes just ended.
        self.discard_next = self.awaiting;
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

fn to_vst3(event: &MidiEvent, last: u32) -> Vst3MidiEvent {
    let offset = event.offset.min(last);
    match event.kind {
        Kind::NoteOn => Vst3MidiEvent::note_on(offset, event.channel, event.data, event.value),
        Kind::NoteOff => Vst3MidiEvent::note_off(offset, event.channel, event.data, event.value),
        Kind::Control => {
            Vst3MidiEvent::control_change(offset, event.channel, u16::from(event.data), event.value)
        }
    }
}

/// A raw MIDI message from the plug-in as a chain event; messages the chain
/// does not carry (system, poly pressure) are dropped. Channel pressure,
/// pitch bend and program change travel as the engine's out-of-band
/// controllers 128 / 129 / 130, as everywhere else in the engine.
fn from_raw(raw: &BridgeMidiOut, last: u32) -> Option<MidiEvent> {
    let offset = raw.sample_offset.min(last);
    let channel = raw.status & 0x0F;
    let unit = |v: u8| f32::from(v & 0x7F) / 127.0;
    Some(match raw.status & 0xF0 {
        0x90 if raw.data2 > 0 => MidiEvent::note_on(offset, channel, raw.data1, unit(raw.data2)),
        0x80 | 0x90 => MidiEvent::note_off(offset, channel, raw.data1),
        0xB0 => MidiEvent::control(offset, channel, raw.data1 & 0x7F, unit(raw.data2)),
        0xC0 => MidiEvent::control(offset, channel, VST3_CTRL_PROGRAM_CHANGE, unit(raw.data1)),
        0xD0 => MidiEvent::control(offset, channel, 128, unit(raw.data1)),
        0xE0 => {
            let bend = u16::from(raw.data1 & 0x7F) | (u16::from(raw.data2 & 0x7F) << 7);
            MidiEvent::control(offset, channel, 129, f32::from(bend) / 16_383.0)
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin_bridge::PluginBridgeSink;
    use parking_lot::Mutex;
    use std::sync::Arc;

    /// A host that answers every request on the next read with the MIDI
    /// "plug-in" makes from what it was sent: every note an octave up.
    #[derive(Debug, Default)]
    struct OctaveHost {
        sent: Mutex<Vec<BridgeMidiOut>>,
        answer: Mutex<Vec<BridgeMidiOut>>,
        requested: Mutex<bool>,
        requests: Mutex<u32>,
    }

    impl PluginBridgeSink for OctaveHost {
        fn dsp_ready(&self) -> bool {
            true
        }
        fn read_output(&self, _l: &mut [f32], _r: &mut [f32], frames: usize) -> usize {
            let mut requested = self.requested.lock();
            if !*requested {
                return 0;
            }
            *requested = false;
            let sent = std::mem::take(&mut *self.sent.lock());
            let mut answer = self.answer.lock();
            for e in sent {
                let up = if e.status & 0xE0 == 0x80 {
                    e.data1 + 12
                } else {
                    e.data1
                };
                answer.push(BridgeMidiOut { data1: up, ..e });
            }
            frames
        }
        fn push_midi(&self, status: u8, data1: u8, data2: u8, sample_offset: u32) {
            self.sent.lock().push(BridgeMidiOut {
                sample_offset,
                status,
                data1,
                data2,
            });
        }
        fn write_input(&self, _l: &[f32], _r: &[f32], _frames: usize) {}
        fn request_block(&self, _frames: u32) {
            *self.requested.lock() = true;
            *self.requests.lock() += 1;
        }
        fn reported_latency_samples(&self) -> u32 {
            64
        }
        fn pop_output_midi(&self, out: &mut [BridgeMidiOut]) -> usize {
            let mut answer = self.answer.lock();
            let n = answer.len().min(out.len());
            out[..n].copy_from_slice(&answer[..n]);
            answer.drain(..n);
            n
        }
    }

    fn ctx() -> Context {
        Context {
            sample_rate: 48_000.0,
            frames: 256,
            tempo_bpm: 120.0,
            ppq: 0.0,
            playing: true,
        }
    }

    fn block(stage: &mut PluginStage, input: &[MidiEvent]) -> Vec<MidiEvent> {
        let mut buffer = EventBuffer::new();
        for e in input {
            buffer.push(*e);
        }
        let mut out = EventBuffer::new();
        stage.process(&ctx(), &buffer, &mut out);
        out.as_slice().to_vec()
    }

    fn stage_on(host: &Arc<OctaveHost>) -> PluginStage {
        let mut stage = PluginStage::new();
        let sink: SharedPluginBridgeSink = host.clone();
        stage.prepare(Some(&sink), &RuntimeTransportContext::default());
        stage
    }

    #[test]
    fn the_plugins_midi_comes_back_one_block_later() {
        let host = Arc::new(OctaveHost::default());
        let mut stage = stage_on(&host);
        let out = block(&mut stage, &[MidiEvent::note_on(10, 0, 60, 1.0)]);
        assert!(out.is_empty(), "nothing before the host answers");
        let out = block(&mut stage, &[MidiEvent::note_off(20, 0, 60)]);
        assert_eq!(out.len(), 1);
        assert_eq!(
            (out[0].kind, out[0].data, out[0].offset),
            (Kind::NoteOn, 72, 10)
        );
        let out = block(&mut stage, &[]);
        assert_eq!((out[0].kind, out[0].data), (Kind::NoteOff, 72));
    }

    #[test]
    fn a_release_ends_what_it_sent_and_drops_the_stale_answer() {
        let host = Arc::new(OctaveHost::default());
        let mut stage = stage_on(&host);
        block(&mut stage, &[MidiEvent::note_on(0, 0, 60, 1.0)]);
        block(&mut stage, &[MidiEvent::note_on(0, 0, 62, 1.0)]); // 72 sounds
        let mut offs = EventBuffer::new();
        stage.release(5, &mut offs);
        let offs: Vec<_> = offs.as_slice().iter().map(|e| (e.kind, e.data)).collect();
        assert_eq!(offs, vec![(Kind::NoteOff, 72)]);
        // The answer to the request made before the release (74) never plays.
        assert!(block(&mut stage, &[]).is_empty());
        // The plug-in was told to end its notes.
        let sent = host.sent.lock().clone();
        assert!(sent.is_empty() || sent.iter().all(|e| e.status & 0xF0 == 0xB0));
    }

    #[test]
    fn an_unloaded_plugin_passes_nothing_and_counts_no_latency() {
        let mut stage = PluginStage::new();
        assert!(block(&mut stage, &[MidiEvent::note_on(0, 0, 60, 1.0)]).is_empty());
        let mut plugins = RuntimeMidiFxPlugins {
            instance_ids: vec!["p".into()],
            sinks: vec![None],
        };
        assert_eq!(plugins.latency_samples(256), 0);
        let host: SharedPluginBridgeSink = Arc::new(OctaveHost::default());
        let registry = std::collections::HashMap::from([("p".to_string(), host)]);
        plugins.resolve(&registry);
        assert_eq!(plugins.latency_samples(256), 64 + 256);
    }

    #[test]
    fn raw_messages_map_to_chain_events() {
        let raw = |status, data1, data2| BridgeMidiOut {
            sample_offset: 9_999,
            status,
            data1,
            data2,
        };
        let e = from_raw(&raw(0x93, 60, 0), 255).unwrap();
        assert_eq!((e.kind, e.channel, e.offset), (Kind::NoteOff, 3, 255));
        let e = from_raw(&raw(0xE0, 0x7F, 0x7F), 255).unwrap();
        assert_eq!((e.kind, e.data, e.value), (Kind::Control, 129, 1.0));
        assert!(from_raw(&raw(0xF8, 0, 0), 255).is_none());
    }
}
