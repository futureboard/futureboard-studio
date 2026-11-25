//! MIDI generators: notes from the transport, not from the input.
//!
//! Both play only while the transport runs (a stopped project makes no
//! notes), on the project grid, and pass their input through when *Thru* is
//! on. Their choices come from [`hash01`] over the grid step, so the same bar
//! plays the same notes on every pass and in every export.

use super::{Values, emit, hash01, pass_control, schedule_off};
use crate::MidiFx;
use crate::event::{EventBuffer, Kind, MidiEvent, NoteTracker, Scheduler};
use crate::params::{
    ParamDesc, RATE_LABELS, RATE_SIXTEENTH, ROOT_LABELS, SCALE_LABELS, Unit, rate_beats, scale_mask,
};
use crate::time::{Context, GridSteps};

/// Input handling shared by the generators: controls pass; notes pass when
/// Thru is on (tracked, so release ends them); the latest held key is kept
/// for generators that follow it.
struct Input {
    latest: Option<(u8, u8, f32)>,
}

impl Input {
    fn take(
        &mut self,
        e: &MidiEvent,
        thru: bool,
        out: &mut EventBuffer,
        tracker: &mut NoteTracker,
    ) {
        match e.kind {
            Kind::Control => pass_control(e, out),
            Kind::NoteOn => {
                self.latest = Some((e.channel, e.data, e.value));
                if thru {
                    emit(*e, out, tracker);
                }
            }
            Kind::NoteOff => {
                if self
                    .latest
                    .is_some_and(|(c, k, _)| c == e.channel && k == e.data)
                {
                    self.latest = None;
                }
                if thru {
                    emit(*e, out, tracker);
                }
            }
        }
    }
}

/// Sweep a block: input events and grid steps in time order, scheduled
/// note-offs drained between them. `on_step(k, offset, now, out)` plays a step.
#[allow(clippy::too_many_arguments)]
fn sweep(
    ctx: &Context,
    clock: u64,
    step: f64,
    input: &EventBuffer,
    out: &mut EventBuffer,
    sched: &mut Scheduler,
    tracker: &mut NoteTracker,
    mut on_input: impl FnMut(&MidiEvent, &mut EventBuffer, &mut NoteTracker),
    mut on_step: impl FnMut(i64, u32, u64, &mut EventBuffer, &mut Scheduler, &mut NoteTracker),
) {
    let spb = ctx.samples_per_beat();
    let mut steps = ctx
        .playing
        .then(|| GridSteps::new(ctx.ppq, step, spb, ctx.frames).peekable());
    let events = input.as_slice();
    let mut ei = 0;
    loop {
        let next_input = events.get(ei).map(|e| e.offset);
        let next_step = steps.as_mut().and_then(|s| s.peek().map(|&(_, o)| o));
        let take_input = match (next_input, next_step) {
            (Some(i), Some(s)) => i <= s,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => break,
        };
        if take_input {
            let e = events[ei];
            ei += 1;
            sched.drain_before(clock, e.offset + 1, out, tracker);
            on_input(&e, out, tracker);
        } else {
            let (k, offset) = steps.as_mut().and_then(|s| s.next()).expect("peeked");
            sched.drain_before(clock, offset + 1, out, tracker);
            on_step(k, offset, clock + u64::from(offset), out, sched, tracker);
        }
    }
    sched.drain_block(clock, ctx.frames, out, tracker);
}

/// Start `key` at `offset`, ending after `len` samples (retriggering a key
/// that is still sounding).
#[allow(clippy::too_many_arguments)]
fn play(
    out: &mut EventBuffer,
    sched: &mut Scheduler,
    tracker: &mut NoteTracker,
    offset: u32,
    now: u64,
    len: u64,
    channel: u8,
    key: u8,
    velocity: f32,
) {
    if tracker.is_on(channel, key) {
        sched.cancel(Kind::NoteOff, channel, key);
        emit(MidiEvent::note_off(offset, channel, key), out, tracker);
    }
    emit(
        MidiEvent::note_on(offset, channel, key, velocity),
        out,
        tracker,
    );
    schedule_off(sched, tracker, out, now + len.max(1), offset, channel, key);
}

// ── Pattern (step / Euclidean) ──────────────────────────────────────────────

const SOURCE_LABELS: &[&str] = &["midi-fx.source.fixed", "midi-fx.source.held"];

