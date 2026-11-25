//! The built-in devices.

mod arp;
mod generators;
mod timing;
mod transform;

use crate::event::{EventBuffer, MidiEvent, NoteTracker, Scheduler, emit};
use crate::params::ParamDesc;
use crate::{DeviceKind, MAX_PARAMS, MidiFx};

pub(crate) fn params_of(kind: DeviceKind) -> &'static [ParamDesc] {
    match kind {
        DeviceKind::Arpeggiator => arp::PARAMS,
        DeviceKind::Chord => transform::CHORD_PARAMS,
        DeviceKind::Scale => transform::SCALE_PARAMS,
        DeviceKind::Transpose => transform::TRANSPOSE_PARAMS,
        DeviceKind::Velocity => transform::VELOCITY_PARAMS,
        DeviceKind::NoteLength => timing::NOTE_LENGTH_PARAMS,
        DeviceKind::Humanize => timing::HUMANIZE_PARAMS,
        DeviceKind::NoteRepeat => timing::NOTE_REPEAT_PARAMS,
        DeviceKind::PatternGenerator => generators::PATTERN_PARAMS,
        DeviceKind::RandomMelody => generators::MELODY_PARAMS,
    }
}

pub(crate) fn create(kind: DeviceKind) -> Box<dyn MidiFx> {
    match kind {
        DeviceKind::Arpeggiator => Box::new(arp::Arpeggiator::new()),
        DeviceKind::Chord => Box::new(transform::Chord::new()),
        DeviceKind::Scale => Box::new(transform::Scale::new()),
        DeviceKind::Transpose => Box::new(transform::Transpose::new()),
        DeviceKind::Velocity => Box::new(transform::Velocity::new()),
        DeviceKind::NoteLength => Box::new(timing::NoteLength::new()),
        DeviceKind::Humanize => Box::new(timing::Humanize::new()),
        DeviceKind::NoteRepeat => Box::new(timing::NoteRepeat::new()),
        DeviceKind::PatternGenerator => Box::new(generators::Pattern::new()),
        DeviceKind::RandomMelody => Box::new(generators::RandomMelody::new()),
    }
}

/// A device's parameter values, clamped to their descriptions.
#[derive(Clone)]
pub(crate) struct Values {
    descs: &'static [ParamDesc],
    v: [f32; MAX_PARAMS],
}

impl Values {
    pub(crate) fn new(descs: &'static [ParamDesc]) -> Self {
        let mut v = [0.0; MAX_PARAMS];
        for (slot, d) in v.iter_mut().zip(descs) {
            *slot = d.default;
        }
        Self { descs, v }
    }

    pub(crate) fn get(&self, i: usize) -> f32 {
        if i < self.descs.len() { self.v[i] } else { 0.0 }
    }

    pub(crate) fn set(&mut self, i: usize, value: f32) {
        if let Some(d) = self.descs.get(i) {
            self.v[i] = d.clamp(value);
        }
    }

    pub(crate) fn int(&self, i: usize) -> i32 {
        self.get(i).round() as i32
    }

    pub(crate) fn on(&self, i: usize) -> bool {
        self.get(i) >= 0.5
    }
}

/// A uniform number in `0.0..1.0` fixed by `seed` and two coordinates (a
/// grid step, a note). Same inputs, same number — so a loop or an export
/// repeats exactly.
pub(crate) fn hash01(seed: u64, a: i64, b: u64) -> f32 {
    let mut x = seed
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add((a as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F))
        .wrapping_add(b.wrapping_mul(0x1656_67B1_9E37_79F9));
    // splitmix64 finalizer
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    (x >> 40) as f32 / (1u64 << 24) as f32
}

/// Index of a `(channel, key)` pair in per-note tables.
pub(crate) fn slot(channel: u8, key: u8) -> usize {
    usize::from(channel & 0x0F) * 128 + usize::from(key & 0x7F)
}

/// Pass a controller through untouched (every device does).
pub(crate) fn pass_control(event: &MidiEvent, out: &mut EventBuffer) {
    out.push(*event);
}

/// End a note now when the scheduler is too full to hold its note-off.
pub(crate) fn schedule_off(
    sched: &mut Scheduler,
    tracker: &mut NoteTracker,
    out: &mut EventBuffer,
    due: u64,
    now_offset: u32,
    channel: u8,
    key: u8,
) {
    if !sched.schedule(due, MidiEvent::note_off(0, channel, key)) {
        emit(MidiEvent::note_off(now_offset, channel, key), out, tracker);
    }
}

pub(crate) fn key_in_range(key: i32) -> Option<u8> {
    (0..=127).contains(&key).then_some(key as u8)
}

#[cfg(test)]
pub(crate) mod test_util {
    use crate::{Context, EventBuffer, Kind, MidiEvent, MidiFx};

    pub fn ctx(ppq: f64, frames: u32, playing: bool) -> Context {
        Context {
            sample_rate: 48_000.0,
            frames,
            tempo_bpm: 120.0,
            ppq,
            playing,
        }
    }

    /// Run `device` for `blocks` blocks of `frames` from ppq 0 while playing,
    /// feeding `input` events at absolute sample positions. Returns every
    /// output event at its absolute sample position.
    pub fn run(
        device: &mut dyn MidiFx,
        blocks: u32,
        frames: u32,
        playing: bool,
        input: &[(u64, MidiEvent)],
    ) -> Vec<(u64, MidiEvent)> {
        let mut result = Vec::new();
        let mut out = EventBuffer::new();
        let spb = 24_000.0; // 120 BPM at 48 kHz
        for block in 0..blocks {
            let start = u64::from(block) * u64::from(frames);
            let mut inb = EventBuffer::new();
            for (at, ev) in input {
                if *at >= start && *at < start + u64::from(frames) {
                    let mut e = *ev;
                    e.offset = (*at - start) as u32;
                    inb.push(e);
                }
            }
            let c = ctx(start as f64 / spb, frames, playing);
            device.process(&c, &inb, &mut out);
            for e in out.as_slice() {
                result.push((start + u64::from(e.offset), *e));
            }
        }
        result
    }

    pub fn notes(events: &[(u64, MidiEvent)], kind: Kind) -> Vec<(u64, u8)> {
        events
            .iter()
            .filter(|(_, e)| e.kind == kind)
            .map(|(t, e)| (*t, e.data))
            .collect()
    }

    /// Every note-on has a later note-off and nothing is left sounding.
    pub fn assert_balanced(events: &[(u64, MidiEvent)]) {
        let mut sounding = std::collections::HashMap::new();
        for (_, e) in events {
            let k = (e.channel, e.data);
            match e.kind {
                Kind::NoteOn => *sounding.entry(k).or_insert(0i32) += 1,
                Kind::NoteOff => *sounding.entry(k).or_insert(0i32) -= 1,
                Kind::Control => {}
            }
        }
        assert!(
            sounding.values().all(|&v| v == 0),
            "unbalanced notes: {sounding:?}"
        );
    }
}
