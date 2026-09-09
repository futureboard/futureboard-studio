//! DAUx shared audio render kernel.
//!
//! `fill_output_f32` is the realtime hot path shared by all backends.
//! It is realtime-safe: no allocation, no locks, no I/O.
//!
//! Each backend creates a `LocalAudioState` per-stream and passes it along
//! with the shared `SharedState` and the mutable `RuntimeProject`.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use crate::audio_file::AuditionPlayer;
use crate::command::EngineCommand;
use crate::dsp::{meter::smooth_peak, oscillator::SineOscillator};
use crate::engine::{SharedState, PEAK_DECAY, TEST_TONE_AMPLITUDE};
use crate::runtime::{RuntimePreviewMode, RuntimeProject};
use crate::transport;

// Re-export helpers so wasapi_exclusive.rs can use them through render.
pub use crate::engine::{
    render_project_block_interleaved, render_project_block_interleaved_with_inputs,
    render_project_block_interleaved_with_live_input, render_project_sample,
};

fn command_debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("FUTUREBOARD_AUDIO_COMMAND_DEBUG").is_some())
}

/// `FUTUREBOARD_AUDIO_CALLBACK_DEBUG=1` enables the realtime callback's
/// occasional eprintln traces (graph swap, mute, render-path). Off by default
/// so the audio thread never formats strings or writes to stdio — see
/// `tasks/native/audio-system-spec.md` §1 and Phase A finding A.2.2. Cached on
/// first read so the callback never touches the environment.
fn callback_debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("FUTUREBOARD_AUDIO_CALLBACK_DEBUG").is_some())
}

fn transport_freeze_debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("FUTUREBOARD_TRANSPORT_FREEZE_DEBUG").is_some())
}

/// Cached `FUTUREBOARD_PDC_DEBUG` check. Used to gate the one-shot realtime
/// latency-compensation dump on transport start/seek so the audio thread never
/// touches the environment in steady state.
fn pdc_debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("FUTUREBOARD_PDC_DEBUG").is_some())
}

/// `FUTUREBOARD_METRONOME_DEBUG=1` prints click scheduling decisions. Cached so
/// the audio callback never reads the environment in steady state.
fn metronome_debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("FUTUREBOARD_METRONOME_DEBUG").is_some())
}

/// Logs the first N audio blocks after `StartTransport` when freeze debug is on.
static POST_PLAY_CALLBACK_LOGS: AtomicU32 = AtomicU32::new(0);

#[inline]
fn log_post_play_callback(step: &str) {
    let remaining = POST_PLAY_CALLBACK_LOGS.load(Ordering::Relaxed);
    if remaining == 0 || !transport_freeze_debug_enabled() {
        return;
    }
    let left = POST_PLAY_CALLBACK_LOGS
        .fetch_sub(1, Ordering::Relaxed)
        .saturating_sub(1);
    eprintln!("[play-debug callback] {step} (remaining={left})");
}

// ── Metronome voice ───────────────────────────────────────────────────────────

/// Which click the metronome synthesises (Settings → Recording → Metronome).
///
/// Two genuinely different voices rather than two levels of one: Woodblock is a
/// short squared decay that reads as a transient, Beep is a longer flat-topped
/// tone. Told apart by envelope as well as pitch, so they stay distinguishable
/// through a dense mix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetronomeSound {
    Woodblock,
    Beep,
}

impl MetronomeSound {
    /// Compact wire form for [`EngineCommand::SetMetronomeVoice`] — the same
    /// code-in-a-command shape `SetTrackPreviewMode` uses.
    #[inline]
    pub fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Beep,
            _ => Self::Woodblock,
        }
    }

    #[inline]
    pub fn code(self) -> u8 {
        match self {
            Self::Woodblock => 0,
            Self::Beep => 1,
        }
    }

    /// Settings persists the timbre as its display label; the engine owns the
    /// mapping so the UI never has to know the codes.
    pub fn from_label(label: &str) -> Self {
        if label.eq_ignore_ascii_case("beep") {
            Self::Beep
        } else {
            Self::Woodblock
        }
    }
}

/// Fraction of the beep's length spent ramping in — long enough to avoid a
/// click at the onset, short enough to still land on the beat.
const BEEP_ATTACK_FRACTION: f32 = 0.06;
/// Fraction of the beep's length spent ramping out.
const BEEP_RELEASE_FRACTION: f32 = 0.3;

// ── Per-stream oscillator + local playback state ──────────────────────────────

/// Local (non-shared) state for one audio stream.
/// Lives on the audio thread — no locks needed.
pub struct LocalAudioState {
    pub osc_l: SineOscillator,
    pub osc_r: SineOscillator,
    pub osc_freq: f32,
    pub osc_on: bool,
    pub playing_local: bool,
    pub prev_peak_l: f32,
    pub prev_peak_r: f32,
    /// Read cursor into the shared input ring (Layer 4 consumer state).
    pub input_read_frames: u64,
    /// Smoothed input-bus peaks for diagnostics (Layer 4 verification).
    pub prev_input_bus_l: f32,
    pub prev_input_bus_r: f32,
    /// Smoothed Control Room peaks, taken after the monitor insert chain and
    /// control processor — i.e. the signal actually leaving for the monitoring
    /// output, not the master bus feed.
    pub prev_monitor_peak_l: f32,
    pub prev_monitor_peak_r: f32,
    /// Preallocated live-input block injected into monitored track buffers before
    /// the normal graph pass. Never resized from the callback.
    pub monitor_input_l: Vec<f32>,
    pub monitor_input_r: Vec<f32>,
    pub render_path_logged: bool,
    /// Last logged preview-note count (gates PreviewRenderWake spam).
    pub prev_logged_preview_notes: u32,
    /// Blocks since the last stopped-transport heartbeat line. Diagnostics
    /// only; see `log_stopped_graph_heartbeat`.
    pub stopped_heartbeat_blocks: u64,
    /// Blocks until next PreviewRenderWake log while preview is active.
    pub preview_wake_log_cooldown: u32,
    pub metronome_enabled: bool,
    pub metronome_ts_num: u32,
    pub metronome_ts_den: u32,
    pub time_signature_map: crate::time_signature_map::RuntimeTimeSignatureMapSnapshot,
    /// Next click position in quarter-note beats.
    pub metronome_next_beat: f64,
    pub tempo_map: crate::tempo_map::RuntimeTempoMapSnapshot,
    pub metronome_click_remaining: u32,
    /// Base click length in samples (the Woodblock voice's whole duration).
    pub metronome_click_len: u32,
    /// Length of the click currently sounding — the envelope's denominator.
    /// Voices have different durations, so this is not always `click_len`.
    pub metronome_click_span: u32,
    pub metronome_click_phase: f64,
    pub metronome_click_phase_inc: f64,
    pub metronome_click_gain: f32,
    /// User click level from Settings, already mapped to a linear multiplier
    /// (the persisted default is unity). Folded into `metronome_click_gain` at
    /// arm time so every mix site inherits it.
    pub metronome_volume: f32,
    pub metronome_sound: MetronomeSound,
    /// Voice of the click currently sounding. Latched at arm time so switching
    /// timbre in Settings cannot step the envelope of a click already in flight.
    pub metronome_click_sound: MetronomeSound,
    /// When true, metronome scheduling and output are suppressed (playhead scrub).
    pub metronome_suspended: bool,
    /// Samples left in an audible record count-in.
    ///
    /// A count-in is a pre-roll the transport does not take part in: the
    /// playhead stays parked on the beat about to be recorded while the click
    /// counts the player in. The transport metronome cannot do that — it is
    /// scheduled off the playhead and returns silence whenever the transport is
    /// stopped, which is why a count-in from bar 1 used to be a silent wait.
    ///
    /// So the pre-roll runs on its own sample clock, and deliberately ignores
    /// `metronome_enabled`: asking for a count-in *is* asking for clicks.
    pub count_in_remaining: u64,
    /// Samples until the next pre-roll click.
    count_in_to_next_click: u64,
    /// Samples between pre-roll clicks, from the tempo at the record position.
    count_in_click_interval: u64,
    /// Which beat of the count-in is next, for the bar accent.
    count_in_beat_index: u32,
    count_in_beats_per_bar: u32,
    /// Standalone File Browser audition, owned by this stream/callback.
    pub audition: AuditionPlayer,
}

impl LocalAudioState {
    pub fn new(sample_rate: f64) -> Self {
        Self::with_monitor_capacity(sample_rate, 0)
    }

    pub fn with_monitor_capacity(sample_rate: f64, monitor_capacity: usize) -> Self {
        Self {
            osc_l: SineOscillator::new(440.0, sample_rate),
            osc_r: SineOscillator::new(440.0, sample_rate),
            osc_freq: 440.0,
            osc_on: false,
            playing_local: false,
            prev_peak_l: 0.0,
            prev_peak_r: 0.0,
            prev_monitor_peak_l: 0.0,
            prev_monitor_peak_r: 0.0,
            input_read_frames: 0,
            prev_input_bus_l: 0.0,
            prev_input_bus_r: 0.0,
            monitor_input_l: vec![0.0; monitor_capacity],
            monitor_input_r: vec![0.0; monitor_capacity],
            render_path_logged: false,
            prev_logged_preview_notes: u32::MAX,
            stopped_heartbeat_blocks: 0,
            preview_wake_log_cooldown: 0,
            metronome_enabled: false,
            metronome_ts_num: 4,
            metronome_ts_den: 4,
            time_signature_map:
                crate::time_signature_map::RuntimeTimeSignatureMapSnapshot::static_sig(4, 4),
            metronome_next_beat: 0.0,
            tempo_map: crate::tempo_map::RuntimeTempoMapSnapshot::static_tempo(120.0),
            metronome_click_remaining: 0,
            metronome_click_len: (sample_rate * 0.024).round().max(1.0) as u32,
            metronome_click_span: (sample_rate * 0.024).round().max(1.0) as u32,
            metronome_click_phase: 0.0,
            metronome_click_phase_inc: 0.0,
            metronome_click_gain: 0.0,
            metronome_volume: 1.0,
            metronome_sound: MetronomeSound::Woodblock,
            metronome_click_sound: MetronomeSound::Woodblock,
            metronome_suspended: false,
            count_in_remaining: 0,
            count_in_to_next_click: 0,
            count_in_click_interval: 0,
            count_in_beat_index: 0,
            count_in_beats_per_bar: 4,
            audition: AuditionPlayer::default(),
        }
    }

    /// Start an audible count-in of `beats` beats, `samples_per_beat` apart,
    /// accenting every `beats_per_bar`-th one.
    ///
    /// The caller resolves the spacing from the tempo map *at the beat the take
    /// starts on*, so a count-in into a section at 90 BPM counts at 90 — not at
    /// whatever the project's nominal tempo happens to be.
    pub fn begin_count_in(&mut self, beats: u32, beats_per_bar: u32, samples_per_beat: u64) {
        let beats = beats.max(1) as u64;
        let interval = samples_per_beat.max(1);
        self.count_in_click_interval = interval;
        self.count_in_remaining = interval.saturating_mul(beats);
        // Zero, so the first click lands on the first sample of the pre-roll
        // rather than a beat late.
        self.count_in_to_next_click = 0;
        self.count_in_beat_index = 0;
        self.count_in_beats_per_bar = beats_per_bar.max(1);
        self.metronome_click_remaining = 0;
    }

    /// Abandon a count-in in progress (the user cancelled, or the take failed
    /// to arm). Silent immediately — a click left ringing after a cancel is the
    /// clearest possible sign that a control did not take.
    pub fn cancel_count_in(&mut self) {
        self.count_in_remaining = 0;
        self.count_in_to_next_click = 0;
        self.metronome_click_remaining = 0;
    }

    pub fn count_in_active(&self) -> bool {
        self.count_in_remaining > 0
    }

    /// Arm one click voice. Shared by the transport metronome and the count-in
    /// pre-roll so the two are the same sound at the same level — a count-in
    /// that does not match the click it runs into is worse than none.
    fn arm_click(&mut self, accent: crate::time_signature_map::MetronomeAccent, sample_rate: u32) {
        use crate::time_signature_map::MetronomeAccent;

        let sr = sample_rate.max(1) as f64;
        let (freq, gain) = match self.metronome_sound {
            MetronomeSound::Woodblock => match accent {
                MetronomeAccent::Downbeat => (1760.0, 0.34),
                MetronomeAccent::Group => (1320.0, 0.28),
                MetronomeAccent::Normal => (980.0, 0.22),
            },
            // The beep sits a register lower and holds, so it reads as a tone
            // against the woodblock's tick rather than as the same click at
            // another pitch.
            MetronomeSound::Beep => match accent {
                MetronomeAccent::Downbeat => (880.0, 0.30),
                MetronomeAccent::Group => (660.0, 0.25),
                MetronomeAccent::Normal => (440.0, 0.20),
            },
        };
        self.metronome_click_phase = 0.0;
        self.metronome_click_phase_inc = freq / sr;
        self.metronome_click_gain = gain * self.metronome_volume;
        self.metronome_click_sound = self.metronome_sound;
        self.metronome_click_span = match self.metronome_sound {
            MetronomeSound::Woodblock => self.metronome_click_len,
            // ~48 ms: long enough to be heard as a pitch, still inside a beat
            // at any usable tempo.
            MetronomeSound::Beep => self.metronome_click_len.saturating_mul(2).max(1),
        };
        self.metronome_click_remaining = self.metronome_click_span;
    }

