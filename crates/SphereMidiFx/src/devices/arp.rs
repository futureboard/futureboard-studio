//! The arpeggiator.

use super::{Values, emit, hash01, pass_control, schedule_off};
use crate::MidiFx;
use crate::event::{EventBuffer, Kind, MidiEvent, NoteTracker, Scheduler};
use crate::params::{ParamDesc, RATE_LABELS, RATE_SIXTEENTH, Unit, rate_beats};
use crate::time::{Clock, Context, GridSteps};

const PATTERN_LABELS: &[&str] = &[
    "midi-fx.arp.up",
    "midi-fx.arp.down",
    "midi-fx.arp.up-down",
    "midi-fx.arp.down-up",
    "midi-fx.arp.as-played",
    "midi-fx.arp.random",
    "midi-fx.arp.chord",
];
const VELOCITY_MODE_LABELS: &[&str] = &[
    "midi-fx.velocity-mode.as-played",
    "midi-fx.velocity-mode.fixed",
];

pub(crate) const PARAMS: &[ParamDesc] = &[
    ParamDesc::choice("pattern", "midi-fx.param.pattern", PATTERN_LABELS, 0),
    ParamDesc::choice("rate", "midi-fx.param.rate", RATE_LABELS, RATE_SIXTEENTH),
    ParamDesc::integer(
        "octaves",
        "midi-fx.param.octaves",
        1.0,
        4.0,
        1.0,
        Unit::None,
    ),
    ParamDesc::continuous(
        "gate",
        "midi-fx.param.gate",
        10.0,
        100.0,
        80.0,
        Unit::Percent,
    ),
    ParamDesc::continuous(
        "swing",
        "midi-fx.param.swing",
        0.0,
        75.0,
        0.0,
        Unit::Percent,
    ),
    ParamDesc::toggle("latch", "midi-fx.param.latch", false),
    ParamDesc::choice(
        "velocity-mode",
        "midi-fx.param.velocity-mode",
        VELOCITY_MODE_LABELS,
        0,
    ),
    ParamDesc::integer(
        "velocity",
        "midi-fx.param.velocity",
        1.0,
        127.0,
        100.0,
        Unit::Midi,
    ),
];
const P_PATTERN: usize = 0;
const P_RATE: usize = 1;
const P_OCTAVES: usize = 2;
const P_GATE: usize = 3;
const P_SWING: usize = 4;
const P_LATCH: usize = 5;
const P_VEL_MODE: usize = 6;
const P_VELOCITY: usize = 7;

const MAX_HELD: usize = 32;

#[derive(Clone, Copy, Default)]
struct Note {
    channel: u8,
    key: u8,
    velocity: f32,
}

/// Plays the held notes one at a time on the grid. Input notes are consumed;
/// controllers pass.
pub(crate) struct Arpeggiator {
    values: Values,
    tracker: NoteTracker,
    sched: Scheduler,
    clock: Clock,
    /// Notes in the order they were played.
    held: [Note; MAX_HELD],
    held_len: usize,
    /// Keys physically down (latch keeps `held` after they are released).
    down: u32,
    /// Steps played since the chord started.
    pos: u32,
}

impl Arpeggiator {
    pub(crate) fn new() -> Self {
        Self {
            values: Values::new(PARAMS),
            tracker: NoteTracker::default(),
            sched: Scheduler::default(),
            clock: Clock::default(),
            held: [Note::default(); MAX_HELD],
            held_len: 0,
            down: 0,
            pos: 0,
        }
    }

    fn remove(&mut self, channel: u8, key: u8) {
        if let Some(i) = self.held[..self.held_len]
            .iter()
            .position(|n| n.channel == channel && n.key == key)
        {
            self.held.copy_within(i + 1..self.held_len, i);
            self.held_len -= 1;
        }
    }

    /// The notes step `pos` plays (several for the Chord pattern), into
    /// `picked`; returns how many.
    fn pick(&self, k: i64, picked: &mut [Note; MAX_HELD * 4]) -> usize {
        let n = self.held_len;
        if n == 0 {
            return 0;
        }
        let octaves = self.values.int(P_OCTAVES).clamp(1, 4) as usize;
        let pattern = self.values.int(P_PATTERN);
        // Played order, or sorted low to high for the directional patterns.
        let mut base = [Note::default(); MAX_HELD];
        base[..n].copy_from_slice(&self.held[..n]);
        if pattern != 4 {
            base[..n].sort_by_key(|note| note.key);
        }
        let len = n * octaves;
        let at = |i: usize| -> Option<Note> {
            let note = base[i % n];
            let key = i32::from(note.key) + 12 * (i / n) as i32;
            (key <= 127).then_some(Note {
                key: key as u8,
                ..note
            })
        };
        if pattern == 6 {
            // Chord: everything held, every octave, on each step.
            let mut count = 0;
            for i in 0..len {
                if let Some(note) = at(i) {
                    picked[count] = note;
                    count += 1;
                }
            }
            return count;
        }
        let p = self.pos as usize;
        let idx = match pattern {
            1 => len - 1 - p % len,
            2 | 3 => {
                let period = (2 * len).saturating_sub(2).max(1);
                let i = p % period;
                let up = if i < len { i } else { period - i };
                if pattern == 2 { up } else { len - 1 - up }
            }
            5 => ((hash01(0xA4E9, k, u64::from(self.pos)) * len as f32) as usize).min(len - 1),
            _ => p % len,
        };
        match at(idx) {
            Some(note) => {
                picked[0] = note;
                1
            }
            None => 0,
        }
    }

