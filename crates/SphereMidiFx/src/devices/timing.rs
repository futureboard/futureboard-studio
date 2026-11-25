//! Devices that move notes in time: Note Length, Humanize, Note Repeat.

use super::{Values, emit, hash01, pass_control, schedule_off, slot};
use crate::MidiFx;
use crate::event::{EventBuffer, Kind, MidiEvent, NoteTracker, Scheduler};
use crate::params::{ParamDesc, RATE_LABELS, RATE_SIXTEENTH, Unit, rate_beats};
use crate::time::{Clock, Context, GridSteps};

// ── Note Length ─────────────────────────────────────────────────────────────

const LENGTH_MODE_LABELS: &[&str] = &["midi-fx.length-mode.fixed", "midi-fx.length-mode.minimum"];

pub(crate) const NOTE_LENGTH_PARAMS: &[ParamDesc] = &[
    ParamDesc::choice("mode", "midi-fx.param.mode", LENGTH_MODE_LABELS, 0),
    ParamDesc::choice(
        "length",
        "midi-fx.param.length",
        RATE_LABELS,
        RATE_SIXTEENTH,
    ),
];

/// Every note lasts exactly the set length (Fixed), or at least it
/// (Minimum: a longer note keeps its own end).
pub(crate) struct NoteLength {
    values: Values,
    tracker: NoteTracker,
    sched: Scheduler,
    clock: u64,
    onset: Box<[u64; 16 * 128]>,
}

impl NoteLength {
    pub(crate) fn new() -> Self {
        Self {
            values: Values::new(NOTE_LENGTH_PARAMS),
            tracker: NoteTracker::default(),
            sched: Scheduler::default(),
            clock: 0,
            onset: Box::new([0; 16 * 128]),
        }
    }
}

impl MidiFx for NoteLength {
    fn param(&self, i: usize) -> f32 {
        self.values.get(i)
    }
    fn set_param(&mut self, i: usize, v: f32) {
        self.values.set(i, v);
    }
    fn process(&mut self, ctx: &Context, input: &EventBuffer, out: &mut EventBuffer) {
        out.clear();
        let fixed = self.values.int(0) == 0;
        let len = (rate_beats(self.values.get(1)) * ctx.samples_per_beat()).round() as u64;
        let clock = self.clock;
        for e in input.as_slice() {
            self.sched
                .drain_before(clock, e.offset + 1, out, &mut self.tracker);
            let now = clock + u64::from(e.offset);
            match e.kind {
                Kind::Control => pass_control(e, out),
                Kind::NoteOn => {
                    if self.tracker.is_on(e.channel, e.data) {
                        // Retriggered before its end: end it here, then start.
                        self.sched.cancel(Kind::NoteOff, e.channel, e.data);
                        emit(
                            MidiEvent::note_off(e.offset, e.channel, e.data),
                            out,
                            &mut self.tracker,
                        );
                    }
                    emit(*e, out, &mut self.tracker);
                    self.onset[slot(e.channel, e.data)] = now;
                    if fixed {
                        schedule_off(
                            &mut self.sched,
                            &mut self.tracker,
                            out,
                            now + len.max(1),
                            e.offset,
                            e.channel,
                            e.data,
                        );
                    }
                }
                Kind::NoteOff => {
                    if fixed || !self.tracker.is_on(e.channel, e.data) {
                        continue;
                    }
                    let due = self.onset[slot(e.channel, e.data)] + len.max(1);
                    if due <= now {
                        emit(*e, out, &mut self.tracker);
                    } else {
                        self.sched.cancel(Kind::NoteOff, e.channel, e.data);
                        schedule_off(
                            &mut self.sched,
                            &mut self.tracker,
                            out,
                            due,
                            e.offset,
                            e.channel,
                            e.data,
                        );
                    }
                }
            }
        }
        self.sched
            .drain_block(clock, ctx.frames, out, &mut self.tracker);
        self.clock += u64::from(ctx.frames);
    }
    fn release(&mut self, offset: u32, out: &mut EventBuffer) {
        self.sched.clear();
        self.tracker.release_all(offset, out);
    }
}

// ── Humanize ────────────────────────────────────────────────────────────────

pub(crate) const HUMANIZE_PARAMS: &[ParamDesc] = &[
    ParamDesc::continuous(
        "timing",
        "midi-fx.param.timing",
        0.0,
        50.0,
        10.0,
        Unit::Milliseconds,
    ),
    ParamDesc::integer(
        "velocity",
        "midi-fx.param.velocity-range",
        0.0,
        64.0,
        10.0,
        Unit::Midi,
    ),
];

