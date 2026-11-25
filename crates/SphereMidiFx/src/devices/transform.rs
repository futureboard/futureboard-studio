//! Devices that map each input note to output notes at the same time:
//! Transpose, Velocity, Scale, Chord.
//!
//! Each remembers which output notes an input note produced, so its note-off
//! ends exactly those — a parameter changed while a key is held never leaves
//! the old output note hanging.

use super::{Values, emit, hash01, key_in_range, pass_control, slot};
use crate::MidiFx;
use crate::event::{EventBuffer, Kind, MidiEvent, NoteTracker};
use crate::params::{ParamDesc, ROOT_LABELS, SCALE_LABELS, Unit};
use crate::time::Context;

const NONE: u8 = 0xFF;
/// Output notes one input note can produce (Chord: root + 4).
const FAN: usize = 5;

/// Output notes per input note, `NONE` where unused.
struct NoteMap {
    out: Box<[[u8; FAN]; 16 * 128]>,
}

impl NoteMap {
    fn new() -> Self {
        Self {
            out: Box::new([[NONE; FAN]; 16 * 128]),
        }
    }

    fn take(&mut self, channel: u8, key: u8) -> [u8; FAN] {
        std::mem::replace(&mut self.out[slot(channel, key)], [NONE; FAN])
    }

    fn set(&mut self, channel: u8, key: u8, keys: [u8; FAN]) {
        self.out[slot(channel, key)] = keys;
    }

    fn clear(&mut self) {
        for entry in self.out.iter_mut() {
            *entry = [NONE; FAN];
        }
    }
}

/// End the output notes `keys` (from one earlier input note).
fn end_notes(
    keys: [u8; FAN],
    offset: u32,
    channel: u8,
    out: &mut EventBuffer,
    t: &mut NoteTracker,
) {
    for k in keys.into_iter().filter(|&k| k != NONE) {
        emit(MidiEvent::note_off(offset, channel, k), out, t);
    }
}

/// The shared loop of every mapping device: controls pass, a note-on ends
/// whatever the same input key still had sounding and starts `map(event)`,
/// a note-off ends what that key started.
fn process_mapped(
    input: &EventBuffer,
    out: &mut EventBuffer,
    tracker: &mut NoteTracker,
    notes: &mut NoteMap,
    mut map: impl FnMut(&MidiEvent) -> ([u8; FAN], [f32; FAN]),
) {
    out.clear();
    for e in input.as_slice() {
        match e.kind {
            Kind::Control => pass_control(e, out),
            Kind::NoteOn => {
                end_notes(
                    notes.take(e.channel, e.data),
                    e.offset,
                    e.channel,
                    out,
                    tracker,
                );
                let (keys, velocities) = map(e);
                for (k, v) in keys.into_iter().zip(velocities) {
                    if k != NONE {
                        emit(MidiEvent::note_on(e.offset, e.channel, k, v), out, tracker);
                    }
                }
                notes.set(e.channel, e.data, keys);
            }
            Kind::NoteOff => {
                end_notes(
                    notes.take(e.channel, e.data),
                    e.offset,
                    e.channel,
                    out,
                    tracker,
                );
            }
        }
    }
}

fn single(key: Option<u8>, velocity: f32) -> ([u8; FAN], [f32; FAN]) {
    let mut keys = [NONE; FAN];
    let mut vels = [0.0; FAN];
    if let Some(k) = key {
        keys[0] = k;
        vels[0] = velocity;
    }
    (keys, vels)
}

// ── Transpose ───────────────────────────────────────────────────────────────

pub(crate) const TRANSPOSE_PARAMS: &[ParamDesc] = &[ParamDesc::integer(
    "semitones",
    "midi-fx.param.semitones",
    -48.0,
    48.0,
    0.0,
    Unit::Semitones,
)];

pub(crate) struct Transpose {
    values: Values,
    tracker: NoteTracker,
    notes: NoteMap,
}

impl Transpose {
    pub(crate) fn new() -> Self {
        Self {
            values: Values::new(TRANSPOSE_PARAMS),
            tracker: NoteTracker::default(),
            notes: NoteMap::new(),
        }
    }
}

impl MidiFx for Transpose {
    fn param(&self, i: usize) -> f32 {
        self.values.get(i)
    }
    fn set_param(&mut self, i: usize, v: f32) {
        self.values.set(i, v);
    }
    fn process(&mut self, _ctx: &Context, input: &EventBuffer, out: &mut EventBuffer) {
        let semis = self.values.int(0);
        process_mapped(input, out, &mut self.tracker, &mut self.notes, |e| {
            single(key_in_range(i32::from(e.data) + semis), e.value)
        });
    }
    fn release(&mut self, offset: u32, out: &mut EventBuffer) {
        self.tracker.release_all(offset, out);
        self.notes.clear();
    }
}