    /// One sample of whatever click is currently sounding, advancing it.
    fn render_click(&mut self) -> f32 {
        if self.metronome_click_remaining == 0 {
            return 0.0;
        }
        let span = self.metronome_click_span.max(1);
        let age = span.saturating_sub(self.metronome_click_remaining) as f32;
        let t = (age / span as f32).clamp(0.0, 1.0);
        let env = match self.metronome_click_sound {
            // Percussive: squared decay from the transient.
            MetronomeSound::Woodblock => {
                let decay = (1.0 - t).max(0.0);
                decay * decay
            }
            // Tonal: fast attack, flat body, short release.
            MetronomeSound::Beep => {
                if t < BEEP_ATTACK_FRACTION {
                    t / BEEP_ATTACK_FRACTION
                } else if t > 1.0 - BEEP_RELEASE_FRACTION {
                    ((1.0 - t) / BEEP_RELEASE_FRACTION).clamp(0.0, 1.0)
                } else {
                    1.0
                }
            }
        };
        let sample = (self.metronome_click_phase * std::f64::consts::TAU).sin() as f32
            * env
            * self.metronome_click_gain;
        self.metronome_click_phase += self.metronome_click_phase_inc;
        self.metronome_click_phase -= self.metronome_click_phase.floor();
        self.metronome_click_remaining = self.metronome_click_remaining.saturating_sub(1);
        sample
    }

    /// One sample of the count-in pre-roll, or `None` when there is none.
    #[inline]
    fn count_in_sample(&mut self, sample_rate: u32) -> Option<f32> {
        use crate::time_signature_map::MetronomeAccent;

        if self.count_in_remaining == 0 {
            return None;
        }
        if self.count_in_to_next_click == 0 {
            let accent = if self.count_in_beat_index % self.count_in_beats_per_bar == 0 {
                MetronomeAccent::Downbeat
            } else {
                MetronomeAccent::Normal
            };
            self.arm_click(accent, sample_rate);
            self.count_in_beat_index = self.count_in_beat_index.wrapping_add(1);
            self.count_in_to_next_click = self.count_in_click_interval.max(1);
        }
        self.count_in_to_next_click = self.count_in_to_next_click.saturating_sub(1);
        self.count_in_remaining = self.count_in_remaining.saturating_sub(1);
        Some(self.render_click())
    }

    pub fn set_metronome_enabled(&mut self, enabled: bool, position_sample: u64, sample_rate: u32) {
        self.metronome_enabled = enabled;
        self.metronome_click_remaining = 0;
        self.reset_metronome_schedule(position_sample, sample_rate);
    }

    /// Click level and timbre. Takes effect from the next click: changing the
    /// voice under a sounding one would step its envelope.
    pub fn set_metronome_voice(&mut self, volume: f32, sound: MetronomeSound) {
        self.metronome_volume = volume.clamp(0.0, 2.0);
        self.metronome_sound = sound;
    }

    pub fn set_bpm(&mut self, bpm: f64, position_sample: u64, sample_rate: u32) {
        self.tempo_map = crate::tempo_map::RuntimeTempoMapSnapshot::static_tempo(bpm);
        self.reset_metronome_schedule(position_sample, sample_rate);
    }

    pub fn set_tempo_map(
        &mut self,
        tempo_map: crate::tempo_map::RuntimeTempoMapSnapshot,
        position_sample: u64,
        sample_rate: u32,
    ) {
        self.tempo_map = tempo_map;
        self.reset_metronome_schedule(position_sample, sample_rate);
    }

    pub fn set_time_signature(
        &mut self,
        numerator: u32,
        denominator: u32,
        position_sample: u64,
        sample_rate: u32,
    ) {
        self.metronome_ts_num = numerator.clamp(1, 64);
        self.metronome_ts_den = denominator.clamp(1, 64);
        self.time_signature_map =
            crate::time_signature_map::RuntimeTimeSignatureMapSnapshot::static_sig(
                self.metronome_ts_num as u16,
                self.metronome_ts_den as u16,
            );
        self.reset_metronome_schedule(position_sample, sample_rate);
    }

    pub fn set_time_signature_map(
        &mut self,
        map: crate::time_signature_map::RuntimeTimeSignatureMapSnapshot,
        position_sample: u64,
        sample_rate: u32,
    ) {
        // The accent lookup indexes this map from the audio thread. Its
        // constructors guarantee at least one point; if that invariant is ever
        // broken upstream, keep the last valid map rather than panic in the
        // device callback.
        if map.points().is_empty() {
            return;
        }
        self.time_signature_map = map;
        if let Some(pt) = self.time_signature_map.points().first() {
            self.metronome_ts_num = pt.numerator as u32;
            self.metronome_ts_den = pt.denominator as u32;
        }
        self.reset_metronome_schedule(position_sample, sample_rate);
    }

    pub fn clear_metronome_clicks(&mut self, reason: &str) {
        self.metronome_click_remaining = 0;
        self.metronome_click_gain = 0.0;
        self.metronome_click_phase = 0.0;
        if callback_debug_enabled() {
            eprintln!("[Metronome] clear scheduled clicks reason={reason}");
        }
    }

    pub fn set_metronome_suspended(&mut self, suspended: bool) {
        if self.metronome_suspended == suspended {
            return;
        }
        self.metronome_suspended = suspended;
        self.clear_metronome_clicks(if suspended { "suspend" } else { "resume" });
        if callback_debug_enabled() {
            if suspended {
                eprintln!("[Metronome] suspend during drag");
            } else {
                eprintln!("[Metronome] resume after drag");
            }
        }
    }

    /// Re-arm the metronome whenever transport starts. A ruler drag can end
    /// outside its GPUI hit region, so its mouse-up callback is not a reliable
    /// audio-state boundary. StartTransport is: playback must never inherit a
    /// stale scrub suspension.
    pub fn prepare_metronome_for_transport_start(
        &mut self,
        position_sample: u64,
        sample_rate: u32,
    ) {
        self.set_metronome_suspended(false);
        self.reset_metronome_schedule(position_sample, sample_rate);
    }

    pub fn reset_metronome_schedule(&mut self, position_sample: u64, sample_rate: u32) {
        self.clear_metronome_clicks("seek");
        let sr = sample_rate.max(1) as f64;
        let current_beat = self.tempo_map.beat_at_samples(position_sample, sr);
        self.metronome_next_beat = self
            .time_signature_map
            .next_metronome_click_at_or_after(current_beat);
        if callback_debug_enabled() {
            eprintln!(
                "[Metronome] reset phase position={position_sample} next_beat={:.3}",
                self.metronome_next_beat
            );
        }
    }

    #[inline]
    pub fn metronome_sample(
        &mut self,
        output_sample_position: u64,
        click_render_sample_offset_in_block: u64,
        sample_rate: u32,
        transport_playing: bool,
        graph_max_latency_samples: u32,
        metronome_compensation_delay_samples: u32,
    ) -> f32 {
        if self.metronome_suspended {
            self.metronome_click_remaining = 0;
            self.count_in_remaining = 0;
            return 0.0;
        }
        if self.count_in_remaining > 0 {
            if !transport_playing {
                // The pre-roll owns the click while the transport is parked.
                if let Some(sample) = self.count_in_sample(sample_rate) {
                    return sample;
                }
            } else {
                // The take started: the count-in has done its job, and leaving
                // it running would double the click against the transport's.
                self.cancel_count_in();
            }
        }
        if !self.metronome_enabled || !transport_playing {
            if !transport_playing {
                self.metronome_click_remaining = 0;
            }
            return 0.0;
        }

        let sr = sample_rate.max(1) as f64;
        // The transport/playhead remains raw project time. Clicks are emitted at
        // the output sample that carries the same project beat after realtime PDC
        // and master-insert latency have made rendered tracks audible.
        let compensation_delay = metronome_compensation_delay_samples as u64;
        while {
            let next_click_sample_raw =
                self.tempo_map.samples_at_beat(self.metronome_next_beat, sr);
            let next_click_sample_compensated =
                next_click_sample_raw.saturating_add(compensation_delay);
            output_sample_position >= next_click_sample_compensated
        } {
            let next_click_sample_raw =
                self.tempo_map.samples_at_beat(self.metronome_next_beat, sr);
            let next_click_sample_compensated =
                next_click_sample_raw.saturating_add(compensation_delay);
            let accent = self
                .time_signature_map
                .metronome_accent_at_beat(self.metronome_next_beat);
            self.arm_click(accent, sample_rate);
            if metronome_debug_enabled() {
                let compensated_audible_sample_position =
                    output_sample_position.saturating_sub(compensation_delay);
                eprintln!(
                    "[metronome-sync] metronome_enabled={} raw_transport_sample_position={} \
                     compensated_audible_sample_position={} graph_max_latency_samples={} \
                     metronome_compensation_delay_samples={} next_click_sample_raw={} \
                     next_click_sample_compensated={} click_render_sample_offset_in_block={} \
                     tempo_at_click={:.3} time_signature_at_click={}/{} playback_graph_version=unknown",
                    self.metronome_enabled,
                    output_sample_position,
                    compensated_audible_sample_position,
                    graph_max_latency_samples,
                    metronome_compensation_delay_samples,
                    next_click_sample_raw,
                    next_click_sample_compensated,
                    click_render_sample_offset_in_block,
                    self.tempo_map.bpm_at_beat(self.metronome_next_beat),
                    self.metronome_ts_num,
                    self.metronome_ts_den,
                );
            }
            let previous_beat = self.metronome_next_beat;
            self.metronome_next_beat = self
                .time_signature_map
                .next_metronome_click_after(previous_beat);
            // The non-finite case takes the guard too: `samples_at_beat(NaN)`
            // clamps to 0, which would make the scan condition permanently true.
            if !self.metronome_next_beat.is_finite() || self.metronome_next_beat <= previous_beat {
                // A malformed map (a segment that is not a whole number of bars
                // can make the bar-start and bar-beat derivations disagree) can
                // hand back the beat we just fired, which would spin this loop
                // forever inside the device callback. Step past it instead: one
                // click may land on the wrong subdivision, the audio thread does
                // not hang.
                self.metronome_next_beat = previous_beat + 1.0;
                break;
            }
        }

        self.render_click()
    }
}

#[inline]
pub(crate) fn metronome_graph_max_latency_samples(runtime: &RuntimeProject) -> u32 {
    if runtime.pdc_enabled {
        runtime.latency_graph.max_path_latency_samples
    } else {
        0
    }
}

#[inline]
pub(crate) fn metronome_compensation_delay_samples(runtime: &RuntimeProject) -> u32 {
    // The metronome is mixed after project graph/master processing, so it needs
    // the track-graph PDC delay plus latency added by master inserts.
    metronome_graph_max_latency_samples(runtime)
        .saturating_add(runtime.latency_graph.master_plugin_latency)
}

// ── f32 helper store/load ─────────────────────────────────────────────────────

#[inline]
pub fn f32_store(v: f32) -> u32 {
    v.to_bits()
}
#[inline]
pub fn f32_load(v: u32) -> f32 {
    f32::from_bits(v)
}

// ── Command drain ─────────────────────────────────────────────────────────────