    fn play_step(
        &mut self,
        k: i64,
        offset: u32,
        now: u64,
        step_samples: f64,
        out: &mut EventBuffer,
    ) {
        let mut picked = [Note::default(); MAX_HELD * 4];
        let count = self.pick(k, &mut picked);
        self.pos = self.pos.wrapping_add(1);
        if count == 0 {
            return;
        }
        let swing = f64::from(self.values.get(P_SWING)) / 100.0;
        let delay = if k.rem_euclid(2) == 1 {
            (step_samples * swing * 0.5) as u64
        } else {
            0
        };
        let gate = f64::from(self.values.get(P_GATE)) / 100.0;
        let len = (((step_samples - delay as f64) * gate) as u64).max(1);
        let fixed = self.values.int(P_VEL_MODE) == 1;
        let fixed_vel = self.values.get(P_VELOCITY) / 127.0;
        for note in &picked[..count] {
            let velocity = if fixed { fixed_vel } else { note.velocity };
            if self.tracker.is_on(note.channel, note.key) {
                self.sched.cancel(Kind::NoteOff, note.channel, note.key);
                emit(
                    MidiEvent::note_off(offset, note.channel, note.key),
                    out,
                    &mut self.tracker,
                );
            }
            let start = now + delay;
            let on = MidiEvent::note_on(offset, note.channel, note.key, velocity);
            if delay == 0 || !self.sched.schedule(start, on) {
                emit(on, out, &mut self.tracker);
            }
            schedule_off(
                &mut self.sched,
                &mut self.tracker,
                out,
                start + len,
                offset,
                note.channel,
                note.key,
            );
        }
    }
}