// ── Velocity ────────────────────────────────────────────────────────────────

pub(crate) const VELOCITY_PARAMS: &[ParamDesc] = &[
    ParamDesc::continuous(
        "scale",
        "midi-fx.param.scale",
        0.0,
        200.0,
        100.0,
        Unit::Percent,
    ),
    ParamDesc::integer(
        "offset",
        "midi-fx.param.offset",
        -64.0,
        64.0,
        0.0,
        Unit::Midi,
    ),
    ParamDesc::integer("random", "midi-fx.param.random", 0.0, 64.0, 0.0, Unit::Midi),
    ParamDesc::integer("min", "midi-fx.param.min", 1.0, 127.0, 1.0, Unit::Midi),
    ParamDesc::integer("max", "midi-fx.param.max", 1.0, 127.0, 127.0, Unit::Midi),
];
const V_SCALE: usize = 0;
const V_OFFSET: usize = 1;
const V_RANDOM: usize = 2;
const V_MIN: usize = 3;
const V_MAX: usize = 4;

pub(crate) struct Velocity {
    values: Values,
    tracker: NoteTracker,
    notes: NoteMap,
    clock: u64,
}

impl Velocity {
    pub(crate) fn new() -> Self {
        Self {
            values: Values::new(VELOCITY_PARAMS),
            tracker: NoteTracker::default(),
            notes: NoteMap::new(),
            clock: 0,
        }
    }
}

impl MidiFx for Velocity {
    fn param(&self, i: usize) -> f32 {
        self.values.get(i)
    }
    fn set_param(&mut self, i: usize, v: f32) {
        self.values.set(i, v);
    }
    fn process(&mut self, ctx: &Context, input: &EventBuffer, out: &mut EventBuffer) {
        let scale = self.values.get(V_SCALE) / 100.0;
        let offset = self.values.get(V_OFFSET);
        let random = self.values.get(V_RANDOM);
        let lo = self.values.get(V_MIN);
        let hi = self.values.get(V_MAX).max(lo);
        let clock = self.clock;
        process_mapped(input, out, &mut self.tracker, &mut self.notes, |e| {
            let jitter = if random > 0.0 {
                (hash01(
                    0x7E10,
                    (clock + u64::from(e.offset)) as i64,
                    u64::from(e.data),
                ) * 2.0
                    - 1.0)
                    * random
            } else {
                0.0
            };
            let v = (e.value * 127.0 * scale + offset + jitter)
                .round()
                .clamp(lo, hi);
            single(Some(e.data), v / 127.0)
        });
        self.clock += u64::from(ctx.frames);
    }
    fn release(&mut self, offset: u32, out: &mut EventBuffer) {
        self.tracker.release_all(offset, out);
        self.notes.clear();
    }
}

// ── Scale ───────────────────────────────────────────────────────────────────

const SCALE_MODE_LABELS: &[&str] = &[
    "midi-fx.scale-mode.nearest",
    "midi-fx.scale-mode.up",
    "midi-fx.scale-mode.down",
    "midi-fx.scale-mode.filter",
];

pub(crate) const SCALE_PARAMS: &[ParamDesc] = &[
    ParamDesc::choice("root", "midi-fx.param.root", ROOT_LABELS, 0),
    ParamDesc::choice("scale", "midi-fx.param.scale-type", SCALE_LABELS, 0),
    ParamDesc::choice("mode", "midi-fx.param.mode", SCALE_MODE_LABELS, 0),
];

/// `key` moved onto the scale, or `None` when Filter drops it.
pub(crate) fn quantize(key: u8, root: i32, mask: u16, mode: i32) -> Option<u8> {
    let in_scale = |k: i32| mask & (1 << (k - root).rem_euclid(12)) != 0;
    let k = i32::from(key);
    if in_scale(k) {
        return Some(key);
    }
    let up = (1..12).find(|d| in_scale(k + d)).map(|d| k + d);
    let down = (1..12).find(|d| in_scale(k - d)).map(|d| k - d);
    let target = match mode {
        1 => up,
        2 => down,
        3 => None,
        // Nearest; a tie goes down (the more common resolution).
        _ => match (down, up) {
            (Some(d), Some(u)) => Some(if k - d <= u - k { d } else { u }),
            (d, u) => d.or(u),
        },
    };
    target.and_then(key_in_range)
}