/// Small random delays (never earlier: the future is not known) and velocity
/// changes. A note's end is delayed by the same amount as its start, so its
/// length is kept.
pub(crate) struct Humanize {
    values: Values,
    tracker: NoteTracker,
    sched: Scheduler,
    clock: u64,
    delay: Box<[u32; 16 * 128]>,
}

impl Humanize {
    pub(crate) fn new() -> Self {
        Self {
            values: Values::new(HUMANIZE_PARAMS),
            tracker: NoteTracker::default(),
            sched: Scheduler::default(),
            clock: 0,
            delay: Box::new([0; 16 * 128]),
        }
    }
}

impl MidiFx for Humanize {
    fn param(&self, i: usize) -> f32 {
        self.values.get(i)
    }
    fn set_param(&mut self, i: usize, v: f32) {
        self.values.set(i, v);
    }
    fn process(&mut self, ctx: &Context, input: &EventBuffer, out: &mut EventBuffer) {
        out.clear();
        let max_delay = ctx.samples_for_ms(self.values.get(0));
        let vel_range = self.values.get(1);
        let clock = self.clock;
        for e in input.as_slice() {
            self.sched
                .drain_before(clock, e.offset + 1, out, &mut self.tracker);
            let now = clock + u64::from(e.offset);
            let s = slot(e.channel, e.data);
            match e.kind {
                Kind::Control => pass_control(e, out),
                Kind::NoteOn => {
                    let r = hash01(0x4A11, now as i64, u64::from(e.data));
                    let d = (f64::from(r) * max_delay as f64) as u32;
                    let jitter =
                        (hash01(0x4A12, now as i64, u64::from(e.data)) * 2.0 - 1.0) * vel_range;
                    let vel = (e.value * 127.0 + jitter).round().clamp(1.0, 127.0) / 127.0;
                    let on = MidiEvent::note_on(0, e.channel, e.data, vel);
                    self.delay[s] = d;
                    if d == 0 || !self.sched.schedule(now + u64::from(d), on) {
                        self.delay[s] = 0;
                        emit(
                            MidiEvent {
                                offset: e.offset,
                                ..on
                            },
                            out,
                            &mut self.tracker,
                        );
                    }
                }
                Kind::NoteOff => {
                    let d = std::mem::take(&mut self.delay[s]);
                    if d == 0 {
                        emit(*e, out, &mut self.tracker);
                    } else if !self.sched.schedule(
                        now + u64::from(d),
                        MidiEvent::note_off(0, e.channel, e.data),
                    ) {
                        // No room for the delayed end: a start still pending
                        // never happens; one already sounding ends now.
                        if !self.sched.cancel(Kind::NoteOn, e.channel, e.data) {
                            emit(*e, out, &mut self.tracker);
                        }
                    }
                }
            }
        }
        self.sched
            .drain_block(clock, ctx.frames, out, &mut self.tracker);
        self.clock += u64::from(ctx.frames);
    }
    fn release(&mut self, offset: u32, out: &mut EventBuffer) {
        self.sched.clear();
        self.tracker.release_all(offset, out);
        self.delay.fill(0);
    }
}

// ── Note Repeat ─────────────────────────────────────────────────────────────

pub(crate) const NOTE_REPEAT_PARAMS: &[ParamDesc] = &[
    ParamDesc::choice("rate", "midi-fx.param.rate", RATE_LABELS, RATE_SIXTEENTH),
    ParamDesc::continuous(
        "gate",
        "midi-fx.param.gate",
        10.0,
        100.0,
        50.0,
        Unit::Percent,
    ),
    ParamDesc::continuous(
        "decay",
        "midi-fx.param.velocity-decay",
        0.0,
        100.0,
        0.0,
        Unit::Percent,
    ),
];

const MAX_HELD: usize = 32;

#[derive(Clone, Copy)]
struct Held {
    channel: u8,
    key: u8,
    velocity: f32,
    hits: u32,
    /// Device clock of the last hit, so a step at the note's own start does
    /// not hit it twice.
    last_hit: u64,
}

/// Retriggers every held note on the grid while it is held.
pub(crate) struct NoteRepeat {
    values: Values,
    tracker: NoteTracker,
    sched: Scheduler,
    clock: Clock,
    held: [Held; MAX_HELD],
    held_len: usize,
}

