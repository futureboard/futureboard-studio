//! A track's MIDI FX chain on the audio thread.
//!
//! Every event bound for a track's instrument — clip notes, live input,
//! previews — is collected in `RuntimeTrack::midi_block_events` (a track with
//! MIDI FX never writes its instrument's bridge ring directly), then
//! [`RuntimeMidiFx::process_block`] runs the chain over it in place, before
//! any instrument renders. The bridged instrument then receives the result
//! through the same `midi_block_events` delivery as preview notes, so clips,
//! live input, offline export and bridged / in-process instruments all go
//! through one path.
//!
//! The chain lives behind an `Arc` shared by the live graph and the control
//! thread's mirror of it. A rebuild hands the *same* chain to the new graph
//! ([`RuntimeMidiFx::reconcile`]), so an arpeggio keeps its notes and position
//! through an edit and no window exists where notes skip the chain. The audio
//! thread only `try_lock`s it; a block that finds it locked (an edit in
//! progress) holds its events for the next block instead of passing them
//! unprocessed.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use sphere_midi_fx::{Chain, Context, DeviceKind, EventBuffer, Kind, MidiEvent, MidiFx};

use super::midi_fx_plugin::{PluginStage, RuntimeMidiFxPlugins, MIDI_FX_PLUGIN_DEVICE};
use crate::types::EngineMidiFxSnapshot;
use crate::vst3_processor::{RuntimeTransportContext, Vst3MidiEvent, Vst3MidiEventKind};

/// Events a track's MIDI list is reserved for when it has MIDI FX (a chord
/// or an arpeggio multiplies the input).
pub const MIDI_FX_EVENT_CAPACITY: usize = sphere_midi_fx::MAX_EVENTS;

/// What a slot runs.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SlotKind {
    Device(DeviceKind),
    /// A hosted plug-in, by bridge instance id.
    Plugin(String),
}

impl SlotKind {
    fn of(spec: &EngineMidiFxSnapshot) -> Option<Self> {
        if spec.device == MIDI_FX_PLUGIN_DEVICE {
            return spec.plugin_instance_id.clone().map(SlotKind::Plugin);
        }
        DeviceKind::from_id(&spec.device).map(SlotKind::Device)
    }

    fn create(&self) -> Box<dyn MidiFx> {
        match self {
            SlotKind::Device(kind) => kind.create(),
            SlotKind::Plugin(_) => Box::new(PluginStage::new()),
        }
    }
}

struct Slot {
    id: String,
    kind: SlotKind,
}

struct State {
    chain: Chain,
    slots: Vec<Slot>,
    io: Box<EventBuffer>,
    /// Note-offs owed to the instrument by devices that were removed or reset
    /// on the control thread; sent at the start of the next block.
    pending: Box<EventBuffer>,
    sample_rate: f64,
}

pub struct RuntimeMidiFx {
    state: Mutex<State>,
    /// Set by a panic / all-notes-off: the next block releases every device.
    release_requested: AtomicBool,
}

impl std::fmt::Debug for RuntimeMidiFx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeMidiFx").finish_non_exhaustive()
    }
}

fn apply_params(device: &mut dyn MidiFx, kind: &SlotKind, params: &[(String, f32)]) {
    let SlotKind::Device(kind) = *kind else {
        return;
    };
    for (id, value) in params {
        if let Some(index) = kind.param_index(id) {
            device.set_param(index, *value);
        }
    }
}

impl RuntimeMidiFx {
    /// A chain for `specs`, or `None` when none names a known device.
    /// Control thread (allocates the devices).
    pub fn build(specs: &[EngineMidiFxSnapshot], sample_rate: u32) -> Option<Arc<Self>> {
        let mut chain = Chain::new();
        let mut slots = Vec::with_capacity(specs.len());
        for spec in specs {
            let Some(kind) = SlotKind::of(spec) else {
                continue;
            };
            let mut device = kind.create();
            apply_params(device.as_mut(), &kind, &spec.params);
            chain.push(device, spec.enabled);
            slots.push(Slot {
                id: spec.id.clone(),
                kind,
            });
        }
        if slots.is_empty() {
            return None;
        }
        Some(Arc::new(Self {
            state: Mutex::new(State {
                chain,
                slots,
                io: Box::default(),
                pending: Box::default(),
                sample_rate: f64::from(sample_rate.max(1)),
            }),
            release_requested: AtomicBool::new(false),
        }))
    }

