//! WhiteSharp's live displays: the stage (band, trace and dial), the note
//! readout in the ring, the In and Out meters, and the keyboard's lit note.
//!
//! The DSP publishes its newest pitch readings through the per-insert level
//! block (`whitesharp::telemetry`); the editor window's root polls it once a
//! frame and paints here, into bounds the cached panel recorded — so a frame
//! redraws the live layers and nothing else.
//!
//! The stage is drawn by wgpu off screen — the visualizers' renderer,
//! composited the same way (a shared texture on Windows, a read-back image
//! elsewhere) — under the panel's controls. The text and the meters are GPUI
//! quads and text over them, placed with the same [`StageLayout`].

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use gpui::{
    point, px, size, App, Bounds, FontWeight, Hsla, Pixels, SharedString, TextAlign, TextRun,
    Window,
};

use crate::components::builtin_plugin_editor_window::{
    BuiltinMeterSource, BuiltinPadLevelSource, PluginInstanceKey,
};
use crate::components::white_sharp_stage::{Rect, StageLayout, TracePoint, RING};
use crate::components::white_sharp_style::{self as style, BAND_MAX_REL, BAND_MIN_REL};
use whitesharp::telemetry::{self, Reading};

/// Readings a pause in the voice may last before the dial lets go.
const RECENT: usize = 6;
/// No new readings for this long and the voice is taken to have stopped —
/// the transport halted, or the insert was bypassed upstream.
const STALE: Duration = Duration::from_millis(160);
/// The trace's length: comfortably more than the band's left half holds.
const TRACE_POINTS: usize = 1_024;
/// How close to the readout's note counts as locked, in cents.
const LOCK_CENTS: f32 = 5.0;
const LOCK_PULSE: Duration = Duration::from_millis(720);
const NOTE_FADE: Duration = Duration::from_millis(360);

/// Two octaves of keys, C to B: the keyboard edits pitch classes, so both
/// octaves show the same marks.
pub const PIANO_KEYS: u8 = 24;
const PIANO_WHITES: f32 = 14.0;
/// A black key's height, as a share of the keyboard's.
pub const BLACK_KEY_HEIGHT: f32 = 0.58;

/// Where key `n` (0 = C, `PIANO_KEYS` keys) sits: its left edge and width
/// as shares of the keyboard's width, and whether it is a black key.
pub fn piano_key(n: u8) -> (f32, f32, bool) {
    let octave = f32::from(n / 12) * 7.0;
    let unit = 1.0 / PIANO_WHITES;
    let white = |index: f32| ((octave + index) * unit, unit, false);
    let black = |offset: f32| ((octave + offset) * unit, 0.58 * unit, true);
    match n % 12 {
        0 => white(0.0),
        2 => white(1.0),
        4 => white(2.0),
        5 => white(3.0),
        7 => white(4.0),
        9 => white(5.0),
        11 => white(6.0),
        1 => black(0.66),
        3 => black(1.74),
        6 => black(3.58),
        8 => black(4.68),
        _ => black(5.74),
    }
}

/// What the panel tells the displays each frame.
#[derive(Debug, Clone, Copy)]
pub struct LiveInputs {
    pub key: u8,
    pub scale: whitesharp::Scale,
    /// The notes a voice can be pulled to.
    pub allowed: u16,
    pub power: bool,
    pub sample_rate: f32,
}

/// Where the panel laid the live layers out, in window coordinates.
#[derive(Debug, Clone, Copy, Default)]
pub struct LiveBounds {
    pub stage: Option<Bounds<Pixels>>,
    pub orb: Option<Bounds<Pixels>>,
    pub meter_in: Option<Bounds<Pixels>>,
    pub meter_out: Option<Bounds<Pixels>>,
    pub keyboard: Option<Bounds<Pixels>>,
}

/// The note in the ring and the line under it.
#[derive(Debug, Clone, PartialEq)]
pub struct Readout {
    pub name: &'static str,
    pub detail: String,
    pub class: i32,
    /// A voice is sounding.
    pub live: bool,
    pub locked: bool,
}