pub(crate) struct Scale {
    values: Values,
    tracker: NoteTracker,
    notes: NoteMap,
}

impl Scale {
    pub(crate) fn new() -> Self {
        Self {
            values: Values::new(SCALE_PARAMS),
            tracker: NoteTracker::default(),
            notes: NoteMap::new(),
        }
    }
}

impl MidiFx for Scale {
    fn param(&self, i: usize) -> f32 {
        self.values.get(i)
    }
    fn set_param(&mut self, i: usize, v: f32) {
        self.values.set(i, v);
    }
    fn process(&mut self, _ctx: &Context, input: &EventBuffer, out: &mut EventBuffer) {
        let root = self.values.int(0);
        let mask = crate::params::scale_mask(self.values.get(1));
        let mode = self.values.int(2);
        process_mapped(input, out, &mut self.tracker, &mut self.notes, |e| {
            single(quantize(e.data, root, mask, mode), e.value)
        });
    }
    fn release(&mut self, offset: u32, out: &mut EventBuffer) {
        self.tracker.release_all(offset, out);
        self.notes.clear();
    }
}

// ── Chord ───────────────────────────────────────────────────────────────────

const CHORD_SHAPE_LABELS: &[&str] = &[
    "midi-fx.chord.custom",
    "midi-fx.chord.major",
    "midi-fx.chord.minor",
    "midi-fx.chord.sus2",
    "midi-fx.chord.sus4",
    "midi-fx.chord.maj7",
    "midi-fx.chord.min7",
    "midi-fx.chord.dom7",
    "midi-fx.chord.dim",
    "midi-fx.chord.aug",
    "midi-fx.chord.power",
    "midi-fx.chord.octave",
];

pub(crate) const CHORD_PARAMS: &[ParamDesc] = &[
    ParamDesc::choice("shape", "midi-fx.param.shape", CHORD_SHAPE_LABELS, 1),
    ParamDesc::integer(
        "interval1",
        "midi-fx.param.interval1",
        -24.0,
        24.0,
        4.0,
        Unit::Semitones,
    ),
    ParamDesc::integer(
        "interval2",
        "midi-fx.param.interval2",
        -24.0,
        24.0,
        7.0,
        Unit::Semitones,
    ),
    ParamDesc::integer(
        "interval3",
        "midi-fx.param.interval3",
        -24.0,
        24.0,
        0.0,
        Unit::Semitones,
    ),
    ParamDesc::continuous(
        "velocity",
        "midi-fx.param.added-velocity",
        0.0,
        100.0,
        100.0,
        Unit::Percent,
    ),
];

/// Intervals above the played note for each shape (`0` = unused).
fn chord_intervals(shape: i32, custom: [i32; 3]) -> [i32; FAN - 1] {
    match shape {
        1 => [4, 7, 0, 0],
        2 => [3, 7, 0, 0],
        3 => [2, 7, 0, 0],
        4 => [5, 7, 0, 0],
        5 => [4, 7, 11, 0],
        6 => [3, 7, 10, 0],
        7 => [4, 7, 10, 0],
        8 => [3, 6, 0, 0],
        9 => [4, 8, 0, 0],
        10 => [7, 12, 0, 0],
        11 => [12, 0, 0, 0],
        _ => [custom[0], custom[1], custom[2], 0],
    }
}

pub(crate) struct Chord {
    values: Values,
    tracker: NoteTracker,
    notes: NoteMap,
}

impl Chord {
    pub(crate) fn new() -> Self {
        Self {
            values: Values::new(CHORD_PARAMS),
            tracker: NoteTracker::default(),
            notes: NoteMap::new(),
        }
    }
}