/// Drain all pending engine commands.  Returns true if the engine should stop.
///
/// Realtime-safe: only modifies local state or atomics.
pub fn drain_commands(
    cmd_rx: &crossbeam_channel::Receiver<EngineCommand>,
    runtime: &mut RuntimeProject,
    shared: &Arc<SharedState>,
    local: &mut LocalAudioState,
    output_sample_rate: u32,
) -> bool {
    while let Ok(cmd) = cmd_rx.try_recv() {
        match cmd {
            EngineCommand::LoadProject(next_runtime) => {
                if callback_debug_enabled() {
                    eprintln!(
                        "[DAUx] LoadProject: {} tracks, {} clips (sr={})",
                        next_runtime.tracks.len(),
                        next_runtime.clips.len(),
                        output_sample_rate,
                    );
                }
                // Swap in the new graph and retire the old one to the
                // background dropper — never run its destructor on this
                // realtime thread (frees buffers / munmaps sources / destroys
                // VST3 handles). See `crate::graveyard`.
                runtime.all_notes_off("project_load");
                let old = std::mem::replace(runtime, *next_runtime);
                runtime.sample_rate = output_sample_rate;
                // The plugin-bridge sinks (Stage 3b) are carried across the
                // reload by `EngineInner::load_project`, which copies its
                // control-thread mirror into the graph and resolves the
                // per-insert handles there. Cloning the map and re-resolving
                // every insert used to happen right here, on the audio thread,
                // in the same callback as Play.
                runtime.bridge_editor_active = old.bridge_editor_active.clone();
                // The panic the all_notes_off above pushed into the (preserved)
                // sinks still needs flushing through the new graph.
                // Keep the live master-fader ramp continuous across media/control
                // graph swaps (clip mute/gain sync). Prefer the live master
                // atomic (authoritative fader position) so a fresh runtime that
                // still carries the build-time seed of 1.0 cannot briefly
                // bypass a lowered master on the first blocks after LoadProject.
                runtime.smoothed_master_gain =
                    f32_load(shared.master_volume.load(Ordering::Relaxed));
                crate::graveyard::retire(old);
                // Transport/audio-graph separation: a graph swap must never
                // change the user's transport state.  If the transport was
                // Running when the swap arrived (e.g. an insert was added
                // during playback), keep rendering the new graph immediately —
                // the user must not have to press Play again.  If the
                // transport was stopped (project open/close paths call
                // StopTransport first, which clears `shared.playing`), the
                // swap lands in Paused exactly as before.
                let was_playing = shared.playing.load(Ordering::Relaxed);
                local.playing_local = was_playing;
                let pos = shared.position_samples.load(Ordering::Relaxed);
                runtime.reset_midi_playback(pos);
                // Cloning the segment vector here allocates (and frees the old
                // one) on the audio thread. A project sync almost never changes
                // the tempo, so compare first and re-arm the schedule without
                // touching the heap when it has not.
                if local.tempo_map.segments == runtime.tempo_map.segments {
                    local.reset_metronome_schedule(pos, output_sample_rate);
                } else {
                    local.set_tempo_map(runtime.tempo_map.clone(), pos, output_sample_rate);
                }
                let old_state = crate::engine::AudioEngineState::from_u8(
                    shared.engine_state.load(Ordering::Relaxed),
                );
                let new_state = if was_playing {
                    crate::engine::AudioEngineState::Running
                } else {
                    crate::engine::AudioEngineState::Paused
                };
                shared
                    .engine_state
                    .store(new_state as u8, Ordering::Relaxed);
                if callback_debug_enabled() || command_debug_enabled() {
                    eprintln!(
                        "[AudioEngineState] old={old_state:?} new={new_state:?} source=graph_swap was_playing={was_playing}"
                    );
                }
            }
            EngineCommand::SetTestTone { enabled, frequency } => {
                local.osc_on = enabled;
                local.osc_freq = frequency;
                local.osc_l.set_frequency(frequency as f64);
                local.osc_r.set_frequency(frequency as f64);
            }
            EngineCommand::StartAudition { token, source } => {
                // Reject a decode the user has already moved past; the source
                // leaves through the graveyard so this block never frees it on
                // the audio thread.
                if token == shared.audition_request_token.load(Ordering::Relaxed) {
                    local.audition.start(source, output_sample_rate);
                } else {
                    crate::graveyard::retire_audio_file(source);
                }
            }
            EngineCommand::StopAudition => {
                local.audition.stop(output_sample_rate);
            }
            EngineCommand::StartTransport => {
                let pos = shared.position_samples.load(Ordering::Relaxed);
                if command_debug_enabled() {
                    // Counting active clips walks the whole clip list; it is a
                    // diagnostic, so it stays inside the guard rather than
                    // running on every Play.
                    eprintln!(
                        "[DAUx] StartTransport: pos={}sa ({:.3}s), active={}, scheduled={}",
                        pos,
                        pos as f64 / output_sample_rate as f64,
                        runtime.active_clip_count_at_sample(pos),
                        runtime.clips.len(),
                    );
                }
                if transport_freeze_debug_enabled() {
                    eprintln!("[play-debug callback] StartTransport command applied");
                    POST_PLAY_CALLBACK_LOGS.store(5, Ordering::Relaxed);
                }
                local.playing_local = true;
                shared.playing.store(true, Ordering::Relaxed);
                let old_state = crate::engine::AudioEngineState::from_u8(shared.engine_state.swap(
                    crate::engine::AudioEngineState::Running as u8,
                    Ordering::Relaxed,
                ));
                if callback_debug_enabled() || command_debug_enabled() {
                    eprintln!(
                        "[AudioEngineState] old={old_state:?} new=Running source=StartTransport"
                    );
                }
                runtime.reset_midi_playback(pos);
                // Clear stale PDC delay-line audio so the compensated tracks start
                // settled and stay aligned with plugin/VSTi-latency tracks from the
                // first audible block — parity with offline export's fresh-runtime
                // + warmup start. Realtime-safe zero-fill; runs only on Start.
                runtime.reset_pdc_delay_lines();
                local.prepare_metronome_for_transport_start(pos, output_sample_rate);
                if pdc_debug_enabled() {
                    runtime.dump_latency_compensation_graph("StartTransport");
                    eprintln!(
                        "[metronome-sync] context=StartTransport metronome_enabled={} \
                         raw_transport_sample_position={} graph_max_latency_samples={} \
                         metronome_compensation_delay_samples={}",
                        local.metronome_enabled,
                        pos,
                        metronome_graph_max_latency_samples(runtime),
                        metronome_compensation_delay_samples(runtime),
                    );
                }
            }
            EngineCommand::StopTransport => {
                if command_debug_enabled() {
                    eprintln!("[DAUx] StopTransport");
                }
                local.playing_local = false;
                shared.playing.store(false, Ordering::Relaxed);
                let old_state = crate::engine::AudioEngineState::from_u8(shared.engine_state.swap(
                    crate::engine::AudioEngineState::Paused as u8,
                    Ordering::Relaxed,
                ));
                if callback_debug_enabled() || command_debug_enabled() {
                    eprintln!(
                        "[AudioEngineState] old={old_state:?} new=Paused source=StopTransport"
                    );
                }
                runtime.all_notes_off("stop");
                // Drop latency-compensated pre-stop audio immediately so Stop
                // cuts the timeline at the command. Plugin release and reverb
                // tails are untouched: the graph keeps running, so they ring
                // out on their own. Only the PDC rings are cleared.
                runtime.reset_pdc_delay_lines();
            }
            EngineCommand::Seek { position_seconds } => {
                let sr = shared.sample_rate.load(Ordering::Relaxed) as f64;
                let pos = (position_seconds * sr) as u64;
                if command_debug_enabled() {
                    eprintln!("[DAUx] Seek -> {:.3}s ({}sa)", position_seconds, pos);
                }
                shared.position_samples.store(pos, Ordering::Relaxed);
                local.reset_metronome_schedule(pos, output_sample_rate);
                runtime.reset_midi_playback(pos);
                // A seek repositions the playhead; the PDC delay lines still hold
                // audio from the pre-seek position. Clear them so the compensated
                // tracks refill from the new position and stay aligned (spec:
                // "Seeking must reset and refill latency compensation buffers").
                runtime.reset_pdc_delay_lines();
                if pdc_debug_enabled() {
                    runtime.dump_latency_compensation_graph("Seek");
                    eprintln!(
                        "[metronome-sync] context=Seek metronome_enabled={} \
                         raw_transport_sample_position={} graph_max_latency_samples={} \
                         metronome_compensation_delay_samples={}",
                        local.metronome_enabled,
                        pos,
                        metronome_graph_max_latency_samples(runtime),
                        metronome_compensation_delay_samples(runtime),
                    );
                }
            }
            EngineCommand::SetMetronomeEnabled(enabled) => {
                let pos = shared.position_samples.load(Ordering::Relaxed);
                shared.metronome_enabled.store(enabled, Ordering::Relaxed);
                local.set_metronome_enabled(enabled, pos, output_sample_rate);
            }
            EngineCommand::SetMetronomeSuspended(suspended) => {
                local.set_metronome_suspended(suspended);
            }
            EngineCommand::SetMetronomeVoice { volume, sound } => {
                local.set_metronome_voice(volume, MetronomeSound::from_code(sound));
            }
            EngineCommand::StartCountIn {
                beats,
                beats_per_bar,
                samples_per_beat,
            } => {
                local.begin_count_in(beats, beats_per_bar, samples_per_beat);
                shared
                    .count_in_remaining_samples
                    .store(local.count_in_remaining, Ordering::Relaxed);
            }
            EngineCommand::CancelCountIn => {
                local.cancel_count_in();
                shared
                    .count_in_remaining_samples
                    .store(0, Ordering::Relaxed);
            }
            EngineCommand::SetBpm(bpm) => {
                let pos = shared.position_samples.load(Ordering::Relaxed);
                transport::store_f64_bits(&shared.bpm_bits, bpm);
                let map = crate::tempo_map::RuntimeTempoMapSnapshot::static_tempo(bpm);
                let next_pos = runtime.apply_tempo_map(map, pos);
                shared.position_samples.store(next_pos, Ordering::Relaxed);
                local.set_tempo_map(runtime.tempo_map.clone(), next_pos, output_sample_rate);
            }
            EngineCommand::SetTempoMap(map) => {
                let pos = shared.position_samples.load(Ordering::Relaxed);
                let next_pos = runtime.apply_tempo_map(map, pos);
                shared.position_samples.store(next_pos, Ordering::Relaxed);
                local.set_tempo_map(runtime.tempo_map.clone(), next_pos, output_sample_rate);
            }
            EngineCommand::SetTimeSignature(num, den) => {
                let pos = shared.position_samples.load(Ordering::Relaxed);
                shared.time_sig_num.store(num.max(1), Ordering::Relaxed);
                shared.time_sig_den.store(den.max(1), Ordering::Relaxed);
                local.set_time_signature(num, den, pos, output_sample_rate);
            }
            EngineCommand::SetTimeSignatureMap(map) => {
                let pos = shared.position_samples.load(Ordering::Relaxed);
                if let Some(pt) = map.points().first() {
                    shared
                        .time_sig_num
                        .store(pt.numerator.max(1) as u32, Ordering::Relaxed);
                    shared
                        .time_sig_den
                        .store(pt.denominator.max(1) as u32, Ordering::Relaxed);
                }
                local.set_time_signature_map(map, pos, output_sample_rate);
            }
            EngineCommand::SetLoop {
                enabled,
                start_seconds,
                end_seconds,
            } => {
                let sr = shared.sample_rate.load(Ordering::Relaxed) as f64;
                let start = (start_seconds.max(0.0) * sr) as u64;
                let end = (end_seconds.max(0.0) * sr) as u64;
                shared.loop_enabled.store(enabled, Ordering::Relaxed);
                shared.loop_start_samples.store(start, Ordering::Relaxed);
                shared.loop_end_samples.store(end, Ordering::Relaxed);
            }
            EngineCommand::SetMasterVolume { value } => {
                shared
                    .master_volume
                    .store(f32_store(value), Ordering::Relaxed);
            }
            EngineCommand::SetMonitorSource { source } => {
                runtime.monitor.source = source;
                // Re-resolve the id -> index mapping for the new selection.
                runtime.resolve_indices();
            }
            EngineCommand::SetMonitorControl { control } => {
                runtime.monitor.control = control;
                // Keep the legacy input-monitor gain atomic in step so the
                // software input-monitor path and the Control Room cannot
                // disagree about level.
                shared
                    .monitor_gain
                    .store(f32_store(control.effective_gain()), Ordering::Relaxed);
            }
            EngineCommand::SetMonitorOutput { target } => {
                runtime.monitor.output = target;
            }
            // Plain integers: applying ownership on this thread stores a small
            // enum and two channel pairs — no lookup, no allocation.
            EngineCommand::SetHardwareOutputOwnership {
                owner,
                master,
                monitor,
            } => {
                runtime.monitor.hardware_owner = owner;
                runtime.monitor.master_output = master;
                if let Some((left, _right)) = monitor {
                    runtime.monitor.output.left_channel = left;
                }
            }
            EngineCommand::SetTrackListen {
                track_index,
                listen,
            } => {
                if let Some(track) = runtime.tracks.get_mut(track_index) {
                    track.listen = listen;
                }
            }
            EngineCommand::ClearAllListen => {
                for track in runtime.tracks.iter_mut() {
                    track.listen = crate::monitor::ListenMode::Off;
                }
            }
            EngineCommand::SetAraRenderers {
                track_id,
                renderers,
            } => {
                runtime.set_ara_renderers(&track_id, renderers);
            }
            EngineCommand::SetPluginBridgeSink { insert_id, sink } => {
                match sink {
                    Some(sink) => {
                        runtime.plugin_bridge_sinks.insert(insert_id, sink);
                    }
                    None => {
                        runtime.plugin_bridge_sinks.remove(&insert_id);
                    }
                }
                // Re-cache per-insert sink handles for the block path.
                runtime.resolve_bridge_sinks();
            }
            EngineCommand::CommandBarrier { ack } => {
                // Wait-free ack: every command sent before this one has now
                // been applied to the callback's runtime.
                ack.store(true, Ordering::Release);
            }
            EngineCommand::SetBridgeEditorActive { track_id, active } => {
                // Bookkeeping only. Closing an editor no longer has to buy the
                // graph a window to drain the host's note-offs: the graph runs
                // every block, so the bridge handshake is always alive.
                runtime.set_bridge_editor_active(&track_id, active);
            }
            EngineCommand::SetTrackVolume { track_id, value } => {
                // Whether the id matched is the whole question when a fader
                // moves and nothing gets quieter: `update_track_volume` is a
                // silent no-op for an id the graph does not have, and that is
                // indistinguishable from "applied" without this line.
                let applied = runtime.update_track_volume(&track_id, value);
                if command_debug_enabled() {
                    eprintln!(
                        "[DAUx] SetTrackVolume track={track_id} linear={value:.4} applied={applied}"
                    );
                }
            }
            EngineCommand::SetTrackPan { track_id, value } => {
                runtime.update_track_pan(&track_id, value);
            }
            EngineCommand::SetTrackMute { track_id, muted } => {
                if callback_debug_enabled() {
                    eprintln!("[DAUx] SetTrackMute track={track_id} muted={muted}");
                }
                // No note-off (mirrors the cpal path): the render pass keeps a
                // muted/unsoloed track's instrument running under the silence,
                // so its notes play on and resume audibly on release.
                runtime.update_track_mute(&track_id, muted);
            }
            EngineCommand::SetTrackSolo { track_id, solo } => {
                runtime.update_track_solo(&track_id, solo);
            }
            EngineCommand::SetTrackInputState {
                track_index,
                record_armed,
                monitor_enabled,
                input_source,
            } => runtime.update_track_input_state(
                track_index,
                record_armed,
                monitor_enabled,
                input_source,
            ),
            EngineCommand::SetTrackLoopbackPublish {
                track_index,
                publish,
            } => {
                runtime.update_track_loopback_publish(track_index, publish);
            }
            EngineCommand::SetTrackJamPublish { track_index, slot } => {
                runtime.update_track_jam_publish(track_index, slot);
            }
            EngineCommand::SetJamMultitrackPairs { pairs } => {
                runtime.apply_jam_multitrack_pairs(&pairs);
            }
            EngineCommand::SetTrackPreviewMode { track_id, value } => {
                runtime.update_track_preview_mode(&track_id, RuntimePreviewMode::from_code(value));
            }
            EngineCommand::SetInsertParam {
                track_id,
                insert_id,
                param_id,
                value,
            } => {
                runtime.update_insert_param(&track_id, &insert_id, &param_id, value);
            }
            EngineCommand::MidiPreviewNoteOn {
                track_id,
                channel,
                pitch,
                velocity,
            } => {
                runtime.midi_preview_note_on(&track_id, channel, pitch, velocity);
            }
            EngineCommand::MidiPreviewNoteOff {
                track_id,
                channel,
                pitch,
            } => {
                runtime.midi_preview_note_off(&track_id, channel, pitch);
            }
            EngineCommand::MidiPreviewControlChange {
                track_id,
                channel,
                controller,
                value,
            } => {
                runtime.midi_preview_control_change(&track_id, channel, controller, value);
            }
            EngineCommand::MidiPreviewAllNotesOff { track_id } => {
                runtime.midi_preview_all_notes_off(&track_id);
            }
            EngineCommand::PluginPreviewNoteOn {
                track_id,
                plugin_instance_id,
                channel,
                pitch,
                velocity,
            } => {
                if crate::forensic_trace::engine_midi_verbose_enabled() {
                    eprintln!(
                        "[midi-preview-audio] dequeue note_on instance={plugin_instance_id} pitch={pitch}"
                    );
                }
                runtime.bridge_preview_note_on(
                    &track_id,
                    &plugin_instance_id,
                    channel,
                    pitch,
                    velocity,
                );
            }
            EngineCommand::PluginPreviewNoteOff {
                track_id,
                plugin_instance_id,
                channel,
                pitch,
            } => {
                if crate::forensic_trace::engine_midi_verbose_enabled() {
                    eprintln!(
                        "[midi-preview-audio] dequeue note_off instance={plugin_instance_id} pitch={pitch}"
                    );
                }
                runtime.bridge_preview_note_off(&track_id, &plugin_instance_id, channel, pitch);
            }
            EngineCommand::PluginPreviewControlChange {
                track_id,
                plugin_instance_id,
                channel,
                controller,
                value,
            } => {
                runtime.bridge_preview_control_change(
                    &track_id,
                    &plugin_instance_id,
                    channel,
                    controller,
                    value,
                );
            }
            EngineCommand::PluginPreviewAllNotesOff {
                track_id,
                plugin_instance_id,
            } => {
                runtime.bridge_preview_all_notes_off(&track_id, &plugin_instance_id);
            }
        }
    }
    // Publish the count-in's remaining length once per block. One atomic store;
    // the control thread polls it to know when to start the take, which is what
    // puts the downbeat on the audio clock instead of on a timer.
    shared
        .count_in_remaining_samples
        .store(local.count_in_remaining, Ordering::Relaxed);
    false
}