impl MidiFx for Arpeggiator {
    fn param(&self, i: usize) -> f32 {
        self.values.get(i)
    }
    fn set_param(&mut self, i: usize, v: f32) {
        let was_latched = self.values.on(P_LATCH);
        self.values.set(i, v);
        if was_latched && !self.values.on(P_LATCH) && self.down == 0 {
            // Unlatching with no key down lets the latched chord go.
            self.held_len = 0;
        }
    }
    fn process(&mut self, ctx: &Context, input: &EventBuffer, out: &mut EventBuffer) {
        out.clear();
        let spb = ctx.samples_per_beat();
        let step = rate_beats(self.values.get(P_RATE));
        let step_samples = step * spb;
        let latch = self.values.on(P_LATCH);
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
                match e.kind {
                    Kind::Control => pass_control(&e, out),
                    Kind::NoteOn => {
                        if latch && self.down == 0 {
                            // A new chord after letting go replaces the latched one.
                            self.held_len = 0;
                        }
                        if self.held_len == 0 {
                            self.pos = 0;
                            if !ctx.playing {
                                // Stopped: the pattern starts on this note.
                                self.clock.restart_free(-(f64::from(e.offset)) / spb);
                                block_ppq = self.clock.block_ppq(ctx);
                                steps = GridSteps::new(block_ppq, step, spb, ctx.frames).peekable();
                            }
                        }
                        self.down += 1;
                        self.remove(e.channel, e.data);
                        if self.held_len < MAX_HELD {
                            self.held[self.held_len] = Note {
                                channel: e.channel,
                                key: e.data,
                                velocity: e.value,
                            };
                            self.held_len += 1;
                        }
                    }
                    Kind::NoteOff => {
                        self.down = self.down.saturating_sub(1);
                        if !latch {
                            self.remove(e.channel, e.data);
                        }
                    }
                }
            } else {
                let (k, offset) = steps.next().expect("peeked");
                self.sched
                    .drain_before(clock, offset + 1, out, &mut self.tracker);
                self.play_step(k, offset, clock + u64::from(offset), step_samples, out);
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
        self.down = 0;
        self.pos = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::test_util::{assert_balanced, notes, run};

    const SIXTEENTH: u64 = 6_000;

    fn chord_input(release_at: u64) -> Vec<(u64, MidiEvent)> {
        vec![
            (0, MidiEvent::note_on(0, 0, 64, 0.9)),
            (0, MidiEvent::note_on(0, 0, 60, 0.9)),
            (0, MidiEvent::note_on(0, 0, 67, 0.9)),
            (release_at, MidiEvent::note_off(0, 0, 60)),
            (release_at, MidiEvent::note_off(0, 0, 64)),
            (release_at, MidiEvent::note_off(0, 0, 67)),
        ]
    }

    fn keys_on(ev: &[(u64, MidiEvent)]) -> Vec<u8> {
        notes(ev, Kind::NoteOn)
            .into_iter()
            .map(|(_, k)| k)
            .collect()
    }

    #[test]
    fn up_plays_sorted_notes_on_sixteenths() {
        let mut a = Arpeggiator::new();
        // 70 blocks reach past the last note's end (step 5 + 80 % gate).
        let ev = run(&mut a, 70, 512, true, &chord_input(6 * SIXTEENTH - 10));
        let ons = notes(&ev, Kind::NoteOn);
        assert_eq!(keys_on(&ev), vec![60, 64, 67, 60, 64, 67]);
        let times: Vec<_> = ons.iter().map(|(t, _)| *t).collect();
        assert_eq!(times, (0..6).map(|i| i * SIXTEENTH).collect::<Vec<_>>());
        // 80 % gate.
        let first_off = notes(&ev, Kind::NoteOff)[0];
        assert_eq!(first_off, (4_800, 60));
        assert_balanced(&ev);
    }

    #[test]
    fn patterns_and_octaves() {
        let mut a = Arpeggiator::new();
        a.set_param(P_PATTERN, 2.0); // up-down
        a.set_param(P_OCTAVES, 2.0);
        let ev = run(&mut a, 120, 512, true, &chord_input(10 * SIXTEENTH - 10));
        assert_eq!(keys_on(&ev), vec![60, 64, 67, 72, 76, 79, 76, 72, 67, 64]);

        let mut a = Arpeggiator::new();
        a.set_param(P_PATTERN, 4.0); // as played
        let ev = run(&mut a, 60, 512, true, &chord_input(3 * SIXTEENTH - 10));
        assert_eq!(keys_on(&ev), vec![64, 60, 67]);

        let mut a = Arpeggiator::new();
        a.set_param(P_PATTERN, 6.0); // chord
        let ev = run(&mut a, 60, 512, true, &chord_input(2 * SIXTEENTH - 10));
        assert_eq!(keys_on(&ev), vec![60, 64, 67, 60, 64, 67]);
        assert_balanced(&ev);
    }

    #[test]
    fn latch_keeps_the_chord_until_a_new_one() {
        let mut a = Arpeggiator::new();
        a.set_param(P_LATCH, 1.0);
        let mut input = chord_input(SIXTEENTH / 2);
        input.push((5 * SIXTEENTH + 10, MidiEvent::note_on(0, 0, 50, 1.0)));
        input.push((7 * SIXTEENTH, MidiEvent::note_off(0, 0, 50)));
        let ev = run(&mut a, 100, 512, true, &input);
        let keys = keys_on(&ev);
        // Steps 0-5 play the chord (released after step 0, latched); the new
        // note arrives just after step 5 and owns every step from 6.
        assert_eq!(
            &keys[..6],
            &[60, 64, 67, 60, 64, 67],
            "latched after release"
        );
        assert!(keys.len() > 6);
        assert!(
            keys[6..].iter().all(|&k| k == 50),
            "new chord replaces it: {keys:?}"
        );
        let mut out = EventBuffer::new();
        a.release(0, &mut out);
        let mut all = ev.clone();
        all.extend(out.as_slice().iter().map(|e| (u64::MAX, *e)));
        assert_balanced(&all);
    }

    #[test]
    fn swing_delays_odd_steps() {
        let mut a = Arpeggiator::new();
        a.set_param(P_SWING, 50.0);
        let ev = run(&mut a, 60, 512, true, &chord_input(4 * SIXTEENTH - 10));
        let times: Vec<_> = notes(&ev, Kind::NoteOn)
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        assert_eq!(
            times,
            vec![0, SIXTEENTH + 1_500, 2 * SIXTEENTH, 3 * SIXTEENTH + 1_500]
        );
        assert_balanced(&ev);
    }

    #[test]
    fn controls_pass_and_input_notes_are_consumed() {
        let mut a = Arpeggiator::new();
        let ev = run(
            &mut a,
            4,
            512,
            true,
            &[
                (3, MidiEvent::control(0, 0, 1, 0.5)),
                (10, MidiEvent::note_on(0, 0, 60, 1.0)),
                (20, MidiEvent::note_off(0, 0, 60)),
            ],
        );
        assert_eq!(ev[0], (3, MidiEvent::control(3, 0, 1, 0.5)));
        // The note started at 10 is not passed; the arp plays nothing until
        // the next step (and the key is gone by then).
        assert!(notes(&ev, Kind::NoteOn).is_empty());
    }
}