/// The live displays' state across frames.
pub struct LiveDisplay {
    source: Option<BuiltinPadLevelSource>,
    meters: Option<BuiltinMeterSource>,
    key: Option<PluginInstanceKey>,
    block_seq: Option<u32>,
    /// The DSP's reading counter at the last block taken.
    seen: Option<u32>,
    trace: VecDeque<TracePoint>,
    next_index: u64,
    last_fresh: Option<Instant>,
    /// Set by [`Self::show`]: the shown reading never goes stale.
    pinned: bool,
    /// What the voice is doing now; `None` when it is not sounding.
    pub reading: Option<Reading>,
    active: bool,
    /// The last note heard, for the ring once the voice stops.
    last_midi: Option<f32>,
    song: Option<(u8, whitesharp::Scale)>,
    /// The band's tonic, as a MIDI note.
    anchor: i32,
    ring: [f32; RING],
    ring_cursor: usize,
    needle: f32,
    tonic_deg: f32,
    pips: [f32; 12],
    tick_alpha: f32,
    was_locked: bool,
    lock_at: Option<Instant>,
    note: Option<&'static str>,
    note_at: Option<Instant>,
    arc: (f32, f32),
    meter_in: f32,
    meter_out: f32,
    readout: Option<Readout>,
    last_tick: Option<Instant>,
    epoch: Instant,
    #[cfg(feature = "gpu-renderer")]
    presenter: presenter::Presenter,
}

impl Default for LiveDisplay {
    fn default() -> Self {
        Self {
            source: None,
            meters: None,
            key: None,
            block_seq: None,
            seen: None,
            trace: VecDeque::with_capacity(TRACE_POINTS),
            next_index: 0,
            last_fresh: None,
            pinned: false,
            reading: None,
            active: false,
            last_midi: None,
            song: None,
            anchor: 60,
            ring: [0.0; RING],
            ring_cursor: 0,
            needle: 0.0,
            tonic_deg: 0.0,
            pips: [0.0; 12],
            tick_alpha: 1.0,
            was_locked: false,
            lock_at: None,
            note: None,
            note_at: None,
            arc: (0.0, 0.0),
            meter_in: 0.0,
            meter_out: 0.0,
            readout: None,
            last_tick: None,
            epoch: Instant::now(),
            #[cfg(feature = "gpu-renderer")]
            presenter: presenter::Presenter::default(),
        }
    }
}