// ── Core f32 stereo render ────────────────────────────────────────────────────

/// Output-callback block of the last slow-block log (throttle: one watchdog
/// log per ~200 callbacks so a sustained stall cannot flood stderr).
static SLOW_CALLBACK_LAST_LOG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Fill interleaved f32 output data (stereo, `channels` wide).
///
/// Returns the number of frames written.
/// Realtime-safe — no allocation, no locking.
///
/// Wraps the render kernel with the callback-duration watchdog (audio-hang
/// spec §12): publishes last/max duration to [`SharedState`] and emits a
/// throttled warning when a block exceeds the realtime budget.
pub fn fill_output_f32(
    data: &mut [f32],
    channels: usize,
    runtime: &mut RuntimeProject,
    shared: &Arc<SharedState>,
    local: &mut LocalAudioState,
) -> u64 {
    let started = std::time::Instant::now();
    let frames = fill_output_f32_inner(data, channels, runtime, shared, local);
    let elapsed_us = started.elapsed().as_micros().min(u32::MAX as u128) as u32;
    // Publish last/max/deadline + classify dropout-risk against the active
    // protection mode (shared with the legacy callback so both paths agree).
    let block_frames = data.len().checked_div(channels).unwrap_or(0);
    crate::engine::record_output_callback_timing(
        shared,
        elapsed_us,
        block_frames,
        shared.sample_rate.load(Ordering::Relaxed),
    );
    // Classify against *this block's* deadline, not a fixed microsecond count.
    // A flat 5 ms / 10 ms pair says the same thing about a 5.3 ms block (buffer
    // 256, a near-miss) and a 21 ms one (buffer 1024, half idle), so the line
    // could not be read without knowing the buffer size that produced it.
    // `record_output_callback_timing` just published the real deadline; reuse it.
    let deadline_us = shared.callback_deadline_us.load(Ordering::Relaxed);
    let warn_us = if deadline_us > 0 {
        deadline_us / 2
    } else {
        5_000
    };
    if elapsed_us >= warn_us {
        let cb = shared.output_cb_count.load(Ordering::Relaxed);
        let last = SLOW_CALLBACK_LAST_LOG.load(Ordering::Relaxed);
        if cb.wrapping_sub(last) > 200 {
            SLOW_CALLBACK_LAST_LOG.store(cb, Ordering::Relaxed);
            let state = crate::engine::AudioEngineState::from_u8(
                shared.engine_state.load(Ordering::Relaxed),
            );
            // `error` means the block genuinely missed its deadline (the device
            // starves); `warning` means it is over halfway there.
            let severity = if deadline_us > 0 && elapsed_us >= deadline_us {
                "error"
            } else {
                "warning"
            };
            let load_pct = if deadline_us > 0 {
                (elapsed_us as u64 * 100 / deadline_us as u64) as u32
            } else {
                0
            };
            eprintln!(
                "[AudioCallback] slow block severity={severity} duration_us={elapsed_us} deadline_us={deadline_us} load={load_pct}% state={} frames={frames}",
                state.as_str()
            );
        }
    }
    frames
}

/// One line per second while the transport is stopped, under
/// `FUTUREBOARD_AUDIO_CALLBACK_DEBUG=1`.
///
/// It answers, in order, the questions a "the engine went quiet after Stop"
/// report needs: is the callback still being called, is the engine in a
/// rendering state, is the graph actually processing, is the input callback
/// still delivering samples, is the MIDI preview queue being consumed, and is a
/// recorder still consuming input. A silent engine that keeps printing this
/// line is a graph problem; a line that stops printing is a device/stream
/// problem. Formatting only ever happens behind the flag.
fn log_stopped_graph_heartbeat(
    runtime: &RuntimeProject,
    shared: &Arc<SharedState>,
    local: &mut LocalAudioState,
    frames_in_block: u64,
    software_monitoring: bool,
) {
    let sample_rate = runtime.sample_rate.max(1) as u64;
    let blocks_per_second = sample_rate / frames_in_block.max(1);
    local.stopped_heartbeat_blocks = local.stopped_heartbeat_blocks.saturating_add(1);
    if local.stopped_heartbeat_blocks < blocks_per_second.max(1) {
        return;
    }
    local.stopped_heartbeat_blocks = 0;
    let preview_notes: usize = runtime
        .midi_tracks
        .iter()
        .map(|mt| mt.preview_active.len())
        .sum();
    let queued_midi: usize = runtime
        .tracks
        .iter()
        .map(|t| t.midi_block_events.len())
        .sum();
    eprintln!(
        "[RealtimeGraph] transport=stopped callbacks={} state={} graph=rendering          input_frames={} monitoring={} preview_notes={} queued_midi={} recorder={}",
        shared.output_cb_count.load(Ordering::Relaxed),
        crate::engine::AudioEngineState::from_u8(shared.engine_state.load(Ordering::Relaxed))
            .as_str(),
        shared.input_frames_received.load(Ordering::Relaxed),
        software_monitoring,
        preview_notes,
        queued_midi,
        shared.recording_active.load(Ordering::Relaxed),
    );
}