    /// Bring this (running) chain to `specs`: a slot whose id and device are
    /// unchanged keeps its device and state, new slots get new devices, and
    /// removed or replaced devices release their notes into the next block.
    /// Control thread; the lock is held only while devices are moved, never
    /// while one is created.
    pub fn reconcile(&self, specs: &[EngineMidiFxSnapshot], sample_rate: u32) {
        // Create the devices the new list adds before locking (only the
        // control thread changes the slot list, so it cannot change between
        // this look and the lock below). A parameter-only sync creates none.
        let existing: Vec<(String, SlotKind)> = {
            let st = self.state.lock();
            st.slots
                .iter()
                .map(|s| (s.id.clone(), s.kind.clone()))
                .collect()
        };
        let mut fresh: Vec<Option<Box<dyn MidiFx>>> = specs
            .iter()
            .map(|spec| {
                let kind = SlotKind::of(spec)?;
                let kept = existing.iter().any(|(id, k)| *id == spec.id && *k == kind);
                (!kept).then(|| kind.create())
            })
            .collect();
        let mut st = self.state.lock();
        st.sample_rate = f64::from(sample_rate.max(1));
        let old_devices = st.chain.take_devices();
        let old_slots = std::mem::take(&mut st.slots);
        type Running = (Slot, Box<dyn MidiFx>, bool);
        let mut old: Vec<Option<Running>> = old_slots
            .into_iter()
            .zip(old_devices)
            .map(|(slot, (device, enabled))| Some((slot, device, enabled)))
            .collect();
        for (spec, fresh) in specs.iter().zip(fresh.iter_mut()) {
            let Some(kind) = SlotKind::of(spec) else {
                continue;
            };
            let reused = old.iter_mut().find_map(|entry| {
                if entry
                    .as_ref()
                    .is_some_and(|(slot, _, _)| slot.id == spec.id && slot.kind == kind)
                {
                    entry.take()
                } else {
                    None
                }
            });
            let mut device = match reused {
                Some((_, mut device, was_enabled)) => {
                    if was_enabled && !spec.enabled {
                        let pending = &mut st.pending;
                        device.release(0, pending);
                    }
                    device
                }
                None => match fresh.take() {
                    Some(device) => device,
                    None => continue,
                },
            };
            apply_params(device.as_mut(), &kind, &spec.params);
            st.chain.push(device, spec.enabled);
            st.slots.push(Slot {
                id: spec.id.clone(),
                kind,
            });
        }
        // Devices gone from the list end what they were playing.
        for (_, mut device, enabled) in old.into_iter().flatten() {
            if enabled {
                device.release(0, &mut st.pending);
            }
        }
        drop(st);
        // `fresh` devices that were not needed are dropped here, unlocked.
        drop(fresh);
    }

    /// `true` when the chain has no devices and nothing left to send: the
    /// next rebuild can drop it.
    pub fn is_spent(&self) -> bool {
        let st = self.state.lock();
        st.slots.is_empty() && st.pending.is_empty()
    }

    /// End every note at the next block (panic, all notes off).
    pub fn request_release(&self) {
        self.release_requested.store(true, Ordering::Release);
    }

    /// Set parameter `param` of slot `slot_id` to `value` (control thread).
    pub fn set_param(&self, slot_id: &str, param: &str, value: f32) -> bool {
        let mut st = self.state.lock();
        let Some(index) = st.slots.iter().position(|s| s.id == slot_id) else {
            return false;
        };
        let SlotKind::Device(kind) = st.slots[index].kind else {
            // A plug-in's parameters are its own.
            return false;
        };
        let Some(param_index) = kind.param_index(param) else {
            return false;
        };
        match st.chain.device_mut(index) {
            Some(device) => {
                device.set_param(param_index, value);
                true
            }
            None => false,
        }
    }