pub(crate) const PATTERN_PARAMS: &[ParamDesc] = &[
    ParamDesc::choice("rate", "midi-fx.param.rate", RATE_LABELS, RATE_SIXTEENTH),
    ParamDesc::integer("steps", "midi-fx.param.steps", 1.0, 32.0, 16.0, Unit::None),
    ParamDesc::integer("pulses", "midi-fx.param.pulses", 0.0, 32.0, 4.0, Unit::None),
    ParamDesc::integer("rotate", "midi-fx.param.rotate", 0.0, 31.0, 0.0, Unit::None),
    ParamDesc::choice("source", "midi-fx.param.source", SOURCE_LABELS, 0),
    ParamDesc::integer("note", "midi-fx.param.note", 0.0, 127.0, 36.0, Unit::Note),
    ParamDesc::integer(
        "velocity",
        "midi-fx.param.velocity",
        1.0,
        127.0,
        100.0,
        Unit::Midi,
    ),
    ParamDesc::integer("accent", "midi-fx.param.accent", 0.0, 64.0, 0.0, Unit::Midi),
    ParamDesc::continuous(
        "gate",
        "midi-fx.param.gate",
        10.0,
        100.0,
        50.0,
        Unit::Percent,
    ),
    ParamDesc::toggle("thru", "midi-fx.param.thru", true),
];
const PT_RATE: usize = 0;
const PT_STEPS: usize = 1;
const PT_PULSES: usize = 2;
const PT_ROTATE: usize = 3;
const PT_SOURCE: usize = 4;
const PT_NOTE: usize = 5;
const PT_VELOCITY: usize = 6;
const PT_ACCENT: usize = 7;
const PT_GATE: usize = 8;
const PT_THRU: usize = 9;

/// Whether step `pos` of a `steps`-long Euclidean rhythm with `pulses` hits.
pub(crate) fn euclid_hit(pos: i64, steps: i64, pulses: i64, rotate: i64) -> bool {
    if steps <= 0 || pulses <= 0 {
        return false;
    }
    let pulses = pulses.min(steps);
    let p = (pos + rotate).rem_euclid(steps);
    // Bresenham: a hit wherever the running total of pulses/steps crosses
    // an integer — the evenly spread pattern (E(3,8) = x..x..x.).
    (p * pulses) % steps < pulses
}

/// A step / Euclidean rhythm on a fixed note or on the key being held.
pub(crate) struct Pattern {
    values: Values,
    tracker: NoteTracker,
    sched: Scheduler,
    clock: u64,
    input: Input,
}

impl Pattern {
    pub(crate) fn new() -> Self {
        Self {
            values: Values::new(PATTERN_PARAMS),
            tracker: NoteTracker::default(),
            sched: Scheduler::default(),
            clock: 0,
            input: Input { latest: None },
        }
    }
}

impl MidiFx for Pattern {
    fn param(&self, i: usize) -> f32 {
        self.values.get(i)
    }
    fn set_param(&mut self, i: usize, v: f32) {
        self.values.set(i, v);
    }
    fn process(&mut self, ctx: &Context, input: &EventBuffer, out: &mut EventBuffer) {
        out.clear();
        let v = &self.values;
        let step = rate_beats(v.get(PT_RATE));
        let steps = i64::from(v.int(PT_STEPS));
        let pulses = i64::from(v.int(PT_PULSES));
        let rotate = i64::from(v.int(PT_ROTATE));
        let follow = v.int(PT_SOURCE) == 1;
        let fixed_key = v.int(PT_NOTE) as u8;
        let velocity = v.get(PT_VELOCITY);
        let accent = v.get(PT_ACCENT);
        let len = (step * ctx.samples_per_beat() * f64::from(v.get(PT_GATE)) / 100.0) as u64;
        let thru = v.on(PT_THRU);
        let input_state = &mut self.input;
        let latest = std::cell::Cell::new(input_state.latest);
        sweep(
            ctx,
            self.clock,
            step,
            input,
            out,
            &mut self.sched,
            &mut self.tracker,
            |e, out, tracker| {
                input_state.take(e, thru, out, tracker);
                latest.set(input_state.latest);
            },
            |k, offset, now, out, sched, tracker| {
                if !euclid_hit(k, steps, pulses, rotate) {
                    return;
                }
                let (channel, key) = if follow {
                    match latest.get() {
                        Some((c, key, _)) => (c, key),
                        None => return,
                    }
                } else {
                    (0, fixed_key)
                };
                let first = (k + rotate).rem_euclid(steps.max(1)) == 0;
                let vel = (velocity + if first { accent } else { 0.0 }).clamp(1.0, 127.0) / 127.0;
                play(out, sched, tracker, offset, now, len, channel, key, vel);
            },
        );
        self.clock += u64::from(ctx.frames);
    }
    fn release(&mut self, offset: u32, out: &mut EventBuffer) {
        self.sched.clear();
        self.tracker.release_all(offset, out);
        self.input.latest = None;
    }
}