fn fill_output_f32_inner(
    data: &mut [f32],
    channels: usize,
    runtime: &mut RuntimeProject,
    shared: &Arc<SharedState>,
    local: &mut LocalAudioState,
) -> u64 {
    shared.output_cb_count.fetch_add(1, Ordering::Relaxed);
    let engine_state =
        crate::engine::AudioEngineState::from_u8(shared.engine_state.load(Ordering::Relaxed));
    // The transport advances in Running, and keeps advancing while the control
    // thread prepares the next graph: `LoadingProject` does not touch the graph
    // this callback is holding. A stale `playing_local` left over from a graph
    // swap must still never drive rendering while Paused, or while the device
    // or project is being torn down.
    let transport_playing = local.playing_local && engine_state.renders_transport();
    let software_monitoring = shared.monitor_enabled_any.load(Ordering::Relaxed)
        && shared.live_input_active.load(Ordering::Relaxed)
        && shared.input_ring.is_active();
    // Engine lifecycle, not transport lifecycle. `Paused` renders: the timeline
    // is what stops, and live input, monitoring, MIDI preview, instruments,
    // inserts, sends, master FX and plugin tails all keep running. Only a
    // project teardown, a device swap or a suspended engine is hard silence.
    if engine_state.outputs_silence() {
        {
            for sample in data.iter_mut() {
                *sample = 0.0;
            }
            local.prev_peak_l = 0.0;
            local.prev_peak_r = 0.0;
            shared
                .peak_l
                .store(crate::engine::f32_store(0.0), Ordering::Relaxed);
            shared
                .peak_r
                .store(crate::engine::f32_store(0.0), Ordering::Relaxed);
            shared
                .rms_l
                .store(crate::engine::f32_store(0.0), Ordering::Relaxed);
            shared
                .rms_r
                .store(crate::engine::f32_store(0.0), Ordering::Relaxed);
            runtime.end_meter_block(0);
            let frames = data.len() / channels.max(1);
            if callback_debug_enabled() && shared.output_cb_count.load(Ordering::Relaxed) % 400 == 1
            {
                eprintln!(
                    "[AudioEngine] callback silence reason={} frames={frames}",
                    engine_state.as_str()
                );
                eprintln!("[AudioEngine] output cleared");
            }
            return frames as u64;
        }
    }
    if transport_playing {
        log_post_play_callback("block entered");
    }
    // Sync oscillator from atomics (set from control thread between blocks).
    let tone_on = shared.test_tone_enabled.load(Ordering::Relaxed);
    let tone_freq = f32_load(shared.test_tone_freq.load(Ordering::Relaxed));
    if tone_freq != local.osc_freq {
        local.osc_freq = tone_freq;
        local.osc_l.set_frequency(tone_freq as f64);
        local.osc_r.set_frequency(tone_freq as f64);
    }
    let gen_tone = tone_on || local.osc_on;
    let master_vol = f32_load(shared.master_volume.load(Ordering::Relaxed));
    let loop_bounds = if transport_playing {
        transport::active_loop_bounds(shared)
    } else {
        None
    };
    let raw_base_sample = shared.position_samples.load(Ordering::Relaxed);
    let base_sample = transport::normalize_loop_position(raw_base_sample, loop_bounds);
    if base_sample != raw_base_sample {
        shared
            .position_samples
            .store(base_sample, Ordering::Relaxed);
        runtime.reset_midi_playback(base_sample);
        local.reset_metronome_schedule(base_sample, runtime.sample_rate);
    }

    let mut frames = 0u64;
    runtime.begin_meter_block();

    let mut end_loop_midi_reset = None;
    if transport_playing {
        let frames_needed = data.len().checked_div(channels).unwrap_or(0) as u64;
        if frames_needed > 0 {
            end_loop_midi_reset = crate::engine::schedule_midi_render_block(
                runtime,
                base_sample,
                frames_needed,
                loop_bounds,
            );
        }
    }

    let frames_in_block = data.len().checked_div(channels).unwrap_or(0) as u64;
    // Diagnostics only. Nothing below gates rendering on these any more: the
    // graph runs every block the engine renders, so a preview note, a live
    // input, an open editor or a decaying reverb tail cannot be missed by a
    // predicate that forgot to list it.
    if callback_debug_enabled() && !transport_playing {
        log_stopped_graph_heartbeat(runtime, shared, local, frames_in_block, software_monitoring);
    }

    let monitor_input_ready = if shared.live_input_active.load(Ordering::Relaxed) {
        // Per-track input meters from the latest captured sample (Layer 6).
        let input_l = f32_load(shared.live_input_l.load(Ordering::Relaxed));
        let input_r = f32_load(shared.live_input_r.load(Ordering::Relaxed));
        let source_pair = shared.monitor_source_pair();
        runtime.accumulate_live_input_meters(input_l, input_r, source_pair);
        read_monitor_input(frames_in_block as usize, shared, local)
    } else {
        clear_input_bus_meter(shared, local);
        false
    };

    // Audio Jam live-input tap. The pair `read_monitor_input` just staged is
    // the instrument plugged into the interface, before any track has touched
    // it — which is what a performer sending "my guitar" to the room means.
    // Atomics into a preallocated ring; one load when nothing is bound.
    if monitor_input_ready {
        if let Some(slot) = shared.jam_bus.live_input_publish() {
            let n = frames_in_block as usize;
            slot.write_planar(
                &local.monitor_input_l[..n],
                &local.monitor_input_r[..n],
                n,
                runtime.sample_rate,
            );
        }
    }

    // Whether the metronome reaches the Audio Jam master stream.
    //
    // It does by default. When it is switched off, the click is mixed *after*
    // the publish tap instead of before it, so the stream carries the mix
    // exactly as it would have been without a metronome and the engineer still
    // hears the count. Deferring it costs nothing and, unlike subtracting the
    // click back out at the tap, stays exact when the master is clipping.
    let defer_click = channels >= 2
        && shared.jam_bus.master_publish().is_some()
        && !shared.jam_bus.master_click_published();
    let mut click_deferred = false;

    if channels >= 2 {
        frames = {
            // One call for both input sources. A jam-routed track draws from
            // the jam bus whether or not a capture stream is open, because a
            // remote performer does not depend on this machine having an
            // interface plugged in.
            let live_input = if software_monitoring && monitor_input_ready {
                Some((
                    &local.monitor_input_l[..frames_in_block as usize],
                    &local.monitor_input_r[..frames_in_block as usize],
                ))
            } else {
                None
            };
            render_project_block_interleaved_with_inputs(
                runtime,
                base_sample,
                master_vol,
                data,
                channels,
                transport_playing,
                shared.time_sig_num.load(Ordering::Relaxed),
                shared.time_sig_den.load(Ordering::Relaxed),
                loop_bounds,
                live_input,
                Some(&shared.jam_bus),
            )
        };
        if !local.render_path_logged {
            local.render_path_logged = true;
            if callback_debug_enabled() {
                eprintln!(
                    "[SphereAudio callback] renderPath=daux-block frames={} channels={} tracks={}",
                    frames,
                    channels,
                    runtime.tracks.len()
                );
            }
        }
        let metronome_graph_max_samples = metronome_graph_max_latency_samples(runtime);
        let metronome_delay_samples = metronome_compensation_delay_samples(runtime);
        if gen_tone {
            for frame in data.chunks_mut(channels) {
                let tone_l = local.osc_l.next_sample() * TEST_TONE_AMPLITUDE * master_vol;
                let tone_r = local.osc_r.next_sample() * TEST_TONE_AMPLITUDE * master_vol;
                frame[0] += tone_l;
                frame[1] += tone_r;
            }
        }
        if defer_click {
            click_deferred = true;
        } else {
            mix_metronome_block(
                data,
                channels,
                frames,
                base_sample,
                loop_bounds,
                transport_playing,
                master_vol,
                metronome_graph_max_samples,
                metronome_delay_samples,
                runtime,
                local,
            );
        }
        // Live monitoring is mixed below via the input ring (single, clean
        // path) — the old per-block sample-and-hold monitor was removed because
        // it held one input sample across the whole output block (warble).
    } else if channels >= 2 {
        let metronome_graph_max_samples = metronome_graph_max_latency_samples(runtime);
        let metronome_delay_samples = metronome_compensation_delay_samples(runtime);
        for frame in data.chunks_mut(channels) {
            let (tone_l, tone_r) = if gen_tone {
                (
                    local.osc_l.next_sample() * TEST_TONE_AMPLITUDE * master_vol,
                    local.osc_r.next_sample() * TEST_TONE_AMPLITUDE * master_vol,
                )
            } else {
                (0.0, 0.0)
            };
            let (proj_l, proj_r) = if transport_playing {
                render_project_sample(runtime, base_sample + frames, master_vol)
            } else {
                (0.0, 0.0)
            };
            let click = local.metronome_sample(
                base_sample + frames,
                frames,
                runtime.sample_rate,
                transport_playing,
                metronome_graph_max_samples,
                metronome_delay_samples,
            ) * master_vol;
            let l = tone_l + proj_l + click;
            let r = tone_r + proj_r + click;
            // Live monitor is added afterwards from the input ring (see below).
            frame[0] = l;
            frame[1] = r;
            for extra in frame.iter_mut().skip(2) {
                *extra = 0.0;
            }
            frames += 1;
        }
    } else if channels == 1 {
        let metronome_graph_max_samples = metronome_graph_max_latency_samples(runtime);
        let metronome_delay_samples = metronome_compensation_delay_samples(runtime);
        for sample in data.iter_mut() {
            let tone = if gen_tone {
                local.osc_l.next_sample() * TEST_TONE_AMPLITUDE * master_vol
            } else {
                0.0
            };
            let (proj_l, proj_r) = if transport_playing {
                render_project_sample(runtime, base_sample + frames, master_vol)
            } else {
                (0.0, 0.0)
            };
            let click = local.metronome_sample(
                base_sample + frames,
                frames,
                runtime.sample_rate,
                transport_playing,
                metronome_graph_max_samples,
                metronome_delay_samples,
            ) * master_vol;
            let v = tone + (proj_l + proj_r) * 0.5 + click;
            *sample = v;
            frames += 1;
        }
    }

    // Browser sample preview, summed after the graph so it is metered like the
    // rest of the output but never counts as bridge-tail activity above.
    if channels >= 2 {
        local.audition.mix_into(data, channels);
    }
    shared.publish_audition_position(local.audition.position_seconds());

    // Legacy master-bus bridge fallback (disabled by default — per-track routing
    // through external-bridge-plugin inserts is the normal path).
    if plugin_bridge_master_fallback_enabled() {
        let _ = mix_plugin_bridge(data, channels, runtime, master_vol);
    }

    // Audio Jam publish tap. `data` is the master bus feed, before the Control
    // Room touches it — the same signal an export gets. A jam listener hears
    // the mix, not this engineer's dim, mono or monitor inserts. The write is
    // atomics into a preallocated ring, and it costs one relaxed load when
    // nothing is published.
    if channels >= 2 {
        if let Some(slot) = shared.jam_bus.master_publish() {
            slot.write_interleaved(data, channels, runtime.sample_rate);
        }
    }

    // The click the stream was not meant to carry. It still reaches the device
    // and the Control Room below; it simply arrives after the tap has taken its
    // copy of the mix.
    if click_deferred {
        mix_metronome_block(
            data,
            channels,
            frames,
            base_sample,
            loop_bounds,
            transport_playing,
            master_vol,
            metronome_graph_max_latency_samples(runtime),
            metronome_compensation_delay_samples(runtime),
            runtime,
            local,
        );
    }

    // ── Control Room ────────────────────────────────────────────────────────
    // Everything above produced the master bus feed in `data`. The Control Room
    // now routes/processes it and writes the result to the monitoring output
    // pair. Because this lives in the device callback and export renders the
    // graph directly, no stage below can reach exported or recorded audio.
    apply_control_room(data, channels, runtime, shared, local);

    // Audio Jam Monitor tap. Deliberately *after* the Control Room, which is
    // the whole difference from the master tap above: master is the mix an
    // export gets, this is the signal that actually leaves for the monitoring
    // output. "Send what I am hearing" means this one.
    //
    // One atomic load when nobody is sending it.
    if channels >= 2 {
        if let Some(slot) = shared.jam_bus.monitor_publish() {
            slot.write_interleaved(data, channels, runtime.sample_rate);
        }
    }

    // Meter the final output after playback, software monitoring, and bridge
    // contributions have all been summed. This avoids under-reporting monitor
    // gain and catches clipping caused by the actual final mix.
    let mut peak_l = 0.0f32;
    let mut peak_r = 0.0f32;
    let mut sum_sq_l = 0.0f32;
    let mut sum_sq_r = 0.0f32;
    frames = (data.len() / channels.max(1)) as u64;
    for frame in data.chunks(channels.max(1)) {
        let l = frame.first().copied().unwrap_or(0.0);
        let r = frame.get(1).copied().unwrap_or(l);
        peak_l = peak_l.max(l.abs());
        peak_r = peak_r.max(r.abs());
        sum_sq_l += l * l;
        sum_sq_r += r * r;
    }

    // Update meters.
    let rms_l = if frames > 0 {
        (sum_sq_l / frames as f32).sqrt()
    } else {
        0.0
    };
    let (pk_r, rms_r) = if channels >= 2 {
        (
            peak_r,
            if frames > 0 {
                (sum_sq_r / frames as f32).sqrt()
            } else {
                0.0
            },
        )
    } else {
        (peak_l, rms_l)
    };
    runtime.end_meter_block(frames);

    local.prev_peak_l = smooth_peak(local.prev_peak_l, peak_l, PEAK_DECAY);
    local.prev_peak_r = smooth_peak(local.prev_peak_r, pk_r, PEAK_DECAY);

    shared
        .peak_l
        .store(f32_store(local.prev_peak_l), Ordering::Relaxed);
    shared
        .peak_r
        .store(f32_store(local.prev_peak_r), Ordering::Relaxed);
    shared.rms_l.store(f32_store(rms_l), Ordering::Relaxed);
    shared.rms_r.store(f32_store(rms_r), Ordering::Relaxed);

    // Advance transport position.
    if transport_playing && channels > 0 {
        let (next_position, _) = transport::advance_loop_position(base_sample, frames, loop_bounds);
        shared
            .position_samples
            .store(next_position, Ordering::Relaxed);
        if let Some(reset_sample) = end_loop_midi_reset {
            runtime.reset_midi_playback(reset_sample);
            local.reset_metronome_schedule(reset_sample, runtime.sample_rate);
        }
    }

    // Consumed for this block — clear AFTER render so drain_commands preview
    // events queued earlier in the same callback survive until apply_insert.
    //
    // The Solfege pitch and articulation lists are per-block for the same
    // reason and must be cleared here too. Leaving them was a real fault on
    // this backend: `solfege_events` splits the render block at every queued
    // offset, so a list that only ever grows re-applies every pitch and
    // articulation change from earlier blocks on each callback — heard as the
    // pitch warbling back through stale values — and once it reaches the
    // capacity the producer checks (`runtime.rs`), new edits stop arriving at
    // all.
    for track in &mut runtime.tracks {
        track.midi_block_events.clear();
        track.solfege_pitch_events.clear();
        track.solfege_articulation_events.clear();
    }

    frames
}