    /// Bypass or re-enable slot `slot_id` (control thread).
    pub fn set_enabled(&self, slot_id: &str, enabled: bool) -> bool {
        let mut st = self.state.lock();
        let Some(index) = st.slots.iter().position(|s| s.id == slot_id) else {
            return false;
        };
        let State { chain, pending, .. } = &mut *st;
        chain.set_enabled(index, enabled, 0, pending);
        true
    }

    /// Run the chain over this block's events, in place. Audio thread:
    /// `try_lock` only, no allocation (both lists are reserved at build).
    ///
    /// `hold` keeps a block's events when the chain is locked by an edit; they
    /// go through the chain at the start of the next block. `plugins` is the
    /// track's plug-in stages in chain order (built from the same snapshot as
    /// this chain's slots, so the k-th plug-in slot is the k-th entry).
    pub fn process_block(
        &self,
        events: &mut Vec<Vst3MidiEvent>,
        hold: &mut Vec<Vst3MidiEvent>,
        frames: usize,
        transport: &RuntimeTransportContext,
        plugins: &RuntimeMidiFxPlugins,
    ) {
        let Some(mut guard) = self.state.try_lock() else {
            for e in events.drain(..) {
                if hold.len() < hold.capacity() {
                    hold.push(Vst3MidiEvent {
                        sample_offset: 0,
                        ..e
                    });
                }
            }
            return;
        };
        let st = &mut *guard;
        let mut plugin_index = 0;
        for (index, slot) in st.slots.iter().enumerate() {
            if !matches!(slot.kind, SlotKind::Plugin(_)) {
                continue;
            }
            let sink = plugins.sinks.get(plugin_index).and_then(Option::as_ref);
            plugin_index += 1;
            if let Some(stage) = st
                .chain
                .device_mut(index)
                .and_then(MidiFx::as_any_mut)
                .and_then(|any| any.downcast_mut::<PluginStage>())
            {
                stage.prepare(sink, transport);
            }
        }
        st.io.clear();
        for e in hold.drain(..).chain(events.drain(..)) {
            if let Some(ev) = to_fx(&e) {
                st.io.push(ev);
            }
        }
        if self.release_requested.swap(false, Ordering::AcqRel) {
            st.chain.release(0, &mut st.pending);
        }
        let ctx = Context {
            sample_rate: st.sample_rate,
            frames: frames.min(u32::MAX as usize) as u32,
            tempo_bpm: transport.tempo_bpm,
            ppq: transport.ppq_position,
            playing: transport.playing,
        };
        st.chain.process(&ctx, &mut st.io);
        for e in st.pending.as_slice().iter().chain(st.io.as_slice()) {
            if events.len() < events.capacity() {
                events.push(from_fx(e));
            }
        }
        st.pending.clear();
    }
}

fn to_fx(e: &Vst3MidiEvent) -> Option<MidiEvent> {
    let kind = match e.kind {
        k if k == Vst3MidiEventKind::NoteOn as u8 => Kind::NoteOn,
        k if k == Vst3MidiEventKind::NoteOff as u8 => Kind::NoteOff,
        k if k == Vst3MidiEventKind::ControlChange as u8 => Kind::Control,
        _ => return None,
    };
    Some(MidiEvent {
        offset: e.sample_offset,
        kind,
        channel: e.channel,
        data: e.pitch,
        value: e.velocity,
    })
}