// ── Random Melody ───────────────────────────────────────────────────────────

pub(crate) const MELODY_PARAMS: &[ParamDesc] = &[
    ParamDesc::choice("rate", "midi-fx.param.rate", RATE_LABELS, RATE_SIXTEENTH),
    ParamDesc::choice("root", "midi-fx.param.root", ROOT_LABELS, 0),
    ParamDesc::choice("scale", "midi-fx.param.scale-type", SCALE_LABELS, 10),
    ParamDesc::integer(
        "low",
        "midi-fx.param.lowest-note",
        0.0,
        120.0,
        48.0,
        Unit::Note,
    ),
    ParamDesc::integer(
        "range",
        "midi-fx.param.range-octaves",
        1.0,
        4.0,
        2.0,
        Unit::None,
    ),
    ParamDesc::continuous(
        "density",
        "midi-fx.param.density",
        0.0,
        100.0,
        60.0,
        Unit::Percent,
    ),
    ParamDesc::integer(
        "loop",
        "midi-fx.param.loop-steps",
        0.0,
        64.0,
        16.0,
        Unit::None,
    ),
    ParamDesc::integer("seed", "midi-fx.param.seed", 0.0, 9999.0, 1.0, Unit::None),
    ParamDesc::continuous(
        "gate",
        "midi-fx.param.gate",
        10.0,
        100.0,
        60.0,
        Unit::Percent,
    ),
    ParamDesc::integer(
        "velocity-min",
        "midi-fx.param.velocity-min",
        1.0,
        127.0,
        70.0,
        Unit::Midi,
    ),
    ParamDesc::integer(
        "velocity-max",
        "midi-fx.param.velocity-max",
        1.0,
        127.0,
        110.0,
        Unit::Midi,
    ),
    ParamDesc::toggle("thru", "midi-fx.param.thru", true),
];
const RM_RATE: usize = 0;
const RM_ROOT: usize = 1;
const RM_SCALE: usize = 2;
const RM_LOW: usize = 3;
const RM_RANGE: usize = 4;
const RM_DENSITY: usize = 5;
const RM_LOOP: usize = 6;
const RM_SEED: usize = 7;
const RM_GATE: usize = 8;
const RM_VEL_MIN: usize = 9;
const RM_VEL_MAX: usize = 10;
const RM_THRU: usize = 11;

/// Random notes from a scale, repeating every `loop` steps (0 = never).
pub(crate) struct RandomMelody {
    values: Values,
    tracker: NoteTracker,
    sched: Scheduler,
    clock: u64,
    input: Input,
}

impl RandomMelody {
    pub(crate) fn new() -> Self {
        Self {
            values: Values::new(MELODY_PARAMS),
            tracker: NoteTracker::default(),
            sched: Scheduler::default(),
            clock: 0,
            input: Input { latest: None },
        }
    }
}

/// The `n`-th scale note at or above `low`, within `count` candidates.
fn scale_note(low: i32, root: i32, mask: u16, n: usize) -> Option<u8> {
    let mut seen = 0;
    for key in low..=127 {
        if mask & (1 << (key - root).rem_euclid(12)) != 0 {
            if seen == n {
                return Some(key as u8);
            }
            seen += 1;
        }
    }
    None
}