impl MidiFx for Chord {
    fn param(&self, i: usize) -> f32 {
        self.values.get(i)
    }
    fn set_param(&mut self, i: usize, v: f32) {
        self.values.set(i, v);
    }
    fn process(&mut self, _ctx: &Context, input: &EventBuffer, out: &mut EventBuffer) {
        let intervals = chord_intervals(
            self.values.int(0),
            [self.values.int(1), self.values.int(2), self.values.int(3)],
        );
        let added = self.values.get(4) / 100.0;
        process_mapped(input, out, &mut self.tracker, &mut self.notes, |e| {
            let mut keys = [NONE; FAN];
            let mut vels = [0.0; FAN];
            keys[0] = e.data;
            vels[0] = e.value;
            let mut n = 1;
            for iv in intervals.into_iter().filter(|&iv| iv != 0) {
                if let Some(k) = key_in_range(i32::from(e.data) + iv)
                    && !keys[..n].contains(&k)
                {
                    keys[n] = k;
                    vels[n] = (e.value * added).clamp(1.0 / 127.0, 1.0);
                    n += 1;
                }
            }
            (keys, vels)
        });
    }
    fn release(&mut self, offset: u32, out: &mut EventBuffer) {
        self.tracker.release_all(offset, out);
        self.notes.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::test_util::{assert_balanced, notes, run};

    #[test]
    fn transpose_releases_the_note_it_started_after_a_param_change() {
        let mut t = Transpose::new();
        t.set_param(0, 7.0);
        let mut out = EventBuffer::new();
        let ctx = crate::devices::test_util::ctx(0.0, 64, true);
        let mut inb = EventBuffer::new();
        inb.push(MidiEvent::note_on(0, 0, 60, 1.0));
        t.process(&ctx, &inb, &mut out);
        assert_eq!(out.as_slice()[0].data, 67);
        t.set_param(0, -5.0);
        inb.clear();
        inb.push(MidiEvent::note_off(3, 0, 60));
        t.process(&ctx, &inb, &mut out);
        assert_eq!(out.as_slice(), &[MidiEvent::note_off(3, 0, 67)]);
    }

    #[test]
    fn transpose_drops_notes_pushed_out_of_range() {
        let mut t = Transpose::new();
        t.set_param(0, 48.0);
        let ev = run(
            &mut t,
            1,
            64,
            true,
            &[
                (0, MidiEvent::note_on(0, 0, 100, 1.0)),
                (10, MidiEvent::note_off(0, 0, 100)),
            ],
        );
        assert!(ev.is_empty());
    }

    #[test]
    fn scale_snaps_to_c_major_and_filter_drops() {
        let major = crate::params::scale_mask(0.0);
        assert_eq!(quantize(61, 0, major, 0), Some(60)); // C# -> C (tie goes down)
        assert_eq!(quantize(61, 0, major, 1), Some(62)); // up -> D
        assert_eq!(quantize(66, 0, major, 2), Some(65)); // F# down -> F
        assert_eq!(quantize(61, 0, major, 3), None);
        assert_eq!(quantize(64, 0, major, 0), Some(64));
        // D major: C (pc 0) is not in it, C# is.
        assert_eq!(quantize(60, 2, major, 1), Some(61));
    }

    #[test]
    fn chord_builds_shapes_and_dedupes_overlaps() {
        let mut c = Chord::new(); // major by default
        let ev = run(
            &mut c,
            1,
            256,
            true,
            &[
                (0, MidiEvent::note_on(0, 0, 60, 1.0)),
                (4, MidiEvent::note_on(0, 0, 64, 1.0)), // its 64 already sounds
                (100, MidiEvent::note_off(0, 0, 60)),
                (120, MidiEvent::note_off(0, 0, 64)),
            ],
        );
        let ons = notes(&ev, Kind::NoteOn);
        assert_eq!(ons, vec![(0, 60), (0, 64), (0, 67), (4, 68), (4, 71)]);
        // 64 is held by both inputs: it ends only when the second lets go.
        let offs = notes(&ev, Kind::NoteOff);
        assert!(offs.contains(&(100, 60)) && offs.contains(&(100, 67)));
        assert!(offs.contains(&(120, 64)));
        assert_balanced(&ev);
    }

    #[test]
    fn velocity_scales_and_clamps() {
        let mut v = Velocity::new();
        v.set_param(V_SCALE, 50.0);
        v.set_param(V_MIN, 20.0);
        let ev = run(
            &mut v,
            1,
            64,
            true,
            &[
                (0, MidiEvent::note_on(0, 0, 60, 100.0 / 127.0)),
                (1, MidiEvent::note_on(0, 0, 62, 10.0 / 127.0)),
            ],
        );
        let vels: Vec<_> = ev.iter().map(|(_, e)| (e.value * 127.0).round()).collect();
        assert_eq!(vels, vec![50.0, 20.0]);
    }

    #[test]
    fn controls_pass_every_mapping_device() {
        for mut d in [
            Box::new(Transpose::new()) as Box<dyn MidiFx>,
            Box::new(Velocity::new()),
            Box::new(Scale::new()),
            Box::new(Chord::new()),
        ] {
            let ev = run(
                d.as_mut(),
                1,
                64,
                true,
                &[(5, MidiEvent::control(0, 0, 64, 1.0))],
            );
            assert_eq!(ev, vec![(5, MidiEvent::control(5, 0, 64, 1.0))]);
        }
    }
}