impl NoteRepeat {
    pub(crate) fn new() -> Self {
        Self {
            values: Values::new(NOTE_REPEAT_PARAMS),
            tracker: NoteTracker::default(),
            sched: Scheduler::default(),
            clock: Clock::default(),
            held: [Held {
                channel: 0,
                key: 0,
                velocity: 0.0,
                hits: 0,
                last_hit: 0,
            }; MAX_HELD],
            held_len: 0,
        }
    }

    fn hit(&mut self, i: usize, offset: u32, now: u64, step_samples: u64, out: &mut EventBuffer) {
        let gate = self.values.get(1) / 100.0;
        let decay = 1.0 - self.values.get(2) / 100.0;
        let h = self.held[i];
        if self.tracker.is_on(h.channel, h.key) {
            self.sched.cancel(Kind::NoteOff, h.channel, h.key);
            emit(
                MidiEvent::note_off(offset, h.channel, h.key),
                out,
                &mut self.tracker,
            );
        }
        let vel = (h.velocity * decay.powi(h.hits as i32)).max(1.0 / 127.0);
        emit(
            MidiEvent::note_on(offset, h.channel, h.key, vel),
            out,
            &mut self.tracker,
        );
        let len = ((step_samples as f32 * gate) as u64).max(1);
        schedule_off(
            &mut self.sched,
            &mut self.tracker,
            out,
            now + len,
            offset,
            h.channel,
            h.key,
        );
        self.held[i].hits += 1;
        self.held[i].last_hit = now;
    }
}

impl MidiFx for NoteRepeat {
    fn param(&self, i: usize) -> f32 {
        self.values.get(i)
    }
    fn set_param(&mut self, i: usize, v: f32) {
        self.values.set(i, v);
    }
    fn process(&mut self, ctx: &Context, input: &EventBuffer, out: &mut EventBuffer) {
        out.clear();
        let spb = ctx.samples_per_beat();
        let step = rate_beats(self.values.get(0));
        let step_samples = (step * spb).round().max(1.0) as u64;
        let clock = self.clock.samples;
        let mut block_ppq = self.clock.block_ppq(ctx);
        let mut steps = GridSteps::new(block_ppq, step, spb, ctx.frames).peekable();
        let events = input.as_slice();
        let mut ei = 0;
        loop {
            let next_input = events.get(ei).map(|e| e.offset);
            let next_step = steps.peek().map(|&(_, o)| o);
            let take_input = match (next_input, next_step) {
                (Some(i), Some(s)) => i <= s,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => break,
            };
            if take_input {
                let e = events[ei];
                ei += 1;
                self.sched
                    .drain_before(clock, e.offset + 1, out, &mut self.tracker);
                let now = clock + u64::from(e.offset);
                match e.kind {
                    Kind::Control => pass_control(&e, out),
                    Kind::NoteOn => {
                        if self.held_len == 0 && !ctx.playing {
                            // Stopped: the repeat grid starts on this note.
                            self.clock.restart_free(-(f64::from(e.offset)) / spb);
                            block_ppq = self.clock.block_ppq(ctx);
                            steps = GridSteps::new(block_ppq, step, spb, ctx.frames).peekable();
                        }
                        self.held_len =
                            remove_held(&mut self.held, self.held_len, e.channel, e.data);
                        if self.held_len < MAX_HELD {
                            self.held[self.held_len] = Held {
                                channel: e.channel,
                                key: e.data,
                                velocity: e.value,
                                hits: 0,
                                last_hit: 0,
                            };
                            self.held_len += 1;
                            self.hit(self.held_len - 1, e.offset, now, step_samples, out);
                        }
                    }
                    Kind::NoteOff => {
                        self.held_len =
                            remove_held(&mut self.held, self.held_len, e.channel, e.data);
                        if self.tracker.is_on(e.channel, e.data) {
                            self.sched.cancel(Kind::NoteOff, e.channel, e.data);
                            emit(e, out, &mut self.tracker);
                        }
                    }
                }
            } else {
                let (_, offset) = steps.next().expect("peeked");
                self.sched
                    .drain_before(clock, offset + 1, out, &mut self.tracker);
                let now = clock + u64::from(offset);
                for i in 0..self.held_len {
                    if self.held[i].hits > 0 && self.held[i].last_hit == now {
                        continue;
                    }
                    self.hit(i, offset, now, step_samples, out);
                }
            }
        }
        self.sched
            .drain_block(clock, ctx.frames, out, &mut self.tracker);
        self.clock.advance(ctx);
    }
    fn release(&mut self, offset: u32, out: &mut EventBuffer) {
        self.sched.clear();
        self.tracker.release_all(offset, out);
        self.held_len = 0;
    }
}