impl MidiFx for RandomMelody {
    fn param(&self, i: usize) -> f32 {
        self.values.get(i)
    }
    fn set_param(&mut self, i: usize, v: f32) {
        self.values.set(i, v);
    }
    fn process(&mut self, ctx: &Context, input: &EventBuffer, out: &mut EventBuffer) {
        out.clear();
        let v = &self.values;
        let step = rate_beats(v.get(RM_RATE));
        let root = v.int(RM_ROOT);
        let mask = scale_mask(v.get(RM_SCALE));
        let low = v.int(RM_LOW);
        let high = (low + 12 * v.int(RM_RANGE)).min(127);
        let candidates = (low..high)
            .filter(|k| mask & (1 << (k - root).rem_euclid(12)) != 0)
            .count();
        let density = v.get(RM_DENSITY) / 100.0;
        let loop_steps = i64::from(v.int(RM_LOOP));
        let seed = v.int(RM_SEED) as u64;
        let len = (step * ctx.samples_per_beat() * f64::from(v.get(RM_GATE)) / 100.0) as u64;
        let vel_lo = v.get(RM_VEL_MIN);
        let vel_hi = v.get(RM_VEL_MAX).max(vel_lo);
        let thru = v.on(RM_THRU);
        let input_state = &mut self.input;
        sweep(
            ctx,
            self.clock,
            step,
            input,
            out,
            &mut self.sched,
            &mut self.tracker,
            |e, out, tracker| input_state.take(e, thru, out, tracker),
            |k, offset, now, out, sched, tracker| {
                if candidates == 0 {
                    return;
                }
                let idx = if loop_steps > 0 {
                    k.rem_euclid(loop_steps)
                } else {
                    k
                };
                if hash01(seed, idx, 1) >= density {
                    return;
                }
                let n = ((hash01(seed, idx, 2) * candidates as f32) as usize).min(candidates - 1);
                let Some(key) = scale_note(low, root, mask, n) else {
                    return;
                };
                let vel = (vel_lo + hash01(seed, idx, 3) * (vel_hi - vel_lo)).round() / 127.0;
                play(out, sched, tracker, offset, now, len, 0, key, vel);
            },
        );
        self.clock += u64::from(ctx.frames);
    }
    fn release(&mut self, offset: u32, out: &mut EventBuffer) {
        self.sched.clear();
        self.tracker.release_all(offset, out);
        self.input.latest = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::test_util::{assert_balanced, notes, run};

    const SIXTEENTH: u64 = 6_000;

    #[test]
    fn euclid_spreads_pulses_evenly() {
        let p: String = (0..8)
            .map(|i| if euclid_hit(i, 8, 3, 0) { 'x' } else { '.' })
            .collect();
        assert_eq!(p.matches('x').count(), 3);
        let four: String = (0..16)
            .map(|i| if euclid_hit(i, 16, 4, 0) { 'x' } else { '.' })
            .collect();
        assert_eq!(four, "x...x...x...x...");
        assert!(!euclid_hit(0, 16, 0, 0));
    }

    #[test]
    fn pattern_plays_four_on_the_floor_only_while_playing() {
        let mut g = Pattern::new();
        let ev = run(&mut g, 100, 512, true, &[]);
        let ons = notes(&ev, Kind::NoteOn);
        assert_eq!(ons[0], (0, 36));
        assert_eq!(ons[1], (4 * SIXTEENTH, 36));
        // 100 blocks of 512 = 51 200 samples: the last hit (step 8, 48 000)
        // ends at its 50 % gate (51 000), inside the run.
        assert_balanced(&ev);

        let mut stopped = Pattern::new();
        assert!(run(&mut stopped, 100, 512, false, &[]).is_empty());
    }

    #[test]
    fn pattern_follows_the_held_key_and_thru_passes_input() {
        let mut g = Pattern::new();
        g.set_param(PT_SOURCE, 1.0);
        g.set_param(PT_PULSES, 16.0);
        let ev = run(
            &mut g,
            40,
            512,
            true,
            &[
                (100, MidiEvent::note_on(0, 0, 50, 1.0)),
                (3 * SIXTEENTH - 50, MidiEvent::note_off(0, 0, 50)),
            ],
        );
        let ons = notes(&ev, Kind::NoteOn);
        assert_eq!(ons[0], (100, 50), "thru");
        assert!(ons[1..].iter().all(|(_, k)| *k == 50));
        assert_eq!(ons.len(), 3, "steps 1 and 2 while held: {ons:?}");
    }

    #[test]
    fn random_melody_is_in_scale_and_repeats_each_loop() {
        let mut g = RandomMelody::new();
        g.set_param(RM_DENSITY, 100.0);
        g.set_param(RM_LOOP, 4.0);
        let ev = run(&mut g, 120, 512, true, &[]);
        let ons = notes(&ev, Kind::NoteOn);
        assert!(ons.len() >= 8);
        let mask = scale_mask(10.0); // minor pentatonic
        for (_, k) in &ons {
            assert!(
                mask & (1 << (i32::from(*k)).rem_euclid(12)) != 0,
                "{k} in scale"
            );
            assert!((48..72).contains(k));
        }
        let keys: Vec<_> = ons.iter().map(|(_, k)| *k).collect();
        assert_eq!(keys[..4], keys[4..8], "a 4-step loop repeats");
        // Same seed, same notes: a second run is identical.
        let mut again = RandomMelody::new();
        again.set_param(RM_DENSITY, 100.0);
        again.set_param(RM_LOOP, 4.0);
        assert_eq!(run(&mut again, 120, 512, true, &[]), ev);
    }

    #[test]
    fn release_ends_what_a_generator_has_sounding() {
        let mut g = Pattern::new();
        g.set_param(PT_GATE, 100.0);
        let ev = run(&mut g, 2, 512, true, &[]);
        assert_eq!(notes(&ev, Kind::NoteOn).len(), 1);
        let mut out = EventBuffer::new();
        g.release(0, &mut out);
        assert_eq!(out.as_slice(), &[MidiEvent::note_off(0, 0, 36)]);
    }
}