/// Control Room stage: route → listen override → monitor inserts → monitor
/// control → monitoring output pair, then publish the post-processing meter.
///
/// On entry `data` holds the master bus feed on channels 0/1. On exit the
/// monitoring output pair holds the monitored signal.
///
/// Requirement mapping:
///
/// * *No duplicate output path* — the result **replaces** the samples on the
///   monitoring pair rather than summing into them. When the Control Room
///   monitors the main pair (the default) the raw master feed is overwritten,
///   so the same hardware output never carries both.
/// * *Playback only* — this function is reachable only from the device
///   callback. `export::offline_renderer` calls the graph directly.
/// * *No implicit microphone* — a hardware-input source is read from the input
///   ring only when the user selected `HardwareInput`, and it replaces (never
///   sums with) the mix, so it cannot loop back into the master bus.
/// * *Meter shows the monitored signal* — peaks are taken from the final block
///   after inserts and the control processor.
///
/// Realtime-safe: no allocation, no locking. If the scratch buffers are too
/// small for this block (a device block larger than the prepared capacity) the
/// stage degrades to leaving the master feed on the main outs untouched.
fn apply_control_room(
    data: &mut [f32],
    channels: usize,
    runtime: &mut RuntimeProject,
    shared: &Arc<SharedState>,
    local: &mut LocalAudioState,
) {
    if channels < 2 {
        return;
    }
    let frames = data.len() / channels;
    if frames == 0 {
        return;
    }
    // A hardware-input source is the only case that touches the capture ring,
    // and only when the user explicitly selected it. Everything else never
    // asks the input device for anything.
    let hardware_ready = runtime.monitor.source.needs_hardware_input()
        && !runtime.monitor.listen_active
        && local.monitor_input_l.len() >= frames
        && local.monitor_input_r.len() >= frames
        && read_monitor_input(frames, shared, local);

    let transport = crate::vst3_processor::RuntimeTransportContext {
        tempo_bpm: runtime.tempo_map.bpm_at_beat(0.0),
        time_sig_num: local.metronome_ts_num,
        time_sig_den: local.metronome_ts_den,
        project_time_samples: shared.position_samples.load(Ordering::Relaxed) as i64,
        ppq_position: 0.0,
        bar_position_ppq: 0.0,
        playing: local.playing_local,
        recording: false,
    };

    let hardware_input = if hardware_ready {
        Some((
            &local.monitor_input_l[..frames],
            &local.monitor_input_r[..frames],
        ))
    } else {
        None
    };
    let Some((monitor_peak_l, monitor_peak_r)) =
        run_control_room(data, channels, runtime, hardware_input, transport)
    else {
        return;
    };

    // Publish the post-processing monitor meter.
    local.prev_monitor_peak_l = smooth_peak(local.prev_monitor_peak_l, monitor_peak_l, PEAK_DECAY);
    local.prev_monitor_peak_r = smooth_peak(local.prev_monitor_peak_r, monitor_peak_r, PEAK_DECAY);
    shared
        .monitor_peak_l
        .store(f32_store(local.prev_monitor_peak_l), Ordering::Relaxed);
    shared
        .monitor_peak_r
        .store(f32_store(local.prev_monitor_peak_r), Ordering::Relaxed);
}

/// Device-independent Control Room core: router → listen override → monitor
/// inserts → monitor control → monitoring output pair.
///
/// Split out of [`apply_control_room`] so the whole monitoring path is testable
/// without a device, a `SharedState`, or an input ring. `hardware_input` is
/// `Some` only when the selected source is a hardware input *and* the ring
/// actually produced a block — passing `Some` while the source is not
/// `HardwareInput` is ignored, which is what keeps a capture device out of the
/// monitor path unless it was explicitly selected.
///
/// Returns the post-processing `(peak_l, peak_r)` of the monitored signal, or
/// `None` when the Control Room could not run and the master feed was left
/// untouched.
pub(crate) fn run_control_room(
    data: &mut [f32],
    channels: usize,
    runtime: &mut RuntimeProject,
    hardware_input: Option<(&[f32], &[f32])>,
    transport: crate::vst3_processor::RuntimeTransportContext,
) -> Option<(f32, f32)> {
    if channels < 2 {
        return None;
    }
    let frames = data.len() / channels;
    if frames == 0 {
        return None;
    }
    // ── 0. Hardware ownership ───────────────────────────────────────────────
    // Decided on the control thread; the callback only obeys it. The graph has
    // already rendered the master mix into device channels 0/1, so the two
    // non-Control-Room branches are handled entirely here.
    match runtime.monitor.hardware_owner {
        crate::monitor::HardwareOutputOwner::None => {
            // Nothing resolves: silence, never a fallback pair.
            for sample in data.iter_mut() {
                *sample = 0.0;
            }
            return Some((0.0, 0.0));
        }
        crate::monitor::HardwareOutputOwner::MasterDirect => {
            // Master owns the write and the Control Room is out of the path, so
            // no monitor gain, dim, mono, or inserts are applied.
            let Some(pair) = runtime.monitor.master_output else {
                for sample in data.iter_mut() {
                    *sample = 0.0;
                }
                return Some((0.0, 0.0));
            };
            let Some((master_l, master_r)) = resolved_output_pair(pair, channels) else {
                return None;
            };
            move_master_feed(data, channels, frames, master_l, master_r);
            return None;
        }
        crate::monitor::HardwareOutputOwner::MonitorControlRoom => {}
    }

    let (out_l, out_r) = runtime.monitor.output.resolved_pair(channels)?;
    if !runtime.monitor.has_block_capacity(frames) {
        return None;
    }

    // ── 1. Source router ────────────────────────────────────────────────────
    // Listen overrides the selected source whenever any channel is engaged;
    // with every Listen off we fall back to the selected source, which for the
    // default configuration is the master bus.
    let listen_active = runtime.monitor.listen_active;
    let hardware =
        hardware_input.filter(|_| runtime.monitor.source.needs_hardware_input() && !listen_active);
    {
        let monitor = &mut runtime.monitor;
        if listen_active {
            // Listen bus wins; its taps are already summed in listen_*.
            for i in 0..frames {
                monitor.source_l[i] = monitor.listen_l[i];
                monitor.source_r[i] = monitor.listen_r[i];
            }
        } else if let Some((in_l, in_r)) = hardware {
            monitor.source_l[..frames].copy_from_slice(&in_l[..frames]);
            monitor.source_r[..frames].copy_from_slice(&in_r[..frames]);
        } else if !monitor.source_captured {
            // MasterBus, or a bus/track selection whose id no longer resolves —
            // take the complete master feed off the device buffer. This is the
            // path that carries audio tracks, instruments, aux/returns, group
            // buses, and master processing into the Control Room.
            for i in 0..frames {
                let frame = &data[i * channels..i * channels + channels];
                monitor.source_l[i] = frame[0];
                monitor.source_r[i] = frame[1];
            }
        }
        // else: the graph pass already filled source_* from the selected tap.
    }

    // ── 2. Monitor insert chain ─────────────────────────────────────────────
    for insert_ix in 0..runtime.monitor.inserts.len() {
        if !runtime.monitor.inserts[insert_ix].enabled {
            continue;
        }
        let monitor = &mut runtime.monitor;
        let (block_l, block_r) = (&mut monitor.source_l, &mut monitor.source_r);
        crate::engine::apply_insert_block(
            &mut block_l[..frames],
            &mut block_r[..frames],
            &mut monitor.inserts[insert_ix],
            None,
            transport,
        );
    }

    // ── 3. Monitor control processor (mono → dim → gain → mute) ─────────────
    let control = runtime.monitor.control;
    crate::monitor::apply_monitor_control(
        &mut runtime.monitor.source_l[..frames],
        &mut runtime.monitor.source_r[..frames],
        &control,
    );

    // ── 4. Write to the monitoring output pair ──────────────────────────────
    let mut monitor_peak_l = 0.0f32;
    let mut monitor_peak_r = 0.0f32;
    for i in 0..frames {
        // No clip on the way out: the Control Room hands the device what it
        // was given, and the meter below reports the real level.
        let l = runtime.monitor.source_l[i];
        let r = runtime.monitor.source_r[i];
        monitor_peak_l = monitor_peak_l.max(l.abs());
        monitor_peak_r = monitor_peak_r.max(r.abs());
        let frame = &mut data[i * channels..i * channels + channels];
        // Replace, never sum — this is what prevents a duplicate feed of the
        // master bus on the monitoring output.
        frame[out_l] = l;
        frame[out_r] = r;
    }

    // ── 5. Master's direct feed ─────────────────────────────────────────────
    // The Control Room owns the hardware, so Master must not also write. Any
    // channel of Master's own destination that the monitoring pair does not
    // cover is silenced; when the two destinations coincide the monitor block
    // above already replaced (never summed into) those samples. Either way the
    // destination carries the mix exactly once.
    if let Some(master) = runtime.monitor.master_output {
        if let Some((master_l, master_r)) = resolved_output_pair(master, channels) {
            for channel in [master_l, master_r] {
                if channel == out_l || channel == out_r {
                    continue;
                }
                for i in 0..frames {
                    data[i * channels + channel] = 0.0;
                }
            }
        }
    }

    Some((monitor_peak_l, monitor_peak_r))
}

/// Clamp a resolved `(left, right)` pair into a device with `channels` outputs.
///
/// Out-of-range is `None`, not a fallback pair: writing the master mix to some
/// other physical output because the configured one no longer exists would be
/// worse than silence.
fn resolved_output_pair(pair: (u16, u16), channels: usize) -> Option<(usize, usize)> {
    let (left, right) = (pair.0 as usize, pair.1 as usize);
    (left < channels && right < channels).then_some((left, right))
}

/// Move the graph's master feed from device channels 0/1 to `(left, right)`,
/// silencing 0/1 when they are not the destination.
///
/// Realtime-safe: an in-place per-frame swap over the interleaved buffer, no
/// allocation and no scratch.
fn move_master_feed(data: &mut [f32], channels: usize, frames: usize, left: usize, right: usize) {
    if left == 0 && right == 1 {
        return;
    }
    for i in 0..frames {
        let frame = &mut data[i * channels..i * channels + channels];
        let (l, r) = (frame[0], frame[1]);
        if left != 0 && right != 0 {
            frame[0] = 0.0;
        }
        if left != 1 && right != 1 {
            frame[1] = 0.0;
        }
        frame[left] = l;
        frame[right] = r;
    }
}

/// Largest block the bridge mix reads in one callback (stack scratch bound).
const BRIDGE_MAX_FRAMES: usize = 2048;

/// Whether the legacy master-bus bridge mix fallback is enabled. Bridge DSP is
/// normally routed per-track through `external-bridge-plugin` inserts; set
/// `FUTUREBOARD_PLUGIN_BRIDGE_AUDIO=0` to disable the master fallback.
fn plugin_bridge_master_fallback_enabled() -> bool {
    use std::sync::OnceLock;
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| {
        std::env::var("FUTUREBOARD_PLUGIN_BRIDGE_AUDIO")
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                !matches!(v.as_str(), "0" | "false" | "no" | "off")
            })
            .unwrap_or(false)
    })
}

/// Stage 3b: read the external plugin host's previously produced block from the
/// shared region and mix it into the master output `data`, returning the mixed
/// peak so the caller can fold it into the master meter. Then request the next
/// block (one-block latency — never blocks the audio thread).
///
/// Realtime-safe: fixed stack scratch, atomics + arithmetic only, no allocation
/// or locking. No-op unless the bridge audio path is enabled and a sink is set.
fn mix_plugin_bridge(
    data: &mut [f32],
    channels: usize,
    runtime: &RuntimeProject,
    master_vol: f32,
) -> (f32, f32) {
    if runtime.plugin_bridge_sinks.is_empty() {
        return (0.0, 0.0);
    }
    let ch = channels.max(1);
    let frames = data.len() / ch;
    if frames == 0 {
        return (0.0, 0.0);
    }
    let n = frames.min(BRIDGE_MAX_FRAMES);
    let mut scratch_l = [0.0f32; BRIDGE_MAX_FRAMES];
    let mut scratch_r = [0.0f32; BRIDGE_MAX_FRAMES];
    let mut peak_l = 0.0f32;
    let mut peak_r = 0.0f32;
    // Mix every registered track's bridged plugin output into the master.
    // (Per-track routing through each track's fader/mute/solo is a later,
    // runtime-validated step; this sums them onto the master bus for now.)
    for sink in runtime.plugin_bridge_sinks.values() {
        let got = sink.read_output(&mut scratch_l[..n], &mut scratch_r[..n], n);
        for i in 0..got {
            let l = scratch_l[i] * master_vol;
            let r = scratch_r[i] * master_vol;
            let base = i * ch;
            data[base] += l;
            if ch > 1 {
                data[base + 1] += r;
            }
            peak_l = peak_l.max(l.abs());
            peak_r = peak_r.max(r.abs());
        }
        // Request the next block (the host fills it asynchronously for next time).
        sink.request_block(frames as u32);
    }
    (peak_l, peak_r)
}

#[inline]
fn monitor_resync_target_frames(
    output_block_frames: usize,
    sample_rate: u32,
    shared_clock: bool,
) -> u64 {
    let output_block_frames = output_block_frames as u64;
    if shared_clock {
        output_block_frames
    } else {
        ((sample_rate.max(1) as u64 * 15) / 1000).max(output_block_frames.saturating_mul(2))
    }
}

#[inline]
fn monitor_resync_limit_frames(target: u64, output_block_frames: u64, shared_clock: bool) -> u64 {
    if shared_clock {
        target
    } else {
        target.saturating_add(output_block_frames)
    }
}