/// Remove `(channel, key)` from the first `len` entries; returns the new length.
fn remove_held(held: &mut [Held; MAX_HELD], len: usize, channel: u8, key: u8) -> usize {
    match held[..len]
        .iter()
        .position(|h| h.channel == channel && h.key == key)
    {
        Some(i) => {
            held.copy_within(i + 1..len, i);
            len - 1
        }
        None => len,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::test_util::{assert_balanced, notes, run};

    const SIXTEENTH: u64 = 6_000; // 1/16 at 120 BPM, 48 kHz

    #[test]
    fn fixed_length_ends_every_note_after_the_set_length() {
        let mut d = NoteLength::new();
        let ev = run(
            &mut d,
            40,
            512,
            true,
            &[
                (100, MidiEvent::note_on(0, 0, 60, 1.0)),
                (200, MidiEvent::note_off(0, 0, 60)), // shorter: extended
                (12_000, MidiEvent::note_on(0, 0, 62, 1.0)),
                (19_900, MidiEvent::note_off(0, 0, 62)), // longer: cut
            ],
        );
        assert_eq!(
            notes(&ev, Kind::NoteOff),
            vec![(100 + SIXTEENTH, 60), (12_000 + SIXTEENTH, 62)]
        );
        assert_balanced(&ev);
    }

    #[test]
    fn minimum_length_keeps_longer_notes() {
        let mut d = NoteLength::new();
        d.set_param(0, 1.0);
        let ev = run(
            &mut d,
            40,
            512,
            true,
            &[
                (100, MidiEvent::note_on(0, 0, 60, 1.0)),
                (200, MidiEvent::note_off(0, 0, 60)),
                (12_000, MidiEvent::note_on(0, 0, 62, 1.0)),
                (19_900, MidiEvent::note_off(0, 0, 62)),
            ],
        );
        assert_eq!(
            notes(&ev, Kind::NoteOff),
            vec![(100 + SIXTEENTH, 60), (19_900, 62)]
        );
    }

    #[test]
    fn humanize_only_delays_and_keeps_length() {
        let mut d = Humanize::new();
        d.set_param(0, 20.0);
        let mut input = Vec::new();
        for i in 0..16u64 {
            input.push((i * 3_000, MidiEvent::note_on(0, 0, 40 + i as u8, 0.8)));
            input.push((i * 3_000 + 1_000, MidiEvent::note_off(0, 0, 40 + i as u8)));
        }
        let ev = run(&mut d, 120, 512, true, &input);
        let ons = notes(&ev, Kind::NoteOn);
        let offs = notes(&ev, Kind::NoteOff);
        assert_eq!(ons.len(), 16);
        let max = 20 * 48; // 20 ms at 48 kHz
        let mut moved = 0;
        for (i, (t, key)) in ons.iter().enumerate() {
            let planned = i as u64 * 3_000;
            assert!(*t >= planned && *t <= planned + max, "note {key} at {t}");
            if *t != planned {
                moved += 1;
            }
            let off = offs.iter().find(|(_, k)| k == key).unwrap().0;
            assert_eq!(off - t, 1_000, "length of {key} kept");
        }
        assert!(moved > 8, "most notes are moved");
        assert_balanced(&ev);
    }

    #[test]
    fn note_repeat_retriggers_on_the_grid_while_held() {
        let mut d = NoteRepeat::new();
        let ev = run(
            &mut d,
            60,
            512,
            true,
            &[
                (0, MidiEvent::note_on(0, 0, 60, 1.0)),
                (4 * SIXTEENTH - 10, MidiEvent::note_off(0, 0, 60)),
            ],
        );
        let ons: Vec<_> = notes(&ev, Kind::NoteOn)
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        assert_eq!(ons, vec![0, SIXTEENTH, 2 * SIXTEENTH, 3 * SIXTEENTH]);
        assert_balanced(&ev);
    }

    #[test]
    fn note_repeat_runs_on_its_own_grid_while_stopped() {
        let mut d = NoteRepeat::new();
        let ev = run(
            &mut d,
            60,
            512,
            false,
            &[
                (1_000, MidiEvent::note_on(0, 0, 60, 1.0)),
                (1_000 + 2 * SIXTEENTH + 10, MidiEvent::note_off(0, 0, 60)),
            ],
        );
        let ons: Vec<_> = notes(&ev, Kind::NoteOn)
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        assert_eq!(ons, vec![1_000, 1_000 + SIXTEENTH, 1_000 + 2 * SIXTEENTH]);
        assert_balanced(&ev);
    }
}