fn from_fx(e: &MidiEvent) -> Vst3MidiEvent {
    match e.kind {
        Kind::NoteOn => Vst3MidiEvent::note_on(e.offset, e.channel, e.data, e.value),
        Kind::NoteOff => Vst3MidiEvent::note_off(e.offset, e.channel, e.data, e.value),
        Kind::Control => {
            Vst3MidiEvent::control_change(e.offset, e.channel, u16::from(e.data), e.value)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_PLUGINS: RuntimeMidiFxPlugins = RuntimeMidiFxPlugins {
        instance_ids: Vec::new(),
        sinks: Vec::new(),
    };

    fn spec(id: &str, device: &str, params: &[(&str, f32)]) -> EngineMidiFxSnapshot {
        EngineMidiFxSnapshot {
            id: id.into(),
            device: device.into(),
            plugin_instance_id: None,
            enabled: true,
            params: params.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        }
    }

    fn plugin_spec(id: &str, instance: &str) -> EngineMidiFxSnapshot {
        EngineMidiFxSnapshot {
            plugin_instance_id: Some(instance.into()),
            ..spec(id, MIDI_FX_PLUGIN_DEVICE, &[])
        }
    }

    /// A hosted "plug-in" that answers each request on the next read with
    /// the notes it was sent, unchanged.
    #[derive(Debug, Default)]
    struct EchoHost {
        sent: Mutex<Vec<crate::plugin_bridge::BridgeMidiOut>>,
        answer: Mutex<Vec<crate::plugin_bridge::BridgeMidiOut>>,
        requested: AtomicBool,
    }

    impl crate::plugin_bridge::PluginBridgeSink for EchoHost {
        fn dsp_ready(&self) -> bool {
            true
        }
        fn read_output(&self, _l: &mut [f32], _r: &mut [f32], frames: usize) -> usize {
            if !self.requested.swap(false, Ordering::AcqRel) {
                return 0;
            }
            let sent = std::mem::take(&mut *self.sent.lock());
            self.answer.lock().extend(sent);
            frames
        }
        fn push_midi(&self, status: u8, data1: u8, data2: u8, sample_offset: u32) {
            self.sent.lock().push(crate::plugin_bridge::BridgeMidiOut {
                sample_offset,
                status,
                data1,
                data2,
            });
        }
        fn write_input(&self, _l: &[f32], _r: &[f32], _frames: usize) {}
        fn request_block(&self, _frames: u32) {
            self.requested.store(true, Ordering::Release);
        }
        fn pop_output_midi(&self, out: &mut [crate::plugin_bridge::BridgeMidiOut]) -> usize {
            let mut answer = self.answer.lock();
            let n = answer.len().min(out.len());
            out[..n].copy_from_slice(&answer[..n]);
            answer.drain(..n);
            n
        }
    }

    /// Transpose → plug-in → chord: the plug-in sits where it was put. What
    /// reaches it is already transposed, and what it returns (a block later)
    /// is chorded after it.
    #[test]
    fn a_plugin_stage_runs_at_its_place_in_the_chain() {
        let specs = [
            spec("t", "transpose", &[("semitones", 12.0)]),
            plugin_spec("p", "inst-p"),
            spec("c", "chord", &[]),
        ];
        let fx = RuntimeMidiFx::build(&specs, 48_000).unwrap();
        let mut plugins = RuntimeMidiFxPlugins::from_specs(&specs);
        assert_eq!(plugins.instance_ids, vec!["inst-p".to_string()]);
        let host = Arc::new(EchoHost::default());
        let sink: crate::plugin_bridge::SharedPluginBridgeSink = host.clone();
        plugins.resolve(&std::collections::HashMap::from([(
            "inst-p".to_string(),
            sink,
        )]));

        let (mut events, mut hold) = lists();
        events.push(Vst3MidiEvent::note_on(10, 0, 48, 0.8));
        fx.process_block(&mut events, &mut hold, 256, &transport(0.0, true), &plugins);
        assert!(events.is_empty(), "the plug-in answers a block later");
        let sent = host.sent.lock().clone();
        assert_eq!(sent.len(), 1);
        assert_eq!(
            (sent[0].status, sent[0].data1),
            (0x90, 60),
            "transposed first"
        );

        events.clear();
        fx.process_block(&mut events, &mut hold, 256, &transport(0.1, true), &plugins);
        assert_eq!(keys(&events, Vst3MidiEventKind::NoteOn), vec![60, 64, 67]);
        assert!(
            !fx.set_param("p", "anything", 1.0),
            "a plug-in's params are its own"
        );
    }

    fn transport(ppq: f64, playing: bool) -> RuntimeTransportContext {
        RuntimeTransportContext {
            tempo_bpm: 120.0,
            ppq_position: ppq,
            playing,
            ..RuntimeTransportContext::default()
        }
    }

    fn lists() -> (Vec<Vst3MidiEvent>, Vec<Vst3MidiEvent>) {
        (
            Vec::with_capacity(MIDI_FX_EVENT_CAPACITY),
            Vec::with_capacity(MIDI_FX_EVENT_CAPACITY),
        )
    }

    fn keys(events: &[Vst3MidiEvent], kind: Vst3MidiEventKind) -> Vec<u8> {
        events
            .iter()
            .filter(|e| e.kind == kind as u8)
            .map(|e| e.pitch)
            .collect()
    }

    /// Every device in one chain, notes in, generators running, a panic at
    /// the end: the audio-thread path neither allocates nor frees — and does
    /// produce notes, so the zero is not from doing nothing.
    #[test]
    fn processing_every_device_takes_no_heap_work() {
        let specs: Vec<_> = DeviceKind::ALL
            .iter()
            .map(|k| spec(k.id(), k.id(), &[]))
            .collect();
        let fx = RuntimeMidiFx::build(&specs, 48_000).unwrap();
        let (mut events, mut hold) = lists();
        let mut produced = 0usize;
        let (_, heap) = crate::test_alloc::count_heap_events(|| {
            for block in 0..400u32 {
                events.clear();
                let key = 48 + (block / 10 % 12) as u8;
                match block % 10 {
                    0 => events.push(Vst3MidiEvent::note_on(3, 0, key, 0.8)),
                    5 => events.push(Vst3MidiEvent::note_off(7, 0, key, 0.0)),
                    _ => {}
                }
                let ppq = f64::from(block) * 256.0 / 24_000.0;
                fx.process_block(
                    &mut events,
                    &mut hold,
                    256,
                    &transport(ppq, true),
                    &NO_PLUGINS,
                );
                produced += events.len();
            }
            fx.request_release();
            events.clear();
            fx.process_block(
                &mut events,
                &mut hold,
                256,
                &transport(4.3, true),
                &NO_PLUGINS,
            );
        });
        assert_eq!(heap, 0, "heap work on the audio-thread path");
        assert!(produced > 50, "the chain played ({produced} events)");
    }

    #[test]
    fn unknown_devices_build_no_chain() {
        assert!(RuntimeMidiFx::build(&[spec("a", "no-such-device", &[])], 48_000).is_none());
        assert!(RuntimeMidiFx::build(&[], 48_000).is_none());
    }

    #[test]
    fn processes_in_place_with_its_params() {
        let fx = RuntimeMidiFx::build(
            &[
                spec("t", "transpose", &[("semitones", 12.0)]),
                spec("c", "chord", &[]),
            ],
            48_000,
        )
        .unwrap();
        let (mut events, mut hold) = lists();
        events.push(Vst3MidiEvent::note_on(10, 0, 48, 0.8));
        events.push(Vst3MidiEvent::control_change(20, 0, 64, 1.0));
        fx.process_block(
            &mut events,
            &mut hold,
            256,
            &transport(0.0, true),
            &NO_PLUGINS,
        );
        assert_eq!(keys(&events, Vst3MidiEventKind::NoteOn), vec![60, 64, 67]);
        assert_eq!(
            events.last().unwrap().kind,
            Vst3MidiEventKind::ControlChange as u8
        );
        assert!(
            events.capacity() >= MIDI_FX_EVENT_CAPACITY,
            "no reallocation"
        );
    }

    #[test]
    fn reconcile_keeps_running_devices_and_releases_removed_ones() {
        let fx = RuntimeMidiFx::build(&[spec("c", "chord", &[])], 48_000).unwrap();
        let (mut events, mut hold) = lists();
        events.push(Vst3MidiEvent::note_on(0, 0, 60, 1.0));
        fx.process_block(
            &mut events,
            &mut hold,
            256,
            &transport(0.0, true),
            &NO_PLUGINS,
        );
        assert_eq!(keys(&events, Vst3MidiEventKind::NoteOn), vec![60, 64, 67]);

        // Same slot, new params: the held chord is still the device's to end.
        fx.reconcile(&[spec("c", "chord", &[("shape", 2.0)])], 48_000);
        events.clear();
        events.push(Vst3MidiEvent::note_off(5, 0, 60, 0.0));
        fx.process_block(
            &mut events,
            &mut hold,
            256,
            &transport(0.0, true),
            &NO_PLUGINS,
        );
        assert_eq!(keys(&events, Vst3MidiEventKind::NoteOff), vec![60, 64, 67]);

        // Removed while sounding: its notes end at the next block.
        events.clear();
        events.push(Vst3MidiEvent::note_on(0, 0, 62, 1.0));
        fx.process_block(
            &mut events,
            &mut hold,
            256,
            &transport(0.0, true),
            &NO_PLUGINS,
        );
        fx.reconcile(&[], 48_000);
        events.clear();
        fx.process_block(
            &mut events,
            &mut hold,
            256,
            &transport(0.0, true),
            &NO_PLUGINS,
        );
        assert_eq!(keys(&events, Vst3MidiEventKind::NoteOff), vec![62, 65, 69]);
        assert!(fx.is_spent());
    }

    #[test]
    fn a_locked_chain_holds_events_for_the_next_block() {
        let fx = RuntimeMidiFx::build(&[spec("c", "chord", &[])], 48_000).unwrap();
        let (mut events, mut hold) = lists();
        events.push(Vst3MidiEvent::note_on(40, 0, 60, 1.0));
        {
            let _edit = fx.state.lock();
            fx.process_block(
                &mut events,
                &mut hold,
                256,
                &transport(0.0, true),
                &NO_PLUGINS,
            );
        }
        assert!(
            events.is_empty(),
            "nothing reaches the instrument unprocessed"
        );
        assert_eq!(hold.len(), 1);
        fx.process_block(
            &mut events,
            &mut hold,
            256,
            &transport(0.0, true),
            &NO_PLUGINS,
        );
        assert_eq!(keys(&events, Vst3MidiEventKind::NoteOn), vec![60, 64, 67]);
        assert!(hold.is_empty());
    }

    #[test]
    fn panic_releases_everything_and_bypass_ends_a_devices_notes() {
        let fx = RuntimeMidiFx::build(&[spec("c", "chord", &[])], 48_000).unwrap();
        let (mut events, mut hold) = lists();
        events.push(Vst3MidiEvent::note_on(0, 0, 60, 1.0));
        fx.process_block(
            &mut events,
            &mut hold,
            256,
            &transport(0.0, true),
            &NO_PLUGINS,
        );
        fx.request_release();
        events.clear();
        fx.process_block(
            &mut events,
            &mut hold,
            256,
            &transport(0.0, true),
            &NO_PLUGINS,
        );
        assert_eq!(keys(&events, Vst3MidiEventKind::NoteOff).len(), 3);

        events.clear();
        events.push(Vst3MidiEvent::note_on(0, 0, 60, 1.0));
        fx.process_block(
            &mut events,
            &mut hold,
            256,
            &transport(0.0, true),
            &NO_PLUGINS,
        );
        assert!(fx.set_enabled("c", false));
        events.clear();
        fx.process_block(
            &mut events,
            &mut hold,
            256,
            &transport(0.0, true),
            &NO_PLUGINS,
        );
        assert_eq!(keys(&events, Vst3MidiEventKind::NoteOff).len(), 3);
        assert!(fx.set_param("c", "shape", 3.0));
        assert!(!fx.set_param("c", "no-such-param", 1.0));
    }
}