/// Meter position for a linear level: -60 dBFS at the left, 0 dBFS at the
/// right.
fn meter_fraction(level: f32) -> f32 {
    if level <= 1.0e-6 {
        return 0.0;
    }
    ((20.0 * level.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
}

/// `cents` as the readout prints it: a true minus, no sign at zero.
pub fn format_cents(cents: i32) -> String {
    match cents {
        0 => "0".to_string(),
        c if c > 0 => format!("+{c}"),
        c => format!("\u{2212}{}", c.unsigned_abs()),
    }
}

/// The shortest turn from `from` to `to`, in degrees.
fn turn(from: f32, to: f32) -> f32 {
    (to - from + 540.0).rem_euclid(360.0) - 180.0
}

/// The tonic in the octave that puts `midi` nearest the band's middle.
fn anchor_for(midi: f32, key: u8) -> i32 {
    let middle = (BAND_MIN_REL + BAND_MAX_REL) * 0.5;
    let octave = ((midi - f32::from(key) - middle) / 12.0).round() as i32;
    i32::from(key) + 12 * octave
}

impl LiveDisplay {
    /// Points the displays at an insert's published readings and meters.
    pub fn bind(
        &mut self,
        key: PluginInstanceKey,
        source: Option<BuiltinPadLevelSource>,
        meters: Option<BuiltinMeterSource>,
    ) {
        if self.key.as_ref() != Some(&key) {
            self.trace.clear();
            self.block_seq = None;
            self.seen = None;
            self.reading = None;
            self.last_midi = None;
            self.last_fresh = None;
        }
        self.key = Some(key);
        self.source = source;
        self.meters = meters;
    }

    /// Shows `reading` as if the DSP had published it for a while — the
    /// UI preview's synthetic voice.
    pub fn show(&mut self, reading: Reading) {
        self.pinned = true;
        self.last_fresh = Some(Instant::now());
        for _ in 0..RECENT {
            self.push(reading);
        }
    }

    fn push(&mut self, reading: Reading) {
        if self.trace.len() == TRACE_POINTS {
            self.trace.pop_front();
        }
        self.trace.push_back(TracePoint {
            index: self.next_index,
            input: reading.input,
            output: reading.output,
            target: reading.target,
        });
        self.next_index += 1;
    }

    /// Takes the readings published since the last block.
    fn poll(&mut self, now: Instant) {
        let (Some(source), Some(key)) = (self.source.as_ref(), self.key.as_ref()) else {
            return;
        };
        let Some((seq, block)) = source(key) else {
            return;
        };
        if self.block_seq == Some(seq) {
            return;
        }
        self.block_seq = Some(seq);
        let Some(slots) = block.first_chunk::<{ telemetry::SLOTS }>() else {
            return;
        };
        let (count, readings) = telemetry::decode(slots);
        let fresh = match self.seen {
            Some(seen) => telemetry::fresh(seen, count),
            None => (count as usize).min(telemetry::POINTS),
        };
        self.seen = Some(count);
        if fresh == 0 {
            return;
        }
        self.last_fresh = Some(now);
        for reading in &readings[telemetry::POINTS - fresh..] {
            self.push(*reading);
        }
    }

    /// The newest sung reading among the last few, if the voice is sounding.
    fn newest_voiced(&self) -> Option<Reading> {
        self.trace
            .iter()
            .rev()
            .take(RECENT)
            .find(|point| point.input.is_some())
            .map(|point| Reading {
                input: point.input,
                output: point.output,
                target: point.target,
            })
    }

    /// One frame: takes new readings and moves everything that eases.
    fn tick(&mut self, inputs: &LiveInputs) {
        let now = Instant::now();
        let dt = self
            .last_tick
            .map_or(1.0 / 60.0, |at| (now - at).as_secs_f32())
            .clamp(0.0, 0.05);
        self.last_tick = Some(now);
        self.poll(now);

        let stale = !self.pinned
            && self
                .last_fresh
                .is_none_or(|at| now.duration_since(at) > STALE);
        self.reading = if stale { None } else { self.newest_voiced() };
        self.active = self.reading.is_some();
        let bypass = !inputs.power;

        // A new key or scale while idle shows the key again, not the last
        // note; the pips swing round from where the old tonic put them.
        let song = (inputs.key, inputs.scale);
        if self.song != Some(song) {
            if let Some((old_key, _)) = self.song {
                let swing = (f32::from(old_key) - f32::from(inputs.key)) * 30.0;
                self.tonic_deg = turn(0.0, self.tonic_deg + swing);
                if !self.active {
                    self.last_midi = None;
                }
                self.anchor = anchor_for(self.anchor as f32 + 4.0, inputs.key);
            } else {
                self.anchor = 60 + i32::from(inputs.key);
                for class in 0..12 {
                    self.pips[class] = if inputs.allowed & (1 << class) != 0 {
                        0.9
                    } else {
                        0.28
                    };
                }
            }
            self.song = Some(song);
        }

        let heard = self.reading.and_then(|reading| {
            if bypass {
                reading.input
            } else {
                reading.output.or(reading.input)
            }
        });
        if let Some(midi) = heard {
            self.last_midi = Some(midi);
            let rel = midi - self.anchor as f32;
            if !(BAND_MIN_REL + 0.3..=BAND_MAX_REL - 0.3).contains(&rel) {
                self.anchor = anchor_for(midi, inputs.key);
            }
        }

        let tonic = 60.0 + f32::from(inputs.key);
        let shown = heard.or(self.last_midi);
        let midi = shown.unwrap_or(tonic);
        let nearest = midi.round();
        let cents = if shown.is_some() {
            ((midi - nearest) * 100.0).round() as i32
        } else {
            0
        };
        let class = (nearest as i32).rem_euclid(12);
        let locked = self.active && !bypass && (cents.abs() as f32) <= LOCK_CENTS;
        let name = style::spell(midi, inputs.key);
        if self.note != Some(name) {
            if self.note.is_some() {
                self.note_at = Some(now);
            }
            self.note = Some(name);
        }
        self.readout = Some(Readout {
            name,
            detail: if shown.is_some() {
                format_cents(cents)
            } else {
                style::scale_short(inputs.scale).to_string()
            },
            class,
            live: self.active,
            locked,
        });
        if locked && !self.was_locked {
            self.lock_at = Some(now);
        }
        self.was_locked = locked;

        // The dial's trace: the newest cents at the cursor; idle, it settles.
        self.ring[self.ring_cursor] = if self.active {
            cents as f32
        } else {
            self.ring[self.ring_cursor] * 0.92
        };
        self.ring_cursor = (self.ring_cursor + 1) % RING;
        if !self.active {
            for cents in &mut self.ring {
                *cents *= 0.985;
            }
        }

        // The needle glides to the sounding note, swung by its cents.
        let steps = (class - i32::from(inputs.key)).rem_euclid(12) as f32;
        let aim = if self.active || cents != 0 {
            steps * 30.0 + (cents as f32).clamp(-50.0, 50.0) * 0.55
        } else {
            0.0
        };
        let glide = 1.0 - (-dt / 0.075).exp();
        self.needle = (self.needle + turn(self.needle, aim) * glide).rem_euclid(360.0);
        let swing = 1.0 - (-dt / 0.15).exp();
        self.tonic_deg -= self.tonic_deg * swing;
        for (index, pip) in self.pips.iter_mut().enumerate() {
            let want = if inputs.allowed & (1 << index) != 0 {
                0.9
            } else {
                0.28
            };
            *pip += (want - *pip) * swing;
        }

        // The ticks breathe while idle.
        let breath = if self.active {
            1.0
        } else {
            let t = self.epoch.elapsed().as_secs_f32();
            0.55 + 0.45 * (0.5 - 0.5 * (t * std::f32::consts::TAU / 6.8).cos())
        };
        self.tick_alpha += (breath - self.tick_alpha) * (1.0 - (-dt / 0.06).exp());

        // The pull: how far the correction is moving the voice.
        let pull = match self.reading {
            Some(Reading {
                input: Some(input),
                output: Some(output),
                ..
            }) if !bypass => ((output - input) * 100.0).abs(),
            _ => 0.0,
        };
        let length = if pull > 2.0 {
            (pull * 0.24).min(32.0) / 100.0
        } else {
            0.0
        };
        let ease = 1.0 - (-dt / 0.06).exp();
        self.arc.0 += (length - self.arc.0) * ease;
        let visible = if length > 0.01 { 1.0 } else { 0.0 };
        self.arc.1 += (visible - self.arc.1) * ease;

        // In and Out: up at once, down gently.
        if let (Some(meters), Some(key)) = (self.meters.as_ref(), self.key.as_ref()) {
            let frame = meters(key);
            let (input, output) = frame.map_or((0.0, 0.0), |frame| {
                (
                    meter_fraction(frame.in_peak),
                    meter_fraction(frame.out_peak),
                )
            });
            let fall = (-dt / 0.25).exp();
            self.meter_in = input.max(self.meter_in * fall);
            self.meter_out = output.max(self.meter_out * fall);
        }
    }

    /// What the ring reads now.
    pub fn readout(&self) -> Option<&Readout> {
        self.readout.as_ref()
    }

    /// Polls, then paints the stage under the panel.
    pub fn paint_stage(
        &mut self,
        inputs: &LiveInputs,
        bounds: &LiveBounds,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.tick(inputs);
        let Some(stage) = bounds.stage else {
            return;
        };
        let (w, h) = (f32::from(stage.size.width), f32::from(stage.size.height));
        if w < 8.0 || h < 8.0 {
            return;
        }
        let layout = self.layout(stage, bounds.orb);
        #[cfg(feature = "gpu-renderer")]
        {
            let lock_pulse = self
                .lock_at
                .map(|at| at.elapsed().as_secs_f32() / LOCK_PULSE.as_secs_f32())
                .filter(|p| *p < 1.0);
            let readout_class = self.readout.as_ref().map_or(0, |r| r.class);
            let trace = self.trace.make_contiguous();
            let newest = trace.last().map_or(0, |point| point.index);
            let target = self
                .reading
                .and_then(|reading| reading.target)
                .filter(|_| self.active);
            let frame = crate::components::white_sharp_stage::StageFrame {
                layout,
                scale: window.scale_factor(),
                anchor: self.anchor,
                key: inputs.key,
                allowed: inputs.allowed,
                bypass: !inputs.power,
                trace,
                newest,
                hop_seconds: whitesharp::detect::hop_seconds(inputs.sample_rate),
                target,
                ring: &self.ring,
                ring_cursor: self.ring_cursor,
                active: self.active,
                note_class: readout_class,
                needle_deg: self.needle,
                tonic_deg: self.tonic_deg,
                pips: &self.pips,
                tick_alpha: self.tick_alpha,
                lock_pulse,
                arc: self.arc,
            };
            let scene = crate::components::white_sharp_stage::build(&frame);
            if let Err(error) = self.presenter.present(&scene, stage, window, cx) {
                paint_text(
                    window,
                    cx,
                    &format!("Display unavailable: {error}"),
                    px(12.0),
                    FontWeight::NORMAL,
                    style::color(style::DIM).into(),
                    stage.origin + point(px(12.0), px(h * 0.5)),
                    TextAlign::Left,
                );
            }
        }
        #[cfg(not(feature = "gpu-renderer"))]
        {
            let _ = (layout, cx);
            window.paint_quad(gpui::fill(stage, style::color(style::PLATE)));
        }
    }

    fn layout(&self, stage: Bounds<Pixels>, orb: Option<Bounds<Pixels>>) -> StageLayout {
        let orb = orb.map(|orb| Rect {
            x: f32::from(orb.origin.x - stage.origin.x),
            y: f32::from(orb.origin.y - stage.origin.y),
            w: f32::from(orb.size.width),
            h: f32::from(orb.size.height),
        });
        StageLayout::new(
            f32::from(stage.size.width),
            f32::from(stage.size.height),
            orb,
        )
    }

    /// Paints the text and the meters over the panel.
    pub fn paint_overlay(
        &self,
        inputs: &LiveInputs,
        bounds: &LiveBounds,
        window: &mut Window,
        cx: &mut App,
    ) {
        let bypass = !inputs.power;
        if let Some(stage) = bounds.stage {
            let layout = self.layout(stage, bounds.orb);
            self.paint_band_names(inputs, stage, &layout, window, cx);
            self.paint_readout(bypass, stage, &layout, window, cx);
        }
        let class = self.readout.as_ref().map_or(0, |r| r.class);
        let out_ink: Hsla = if bypass {
            style::color(0xd5d5d5).into()
        } else {
            style::pitch_hsl(class, 62.0, 70.0, 1.0)
        };
        if let Some(track) = bounds.meter_in {
            paint_meter(window, track, self.meter_in, style::color(0xd5d5d5).into());
        }
        if let Some(track) = bounds.meter_out {
            paint_meter(window, track, self.meter_out, out_ink);
        }
        if let (Some(keyboard), Some(target)) = (
            bounds.keyboard,
            self.reading.and_then(|r| r.target).filter(|_| self.active),
        ) {
            paint_held_keys(window, keyboard, target.rem_euclid(12) as u8);
        }
    }

    /// The scale's note names at the band's left, by their lines.
    fn paint_band_names(
        &self,
        inputs: &LiveInputs,
        stage: Bounds<Pixels>,
        layout: &StageLayout,
        window: &mut Window,
        cx: &mut App,
    ) {
        let size = (layout.band_h * 0.07).clamp(9.0, 12.0);
        let roomy = layout.px_per_semitone() >= 11.0;
        let grey = !inputs.power;
        let mut rel = BAND_MIN_REL.ceil() as i32;
        while rel as f32 <= BAND_MAX_REL {
            let step = rel.rem_euclid(12);
            let note = self.anchor + rel;
            let class = note.rem_euclid(12);
            let in_scale = inputs.allowed & (1 << class) != 0;
            if in_scale && (roomy || step == 0 || step == 7) {
                let y = layout.y_of(rel as f32).round() + 0.5;
                let mut name = style::spell(note as f32, inputs.key).to_string();
                if step == 0 {
                    name.push_str(&(note.div_euclid(12) - 1).to_string());
                }
                paint_text(
                    window,
                    cx,
                    &name,
                    px(size),
                    FontWeight::MEDIUM,
                    style::pitch_ink_or_grey(class, if step == 0 { 1.0 } else { 0.82 }, 80.0, grey),
                    stage.origin + point(px(8.0), px(y)),
                    TextAlign::Left,
                );
            }
            rel += 1;
        }
    }

    /// The note in the ring, and its cents or the scale under it.
    fn paint_readout(
        &self,
        bypass: bool,
        stage: Bounds<Pixels>,
        layout: &StageLayout,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(readout) = self.readout.as_ref() else {
            return;
        };
        let d = layout.orb.w;
        let (cx0, cy0) = layout.orb.centre();
        let centre = stage.origin + point(px(cx0), px(cy0));
        // A new note fades and grows in.
        let fade = self
            .note_at
            .map(|at| (at.elapsed().as_secs_f32() / NOTE_FADE.as_secs_f32()).min(1.0))
            .map_or(1.0, |p| 1.0 - (1.0 - p).powi(3));
        let note_size = d * 0.28 * (0.9 + 0.1 * fade);
        let note_ink: Hsla = if readout.live && !bypass {
            style::pitch_hsl(readout.class, 62.0, 82.0, 0.15 + 0.85 * fade)
        } else if bypass {
            gpui::hsla(0.0, 0.0, 0.824, 0.15 + 0.85 * fade)
        } else {
            gpui::hsla(0.0, 0.0, 0.769, 0.15 + 0.85 * fade)
        };
        let detail_size = d * 0.045;
        let gap = d * 0.011;
        // The note's cap height sits a little above the centre, the detail
        // under it, the pair centred as the mockup's stack.
        let note_box = note_size * 0.8;
        let block = note_box + gap + detail_size * 1.2;
        let top = f32::from(centre.y) - block * 0.5;
        paint_text(
            window,
            cx,
            readout.name,
            px(note_size),
            FontWeight::LIGHT,
            note_ink,
            point(centre.x, px(top + note_box * 0.5)),
            TextAlign::Center,
        );
        let detail_ink: Hsla = if readout.locked && !bypass {
            style::pitch_hsl(readout.class, 55.0, 80.0, 1.0)
        } else if bypass {
            gpui::hsla(0.0, 0.0, 0.824, 1.0)
        } else {
            gpui::hsla(0.0, 0.0, 0.541, 1.0)
        };
        paint_text(
            window,
            cx,
            &style::tracked(&readout.detail),
            px(detail_size),
            FontWeight::NORMAL,
            detail_ink,
            point(centre.x, px(top + note_box + gap + detail_size * 0.6)),
            TextAlign::Center,
        );
    }
}

/// A meter's fill, from the left of its track.
fn paint_meter(window: &mut Window, track: Bounds<Pixels>, level: f32, ink: Hsla) {
    let width = f32::from(track.size.width) * level.clamp(0.0, 1.0);
    if width < 0.5 {
        return;
    }
    let fill = Bounds::new(track.origin, size(px(width), track.size.height));
    window.paint_quad(gpui::fill(fill, ink).corner_radii(track.size.height * 0.5));
}

/// The note a voice is pulled to, pressed in every octave of the keyboard.
fn paint_held_keys(window: &mut Window, keyboard: Bounds<Pixels>, class: u8) {
    let (x0, y0) = (f32::from(keyboard.origin.x), f32::from(keyboard.origin.y));
    let (w, h) = (
        f32::from(keyboard.size.width),
        f32::from(keyboard.size.height),
    );
    for n in (class..PIANO_KEYS).step_by(12) {
        let (left, width, black) = piano_key(n);
        // A white key shows only below the black keys.
        let top = if black { 0.0 } else { h * BLACK_KEY_HEIGHT };
        let bottom = if black { h * BLACK_KEY_HEIGHT } else { h };
        window.paint_quad(gpui::fill(
            Bounds::new(
                point(px(x0 + left * w), px(y0 + top)),
                size(px(width * w), px(bottom - top)),
            ),
            gpui::hsla(0.0, 0.0, 1.0, if black { 0.16 } else { 0.11 }),
        ));
    }
}

/// One line of text in Outfit, vertically centred on `anchor.y`, its left
/// edge or centre at `anchor.x`.
#[allow(clippy::too_many_arguments)]
fn paint_text(
    window: &mut Window,
    cx: &mut App,
    text: &str,
    font_size: Pixels,
    weight: FontWeight,
    color: Hsla,
    anchor: gpui::Point<Pixels>,
    align: TextAlign,
) {
    let line = window.text_system().shape_line(
        SharedString::from(text.to_string()),
        font_size,
        &[TextRun {
            len: text.len(),
            font: style::outfit(weight),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        }],
        None,
    );
    let line_height = font_size * 1.2;
    let x = match align {
        TextAlign::Center => anchor.x - line.width * 0.5,
        TextAlign::Right => anchor.x - line.width,
        TextAlign::Left => anchor.x,
    };
    let origin = point(x, anchor.y - line_height * 0.5);
    let _ = line.paint(origin, line_height, TextAlign::Left, None, window, cx);
}

#[cfg(feature = "gpu-renderer")]
mod presenter {
    //! Renders a scene off screen with the visualizers' wgpu renderer and
    //! puts the result in the window.

    use std::sync::Arc;

    use gpui::{point, size, App, Bounds, Corners, DevicePixels, Pixels, RenderImage, Window};
    use smallvec::SmallVec;

    use crate::components::visualizer::gpu::{self, Presented, Surface};
    use crate::components::visualizer::scene::Scene;

    #[derive(Default)]
    pub struct Presenter {
        surface: Option<Surface>,
        /// Shared textures failed to import once; read back from now on.
        readback_only: bool,
        error: Option<String>,
        #[cfg(target_os = "windows")]
        external: Option<Arc<gpui::D3D11ExternalImage>>,
        image: Option<Arc<RenderImage>>,
        stale: Vec<Arc<RenderImage>>,
    }

    impl Presenter {
        pub fn present(
            &mut self,
            scene: &Scene,
            bounds: Bounds<Pixels>,
            window: &mut Window,
            cx: &mut App,
        ) -> Result<(), String> {
            for image in self.stale.drain(..) {
                cx.drop_image(image, Some(window));
            }
            if let Some(error) = &self.error {
                return Err(error.clone());
            }
            let scale = window.scale_factor();
            let (pw, ph) = (
                (f32::from(bounds.size.width) * scale).round().max(1.0) as u32,
                (f32::from(bounds.size.height) * scale).round().max(1.0) as u32,
            );
            if self.surface.is_none() {
                #[cfg(target_os = "windows")]
                let luid = window.d3d11_adapter_luid();
                #[cfg(not(target_os = "windows"))]
                let luid = None;
                let readback_only = self.readback_only;
                match gpu::context(luid).and_then(|context| {
                    let shared = context.shares_textures() && !readback_only;
                    Surface::new(context, pw, ph, shared)
                }) {
                    Ok(surface) => self.surface = Some(surface),
                    Err(error) => {
                        eprintln!("[whitesharp] GPU unavailable: {error}");
                        self.error = Some(error.clone());
                        return Err(error);
                    }
                }
            }
            let surface = self.surface.as_mut().expect("created above");
            surface.resize(pw, ph)?;
            match surface.render(scene)? {
                #[cfg(target_os = "windows")]
                Presented::Shared {
                    handle,
                    width,
                    height,
                } => {
                    let external = self
                        .external
                        .get_or_insert_with(|| window.create_d3d11_external_image())
                        .clone();
                    match external.update_from_shared_texture(
                        handle,
                        point(DevicePixels(0), DevicePixels(0)),
                        size(DevicePixels(width as i32), DevicePixels(height as i32)),
                    ) {
                        Ok(()) => {
                            let _ = window.paint_d3d11_external_image(
                                bounds,
                                Corners::default(),
                                external,
                            );
                        }
                        Err(error) => {
                            // Keep drawing: the next frame reads back.
                            eprintln!("[whitesharp] shared texture import failed: {error}");
                            self.readback_only = true;
                            self.surface = None;
                        }
                    }
                }
                #[cfg(not(target_os = "windows"))]
                Presented::Shared { .. } => {}
                Presented::Pixels {
                    bgra,
                    width,
                    height,
                } => {
                    let Some(buffer) = image::ImageBuffer::from_raw(width, height, bgra) else {
                        return Ok(());
                    };
                    let next = Arc::new(RenderImage::new(SmallVec::from_elem(
                        image::Frame::new(buffer),
                        1,
                    )));
                    if let Some(previous) = self.image.replace(next.clone()) {
                        self.stale.push(previous);
                    }
                    let _ = window.paint_image(bounds, Corners::default(), next, 0, false);
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_keys_tile_two_octaves() {
        let mut whites: Vec<(f32, f32)> = (0..PIANO_KEYS)
            .map(piano_key)
            .filter(|(_, _, black)| !black)
            .map(|(left, width, _)| (left, width))
            .collect();
        whites.sort_by(|a, b| a.0.total_cmp(&b.0));
        assert_eq!(whites.len(), 14);
        let mut edge = 0.0;
        for (left, width) in whites {
            assert!((left - edge).abs() < 1.0e-6);
            edge = left + width;
        }
        assert!((edge - 1.0).abs() < 1.0e-6);
        let (left, width, black) = piano_key(13);
        assert!(black && left < 8.0 / 14.0 && left + width > 8.0 / 14.0);
    }

    #[test]
    fn cents_read_with_a_true_minus() {
        assert_eq!(format_cents(0), "0");
        assert_eq!(format_cents(12), "+12");
        assert_eq!(format_cents(-7), "\u{2212}7");
    }

    #[test]
    fn the_band_follows_the_voice_by_octaves() {
        assert_eq!(anchor_for(64.0, 0), 60);
        assert_eq!(anchor_for(52.0, 0), 48);
        assert_eq!(anchor_for(76.0, 2), 74);
        assert!((turn(350.0, 10.0) - 20.0).abs() < 1.0e-4);
        assert!((turn(10.0, 350.0) + 20.0).abs() < 1.0e-4);
    }

    #[test]
    fn a_pause_lets_the_voice_go() {
        let mut live = LiveDisplay::default();
        live.push(Reading {
            input: Some(60.2),
            output: Some(60.0),
            target: Some(60),
        });
        assert_eq!(live.newest_voiced().and_then(|r| r.target), Some(60));
        for _ in 0..RECENT {
            live.push(Reading::SILENT);
        }
        assert_eq!(live.newest_voiced(), None);
    }

    #[test]
    fn the_trace_keeps_its_length() {
        let mut live = LiveDisplay::default();
        for _ in 0..TRACE_POINTS + 10 {
            live.push(Reading::SILENT);
        }
        assert_eq!(live.trace.len(), TRACE_POINTS);
        assert_eq!(live.trace.front().map(|p| p.index), Some(10));
    }
}