/// Drain the shared input ring into the preallocated monitor-input block
/// (Layers 4 + 7).
///
/// Always advances the read cursor — even when monitoring is off — so the
/// input-bus peak stays live for diagnostics and the monitor path never
/// replays stale audio when it is toggled on. The staged block is injected
/// into the monitored tracks' buffers before the normal graph pass, so plugin
/// state, PDC, sends, and master DSP all apply exactly once.
///
/// Returns true when a full block of post-gain input is staged in
/// `local.monitor_input_l/r` (underruns are padded with silence, never stale
/// samples).
///
/// Realtime-safe: atomics + arithmetic only, no allocation or locking.
/// Sum the metronome click into a rendered block, following the loop.
///
/// Split out of the callback because the click is mixed at one of two points
/// depending on whether the Audio Jam master stream is meant to carry it — see
/// `defer_click` at the call sites. The loop walk is part of the work rather
/// than around it: crossing a loop boundary re-bases the click schedule, and a
/// mix that ignored that would drift a beat every pass.
///
/// Realtime-safe: no allocation, and nothing but arithmetic over a caller-owned
/// buffer.
#[allow(clippy::too_many_arguments)]
#[inline]
fn mix_metronome_block(
    data: &mut [f32],
    channels: usize,
    frames: u64,
    base_sample: u64,
    loop_bounds: Option<crate::transport::LoopBounds>,
    transport_playing: bool,
    master_vol: f32,
    graph_max_samples: u32,
    delay_samples: u32,
    runtime: &RuntimeProject,
    local: &mut LocalAudioState,
) {
    let mut segment_sample = base_sample;
    let mut callback_offset = 0usize;
    let mut remaining = frames;
    while remaining > 0 {
        let segment_frames =
            transport::segment_frames_until_loop_wrap(segment_sample, remaining, loop_bounds);
        for i in 0..segment_frames as usize {
            let frame = &mut data
                [(callback_offset + i) * channels..(callback_offset + i) * channels + channels];
            let click = local.metronome_sample(
                segment_sample + i as u64,
                (callback_offset + i) as u64,
                runtime.sample_rate,
                transport_playing,
                graph_max_samples,
                delay_samples,
            );
            if click != 0.0 {
                frame[0] += click * master_vol;
                frame[1] += click * master_vol;
            }
        }
        callback_offset += segment_frames as usize;
        remaining -= segment_frames;
        if remaining == 0 {
            break;
        }
        let (next_sample, wrapped) =
            transport::advance_loop_position(segment_sample, segment_frames, loop_bounds);
        if wrapped {
            local.reset_metronome_schedule(next_sample, runtime.sample_rate);
        }
        segment_sample = next_sample;
    }
}

fn read_monitor_input(
    frames: usize,
    shared: &Arc<SharedState>,
    local: &mut LocalAudioState,
) -> bool {
    let ring = &shared.input_ring;
    if !ring.is_active() || frames == 0 {
        return false;
    }
    // The staging buffers are preallocated by the backend; never grow them on
    // the callback. A backend that did not size them cannot stage monitoring.
    if local.monitor_input_l.len() < frames || local.monitor_input_r.len() < frames {
        return false;
    }
    let head = ring.write_head();
    if head == 0 {
        return false;
    }
    let frames64 = frames as u64;

    // Hold a small, stable monitoring latency behind the producer. Separate
    // WASAPI clients retain the existing ≈15 ms / two-block target because
    // their callback sizes and scheduling differ. ASIO input/output callbacks
    // share one device clock, so one output block is sufficient resync backlog.
    let cap = ring.capacity_frames();
    let shared_clock = shared.monitor_shared_clock.load(Ordering::Relaxed);
    let target = monitor_resync_target_frames(
        frames,
        shared.sample_rate.load(Ordering::Relaxed),
        shared_clock,
    );
    let resync_limit = monitor_resync_limit_frames(target, frames64, shared_clock);

    // Resync on gross overrun (cursor lapped) or if the cursor is ahead of the
    // producer (should not happen): jump to `target` frames behind the head.
    if local.input_read_frames > head || head.saturating_sub(local.input_read_frames) > cap {
        local.input_read_frames = head.saturating_sub(target);
        shared.monitor_ring_overruns.fetch_add(1, Ordering::Relaxed);
    }
    // Latency crept too high (input outran output): skip forward to `target`.
    if head.saturating_sub(local.input_read_frames) > resync_limit {
        local.input_read_frames = head.saturating_sub(target);
        shared.monitor_ring_overruns.fetch_add(1, Ordering::Relaxed);
    }

    let available = head.saturating_sub(local.input_read_frames);
    if available < frames64 {
        // Not enough buffered to fill the block — count an underrun. We still
        // read what's there and pad the remainder with silence (never replay
        // stale samples).
        shared
            .monitor_ring_underruns
            .fetch_add(1, Ordering::Relaxed);
        shared.output_xruns.fetch_add(1, Ordering::Relaxed);
    }

    let monitor_on = shared.monitor_enabled_any.load(Ordering::Relaxed);
    let mon_gain = f32_load(shared.monitor_gain.load(Ordering::Relaxed));

    let mut bus_peak_l = 0.0f32;
    let mut bus_peak_r = 0.0f32;
    let mut staged_peak = 0.0f32;
    let mut read = local.input_read_frames;
    let mut consumed = 0u64;

    for frame_index in 0..frames {
        let (in_l, in_r) = if read < head {
            let s = ring.read_frame(read);
            read += 1;
            consumed += 1;
            s
        } else {
            // Underrun: emit silence rather than repeating the last block.
            (0.0, 0.0)
        };
        bus_peak_l = bus_peak_l.max(in_l.abs());
        bus_peak_r = bus_peak_r.max(in_r.abs());
        let staged_l = in_l * mon_gain;
        let staged_r = in_r * mon_gain;
        local.monitor_input_l[frame_index] = staged_l;
        local.monitor_input_r[frame_index] = staged_r;
        staged_peak = staged_peak.max(staged_l.abs()).max(staged_r.abs());
    }
    local.input_read_frames = read;
    shared
        .monitor_frames_consumed
        .fetch_add(consumed, Ordering::Relaxed);

    // Smooth + publish the input-bus peak (pre-gain) and the staged monitor
    // level (post-gain, pre-fader — the graph applies the rest) for
    // diagnostics.
    local.prev_input_bus_l = smooth_peak(local.prev_input_bus_l, bus_peak_l, PEAK_DECAY);
    local.prev_input_bus_r = smooth_peak(local.prev_input_bus_r, bus_peak_r, PEAK_DECAY);
    shared
        .input_bus_peak_l
        .store(f32_store(local.prev_input_bus_l), Ordering::Relaxed);
    shared
        .input_bus_peak_r
        .store(f32_store(local.prev_input_bus_r), Ordering::Relaxed);
    shared.monitor_output_peak.store(
        f32_store(if monitor_on { staged_peak } else { 0.0 }),
        Ordering::Relaxed,
    );

    true
}

/// No live input — clear the input-bus peak so diagnostics decay to 0.
fn clear_input_bus_meter(shared: &Arc<SharedState>, local: &mut LocalAudioState) {
    shared
        .input_bus_peak_l
        .store(f32_store(0.0), Ordering::Relaxed);
    shared
        .input_bus_peak_r
        .store(f32_store(0.0), Ordering::Relaxed);
    local.prev_input_bus_l = 0.0;
    local.prev_input_bus_r = 0.0;
}

#[cfg(test)]
mod count_in_tests {
    use super::*;

    const SR: u32 = 48_000;
    /// One beat at 120 BPM.
    const BEAT: u64 = 24_000;

    fn count_in_state(beats: u32, beats_per_bar: u32) -> LocalAudioState {
        let mut local = LocalAudioState::new(SR as f64);
        local.begin_count_in(beats, beats_per_bar, BEAT);
        local
    }

    /// Sample offsets at which a click starts, over `samples` of stopped
    /// transport.
    fn click_onsets(local: &mut LocalAudioState, samples: u64) -> Vec<u64> {
        let mut onsets = Vec::new();
        let mut sounding = false;
        for sample in 0..samples {
            let value = local.metronome_sample(sample, sample, SR, false, 0, 0);
            let now_sounding = local.metronome_click_remaining > 0 || value != 0.0;
            if now_sounding && !sounding {
                onsets.push(sample);
            }
            sounding = now_sounding;
        }
        onsets
    }

    /// The whole point. The transport metronome is scheduled off the playhead
    /// and is silent while stopped, so a count-in from bar 1 — where there is
    /// no earlier timeline to roll through — used to be a silent wait.
    #[test]
    fn a_count_in_clicks_with_the_transport_stopped() {
        let mut local = count_in_state(4, 4);
        assert!(!local.metronome_enabled, "the take never enabled the click");
        let onsets = click_onsets(&mut local, BEAT * 4);
        assert_eq!(
            onsets,
            vec![0, BEAT, BEAT * 2, BEAT * 3],
            "a count-in must click on every beat from the first sample"
        );
    }

    /// It has to end on its own, exactly where it said it would — the take is
    /// started off this length.
    #[test]
    fn a_count_in_ends_after_exactly_the_beats_it_was_given() {
        let mut local = count_in_state(2, 4);
        assert_eq!(local.count_in_remaining, BEAT * 2);
        for sample in 0..(BEAT * 2) {
            local.metronome_sample(sample, sample, SR, false, 0, 0);
        }
        assert_eq!(local.count_in_remaining, 0);
        assert!(!local.count_in_active());
        // And nothing more is emitted afterwards.
        let onsets = click_onsets(&mut local, BEAT);
        assert!(onsets.is_empty(), "the count-in kept clicking past its end");
    }

    /// A count-in accents the downbeat of every bar, so a player can hear where
    /// "one" is rather than counting four identical ticks.
    #[test]
    fn a_count_in_accents_the_first_beat_of_each_bar() {
        let mut local = count_in_state(8, 4);
        let mut gains = Vec::new();
        let mut sounding = false;
        for sample in 0..(BEAT * 8) {
            local.metronome_sample(sample, sample, SR, false, 0, 0);
            let now = local.metronome_click_remaining > 0;
            if now && !sounding {
                gains.push(local.metronome_click_gain);
            }
            sounding = now;
        }
        assert_eq!(gains.len(), 8);
        assert!(gains[0] > gains[1], "beat 1 must be accented");
        assert!(
            (gains[4] - gains[0]).abs() < 1.0e-6,
            "the next bar's downbeat must be accented too"
        );
        assert!((gains[3] - gains[1]).abs() < 1.0e-6);
    }

    /// When the take starts, the transport metronome takes over. Leaving the
    /// pre-roll running would double every click against it.
    #[test]
    fn the_transport_starting_ends_the_count_in() {
        let mut local = count_in_state(4, 4);
        local.metronome_sample(0, 0, SR, false, 0, 0);
        assert!(local.count_in_active());
        local.metronome_sample(1, 1, SR, true, 0, 0);
        assert!(
            !local.count_in_active(),
            "the pre-roll outlived the downbeat"
        );
    }

    /// Cancelling is immediate and silent — a click left ringing after a
    /// cancelled take is the clearest possible sign a control did not take.
    #[test]
    fn cancelling_a_count_in_is_silent_immediately() {
        let mut local = count_in_state(4, 4);
        local.metronome_sample(0, 0, SR, false, 0, 0);
        local.cancel_count_in();
        assert!(!local.count_in_active());
        assert_eq!(local.metronome_sample(1, 1, SR, false, 0, 0), 0.0);
    }

    /// Scrubbing the playhead suspends the metronome; it must silence a
    /// count-in too rather than leaving one counting under the gesture.
    #[test]
    fn suspending_the_metronome_silences_a_count_in() {
        let mut local = count_in_state(4, 4);
        local.set_metronome_suspended(true);
        assert_eq!(local.metronome_sample(0, 0, SR, false, 0, 0), 0.0);
        assert!(!local.count_in_active());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_clock_monitor_target_is_one_output_block() {
        assert_eq!(monitor_resync_target_frames(256, 48_000, true), 256);
        assert_eq!(monitor_resync_target_frames(512, 96_000, true), 512);
    }

    #[test]
    fn independent_clock_monitor_target_preserves_wasapi_backlog() {
        assert_eq!(monitor_resync_target_frames(256, 48_000, false), 720);
        assert_eq!(monitor_resync_target_frames(512, 48_000, false), 1024);
    }

    #[test]
    fn shared_clock_resyncs_as_soon_as_backlog_exceeds_one_block() {
        assert_eq!(monitor_resync_limit_frames(256, 256, true), 256);
        assert_eq!(monitor_resync_limit_frames(720, 256, false), 976);
    }

    /// Losing the transport mid-beat truncates the click that was sounding and
    /// does **not** schedule an extra one: the scan has already moved on to the
    /// next beat, and coming back inside the same beat finds nothing due.
    ///
    /// Worth pinning because the click is a single armed voice rather than a
    /// queue, so anything that re-armed it out of turn would be heard as an
    /// extra beat rather than as a dropout. It also rules the click scheduler
    /// out as the source of a burst of fast clicks — whatever produces that, it
    /// is not the transport being taken away and given back.
    #[test]
    fn losing_the_transport_mid_beat_truncates_rather_than_repeats_the_click() {
        let mut local = LocalAudioState::new(48_000.0);
        local.set_metronome_enabled(true, 0, 48_000);

        // 120 BPM: beat 0 at sample 0, beat 1 at 24 000. Nothing inside one
        // beat should click twice.
        let _ = local.metronome_sample(0, 0, 48_000, true, 0, 0);
        assert!(local.metronome_click_remaining > 0, "beat 0 arms the click");
        for sample in 1..2_000u64 {
            local.metronome_sample(sample, sample, 48_000, true, 0, 0);
        }
        let quiet = local.metronome_click_remaining;
        assert_eq!(
            quiet, 0,
            "the click must finish and not re-arm inside a beat"
        );

        // One block rendered with the transport reported as stopped — what the
        // render gate did for the whole of a project sync, before
        // `LoadingProject` stopped counting as "not playing".
        for sample in 2_000..2_256u64 {
            let click = local.metronome_sample(sample, sample, 48_000, false, 0, 0);
            assert_eq!(click, 0.0, "a stopped transport must be silent");
        }

        // Back to playing, still inside beat 0-to-1: nothing is due until beat
        // 1 at sample 24 000, so no extra click may appear.
        for sample in 2_256..20_000u64 {
            let click = local.metronome_sample(sample, sample, 48_000, true, 0, 0);
            assert_eq!(
                click, 0.0,
                "an extra click appeared at sample {sample}, inside beat 0"
            );
        }

        // And the real next beat still arrives on time.
        local.metronome_sample(24_000, 24_000, 48_000, true, 0, 0);
        assert!(
            local.metronome_click_remaining > 0,
            "beat 1 must still click after the transport was interrupted"
        );
    }

    #[test]
    fn metronome_click_waits_for_compensation_delay() {
        let mut local = LocalAudioState::new(48_000.0);
        local.set_metronome_enabled(true, 0, 48_000);

        for sample in 0..512 {
            let click = local.metronome_sample(sample, sample, 48_000, true, 512, 512);
            assert_eq!(
                click, 0.0,
                "click leaked before compensated sample {sample}"
            );
            assert_eq!(local.metronome_click_remaining, 0);
        }

        let first = local.metronome_sample(512, 512, 48_000, true, 512, 512);
        assert_eq!(
            first, 0.0,
            "first click oscillator sample starts at phase zero"
        );
        assert!(
            local.metronome_click_remaining > 0,
            "click should arm exactly at raw click sample plus compensation delay"
        );
    }

    #[test]
    fn metronome_click_without_compensation_arms_on_raw_beat() {
        let mut local = LocalAudioState::new(48_000.0);
        local.set_metronome_enabled(true, 0, 48_000);

        let first = local.metronome_sample(0, 0, 48_000, true, 0, 0);
        assert_eq!(
            first, 0.0,
            "first click oscillator sample starts at phase zero"
        );
        assert!(
            local.metronome_click_remaining > 0,
            "uncompensated metronome should arm at the raw beat sample"
        );
    }

    /// Render one whole click into a buffer. 120 BPM puts beat 0 at sample 0,
    /// so the first call arms; 4096 samples is well short of beat 1 (24 000),
    /// so exactly one click lands in the result.
    fn render_one_click(sound: MetronomeSound, volume: f32) -> Vec<f32> {
        let mut local = LocalAudioState::new(48_000.0);
        local.set_metronome_voice(volume, sound);
        local.set_metronome_enabled(true, 0, 48_000);
        (0..4_096u64)
            .map(|sample| local.metronome_sample(sample, sample, 48_000, true, 0, 0))
            .collect()
    }

    fn click_peak(samples: &[f32]) -> f32 {
        samples
            .iter()
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()))
    }

    /// The scrub suspension is the state a ruler drag leaves behind. While it
    /// holds, nothing may reach the output — and lifting it must re-arm without
    /// waiting for the next Play.
    #[test]
    fn a_suspended_metronome_emits_no_click() {
        let mut local = LocalAudioState::new(48_000.0);
        local.set_metronome_enabled(true, 0, 48_000);
        local.set_metronome_suspended(true);

        // One second at 120 BPM covers two beats — both would click otherwise.
        for sample in 0..48_000u64 {
            let click = local.metronome_sample(sample, sample, 48_000, true, 0, 0);
            assert_eq!(click, 0.0, "suspended metronome clicked at sample {sample}");
        }
        assert_eq!(local.metronome_click_remaining, 0);

        local.set_metronome_suspended(false);
        local.reset_metronome_schedule(48_000, 48_000);
        let _ = local.metronome_sample(48_000, 0, 48_000, true, 0, 0);
        assert!(
            local.metronome_click_remaining > 0,
            "resuming must re-arm the click without a transport restart"
        );
    }

    /// Woodblock and Beep are two voices, not one voice at two pitches: the
    /// woodblock has decayed to nothing while the beep is still sounding.
    #[test]
    fn the_beep_voice_differs_from_the_woodblock_in_length_and_envelope() {
        let woodblock = render_one_click(MetronomeSound::Woodblock, 1.0);
        let beep = render_one_click(MetronomeSound::Beep, 1.0);

        assert!(click_peak(&woodblock) > 0.0, "the woodblock never sounded");
        assert!(click_peak(&beep) > 0.0, "the beep never sounded");
        // 24 ms woodblock = 1152 samples at 48 kHz; the beep runs twice that.
        assert_eq!(
            click_peak(&woodblock[1_500..]),
            0.0,
            "the woodblock should be over well before 1500 samples"
        );
        assert!(
            click_peak(&beep[1_500..]) > 0.0,
            "the beep should still be sounding where the woodblock has stopped"
        );
    }

    /// The Settings click level reaches the generator, so every mix site — block
    /// mixer, per-sample fallbacks, and the legacy callback — inherits it.
    #[test]
    fn the_click_level_scales_the_generated_click() {
        let full = click_peak(&render_one_click(MetronomeSound::Woodblock, 1.0));
        let half = click_peak(&render_one_click(MetronomeSound::Woodblock, 0.5));
        let silent = click_peak(&render_one_click(MetronomeSound::Woodblock, 0.0));

        assert!(full > 0.0);
        assert!(
            (half - full * 0.5).abs() < 1e-4,
            "half level should halve the click ({half} vs {full})"
        );
        assert_eq!(silent, 0.0, "a zero click level must be silent");
    }

    /// A segment that is not a whole number of bars can make the bar-start and
    /// bar-beat derivations disagree, which used to be able to hand the click
    /// scan the beat it had just fired — an endless loop inside the device
    /// callback. The schedule must always move forward.
    #[test]
    fn an_irregular_time_signature_map_cannot_stall_the_click_scan() {
        let map = crate::time_signature_map::RuntimeTimeSignatureMapSnapshot::from_points(vec![
            crate::time_signature_map::RuntimeTimeSignaturePointSnapshot {
                beat: 0.0,
                numerator: 4,
                denominator: 4,
                grouping: vec![4],
            },
            crate::time_signature_map::RuntimeTimeSignaturePointSnapshot {
                // Three quarter notes into a 4/4 bar: the marker deliberately
                // does not land on a bar line.
                beat: 3.0,
                numerator: 7,
                denominator: 8,
                grouping: vec![2, 2, 3],
            },
        ]);
        let mut local = LocalAudioState::new(48_000.0);
        local.set_time_signature_map(map, 0, 48_000);
        local.set_metronome_enabled(true, 0, 48_000);

        let mut previous = local.metronome_next_beat;
        // Four seconds at 120 BPM = eight beats, crossing the marker.
        for sample in 0..(4 * 48_000u64) {
            let _ = local.metronome_sample(sample, sample, 48_000, true, 0, 0);
            assert!(
                local.metronome_next_beat >= previous,
                "click schedule went backwards at sample {sample}"
            );
            previous = local.metronome_next_beat;
        }
        assert!(
            previous >= 8.0,
            "click schedule stopped advancing at beat {previous}"
        );
    }

    #[test]
    fn transport_start_recovers_metronome_after_unfinished_scrub() {
        let mut local = LocalAudioState::new(48_000.0);
        local.set_metronome_enabled(true, 0, 48_000);
        local.set_metronome_suspended(true);

        // Simulate releasing a ruler drag outside its hit region: no explicit
        // resume arrives before Play.
        local.prepare_metronome_for_transport_start(48_000, 48_000);
        assert!(!local.metronome_suspended);

        // 120 BPM at sample 48k is beat 2, so the click arms immediately.
        let first = local.metronome_sample(48_000, 0, 48_000, true, 0, 0);
        assert_eq!(first, 0.0, "oscillator starts at phase zero");
        assert!(
            local.metronome_click_remaining > 0,
            "Play must re-arm clicks even when scrub-end was missed"
        );
    }
}

/// Where the metronome sits relative to the Audio Jam publish tap.
///
/// The whole feature is an ordering, so the tests are about ordering: the click
/// reaches the device either way, and the only question is whether the copy the
/// jam took already had it in.
#[cfg(test)]
mod jam_master_click_tests {
    use super::*;
    use crate::jam_bus::PUBLISH_KEY_MASTER;

    const FRAMES: usize = 256;

    /// A callback with the transport running, the metronome on, and an empty
    /// project — so every non-zero sample in the block is the click.
    fn render_one_block(include_click: bool) -> (Vec<f32>, Vec<f32>) {
        let mut runtime = RuntimeProject::default();
        runtime.sample_rate = 48_000;
        let shared = Arc::new(SharedState::default());
        shared.sample_rate.store(48_000, Ordering::Relaxed);
        shared.engine_state.store(
            crate::engine::AudioEngineState::Running as u8,
            Ordering::Relaxed,
        );
        shared
            .jam_bus
            .bind_publish(PUBLISH_KEY_MASTER)
            .expect("free");
        shared.jam_bus.set_master_click_published(include_click);

        let mut local = LocalAudioState::new(48_000.0);
        // `playing_local` is the callback's own copy of the transport, set by
        // the StartTransport command; the test sets it directly rather than
        // pumping a command queue that has nothing else in it.
        local.playing_local = true;
        local.set_metronome_enabled(true, 0, 48_000);

        let mut data = vec![0.0f32; FRAMES * 2];
        fill_output_f32(&mut data, 2, &mut runtime, &shared, &mut local);

        let mut published = Vec::new();
        let taken = shared
            .jam_bus
            .master_publish()
            .expect("bound")
            .read_interleaved(&mut published, FRAMES)
            .map(|(frames, _, _)| frames)
            .unwrap_or(0);
        assert!(taken > 0, "the tap captured nothing at all");
        (data, published)
    }

    fn peak(samples: &[f32]) -> f32 {
        samples
            .iter()
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()))
    }

    /// The default, and what a jam wants: everyone in the room hears the count.
    #[test]
    fn the_published_master_carries_the_click_by_default() {
        let (device, published) = render_one_block(true);
        assert!(peak(&device) > 0.0, "the click never reached the device");
        assert!(
            peak(&published) > 0.0,
            "the click was not in the published mix"
        );
    }

    /// Switched off, the stream is the mix as it would have been with no
    /// metronome at all — and the engineer still hears the count locally.
    #[test]
    fn switching_the_click_off_removes_it_from_the_stream_and_not_the_room() {
        let (device, published) = render_one_block(false);
        assert!(
            peak(&device) > 0.0,
            "the engineer lost their own count, which is not the trade"
        );
        assert_eq!(
            peak(&published),
            0.0,
            "the click leaked into the stream it was excluded from"
        );
    }
}

#[cfg(test)]
mod realtime_graph_lifecycle_tests {
    use crate::engine::AudioEngineState;

    /// The bug this replaced: rendering used to be gated on a hand-written list
    /// of "reasons to wake while paused", and anything missing from that list —
    /// a Browser audition, a live input, a VSTi preview after its window
    /// expired — was simply inaudible with no error anywhere. A stopped
    /// transport now renders unconditionally, so there is no list to forget.
    #[test]
    fn a_stopped_transport_still_renders_the_realtime_graph() {
        assert!(AudioEngineState::Paused.renders_graph());
        assert!(!AudioEngineState::Paused.outputs_silence());
        assert!(
            !AudioEngineState::Paused.renders_transport(),
            "stopped means the timeline stops, not the engine"
        );
    }

    /// Playback and the mid-play project rebuild both render everything.
    #[test]
    fn running_and_loading_render_the_graph_and_the_transport() {
        for state in [AudioEngineState::Running, AudioEngineState::LoadingProject] {
            assert!(state.renders_graph(), "{state:?} must render");
            assert!(state.renders_transport(), "{state:?} must advance");
            assert!(!state.outputs_silence());
        }
    }

    /// The only states that may go hard silent are the ones with nothing to
    /// render into or from.
    #[test]
    fn only_teardown_states_output_silence() {
        for state in [
            AudioEngineState::ClosingProject,
            AudioEngineState::DeviceSwitching,
            AudioEngineState::Suspended,
        ] {
            assert!(state.outputs_silence(), "{state:?} must be silent");
            assert!(!state.renders_graph());
            assert!(!state.renders_transport());
        }
    }

    /// `renders_transport` is strictly narrower than `renders_graph`: every
    /// state that advances the timeline also renders, never the other way
    /// round. If that inverts, a stopped transport is rendering timeline audio.
    #[test]
    fn transport_rendering_implies_graph_rendering() {
        for state in [
            AudioEngineState::Running,
            AudioEngineState::Paused,
            AudioEngineState::LoadingProject,
            AudioEngineState::ClosingProject,
            AudioEngineState::DeviceSwitching,
            AudioEngineState::Suspended,
        ] {
            if state.renders_transport() {
                assert!(state.renders_graph(), "{state:?}");
            }
        }
    }
}
