//! DSP / render kernel for the native audio engine.
//!
//! Split out of `engine.rs` (which owns device lifecycle, command dispatch, and
//! the public engine API) so the realtime render path can be read and modified
//! in isolation. This is a pure relocation of the free render/DSP functions that
//! previously lived inline in `engine.rs` — no behavior change.
//!
//! Realtime rules apply to everything reachable from
//! `render_project_block_interleaved`: no allocation, no locking, no blocking in
//! steady state. `use super::*;` pulls in the shared engine vocabulary
//! (`SharedState`, runtime types, consts, debug-flag helpers).
use super::*;
use crate::monitor::{ListenMode, TapStage};
use SphereAudioProcessor::StretchBackend;

/// `FUTUREBOARD_FADER_DEBUG=1` — trace the gain [`apply_fader`] actually applies.
///
/// Shares its variable with the UI-side fader traces so one run covers the whole
/// chain: the pointer→norm mapping, the norm→linear conversion, the dispatch
/// into `update_track_param`, the `[DAUx] SetTrackVolume` drain, and this — the
/// measured pre/post ratio on the block that reaches the device. "The fader
/// moves and nothing gets quieter" is one of those five links, and reading the
/// code cannot tell you which.
fn fader_debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("FUTUREBOARD_FADER_DEBUG").is_some())
}

#[inline]
pub fn render_project_sample(
    runtime: &mut RuntimeProject,
    project_sample: u64,
    master_volume: f32,
) -> (f32, f32) {
    let mut out_l = 0.0f32;
    let mut out_r = 0.0f32;
    let master_index = runtime.tracks.iter().position(|t| t.track_type == "master");
    let beat = sample_to_beat(runtime, project_sample);

    for clip_index in 0..runtime.clips.len() {
        let clip = &runtime.clips[clip_index];
        if clip.muted {
            continue;
        }
        let clip_start_sample = clip.start_sample;
        let clip_duration_samples = clip.duration_samples;
        if project_sample < clip_start_sample {
            continue;
        }
        let rel = project_sample - clip_start_sample;
        if rel >= clip_duration_samples {
            continue;
        }

        let clip_offset_seconds = clip.offset_seconds;
        let clip_source_read_rate = clip.source_read_rate;
        let clip_reverse = clip.reverse;
        let clip_gain = clip.gain;
        let clip_fade_in = clip.fade_in_samples;
        let clip_fade_out = clip.fade_out_samples;
        let clip_fade_in_curve = clip.fade_in_curve;
        let clip_fade_out_curve = clip.fade_out_curve;
        let source = Arc::clone(&clip.source);

        // Resolved at build time — no id lookup or String clone per sample.
        let Some(track_index) = clip.track_index.filter(|&ti| ti < runtime.tracks.len()) else {
            continue;
        };
        if Some(track_index) == master_index {
            continue;
        }
        let has_solo = runtime.has_solo;
        if effective_track_muted(&runtime.tracks[track_index], beat)
            || (has_solo
                && !runtime.tracks[track_index].solo
                && !has_soloed_vsti_output_child(runtime, track_index))
        {
            continue;
        }

        let source_pos_seconds = clip_source_pos_seconds(
            clip_offset_seconds,
            rel,
            clip_duration_samples,
            runtime.sample_rate,
            if matches!(clip.processor, ClipDspProcessor::PhaseVocoderBasic) {
                1.0 / clip.effective_time_ratio.max(0.01)
            } else {
                clip_source_read_rate
            },
            clip_reverse,
        );
        let source_pos = source_pos_seconds * source.sample_rate() as f64;
        let dry_pos_seconds = clip_source_pos_seconds(
            clip_offset_seconds,
            rel,
            clip_duration_samples,
            runtime.sample_rate,
            clip_source_read_rate,
            clip_reverse,
        );
        let dry_source_pos = dry_pos_seconds * source.sample_rate() as f64;
        let (mut l, mut r) = sample_clip_processor_stereo(
            &source,
            source_pos,
            dry_source_pos,
            clip.effective_time_ratio,
            clip.processor,
        );
        if l == 0.0 && r == 0.0 {
            continue;
        }

        let fade = clip_fade_gain_with_curves(
            rel,
            clip_duration_samples,
            clip_fade_in,
            clip_fade_out,
            clip_fade_in_curve,
            clip_fade_out_curve,
        );
        let g = clip_gain * fade;
        l *= g;
        r *= g;

        // Build-time resolved output index (None for master/missing) — never
        // clone ids or the sends Vec on the audio thread.
        let output_track_index = runtime.tracks[track_index]
            .output_track_index
            .filter(|&t| t < runtime.tracks.len());
        let (track_l, track_r) =
            apply_track_chain_at_beat(l, r, &mut runtime.tracks[track_index], beat);
        let (track_l, track_r) =
            apply_preview_mode(track_l, track_r, runtime.tracks[track_index].preview_mode);
        runtime.accumulate_track_meter(track_index, track_l, track_r);

        if let Some(target_index) = output_track_index {
            let (bus_l, bus_r) = apply_track_chain_at_beat(
                track_l,
                track_r,
                &mut runtime.tracks[target_index],
                beat,
            );
            let (bus_l, bus_r) =
                apply_preview_mode(bus_l, bus_r, runtime.tracks[target_index].preview_mode);
            runtime.accumulate_track_meter(target_index, bus_l, bus_r);
            out_l += bus_l;
            out_r += bus_r;
        } else {
            out_l += track_l;
            out_r += track_r;
        }

        let send_count = runtime.tracks[track_index].sends.len();
        for s in 0..send_count {
            let (enabled, level, return_track_index) = {
                let send = &runtime.tracks[track_index].sends[s];
                (send.enabled, send.level, send.return_track_index)
            };
            if !enabled || level <= 0.0 {
                continue;
            }
            let Some(return_track_index) = return_track_index.filter(|&t| t < runtime.tracks.len())
            else {
                continue;
            };
            let return_track = &runtime.tracks[return_track_index];
            if effective_track_muted(return_track, beat) || (runtime.has_solo && !return_track.solo)
            {
                continue;
            }
            let (send_l, send_r) = apply_track_chain_at_beat(
                track_l * level,
                track_r * level,
                &mut runtime.tracks[return_track_index],
                beat,
            );
            let (send_l, send_r) = apply_preview_mode(
                send_l,
                send_r,
                runtime.tracks[return_track_index].preview_mode,
            );
            runtime.accumulate_track_meter(return_track_index, send_l, send_r);
            out_l += send_l;
            out_r += send_r;
        }
    }

    // ── Master bus: apply master track inserts on the summed output ──
    if let Some(m_idx) = master_index {
        let muted = effective_track_muted(&runtime.tracks[m_idx], beat)
            || (runtime.has_solo && !runtime.tracks[m_idx].solo);
        if !muted {
            let master = &mut runtime.tracks[m_idx];
            for insert in &mut master.inserts {
                if insert.kind_tag != crate::runtime::RuntimeInsertKind::NativePlugin {
                    let plugin_id = canonical_plugin_id(&insert.kind);
                    insert.dsp.refresh_process_params(plugin_id, &insert.params);
                }
                let (l, r) = apply_insert(out_l, out_r, insert);
                out_l = l;
                out_r = r;
            }
            let (l, r) = apply_preview_mode(out_l, out_r, master.preview_mode);
            out_l = l;
            out_r = r;
            runtime.accumulate_track_meter(m_idx, out_l, out_r);
        }
    }

    // Master fader only. No limiter, no soft knee, no clip — the sum leaves
    // this function exactly as the graph built it, scaled.
    (out_l * master_volume, out_r * master_volume)
}

/// Routing track kinds (Phase 3): receive sends rather than hosting clips.
#[inline]
fn is_routing_type(track_type: &str) -> bool {
    is_routing_track_type(track_type)
}

#[inline]
fn is_vsti_output_child_track_id(track_id: &str) -> bool {
    track_id.starts_with("vsti-out:")
}

use crate::runtime::{has_soloed_vsti_output_child, has_soloed_vsti_output_parent};

/// Two distinct mutable elements of a slice without allocation. Panics in
/// debug if `a == b`; callers guarantee distinct indices.
#[inline]
fn two_mut<T>(v: &mut [T], a: usize, b: usize) -> (&mut T, &mut T) {
    debug_assert!(a != b);
    if a < b {
        let (lo, hi) = v.split_at_mut(b);
        (&mut lo[a], &mut hi[0])
    } else {
        let (lo, hi) = v.split_at_mut(a);
        (&mut hi[0], &mut lo[b])
    }
}

#[inline]
pub(crate) fn tempo_map_from_project_snapshot(project: &EngineProjectSnapshot) -> TempoMap {
    if project.tempo_points.is_empty() {
        TempoMap::static_tempo(project.bpm)
    } else {
        TempoMap::from_points(
            project.bpm,
            project
                .tempo_points
                .iter()
                .map(|p| TempoPoint {
                    beat: p.beat,
                    bpm: p.bpm,
                    curve: crate::tempo_map::TempoCurve::from_tag(p.curve),
                })
                .collect(),
        )
    }
}

fn sample_to_beat(runtime: &RuntimeProject, sample: u64) -> f64 {
    runtime
        .tempo_map
        .beat_at_samples(sample, runtime.sample_rate.max(1) as f64)
}

/// Map an in-clip output offset `rel` to a source position in **seconds**,
/// honoring the clip's resample `speed_ratio` and reverse flag.
///
/// Forward playback reads from `offset_seconds` and advances at `speed_ratio`
/// source-seconds per output-second. Reverse reads the same source window from
/// its end backward, so output sample 0 maps to the last source frame and the
/// final output sample maps back to `offset_seconds`. Allocation-free; called
/// from the audio callback.
#[inline]
pub(crate) fn clip_source_pos_seconds(
    offset_seconds: f64,
    rel: u64,
    duration_samples: u64,
    output_sample_rate: u32,
    speed_ratio: f32,
    reverse: bool,
) -> f64 {
    let sr = output_sample_rate.max(1) as f64;
    let advance = if reverse {
        duration_samples.saturating_sub(1).saturating_sub(rel)
    } else {
        rel
    } as f64;
    offset_seconds + (advance / sr) * speed_ratio as f64
}

#[inline]
pub(crate) fn sample_clip_processor_stereo(
    source: &ClipAudioSource,
    source_pos: f64,
    resample_pos: f64,
    effective_time_ratio: f32,
    processor: ClipDspProcessor,
) -> (f32, f32) {
    if !matches!(processor, ClipDspProcessor::PhaseVocoderBasic) {
        return sample_source_stereo(source, resample_pos);
    }
    phase_vocoder_basic_sample(source, source_pos, effective_time_ratio)
}

#[inline]
fn phase_vocoder_basic_sample(
    source: &ClipAudioSource,
    source_pos: f64,
    effective_time_ratio: f32,
) -> (f32, f32) {
    let ratio = effective_time_ratio.clamp(0.05, 20.0) as f64;
    if (ratio - 1.0).abs() < 1e-6 {
        return sample_source_stereo(source, source_pos);
    }

    // Basic streaming OLA/granular stretcher. It is allocation-free and reads
    // from the existing clip source; a higher-quality phase vocoder can replace
    // this processor without changing snapshot/runtime routing.
    let grain = 1024.0_f64;
    let hop_out = grain * 0.5;
    let hop_in = hop_out / ratio;
    let phase = (source_pos / hop_in).fract().clamp(0.0, 1.0);
    let window = 0.5 - 0.5 * (std::f64::consts::TAU * phase).cos();
    let (al, ar) = sample_source_stereo(source, source_pos);
    let (bl, br) = sample_source_stereo(source, source_pos + hop_in);
    let w = window as f32;
    (al * (1.0 - w) + bl * w, ar * (1.0 - w) + br * w)
}

/// Clip-fade gain for a sample at offset `rel` from the clip start.
///
/// `1.0` outside both fade regions; ramps `0→1` across the fade-in and `1→0`
/// across the fade-out, each in its own [`FadeCurve`]. Allocation-free and safe
/// for the realtime render path.
///
/// The curves used to be ignored here: the project and the engine snapshot both
/// carried an `in_curve`/`out_curve` tag, and this function rendered equal power
/// whatever they said — so choosing a curve changed the file and nothing else.
#[inline]
pub(crate) fn clip_fade_gain_with_curves(
    rel: u64,
    duration: u64,
    fade_in: u64,
    fade_out: u64,
    in_curve: crate::runtime::FadeCurve,
    out_curve: crate::runtime::FadeCurve,
) -> f32 {
    let mut gain = 1.0f32;
    if fade_in > 0 && rel < fade_in {
        gain *= in_curve.rising_gain(rel as f32 / fade_in as f32);
    }
    if fade_out > 0 {
        let fade_out_start = duration.saturating_sub(fade_out);
        if rel >= fade_out_start {
            gain *= out_curve
                .falling_gain((rel - fade_out_start) as f32 / fade_out as f32)
                .max(0.0);
        }
    }
    gain
}

/// [`clip_fade_gain_with_curves`] at the default curve on both edges.
#[inline]
pub(crate) fn clip_fade_gain(rel: u64, duration: u64, fade_in: u64, fade_out: u64) -> f32 {
    clip_fade_gain_with_curves(
        rel,
        duration,
        fade_in,
        fade_out,
        crate::runtime::FadeCurve::EqualPower,
        crate::runtime::FadeCurve::EqualPower,
    )
}

#[inline]
pub(crate) fn effective_track_muted(track: &RuntimeTrack, beat: f64) -> bool {
    track
        .automation_values_at_beat(beat)
        .muted
        .unwrap_or(track.muted)
}

/// Apply a track's fader (volume / pan / preview mode) to its `block_*`
/// (which already holds the post-insert signal), write the post-fader result
/// back into `block_*`, and accumulate the track meter. Does **not** sum to any
/// destination — routing is done separately by [`route_main_output`]. No
/// allocation.
#[inline]
fn apply_fader(track: &mut RuntimeTrack, frames: usize, beat: f64, smooth: bool) {
    let automation = track.automation_values_at_beat(beat);
    let volume = automation.volume.unwrap_or(track.volume);
    let pan = automation.pan.unwrap_or(track.pan);
    let (pan_l, pan_r) = pan_gains(pan);
    let target_l = volume * pan_l;
    let target_r = volume * pan_r;
    // Measured, not asserted: `pre` is what the channel produced, `post` is what
    // routing will sum. `automation_override=yes` is the case where `track.volume`
    // is set correctly and ignored anyway, which looks identical from outside.
    let fader_trace_pre = (fader_debug_enabled() && frames > 0).then(|| {
        track.block_l[..frames]
            .iter()
            .fold(0.0f32, |peak, s| peak.max(s.abs()))
    });
    if !smooth {
        // Offline export / tests: exact constant per-block gain (unchanged
        // behavior, deterministic bounce). Keep the smoother aligned with the
        // applied gain so a later realtime block starts without a jump.
        for frame_idx in 0..frames {
            let (l, r) = apply_preview_mode(
                track.block_l[frame_idx] * target_l,
                track.block_r[frame_idx] * target_r,
                track.preview_mode,
            );
            track.block_l[frame_idx] = l;
            track.block_r[frame_idx] = r;
        }
        track.smoothed_gain_l = target_l;
        track.smoothed_gain_r = target_r;
        log_fader_trace(track, frames, fader_trace_pre, volume, &automation);
        return;
    }
    // Realtime: ramp from the previously applied gain to the new target across
    // the block (≈ one block ≈ 10 ms @ 48 k / 512). Each block ends on the
    // target, so successive blocks are continuous — no step at block boundaries,
    // no zipper noise when the fader/pan is dragged. Allocation-free.
    let start_l = track.smoothed_gain_l;
    let start_r = track.smoothed_gain_r;
    let inv = 1.0 / frames as f32;
    let inc_l = (target_l - start_l) * inv;
    let inc_r = (target_r - start_r) * inv;
    for frame_idx in 0..frames {
        let g_l = start_l + inc_l * frame_idx as f32;
        let g_r = start_r + inc_r * frame_idx as f32;
        let (l, r) = apply_preview_mode(
            track.block_l[frame_idx] * g_l,
            track.block_r[frame_idx] * g_r,
            track.preview_mode,
        );
        track.block_l[frame_idx] = l;
        track.block_r[frame_idx] = r;
    }
    track.smoothed_gain_l = target_l;
    track.smoothed_gain_r = target_r;
    log_fader_trace(track, frames, fader_trace_pre, volume, &automation);
}

/// Report what [`apply_fader`] measured. `FUTUREBOARD_FADER_DEBUG=1` only, and
/// throttled — this writes to stderr from the audio callback, which is a
/// diagnostic exception to the realtime rules, not a steady-state path.
///
/// Read it as: `applied` is the ratio the block actually moved by, `volume` is
/// what the engine believes the fader is set to. They agree ⇒ the engine is
/// doing its job and the fault is upstream of `SetTrackVolume`. They disagree ⇒
/// the fault is here.
#[inline]
fn log_fader_trace(
    track: &RuntimeTrack,
    frames: usize,
    pre_peak: Option<f32>,
    volume: f32,
    automation: &crate::runtime::RuntimeTrackAutomationValues,
) {
    let Some(pre) = pre_peak else {
        return;
    };
    if pre <= 1.0e-6 {
        return; // silent block — the ratio would be meaningless
    }
    static FADER_TRACE_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if !FADER_TRACE_COUNT
        .fetch_add(1, Ordering::Relaxed)
        .is_multiple_of(64)
    {
        return;
    }
    let post = track.block_l[..frames]
        .iter()
        .fold(0.0f32, |peak, s| peak.max(s.abs()));
    eprintln!(
        "[fader-apply] track={} volume={:.4} pan={:.2} automation_override={} pre_peak={:.6} post_peak={:.6} applied={:.4}",
        track.id,
        volume,
        track.pan,
        if automation.volume.is_some() {
            "yes(fader ignored)"
        } else {
            "no"
        },
        pre,
        post,
        post / pre
    );
}

#[inline]
fn accumulate_block_meter(track: &mut RuntimeTrack, frames: usize) {
    for frame_idx in 0..frames {
        let l = track.block_l[frame_idx];
        let r = track.block_r[frame_idx];
        track.meter_peak_l = track.meter_peak_l.max(l.abs());
        track.meter_peak_r = track.meter_peak_r.max(r.abs());
        track.meter_sum_sq_l += l * l;
        track.meter_sum_sq_r += r * r;
    }
}

/// Sum a track's post-fader `block_*` into its output destination.
///
/// If `output_track_id` resolves to a routing track (bus/group/return) the
/// full post-fader signal is added to that track's receive buffer (`recv_*`),
/// so it is processed in Pass 2; otherwise it sums into the interleaved master
/// output. Cycle-safe like [`accumulate_sends`]: routing to self, to a
/// non-routing track, or backward between routing tracks falls back to master.
/// No allocation.
#[inline]
pub(crate) fn route_main_output(
    runtime: &mut RuntimeProject,
    src_index: usize,
    frames: usize,
    output: &mut [f32],
    channels: usize,
) {
    // Resolved at build time (None for master/missing) — no id lookup on the
    // audio thread.
    let target = runtime.tracks[src_index]
        .output_track_index
        .filter(|&t| t < runtime.tracks.len());

    if let Some(t) = target {
        let accept = t != src_index && is_routing_type(&runtime.tracks[t].track_type);
        if accept {
            let (src, tgt) = two_mut(&mut runtime.tracks, src_index, t);
            for f in 0..frames {
                tgt.recv_l[f] += src.block_l[f];
                tgt.recv_r[f] += src.block_r[f];
            }
            return;
        }
    }

    // Default / fallback: sum into the master output.
    let track = &runtime.tracks[src_index];
    for f in 0..frames {
        let out = &mut output[f * channels..f * channels + channels];
        out[0] += track.block_l[f];
        out[1] += track.block_r[f];
    }
}

/// Capture this channel's contribution to the Control Room at `stage`.
///
/// Two independent taps share this call:
///
/// * **Listen** — a channel with PFL/AFL engaged sums into the listen bus.
///   PFL is captured pre-fader so its level is independent of the fader; AFL is
///   captured post-fader so it follows the fader.
/// * **Source** — when the Control Room's selected source names this channel
///   (a bus, or a track tapped pre/post fader), its block is copied into the
///   monitor source scratch.
///
/// This only *reads* `block_*`; it never modifies the channel, so monitoring
/// cannot alter the master mix, an export, or a recording. Allocation-free, and
/// an early-out on the overwhelmingly common case (no Listen engaged, master
/// bus source) keeps it off the hot path.
#[inline]
fn capture_monitor_taps(
    runtime: &mut RuntimeProject,
    track_index: usize,
    frames: usize,
    stage: TapStage,
) {
    let wants_listen = runtime.tracks[track_index].listen.is_active()
        && match runtime.tracks[track_index].listen {
            ListenMode::Pfl => stage == TapStage::PreFader,
            ListenMode::Afl => stage == TapStage::PostFader,
            ListenMode::Off => false,
        };
    let wants_source = runtime.monitor.source_track_index == Some(track_index)
        && runtime.monitor.source_stage == Some(stage);
    if !wants_listen && !wants_source {
        return;
    }
    if !runtime.monitor.has_block_capacity(frames) {
        return;
    }
    let (track, monitor) = (&runtime.tracks[track_index], &mut runtime.monitor);
    if wants_listen {
        for i in 0..frames {
            monitor.listen_l[i] += track.block_l[i];
            monitor.listen_r[i] += track.block_r[i];
        }
        monitor.listen_active = true;
    }
    if wants_source {
        monitor.source_l[..frames].copy_from_slice(&track.block_l[..frames]);
        monitor.source_r[..frames].copy_from_slice(&track.block_r[..frames]);
        monitor.source_captured = true;
    }
}

#[allow(clippy::too_many_arguments)]
fn process_track_block(
    runtime: &mut RuntimeProject,
    track_index: usize,
    frames: usize,
    output: &mut [f32],
    channels: usize,
    beat: f64,
    transport: RuntimeTransportContext,
) {
    apply_track_chain_block(&mut runtime.tracks[track_index], frames, transport);
    // Multi-out: demux this track's bridged-instrument output channels into the
    // child "Out Ch" tracks' receive buffers (no-op unless the insert defines
    // child routes). Runs before pass 2, where the child routing tracks consume
    // their `recv_*`.
    scatter_vsti_output_children(runtime, track_index, frames, output, channels);
    // Pre-fader sends tap the post-insert signal currently in block_*.
    accumulate_sends(runtime, track_index, frames, true);
    // PFL and a TrackPreFader monitor source tap the same point the pre-fader
    // sends do: after this channel's inserts, before its fader.
    capture_monitor_taps(runtime, track_index, frames, TapStage::PreFader);
    let smooth = runtime.fader_smoothing;
    apply_fader(&mut runtime.tracks[track_index], frames, beat, smooth);
    accumulate_block_meter(&mut runtime.tracks[track_index], frames);
    // AFL, a TrackAfterFader source, and a Bus source tap after the fader and
    // channel processing, before PDC — matching the post-fader send tap so
    // every monitoring path stays time-aligned with the sends.
    capture_monitor_taps(runtime, track_index, frames, TapStage::PostFader);
    // Post-fader sends tap *before* PDC so return FX latency can be compensated
    // on the dry/main path without also delaying the send feed (which would
    // push wet further behind dry).
    accumulate_sends(runtime, track_index, frames, false);
    let pdc_delay = runtime
        .latency_graph
        .track_pdc_delay
        .get(track_index)
        .copied()
        .unwrap_or(0);
    if pdc_delay > 0 {
        let track = &mut runtime.tracks[track_index];
        apply_pdc_delay_block(
            &mut track.block_l[..frames],
            &mut track.block_r[..frames],
            &mut track.pdc_delay_l,
            &mut track.pdc_delay_r,
            &mut track.pdc_write_pos,
            pdc_delay,
            frames,
        );
    }
    // Route the post-fader (and PDC-aligned) signal to master or the track's output bus.
    route_main_output(runtime, track_index, frames, output, channels);
}

/// Keep an inaudible track's hosted instrument running, then throw the audio
/// away.
///
/// Mute and solo silence a track's **output**, not its instrument. Skipping the
/// chain outright meant the block's MIDI was never delivered: a note that
/// started while the track was muted (or while another track was soloed) was
/// dropped for good, and a note already sounding froze mid-voice instead of
/// playing on underneath. Running the chain and clearing the block before
/// sends, fader, meters, and routing keeps the notes moving with the transport
/// while nothing reaches the mix — so lifting the mute or solo drops back in
/// mid-phrase instead of restarting or staying silent.
///
/// Tracks with no instrument route hold no note state, so they stay skipped and
/// cost nothing. Realtime-safe: same work the audible path already does, plus
/// two slice fills.
fn render_inaudible_instrument_block(
    runtime: &mut RuntimeProject,
    track_index: usize,
    frames: usize,
    transport: RuntimeTransportContext,
) {
    let track = &mut runtime.tracks[track_index];
    if track.midi_instrument_insert_ix.is_none()
        && track.soundfont_player.is_none()
        && track.solfege_engine.is_none()
    {
        return;
    }
    apply_track_chain_block(track, frames, transport);
    track.block_l[..frames].fill(0.0);
    track.block_r[..frames].fill(0.0);
}

/// Add the source track's block (`block_*`, holding either the post-insert or
/// post-fader signal depending on `pre_fader`) into each accepted send target's
/// receive buffer (`recv_*`), scaled by the send level. Only sends whose
/// `pre_fader` flag matches the requested phase are routed.
///
/// Cycle-safe by construction: a send is accepted only when the target is a
/// routing track (bus/return). Cyclic routes are rejected at graph-plan time
/// (`plan_runtime_audio_graph`) and pass-2 processes routing tracks in
/// topological order so chained bus→return feeds land before the target runs.
/// Sends to non-routing tracks or to self are dropped. No allocation on the
/// audio thread.
#[inline]
pub(crate) fn accumulate_sends(
    runtime: &mut RuntimeProject,
    src_index: usize,
    frames: usize,
    pre_fader: bool,
) {
    let send_count = runtime.tracks[src_index].sends.len();
    if send_count == 0 {
        return;
    }
    for s in 0..send_count {
        let (enabled, level, target_index) = {
            let send = &runtime.tracks[src_index].sends[s];
            if send.pre_fader != pre_fader {
                continue;
            }
            (send.enabled, send.level, send.return_track_index)
        };
        if !enabled || level == 0.0 {
            continue;
        }
        // Resolved at build time — no id lookup on the audio thread.
        let Some(t) = target_index.filter(|&t| t < runtime.tracks.len()) else {
            continue;
        };
        if t == src_index || !is_routing_type(&runtime.tracks[t].track_type) {
            continue;
        }
        let (src, tgt) = two_mut(&mut runtime.tracks, src_index, t);
        for f in 0..frames {
            tgt.recv_l[f] += src.block_l[f] * level;
            tgt.recv_r[f] += src.block_r[f] * level;
        }
    }
}

/// Source-stream span consumed to render the output segment
/// `[rel_start, rel_start + frames)` at `time_ratio`. Successive segments tile
/// the source contiguously — block N's `in_start + input_frames` equals block
/// N+1's `in_start` — so the source is read exactly once with no gap or overlap,
/// and the total consumed over the clip is `floor(duration / time_ratio)`
/// (= the source length), never more. This is what keeps the streaming stretcher
/// from over-reading the source or growing an internal backlog.
pub(crate) fn signalsmith_input_span(
    rel_start: u64,
    frames: usize,
    time_ratio: f64,
) -> (i64, usize) {
    let ratio = time_ratio.clamp(0.05, 20.0);
    let in_start = (rel_start as f64 / ratio).floor() as i64;
    let in_end = ((rel_start + frames as u64) as f64 / ratio).floor() as i64;
    (in_start, (in_end - in_start).max(1) as usize)
}

fn render_signalsmith_clip_segment(
    runtime: &mut RuntimeProject,
    clip_index: usize,
    track_index: usize,
    project_start_sample: u64,
    rel_start: u64,
    frame_idx_start: usize,
    frames: usize,
) -> bool {
    let (
        source,
        offset_seconds,
        duration_samples,
        output_sample_rate,
        reverse,
        gain,
        fade_in_samples,
        fade_out_samples,
        fade_in_curve,
        fade_out_curve,
        time_ratio,
    ) = {
        let clip = &runtime.clips[clip_index];
        (
            Arc::clone(&clip.source),
            clip.offset_seconds,
            clip.duration_samples,
            runtime.sample_rate,
            clip.reverse,
            clip.gain,
            clip.fade_in_samples,
            clip.fade_out_samples,
            clip.fade_in_curve,
            clip.fade_out_curve,
            clip.effective_time_ratio.clamp(0.05, 20.0) as f64,
        )
    };

    // Map this output segment [rel_start, rel_start + frames) onto a *contiguous*
    // span of the source stream so successive blocks tile the source with no gap
    // or overlap, and the source is never over-read. The stretcher consumes
    // exactly these `input_frames` samples to produce `frames` output (time ratio
    // = frames / input_frames), so it never has to buffer/grow across calls.
    let (in_start, input_frames) = signalsmith_input_span(rel_start, frames, time_ratio);
    let total_input = (duration_samples as f64 / time_ratio).floor() as i64;
    let output_sr = output_sample_rate.max(1) as f64;
    let source_sr = source.sample_rate() as f64;

    // Source-stream index → source sample position (reverse-aware). Reading the
    // source at the output sample rate lets the seconds map handle the
    // source↔output rate conversion, matching the per-sample resample path.
    // Shared by the pre-roll priming and the per-block feed so both read one
    // contiguous stream.
    let source_pos_at = |stream_index: i64| -> f64 {
        let effective = if reverse {
            (total_input - 1 - stream_index).max(0)
        } else {
            stream_index
        };
        (offset_seconds + effective as f64 / output_sr) * source_sr
    };

    let clip = &mut runtime.clips[clip_index];
    let Some(processor) = clip.stretch_processor.as_mut() else {
        return false;
    };

    // On a (re)start/discontinuity, latency-align the stretcher to this playback
    // position. `output_seek` pre-roll priming makes the *next* `process` output
    // line up with the timeline, so a high-latency preserve-pitch backend
    // (Signalsmith ≈120 ms) does not drift behind the rest of the mix.
    // Zero-latency backends report `seek_input_len == 0` and just reset.
    if clip.stretch_next_project_sample != Some(project_start_sample) {
        let playback_rate = (1.0 / time_ratio.max(0.05)) as f32;
        let seek_len = processor.seek_input_len(playback_rate);
        if seek_len > 0 {
            if clip.stretch_prime_l.len() < seek_len {
                clip.stretch_prime_l.resize(seek_len, 0.0);
                clip.stretch_prime_r.resize(seek_len, 0.0);
            }
            // Pre-roll = the `seek_len` source frames ending just before `in_start`
            // (clamped/silent before the clip's source window).
            for j in 0..seek_len {
                let stream_index = in_start - seek_len as i64 + j as i64;
                let (l, r) = sample_source_stereo(&source, source_pos_at(stream_index));
                clip.stretch_prime_l[j] = l;
                clip.stretch_prime_r[j] = r;
            }
            processor.output_seek(
                &clip.stretch_prime_l[..seek_len],
                &clip.stretch_prime_r[..seek_len],
            );
        } else {
            processor.reset();
        }
    }

    if clip.stretch_input_l.len() < input_frames {
        clip.stretch_input_l.resize(input_frames, 0.0);
        clip.stretch_input_r.resize(input_frames, 0.0);
    }
    if clip.stretch_output_l.len() < frames {
        clip.stretch_output_l.resize(frames, 0.0);
        clip.stretch_output_r.resize(frames, 0.0);
    }

    for k in 0..input_frames {
        let (l, r) = sample_source_stereo(&source, source_pos_at(in_start + k as i64));
        clip.stretch_input_l[k] = l;
        clip.stretch_input_r[k] = r;
    }

    if processor
        .process_stereo(
            &clip.stretch_input_l[..input_frames],
            &clip.stretch_input_r[..input_frames],
            &mut clip.stretch_output_l[..frames],
            &mut clip.stretch_output_r[..frames],
        )
        .is_err()
    {
        clip.stretch_next_project_sample = None;
        return false;
    }
    clip.stretch_next_project_sample = Some(project_start_sample + frames as u64);

    let track = &mut runtime.tracks[track_index];
    for i in 0..frames {
        let rel = rel_start + i as u64;
        let fade = clip_fade_gain_with_curves(
            rel,
            duration_samples,
            fade_in_samples,
            fade_out_samples,
            fade_in_curve,
            fade_out_curve,
        );
        let g = gain * fade;
        let frame_idx = frame_idx_start + i;
        track.block_l[frame_idx] += clip.stretch_output_l[i] * g;
        track.block_r[frame_idx] += clip.stretch_output_r[i] * g;
    }

    true
}

/// `transport_active` — false when this block is rendered while the transport
/// is stopped (MIDI preview, post-panic bridge flush, open plugin editor). In
/// that mode the track/insert graph still runs (so bridged VSTi previews are
/// heard and the host handshake stays alive) but timeline clip material is
/// skipped — otherwise the frozen playhead would stutter-loop the same audio
/// clip slice every callback.
#[allow(clippy::too_many_arguments)]
pub fn render_project_block_interleaved(
    runtime: &mut RuntimeProject,
    base_sample: u64,
    master_volume: f32,
    output: &mut [f32],
    channels: usize,
    transport_active: bool,
    time_sig_num: u32,
    time_sig_den: u32,
    loop_bounds: Option<crate::transport::LoopBounds>,
) -> u64 {
    render_project_block_interleaved_core(
        runtime,
        base_sample,
        master_volume,
        output,
        channels,
        transport_active,
        time_sig_num,
        time_sig_den,
        loop_bounds,
        None,
        None,
        None,
    )
}

/// Realtime variant that injects the selected live-input block into every
/// software-monitored source track before Pass 1. The normal graph then runs
/// exactly once, preserving plugin state, PDC, sends, buses, and master DSP.
#[allow(clippy::too_many_arguments)]
pub fn render_project_block_interleaved_with_live_input(
    runtime: &mut RuntimeProject,
    base_sample: u64,
    master_volume: f32,
    output: &mut [f32],
    channels: usize,
    transport_active: bool,
    time_sig_num: u32,
    time_sig_den: u32,
    loop_bounds: Option<crate::transport::LoopBounds>,
    input_l: &[f32],
    input_r: &[f32],
) -> u64 {
    render_project_block_interleaved_core(
        runtime,
        base_sample,
        master_volume,
        output,
        channels,
        transport_active,
        time_sig_num,
        time_sig_den,
        loop_bounds,
        Some((input_l, input_r)),
        None,
        None,
    )
}

/// Realtime variant that additionally lets tracks routed to an Audio Jam stream
/// draw from the jam bus.
///
/// `live_input` and `jam` are separate on purpose: they are different sources
/// with different clocks, and a track is routed to exactly one of them. Passing
/// both here is normal — one session can have a guitarist on the interface and
/// a second guitarist over the network at the same time.
#[allow(clippy::too_many_arguments)]
pub fn render_project_block_interleaved_with_inputs(
    runtime: &mut RuntimeProject,
    base_sample: u64,
    master_volume: f32,
    output: &mut [f32],
    channels: usize,
    transport_active: bool,
    time_sig_num: u32,
    time_sig_den: u32,
    loop_bounds: Option<crate::transport::LoopBounds>,
    live_input: Option<(&[f32], &[f32])>,
    jam: Option<&crate::jam_bus::JamAudioBus>,
) -> u64 {
    render_project_block_interleaved_core(
        runtime,
        base_sample,
        master_volume,
        output,
        channels,
        transport_active,
        time_sig_num,
        time_sig_den,
        loop_bounds,
        live_input,
        None,
        jam,
    )
}

/// Offline/export variant that captures every mixer channel post-fader in the
/// same graph pass. `track_taps` follows `runtime.tracks` order and is ignored
/// by realtime callers through the wrapper above.
#[allow(clippy::too_many_arguments)]
pub fn render_project_block_interleaved_with_taps(
    runtime: &mut RuntimeProject,
    base_sample: u64,
    master_volume: f32,
    output: &mut [f32],
    channels: usize,
    transport_active: bool,
    time_sig_num: u32,
    time_sig_den: u32,
    loop_bounds: Option<crate::transport::LoopBounds>,
    track_taps: Option<&mut [Vec<f32>]>,
) -> u64 {
    render_project_block_interleaved_core(
        runtime,
        base_sample,
        master_volume,
        output,
        channels,
        transport_active,
        time_sig_num,
        time_sig_den,
        loop_bounds,
        None,
        track_taps,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_project_block_interleaved_core(
    runtime: &mut RuntimeProject,
    base_sample: u64,
    master_volume: f32,
    output: &mut [f32],
    channels: usize,
    transport_active: bool,
    time_sig_num: u32,
    time_sig_den: u32,
    loop_bounds: Option<crate::transport::LoopBounds>,
    live_input: Option<(&[f32], &[f32])>,
    mut track_taps: Option<&mut [Vec<f32>]>,
    jam: Option<&crate::jam_bus::JamAudioBus>,
) -> u64 {
    if channels < 2 {
        return 0;
    }
    let frames = output.len() / channels;
    if frames == 0 {
        return 0;
    }
    if let Some(taps) = track_taps.as_deref_mut() {
        for tap in taps.iter_mut().take(runtime.tracks.len()) {
            tap.resize(frames * 2, 0.0);
            tap.fill(0.0);
        }
    }
    // Reset the Control Room taps for this block. Cheap and unconditional: the
    // offline exporter runs the same graph but never consumes these buffers, so
    // clearing them costs a memset over an empty (unallocated) scratch there.
    runtime.monitor.begin_block(frames);
    runtime.refresh_runtime_latency_graph(frames as u32);
    let block_beat = sample_to_beat(runtime, base_sample);
    // Real transport ProcessContext for every plugin processed this block —
    // tempo from the map at this position, time signature from the engine,
    // project position from the playhead, playing = transport state. Replaces
    // the old hardcoded 120 BPM / always-playing stub.
    let transport = RuntimeTransportContext {
        tempo_bpm: runtime.tempo_map.bpm_at_beat(block_beat),
        time_sig_num,
        time_sig_den,
        project_time_samples: base_sample as i64,
        ppq_position: block_beat,
        bar_position_ppq: RuntimeTransportContext::bar_start_ppq(
            block_beat,
            time_sig_num,
            time_sig_den,
        ),
        playing: transport_active,
        recording: false,
    };
    for frame in output.chunks_mut(channels) {
        frame[0] = 0.0;
        frame[1] = 0.0;
        for extra in frame.iter_mut().skip(2) {
            *extra = 0.0;
        }
    }

    for (track_index, track) in runtime.tracks.iter_mut().enumerate() {
        let is_routing_or_master = crate::audio_graph::is_routing_track_type(&track.track_type)
            || crate::audio_graph::is_master_track_type(&track.track_type);
        let source_active = runtime
            .audio_graph
            .active_source_mask
            .get(track_index)
            .copied()
            .unwrap_or(true)
            || track.monitor_enabled
            || !track.midi_block_events.is_empty();
        if !is_routing_or_master && !source_active {
            continue;
        }
        if track.block_l.len() < frames {
            track.block_l.resize(frames, 0.0);
            track.block_r.resize(frames, 0.0);
        }
        // Receive buffers grow lazily to the largest block seen; the audio
        // thread only `fill`s, never allocates, once warmed.
        if track.recv_l.len() < frames {
            track.recv_l.resize(frames, 0.0);
            track.recv_r.resize(frames, 0.0);
        }
        track.block_l[..frames].fill(0.0);
        track.block_r[..frames].fill(0.0);
        track.recv_l[..frames].fill(0.0);
        track.recv_r[..frames].fill(0.0);
    }

    if let Some((input_l, input_r)) = live_input {
        let input_frames = frames.min(input_l.len()).min(input_r.len());
        // A jam-routed track is fed from the jam bus below, not from the
        // hardware input bus. Without this filter a remote performer would
        // double with whatever is plugged into the interface.
        for track in runtime.tracks.iter_mut().filter(|track| {
            track.track_type == "audio"
                && track.monitor_enabled
                && track.input_source.is_routable()
                && !track.input_source.is_internal()
        }) {
            for frame in 0..input_frames {
                track.block_l[frame] += input_l[frame];
                track.block_r[frame] += input_r[frame];
            }
        }
    }

    // Audio Jam input. One relaxed load skips the whole branch for a project
    // that is not in a jam, and each slot read is atomics only — no allocation,
    // no lock, and never a wait on the network.
    if let Some(bus) = jam.filter(|bus| bus.has_inputs()) {
        for track in runtime.tracks.iter_mut().filter(|track| {
            track.track_type == "audio" && track.monitor_enabled && track.input_source.is_jam()
        }) {
            let crate::runtime::RuntimeTrackInputSource::Jam { slot, mode } = track.input_source
            else {
                continue;
            };
            let Some(slot) = bus.input(slot as usize) else {
                continue;
            };
            slot.mix_into(
                mode,
                &mut track.block_l[..frames],
                &mut track.block_r[..frames],
                frames,
            );
        }
    }

    // Track loopback input: another track's post-fader output, as this track's
    // input. What lands here is the block the source rendered in the *previous*
    // callback — see `crate::loopback` for why that is the design and not a
    // shortcoming.
    //
    // Two passes over the index list rather than one over the tracks, because
    // reading one track while writing another needs the two borrows split. Both
    // are bounded by the track count and do nothing but copy.
    if runtime.tracks.iter().any(|track| {
        track.track_type == "audio" && track.monitor_enabled && track.input_source.is_loopback()
    }) {
        for index in 0..runtime.tracks.len() {
            let track = &runtime.tracks[index];
            if track.track_type != "audio" || !track.monitor_enabled {
                continue;
            }
            let crate::runtime::RuntimeTrackInputSource::Loopback { source_index, mode } =
                track.input_source
            else {
                continue;
            };
            let source_index = source_index as usize;
            // A track pointed at itself would read its own previous block and
            // build a feedback loop the user never asked for. The control
            // thread refuses to resolve one; this is the render path's own
            // guard, since a stale index must never be trusted.
            if source_index == index || source_index >= runtime.tracks.len() {
                continue;
            }
            {
                let source = &runtime.tracks[source_index];
                if !source.loopback_publish
                    || source.loopback_out_l.len() < frames
                    || source.loopback_out_r.len() < frames
                {
                    continue;
                }
            }
            // Borrow the source's buffers out and put them back. `Vec::take` is
            // a pointer swap, so this costs nothing and keeps the read of one
            // track and the write of another out of each other's way without
            // reaching for unsafe.
            let source_l = std::mem::take(&mut runtime.tracks[source_index].loopback_out_l);
            let source_r = std::mem::take(&mut runtime.tracks[source_index].loopback_out_r);
            {
                let track = &mut runtime.tracks[index];
                for frame in 0..frames {
                    let (l, r) = mode.apply(source_l[frame], source_r[frame]);
                    track.block_l[frame] += l;
                    track.block_r[frame] += r;
                }
            }
            runtime.tracks[source_index].loopback_out_l = source_l;
            runtime.tracks[source_index].loopback_out_r = source_r;
        }
    }

    let master_index = runtime.audio_graph.master_index;

    for clip_index in 0..runtime.clips.len() {
        if !transport_active {
            break; // stopped-transport preview block — no timeline material
        }
        // An ARA plug-in renders this clip itself (see `render_ara_block`), so
        // the file must not also be mixed in here.
        if runtime.clips[clip_index].ara_rendered {
            continue;
        }
        // The overwhelmingly common non-looping path rejects inactive clips
        // before cloning their Arc source or reading the rest of their DSP
        // metadata. Large arrangements otherwise touched every clip every
        // callback even when only a handful overlap the current block.
        if loop_bounds.is_none() {
            let clip = &runtime.clips[clip_index];
            let block_end = base_sample.saturating_add(frames as u64);
            let clip_end = clip.start_sample.saturating_add(clip.duration_samples);
            if clip_end <= base_sample || clip.start_sample >= block_end {
                continue;
            }
        }
        let (
            clip_muted,
            clip_track_index,
            source,
            clip_start,
            clip_duration,
            clip_offset_seconds,
            clip_source_read_rate,
            clip_effective_time_ratio,
            clip_processor,
            clip_reverse,
            clip_gain,
            clip_fade_in,
            clip_fade_out,
            clip_fade_in_curve,
            clip_fade_out_curve,
            clip_stretch_backend,
        ) = {
            let clip = &runtime.clips[clip_index];
            (
                clip.muted,
                clip.track_index,
                Arc::clone(&clip.source),
                clip.start_sample,
                clip.duration_samples,
                clip.offset_seconds,
                clip.source_read_rate,
                clip.effective_time_ratio,
                clip.processor,
                clip.reverse,
                clip.gain,
                clip.fade_in_samples,
                clip.fade_out_samples,
                clip.fade_in_curve,
                clip.fade_out_curve,
                clip.stretch_backend,
            )
        };
        if clip_muted {
            continue;
        }
        // Resolved at build time (RuntimeProject::resolve_indices) — no id
        // lookup on the audio thread.
        let Some(track_index) = clip_track_index.filter(|&ti| ti < runtime.tracks.len()) else {
            continue;
        };
        if effective_track_muted(&runtime.tracks[track_index], block_beat)
            || (runtime.has_solo
                && !runtime.tracks[track_index].solo
                && !has_soloed_vsti_output_child(runtime, track_index))
        {
            continue;
        }

        let clip_end = clip_start.saturating_add(clip_duration);
        let mut segment_sample =
            crate::transport::normalize_loop_position(base_sample, loop_bounds);
        let mut callback_offset = 0usize;
        let mut remaining = frames as u64;
        while remaining > 0 {
            let segment_frames = crate::transport::segment_frames_until_loop_wrap(
                segment_sample,
                remaining,
                loop_bounds,
            );
            let block_start = segment_sample;
            let block_end = segment_sample.saturating_add(segment_frames);
            if block_end > clip_start && block_start < clip_end {
                let render_start = clip_start.saturating_sub(block_start) as usize;
                let render_end = (clip_end.min(block_end) - block_start) as usize;
                let segment_render_frames = render_end.saturating_sub(render_start);
                let project_render_start = segment_sample + render_start as u64;
                let rel_start = project_render_start - clip_start;
                if clip_stretch_backend == StretchBackend::Signalsmith
                    && render_signalsmith_clip_segment(
                        runtime,
                        clip_index,
                        track_index,
                        project_render_start,
                        rel_start,
                        callback_offset + render_start,
                        segment_render_frames,
                    )
                {
                    // Rendered through the cached SphereAudioProcessor/Signalsmith
                    // path. Export uses this same render kernel.
                } else {
                    for frame_in_segment in render_start..render_end {
                        let frame_idx = callback_offset + frame_in_segment;
                        let project_sample = segment_sample + frame_in_segment as u64;
                        let rel = project_sample - clip_start;
                        let source_pos_seconds = clip_source_pos_seconds(
                            clip_offset_seconds,
                            rel,
                            clip_duration,
                            runtime.sample_rate,
                            if matches!(clip_processor, ClipDspProcessor::PhaseVocoderBasic) {
                                1.0 / clip_effective_time_ratio.max(0.01)
                            } else {
                                clip_source_read_rate
                            },
                            clip_reverse,
                        );
                        let source_pos = source_pos_seconds * source.sample_rate() as f64;
                        let dry_pos_seconds = clip_source_pos_seconds(
                            clip_offset_seconds,
                            rel,
                            clip_duration,
                            runtime.sample_rate,
                            clip_source_read_rate,
                            clip_reverse,
                        );
                        let dry_source_pos = dry_pos_seconds * source.sample_rate() as f64;
                        let (mut l, mut r) = sample_clip_processor_stereo(
                            &source,
                            source_pos,
                            dry_source_pos,
                            clip_effective_time_ratio,
                            clip_processor,
                        );
                        let fade = clip_fade_gain_with_curves(
                            rel,
                            clip_duration,
                            clip_fade_in,
                            clip_fade_out,
                            clip_fade_in_curve,
                            clip_fade_out_curve,
                        );
                        let g = clip_gain * fade;
                        l *= g;
                        r *= g;
                        runtime.tracks[track_index].block_l[frame_idx] += l;
                        runtime.tracks[track_index].block_r[frame_idx] += r;
                    }
                }
            }
            callback_offset += segment_frames as usize;
            remaining -= segment_frames;
            if remaining == 0 {
                break;
            }
            segment_sample = crate::transport::advance_loop_position(
                segment_sample,
                segment_frames,
                loop_bounds,
            )
            .0;
        }
    }

    // ── Pass 1: source tracks (audio / midi / instrument) ───────────────
    // Clips → inserts → fader, sum the post-fader signal into the master
    // output, then feed sends into routing-track receive buffers. Routing
    // tracks (bus/return/group) are deferred to Pass 2 so their inputs are complete.
    // Take the precomputed pass order out by move (zero alloc) rather than
    // cloning the Vec every audio block; the loop body never reads it back, and
    // it is restored below. `audio_graph` is otherwise untouched here.
    let pass1_indices = std::mem::take(&mut runtime.audio_graph.pass1_source_indices);
    for &track_index in &pass1_indices {
        let source_active = runtime
            .audio_graph
            .active_source_mask
            .get(track_index)
            .copied()
            .unwrap_or(true)
            || runtime.tracks[track_index].monitor_enabled
            || !runtime.tracks[track_index].midi_block_events.is_empty();
        if !source_active {
            continue;
        }
        if effective_track_muted(&runtime.tracks[track_index], block_beat)
            || (runtime.has_solo
                && !runtime.tracks[track_index].solo
                && !has_soloed_vsti_output_child(runtime, track_index))
        {
            render_inaudible_instrument_block(runtime, track_index, frames, transport);
            continue;
        }
        if callback_debug_enabled()
            && !runtime.tracks[track_index].inserts.is_empty()
            && !runtime.tracks[track_index].callback_clip_route_log_done
        {
            runtime.tracks[track_index].callback_clip_route_log_done = true;
            let track_id = runtime.tracks[track_index].id.clone();
            let block_start = base_sample;
            let block_end = base_sample.saturating_add(frames as u64);
            let input_peak_l = runtime.tracks[track_index].block_l[..frames]
                .iter()
                .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
            let input_peak_r = runtime.tracks[track_index].block_r[..frames]
                .iter()
                .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
            let mut clip_count = 0usize;
            let mut overlapping = 0usize;
            let mut first_clip = String::from("none");
            for clip in runtime
                .clips
                .iter()
                .filter(|clip| clip.track_id == track_id)
            {
                let clip_start = clip.start_sample;
                let clip_end = clip.start_sample.saturating_add(clip.duration_samples);
                let overlaps = block_end > clip_start && block_start < clip_end;
                if clip_count == 0 {
                    first_clip = format!(
                        "{} range={}..{} offset={:.3}s gain={:.3} read_rate={:.3} stretch={:.3} backend={:?} overlaps={}",
                        clip.id,
                        clip_start,
                        clip_end,
                        clip.offset_seconds,
                        clip.gain,
                        clip.source_read_rate,
                        clip.effective_time_ratio,
                        clip.stretch_backend,
                        overlaps
                    );
                }
                clip_count += 1;
                if overlaps {
                    overlapping += 1;
                }
            }
            eprintln!(
                "[SphereAudio callback] clipRoute track={} block={}..{} clips={} overlapping={} preInsertPeakL={:.6} preInsertPeakR={:.6} firstClip={}",
                track_id,
                block_start,
                block_end,
                clip_count,
                overlapping,
                input_peak_l,
                input_peak_r,
                first_clip
            );
        }
        process_track_block(
            runtime,
            track_index,
            frames,
            output,
            channels,
            block_beat,
            transport,
        );
        if let Some(tap) = track_taps
            .as_deref_mut()
            .and_then(|taps| taps.get_mut(track_index))
        {
            for frame in 0..frames {
                tap[frame * 2] = runtime.tracks[track_index].block_l[frame];
                tap[frame * 2 + 1] = runtime.tracks[track_index].block_r[frame];
            }
        }
        // Audio Jam publish tap: this track's post-fader signal, the same one
        // an export would capture. Atomics into a preallocated ring, and one
        // `Option` check for every track that is not being shared.
        publish_track_to_jam(runtime, track_index, frames, jam);
        // Track loopback tap: the same post-fader signal, kept for whichever
        // audio track reads this one as its input on the next callback. One
        // `bool` check for every track nobody is listening to, which is almost
        // all of them.
        capture_track_loopback(runtime, track_index, frames);
    }
    runtime.audio_graph.pass1_source_indices = pass1_indices;

    // ── Pass 2: routing tracks (bus / return / group) ───────────────────
    // Input = the accumulated send receive buffer. Process inserts → fader and
    // sum to the master output. Solo is ignored for routing tracks so soloing
    // a *source* track still lets its send reach the return. Order comes from
    // the precomputed topological sort in `RuntimeAudioGraph`.
    let pass2_indices = std::mem::take(&mut runtime.audio_graph.pass2_routing_indices);
    let mut child_channels_summed = 0usize;
    for &track_index in &pass2_indices {
        if effective_track_muted(&runtime.tracks[track_index], block_beat) {
            continue;
        }
        // VSTi multi-out child strips are the only routing tracks that obey
        // solo: they are the instrument's own channels, not a shared bus. A
        // channel sounds when it is soloed itself (listen to one drum pad in
        // isolation) or when its parent instrument track is soloed (solo the
        // VSTi from the main track and hear all of its channels).
        if runtime.has_solo
            && is_vsti_output_child_track_id(&runtime.tracks[track_index].id)
            && !runtime.tracks[track_index].solo
            && !has_soloed_vsti_output_parent(runtime, track_index)
        {
            continue;
        }
        {
            let track = &mut runtime.tracks[track_index];
            track.block_l[..frames].copy_from_slice(&track.recv_l[..frames]);
            track.block_r[..frames].copy_from_slice(&track.recv_r[..frames]);
        }
        process_track_block(
            runtime,
            track_index,
            frames,
            output,
            channels,
            block_beat,
            transport,
        );
        if let Some(tap) = track_taps
            .as_deref_mut()
            .and_then(|taps| taps.get_mut(track_index))
        {
            for frame in 0..frames {
                tap[frame * 2] = runtime.tracks[track_index].block_l[frame];
                tap[frame * 2 + 1] = runtime.tracks[track_index].block_r[frame];
            }
        }
        // Audio Jam publish tap: this track's post-fader signal, the same one
        // an export would capture. Atomics into a preallocated ring, and one
        // `Option` check for every track that is not being shared.
        publish_track_to_jam(runtime, track_index, frames, jam);
        if is_vsti_output_child_track_id(&runtime.tracks[track_index].id)
            && (runtime.tracks[track_index]
                .meter_peak_l
                .max(runtime.tracks[track_index].meter_peak_r)
                > 0.0001)
        {
            child_channels_summed = child_channels_summed.saturating_add(1);
        }
    }
    runtime.audio_graph.pass2_routing_indices = pass2_indices;

    // ── Master bus: apply master track inserts on the summed output ──
    if let Some(m_idx) = master_index {
        let muted = effective_track_muted(&runtime.tracks[m_idx], block_beat);
        if !muted {
            let master = &mut runtime.tracks[m_idx];
            // Copy summed output into master scratch buffer.
            for i in 0..frames {
                let frame = &output[i * channels..i * channels + channels];
                master.block_l[i] = frame[0];
                master.block_r[i] = frame[1];
            }
            apply_track_chain_block(master, frames, transport);
            // Write back, accumulate master meter, apply preview mode.
            for i in 0..frames {
                let (l, r) =
                    apply_preview_mode(master.block_l[i], master.block_r[i], master.preview_mode);
                master.meter_peak_l = master.meter_peak_l.max(l.abs());
                master.meter_peak_r = master.meter_peak_r.max(r.abs());
                master.meter_sum_sq_l += l * l;
                master.meter_sum_sq_r += r * r;
                let out = &mut output[i * channels..i * channels + channels];
                out[0] = l;
                out[1] = r;
            }
            if let Some(tap) = track_taps.and_then(|taps| taps.get_mut(m_idx)) {
                for frame in 0..frames {
                    tap[frame * 2] = master.block_l[frame];
                    tap[frame * 2 + 1] = master.block_r[frame];
                }
            }
        } else {
            for frame in output.chunks_mut(channels) {
                frame[0] = 0.0;
                frame[1] = 0.0;
            }
        }
    }

    // Final master volume — gain and nothing else. The output stage does not
    // limit or clip: a mix that runs hot leaves here hot, and reads hot on the
    // meter, instead of being quietly reshaped on its way to the device.
    // In realtime the gain ramps across the block so dragging the master fader
    // does not zipper; offline export applies the exact constant gain.
    if runtime.fader_smoothing {
        let start = runtime.smoothed_master_gain;
        let inc = (master_volume - start) / frames as f32;
        for i in 0..frames {
            let g = start + inc * i as f32;
            let out = &mut output[i * channels..i * channels + channels];
            out[0] *= g;
            out[1] *= g;
        }
        runtime.smoothed_master_gain = master_volume;
    } else {
        runtime.smoothed_master_gain = master_volume;
        for i in 0..frames {
            let out = &mut output[i * channels..i * channels + channels];
            out[0] *= master_volume;
            out[1] *= master_volume;
        }
    }
    // Audio Jam multitrack tap. Last, so every shared track — source, bus and
    // master alike — has its post-fader block for this instant, and the whole
    // stream advances on one head.
    publish_multitrack_to_jam(runtime, frames, jam);

    if crate::forensic_trace::forensic_trace_enabled() {
        static MASTER_INPUT_LOG_SEQ: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let process_seq = MASTER_INPUT_LOG_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if process_seq.is_multiple_of(30) {
            let mut peak_l = 0.0f32;
            let mut peak_r = 0.0f32;
            for i in 0..frames {
                let frame = &output[i * channels..i * channels + channels];
                peak_l = peak_l.max(frame[0].abs());
                peak_r = peak_r.max(frame[1].abs());
            }
            eprintln!(
                "[MASTER INPUT]\nprocess_seq={process_seq}\nnum_child_channels_summed={child_channels_summed}\nmaster_peak_l={peak_l:.6}\nmaster_peak_r={peak_r:.6}\naudio_device_write_peak_l={peak_l:.6}\naudio_device_write_peak_r={peak_r:.6}"
            );
            if child_channels_summed > 0 && peak_l.max(peak_r) <= 0.0001 {
                eprintln!(
                    "[ROUTING ERROR]\nprocess_seq={process_seq}\nreason=child mixer channels had signal but master/audio device output is zero"
                );
            }
        }
    }

    frames as u64
}

/// Schedule one device callback's MIDI events, splitting only the scheduler
/// range at loop boundaries. The graph/bridge render still runs once for the
/// full device block; offsets are absolute within that callback.
///
/// Returns `Some(loop_start)` when the block ended exactly on a loop boundary
/// and the caller should reset MIDI after rendering so the next callback starts
/// from the loop start.
pub fn schedule_midi_render_block(
    runtime: &mut RuntimeProject,
    base_sample: u64,
    frames: u64,
    loop_bounds: Option<crate::transport::LoopBounds>,
) -> Option<u64> {
    if frames == 0 {
        return None;
    }
    let mut segment_sample = crate::transport::normalize_loop_position(base_sample, loop_bounds);
    let mut remaining = frames;
    let mut callback_offset = 0u64;
    let mut end_reset = None;
    while remaining > 0 {
        let segment_frames = crate::transport::segment_frames_until_loop_wrap(
            segment_sample,
            remaining,
            loop_bounds,
        );
        runtime.schedule_midi_block_with_offset(
            segment_sample,
            segment_frames,
            callback_offset.min(u32::MAX as u64) as u32,
        );
        callback_offset = callback_offset.saturating_add(segment_frames);
        remaining -= segment_frames;
        let (next_sample, wrapped) =
            crate::transport::advance_loop_position(segment_sample, segment_frames, loop_bounds);
        if wrapped {
            if remaining > 0 {
                runtime.reset_midi_playback_with_offset(
                    next_sample,
                    callback_offset.min(u32::MAX as u64) as u32,
                );
            } else {
                end_reset = Some(next_sample);
            }
        }
        segment_sample = next_sample;
    }
    end_reset
}

#[inline]
pub fn is_master_output(output: &str) -> bool {
    output.is_empty() || output == "master" || output == "none"
}

#[inline]
pub fn apply_track_chain_at_beat(
    mut l: f32,
    mut r: f32,
    track: &mut RuntimeTrack,
    beat: f64,
) -> (f32, f32) {
    if !track.inserts.is_empty() && !track.callback_insert_log_done {
        track.callback_insert_log_done = true;
        if callback_debug_enabled() {
            eprintln!(
                "[SphereAudio callback] track={} inserts={}",
                track.id,
                track.inserts.len()
            );
        }
    }
    for insert in &mut track.inserts {
        let processed = apply_insert(l, r, insert);
        l = processed.0;
        r = processed.1;
    }
    let automation = track.automation_values_at_beat(beat);
    let volume = automation.volume.unwrap_or(track.volume);
    let pan = automation.pan.unwrap_or(track.pan);
    let (pan_l, pan_r) = pan_gains(pan);
    (l * volume * pan_l, r * volume * pan_r)
}

/// Evaluate this track's resolved plugin-parameter automation lanes at `beat`
/// and push changed normalized values to the matching inserts.
///
/// Realtime-safe: `plugin_param_automation` is pre-resolved off the audio
/// thread, so this does no allocation, no string parsing, and no id lookups.
/// Each binding holds the last value it emitted and only pushes on change, so
/// the lock-free param ring is not flooded with identical values every block.
/// Bridged inserts use the wait-free `push_param` ring; in-process VST3 inserts
/// queue the value via `set_param`.
#[inline]
fn apply_plugin_param_automation(track: &mut RuntimeTrack, beat: f64) {
    if track.plugin_param_automation.is_empty() {
        return;
    }
    // Disjoint field borrows: bindings (mut) + lanes (read) + inserts (mut).
    let crate::runtime::RuntimeTrack {
        automation_lanes,
        inserts,
        plugin_param_automation,
        ..
    } = track;
    for binding in plugin_param_automation.iter_mut() {
        let Some(lane) = automation_lanes.get(binding.lane_ix) else {
            continue;
        };
        let Some(value) = lane.evaluate_normalized(beat) else {
            continue;
        };
        let value = value.clamp(0.0, 1.0);
        if (value - binding.last_value).abs() <= crate::runtime::PLUGIN_PARAM_AUTOMATION_EPS {
            continue;
        }
        binding.last_value = value;
        let Some(insert) = inserts.get_mut(binding.insert_ix) else {
            continue;
        };
        if !insert.enabled {
            continue;
        }
        match insert.kind_tag {
            crate::runtime::RuntimeInsertKind::ExternalBridge => {
                if let Some(sink) = insert.bridge_sink.as_ref() {
                    sink.push_param(binding.param_id, value, 0);
                }
            }
            _ => {
                if let Some(vst3) = insert.vst3.as_mut() {
                    vst3.set_param(binding.param_id, value as f64);
                }
            }
        }
    }
}

/// Mix this track's ARA playback renderers into `block_*`.
///
/// An ARA renderer generates the audio for the playback regions assigned to it,
/// reading the source samples out of band through the host callbacks in
/// `SphereAraHost`. It is therefore fed silence rather than the track signal,
/// and the clips it owns were skipped by the clip loop.
///
/// Realtime rules: the renderer list and its buffers are built on the control
/// thread; this only fills, calls, and sums. Buffers grow once to the largest
/// block seen, exactly like `block_*` and `recv_*` above.
fn render_ara_block(track: &mut RuntimeTrack, frames: usize, transport: RuntimeTransportContext) {
    if frames == 0 || track.ara_renderers.is_empty() {
        return;
    }
    if track.ara_l.len() < frames {
        track.ara_l.resize(frames, 0.0);
        track.ara_r.resize(frames, 0.0);
        track.ara_silence.resize(frames, 0.0);
    }
    // The silence buffer is only ever read, so it is filled once here rather
    // than trusted to stay zero after a plug-in wrote through its input pointer.
    track.ara_silence[..frames].fill(0.0);

    for index in 0..track.ara_renderers.len() {
        // Split the borrow: the renderer is mutated while the track's own
        // buffers are written.
        let (renderers, out_l, out_r, silence, block_l, block_r) = (
            &mut track.ara_renderers,
            &mut track.ara_l,
            &mut track.ara_r,
            &track.ara_silence,
            &mut track.block_l,
            &mut track.block_r,
        );
        let renderer = &mut renderers[index];
        if !renderer.processor.is_processor_valid() {
            continue;
        }
        out_l[..frames].fill(0.0);
        out_r[..frames].fill(0.0);
        // ARA rendering is transport-driven: the plug-in maps the process
        // context's project position onto its playback regions, so the context
        // has to be published before every block, not only while playing.
        renderer.processor.set_process_context(&transport);
        if !renderer.processor.process_stereo_block(
            &silence[..frames],
            &silence[..frames],
            &mut out_l[..frames],
            &mut out_r[..frames],
        ) {
            continue;
        }
        for frame in 0..frames {
            block_l[frame] += out_l[frame];
            block_r[frame] += out_r[frame];
        }
    }
}

fn render_soundfont_instrument_block(track: &mut RuntimeTrack, frames: usize) {
    if frames == 0
        || track
            .soundfont_player
            .as_ref()
            .and_then(|soundfont| soundfont.player.as_ref())
            .is_none()
    {
        // A track with no player sounds nothing; leaving a stale count here
        // would keep the meter lit after the instrument was removed.
        track.active_voices = 0;
        return;
    }

    let mut cursor = 0usize;
    for index in 0..track.midi_block_events.len() {
        let event = track.midi_block_events[index];
        let offset = (event.sample_offset as usize).min(frames);
        if offset > cursor {
            render_soundfont_segment(track, cursor, offset);
            cursor = offset;
        }
        if let Some(player) = track
            .soundfont_player
            .as_mut()
            .and_then(|soundfont| soundfont.player.as_mut())
        {
            match event.kind {
                1 => {
                    let velocity = (event.velocity.clamp(0.0, 1.0) * 127.0).round() as u8;
                    let _ = player.note_on(
                        event.channel.min(15),
                        event.pitch.min(127),
                        velocity.max(1),
                    );
                }
                0 => {
                    let _ = player.note_off(event.channel.min(15), event.pitch.min(127));
                }
                2 => {
                    // `pitch` carries the controller number here, including the
                    // engine's out-of-band numbers for pitch bend and channel
                    // pressure — clamping it to 127 would turn a bend lane into
                    // a random CC, so the player does the translation.
                    let value = (event.velocity.clamp(0.0, 1.0) * 127.0).round() as u8;
                    let _ = player.controller(event.channel.min(15), event.pitch, value);
                }
                _ => {}
            }
        }
    }
    if cursor < frames {
        render_soundfont_segment(track, cursor, frames);
    }
    // Read after the last segment so the count describes the block that was
    // just produced. One field read on the synth — no allocation, no lock.
    track.active_voices = track
        .soundfont_player
        .as_ref()
        .and_then(|soundfont| soundfont.player.as_ref())
        .map(|player| player.active_voice_count() as u32)
        .unwrap_or(0);
}

fn render_soundfont_segment(track: &mut RuntimeTrack, start: usize, end: usize) {
    if end <= start {
        return;
    }
    let Some(soundfont) = track.soundfont_player.as_mut() else {
        return;
    };
    let Some(player) = soundfont.player.as_mut() else {
        return;
    };
    let len = end - start;
    track.soundfont_l[..len].fill(0.0);
    track.soundfont_r[..len].fill(0.0);
    if player
        .render(&mut track.soundfont_l[..len], &mut track.soundfont_r[..len])
        .is_err()
    {
        return;
    }
    for i in 0..len {
        track.block_l[start + i] += track.soundfont_l[i];
        track.block_r[start + i] += track.soundfont_r[i];
    }
}

fn render_solfege_instrument_block(track: &mut RuntimeTrack, frames: usize) {
    if frames == 0 || track.solfege_engine.is_none() {
        return;
    }
    // Two ordered streams — note/controller events and continuous-pitch
    // targets — merged into one forward walk so the block is split at every
    // event, in time order, whichever list it came from. Both lists are already
    // sorted by sample offset (the scheduler emits them from one sorted event
    // list), so this is a merge, not a sort: no allocation, no comparison
    // sort, nothing that would be unsafe in the audio callback.
    let midi_count = track.midi_block_events.len();
    let pitch_count = track.solfege_pitch_events.len();
    let articulation_count = track.solfege_articulation_events.len();
    let mut midi_ix = 0usize;
    let mut pitch_ix = 0usize;
    let mut articulation_ix = 0usize;
    let mut cursor = 0usize;
    while midi_ix < midi_count || pitch_ix < pitch_count || articulation_ix < articulation_count {
        let midi_at = (midi_ix < midi_count)
            .then(|| (track.midi_block_events[midi_ix].sample_offset as usize).min(frames));
        let pitch_at = (pitch_ix < pitch_count)
            .then(|| (track.solfege_pitch_events[pitch_ix].0 as usize).min(frames));
        let articulation_at = (articulation_ix < articulation_count)
            .then(|| (track.solfege_articulation_events[articulation_ix].0 as usize).min(frames));

        // Tie-breaking at one offset decides whether a note is heard correctly:
        // an articulation chooses the recording, so it must arrive *before* the
        // note-on that resolves it; a pitch target addresses a voice, so it must
        // arrive *after*. Hence articulation, then MIDI, then pitch.
        let earliest = [articulation_at, midi_at, pitch_at]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(frames);
        if earliest > cursor {
            render_solfege_segment(track, cursor, earliest);
            cursor = earliest;
        }

        if articulation_at == Some(earliest) {
            let (_, note_id, articulation) = track.solfege_articulation_events[articulation_ix];
            articulation_ix += 1;
            if let Some(solfege) = track.solfege_engine.as_mut() {
                solfege.handle_articulation_event(note_id, articulation);
            }
        } else if midi_at == Some(earliest) {
            let event = track.midi_block_events[midi_ix];
            midi_ix += 1;
            if let Some(solfege) = track.solfege_engine.as_mut() {
                solfege.handle_midi_event(event);
            }
        } else {
            let (_, note_id, hz) = track.solfege_pitch_events[pitch_ix];
            pitch_ix += 1;
            if let Some(solfege) = track.solfege_engine.as_mut() {
                solfege.handle_pitch_event(note_id, hz);
            }
        }
    }
    if cursor < frames {
        render_solfege_segment(track, cursor, frames);
    }
}

fn render_solfege_segment(track: &mut RuntimeTrack, start: usize, end: usize) {
    if end <= start {
        return;
    }
    let len = end - start;
    // Both scratch planes are preallocated to the callback capacity. Keeping
    // the bank stereo avoids folding phase-opposed source material to mono,
    // which can sound thin or dirty on sustained notes.
    let output_l = &mut track.soundfont_l[..len];
    let output_r = &mut track.soundfont_r[..len];
    output_l.fill(0.0);
    output_r.fill(0.0);
    if let Some(solfege) = track.solfege_engine.as_mut() {
        solfege.render_segment_stereo(output_l, output_r);
    }
    for i in 0..len {
        track.block_l[start + i] += output_l[i];
        track.block_r[start + i] += output_r[i];
    }
}

/// Apply every insert on a channel strip, including external-bridge inserts on
/// the master bus. Each bridge insert uses its build/command-time cached
/// `bridge_sink` (no per-block `HashMap<String, _>` lookup).
pub fn apply_track_chain_block(
    track: &mut RuntimeTrack,
    frames: usize,
    transport: RuntimeTransportContext,
) {
    if !track.inserts.is_empty() && !track.callback_insert_log_done {
        track.callback_insert_log_done = true;
        if callback_debug_enabled() {
            eprintln!(
                "[SphereAudio callback] track={} inserts={} blockFrames={}",
                track.id,
                track.inserts.len(),
                frames
            );
        }
    }
    // Sample plugin-parameter automation for this block and push the resolved
    // normalized values to the matching inserts before they process, so the
    // bridged host applies them on the same block it renders. Realtime-safe:
    // pre-resolved bindings, ring push / set_param only, no allocation. Only
    // while playing so manual edits / the plugin editor own the value when the
    // transport is stopped.
    if transport.playing {
        apply_plugin_param_automation(track, transport.ppq_position);
    }

    render_ara_block(track, frames, transport);
    render_soundfont_instrument_block(track, frames);
    render_solfege_instrument_block(track, frames);

    let instrument_ix = track.midi_instrument_insert_ix;
    let midi_events = &track.midi_block_events;
    for (ix, insert) in track.inserts.iter_mut().enumerate() {
        let midi = instrument_ix
            .filter(|&i| i == ix)
            .map(|_| midi_events.as_slice());
        // What this one insert costs, so a plug-in's own editor can say what it
        // is spending. Two clock reads per insert per block, no allocation and
        // no lock -- the same instrument the callback already applies to itself.
        let started = std::time::Instant::now();
        if insert.kind_tag == crate::runtime::RuntimeInsertKind::ExternalBridge {
            // Arc clone (refcount bump only) so the sink can be borrowed
            // alongside the &mut insert.
            let bridge_sink = insert.bridge_sink.clone();
            apply_external_bridge_insert_block(
                &mut track.block_l[..frames],
                &mut track.block_r[..frames],
                insert,
                midi,
                bridge_sink.as_deref(),
                ix,
                transport,
            );
        } else {
            apply_insert_block(
                &mut track.block_l[..frames],
                &mut track.block_r[..frames],
                insert,
                midi,
                transport,
            );
        }
        publish_insert_cpu(insert, started.elapsed());
    }
}

/// Fold one insert's block time into its smoothed meter.
///
/// A single block is far too jumpy to read -- a plug-in that allocates on one
/// block and coasts on the next would swing the readout wildly -- so this is an
/// exponential average with a short attack. Store-only from the audio thread;
/// the control side reads the same atomic.
#[inline]
fn publish_insert_cpu(insert: &crate::runtime::RuntimeInsert, elapsed: std::time::Duration) {
    use std::sync::atomic::Ordering;
    const SMOOTHING: f32 = 0.15;
    let micros = elapsed.as_micros().min(u32::MAX as u128) as f32;
    let previous = insert.cpu_us.load(Ordering::Relaxed) as f32;
    let smoothed = previous + (micros - previous) * SMOOTHING;
    insert
        .cpu_us
        .store(smoothed.round().max(0.0) as u32, Ordering::Relaxed);
}

fn push_vst3_midi_to_sink(
    sink: &dyn crate::plugin_bridge::PluginBridgeSink,
    events: &[crate::vst3_processor::Vst3MidiEvent],
    instance_id: &str,
) {
    let verbose = crate::runtime::midi_verbose_enabled();
    for ev in events {
        crate::runtime::push_vst3_midi_event_to_sink(sink, ev, instance_id, verbose);
    }
}

/// Apply a bridged insert's freshly-read output to the track block, honoring the
/// realtime **bypass policy** when the host produced no fresh block (`got == 0`,
/// e.g. its service thread is stalled behind a plugin load or editor open):
///
/// * an **effect** replaces the dry block only when the host delivered a **full**
///   block (`got == frames`). A partial wet|dry splice clicks and reads as a
///   stutter / doubled image in both live playback and offline WAV/MP3 bounce —
///   so a short read is treated like a miss and dry passes through;
/// * an **instrument** adds only the `0..got` frames it actually received, so a
///   not-ready instrument contributes silence.
///
/// Returns the `(L, R)` output peaks for diagnostics. Wait-free: only slice
/// copies/adds over `got` frames, no allocation, no locks — safe to call from
/// the audio callback. `got` must be `<=` every slice length (the caller sizes
/// `scratch` to `frames` and reads at most `frames`).
#[inline]
pub(crate) fn apply_bridge_insert_output(
    is_effect: bool,
    got: usize,
    block_l: &mut [f32],
    block_r: &mut [f32],
    scratch_l: &[f32],
    scratch_r: &[f32],
) -> (f32, f32) {
    let need_peaks = crate::forensic_trace::engine_midi_verbose_enabled()
        || plugin_restore_debug_enabled()
        || crate::runtime::midi_verbose_enabled();
    let mut peak_l = 0.0f32;
    let mut peak_r = 0.0f32;
    let frames = block_l.len().min(block_r.len());
    // Effects: all-or-nothing. Partial replace left a wet head and dry tail.
    let effect_frames = if is_effect && got >= frames && frames > 0 {
        frames
    } else {
        0
    };
    if is_effect && effect_frames > 0 {
        block_l[..effect_frames].copy_from_slice(&scratch_l[..effect_frames]);
        block_r[..effect_frames].copy_from_slice(&scratch_r[..effect_frames]);
        if need_peaks {
            peak_l = scratch_l[..effect_frames]
                .iter()
                .fold(0.0f32, |p, s| p.max(s.abs()));
            peak_r = scratch_r[..effect_frames]
                .iter()
                .fold(0.0f32, |p, s| p.max(s.abs()));
        }
    } else if !is_effect {
        let n = got.min(frames);
        for i in 0..n {
            block_l[i] += scratch_l[i];
            block_r[i] += scratch_r[i];
            if need_peaks {
                peak_l = peak_l.max(scratch_l[i].abs());
                peak_r = peak_r.max(scratch_r[i].abs());
            }
        }
    }
    (peak_l, peak_r)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_external_bridge_insert_block(
    block_l: &mut [f32],
    block_r: &mut [f32],
    insert: &mut RuntimeInsert,
    midi_events: Option<&[crate::vst3_processor::Vst3MidiEvent]>,
    bridge_sink: Option<&dyn crate::plugin_bridge::PluginBridgeSink>,
    slot_index: usize,
    transport: RuntimeTransportContext,
) {
    let frames = block_l.len().min(block_r.len());
    if frames == 0 || !insert.enabled {
        return;
    }
    let Some(sink) = bridge_sink else {
        if plugin_restore_debug_enabled() && insert.bridge_missed_blocks == 0 {
            eprintln!(
                "[AudioGraph] processing insert skipped instance={} reason=no_bridge_sink",
                insert.id
            );
        }
        return;
    };
    if plugin_restore_debug_enabled() && insert.bridge_missed_blocks == 0 {
        let input_peak = block_l[..frames]
            .iter()
            .chain(block_r[..frames].iter())
            .fold(0.0f32, |p, s| p.max(s.abs()));
        eprintln!(
            "[BridgeProcess] track=<chain> slot={slot_index} instance={} input_peak={input_peak:.6}",
            insert.id
        );
    }

    // Clip MIDI for bridged plugins is pushed in schedule_midi_block. Preview
    // MIDI is pushed in drain_commands. Non-bridge inserts still use midi_block_events.
    if let Some(events) = midi_events.filter(|e| !e.is_empty()) {
        let verbose = crate::runtime::midi_verbose_enabled();
        if verbose {
            eprintln!(
                "[plugin-dsp-midi-write] instance={} events={}",
                insert.id,
                events.len()
            );
        }
        push_vst3_midi_to_sink(sink, events, &insert.id);
    }

    // `params["role"]` resolved at build time — no params-map read per block.
    let is_effect = insert.bridge_is_effect;

    if insert.scratch_l.len() < frames || insert.scratch_r.len() < frames {
        // Scratches are pre-sized at graph build / `resolve_bridge_sinks`.
        // Growing here would allocate on the audio thread — skip this insert.
        return;
    }

    // One-block handshake ownership (critical):
    //   1. read previous wet  — proves the host finished the last cycle and
    //      released `audio_in` (it copies input before publishing `done_seq`);
    //   2. write next dry     — only now is overwriting `audio_in` safe;
    //   3. apply wet to block;
    //   4. request next       — host may read `audio_in` after this.
    // The old write→read→request order raced the host on the single `audio_in`
    // buffer. Offline export almost always lost that race (near-zero gap between
    // request and the next write), tearing wet blocks into stutter / overlap.
    // Live hits the same bug whenever the host overruns the device period.
    let got = if insert.vsti_output_children.is_empty() {
        // Default single-track path (unchanged): fold the selected channels into
        // this track's stereo.
        sink.read_output_for_channels(
            &mut insert.scratch_l[..frames],
            &mut insert.scratch_r[..frames],
            frames,
            &insert.bridge_enabled_output_channels,
        )
    } else {
        // Multi-out path: ONE freshness-guarded read of the whole plugin block
        // into `scratch_multi` (a second sink read would see the guard return
        // 0). Every active VST3 output bus routes to an explicit child mixer
        // track, including bus 0. With explicit children present the parent
        // instrument track does not receive a fallback downmix.
        let channels = (sink.plugin_output_channels() as usize)
            .clamp(1, crate::runtime::MAX_VSTI_OUTPUT_CHANNELS as usize);
        let needed = frames * channels;
        // `resolve_bridge_sinks` reserves `scratch_multi`'s *capacity* on the
        // control thread for every insert with `vsti_output_children`, so
        // `resize` below only ever adjusts `.len()` within that reserved
        // capacity — it does not allocate on the audio thread.
        // `scatter_vsti_output_children` recovers the channel stride from
        // `scratch_multi.len() / frames`, so the resize must still land at
        // exactly `needed`; skip the read entirely (leaving length/capacity
        // untouched) in the otherwise-unreached case where capacity falls
        // short, rather than growing here.
        if insert.scratch_multi.capacity() < needed {
            0
        } else {
            insert.scratch_multi.resize(needed, 0.0);
            let (got, channels) =
                sink.read_output_multichannel(&mut insert.scratch_multi[..needed], frames);
            let _ = channels;
            insert.scratch_l[..got].fill(0.0);
            insert.scratch_r[..got].fill(0.0);
            got
        }
    };

    // A miss can mean the host still owns the previous request's input buffer.
    // In that case bypass this callback but leave the in-flight block untouched;
    // overwriting or publishing a second request would race the host's raw read.
    let can_publish_request = got > 0 || !sink.request_in_flight();
    if is_effect && can_publish_request {
        // Current dry (pre-apply). Host will process this after `request_block`.
        sink.write_input(&block_l[..frames], &block_r[..frames], frames);
    }

    // Multi-out diagnostic: which plugin output channels actually carry signal
    // this block, and which the engine is folding. Tells separate-out silence
    // apart from "the plugin only writes its main bus" without a debugger.
    // `FUTUREBOARD_PLUGIN_BRIDGE_DEBUG=1`, throttled to ~every 256 blocks.
    if bridge_debug_enabled() && got > 0 {
        static MULTIOUT_PEAK_LOG: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        if MULTIOUT_PEAK_LOG
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .is_multiple_of(256)
        {
            let mut ch_peaks = [0.0f32; 32];
            for (i, peak) in ch_peaks.iter_mut().enumerate() {
                *peak = sink.output_channel_peak((i as u8) + 1);
            }
            eprintln!(
                "[Bridge] multiout instance={} enabled_channels={:?} channel_peaks={:?}",
                insert.id, insert.bridge_enabled_output_channels, ch_peaks
            );
        }
    }

    // Missed-deadline accounting: `read_output` returns 0 when the host has
    // not produced a fresh block (its service thread is stalled behind an
    // editor open/close or a plugin load). The block below then bypasses the
    // insert (effect keeps the dry signal, instrument contributes silence) —
    // stale output is never replayed. A few misses are normal on startup and
    // when resuming from pause, so only log once a stall is established.
    const BRIDGE_MISS_LOG_THRESHOLD: u32 = 8;
    if got == 0 {
        insert.bridge_missed_blocks = insert.bridge_missed_blocks.saturating_add(1);
        if plugin_restore_debug_enabled()
            && (insert.bridge_missed_blocks == 1
                || insert.bridge_missed_blocks == BRIDGE_MISS_LOG_THRESHOLD
                || insert.bridge_missed_blocks.is_multiple_of(1024))
        {
            eprintln!(
                "[Bridge] missed/bypass instance_id={} missed_blocks={}",
                insert.id, insert.bridge_missed_blocks
            );
        }
        // Stall accounting stays in `bridge_missed_blocks`; stderr from the
        // audio callback only exists under the bridge debug flag (realtime
        // rules — stdio can block the callback).
        if bridge_debug_enabled()
            && (insert.bridge_missed_blocks == BRIDGE_MISS_LOG_THRESHOLD
                || insert.bridge_missed_blocks.is_multiple_of(1024))
        {
            if is_effect {
                eprintln!(
                    "[AudioEngine] plugin missed deadline; bypassing to dry signal instance={} missed_blocks={}",
                    insert.id, insert.bridge_missed_blocks
                );
            } else {
                eprintln!(
                    "[VSTi] missed bridge block; output silence instance={} missed_blocks={}",
                    insert.id, insert.bridge_missed_blocks
                );
            }
        }
    } else {
        if plugin_restore_debug_enabled() {
            let out_peak = insert.scratch_l[..got]
                .iter()
                .chain(insert.scratch_r[..got].iter())
                .fold(0.0f32, |p, s| p.max(s.abs()));
            eprintln!(
                "[BridgeProcess] track=<chain> slot={slot_index} instance={} fresh output_peak={out_peak:.6} frames={got}",
                insert.id
            );
        }
        if bridge_debug_enabled() && insert.bridge_missed_blocks >= BRIDGE_MISS_LOG_THRESHOLD {
            if is_effect {
                eprintln!(
                    "[AudioEngine] plugin host recovered instance={} missed_blocks={}",
                    insert.id, insert.bridge_missed_blocks
                );
            } else {
                eprintln!(
                    "[VSTi] recovered after missed blocks={} instance={}",
                    insert.bridge_missed_blocks, insert.id
                );
            }
        }
        insert.bridge_missed_blocks = 0;
    }

    let (out_peak_l, out_peak_r) = apply_bridge_insert_output(
        is_effect,
        got,
        block_l,
        block_r,
        &insert.scratch_l,
        &insert.scratch_r,
    );
    if crate::forensic_trace::engine_midi_verbose_enabled()
        && (out_peak_l > 0.0001 || out_peak_r > 0.0001)
    {
        eprintln!(
            "[SphereAudio] external_bridge output_peak_l={:.6} output_peak_r={:.6}",
            out_peak_l, out_peak_r
        );
        eprintln!(
            "[plugin-host-dsp] response_peak_l={:.6} response_peak_r={:.6}",
            out_peak_l, out_peak_r
        );
    }

    if can_publish_request {
        // Publish the real transport ProcessContext for this block before kicking
        // the host, so the bridged plugin sees true tempo/position/playing instead
        // of the old hardcoded stub. Wait-free atomic stores.
        sink.set_transport(&transport);

        // Drive the host DSP handshake: MIDI was already pushed to the shared ring.
        if plugin_restore_debug_enabled() && insert.bridge_missed_blocks == 0 {
            eprintln!(
                "[Bridge] request block instance_id={} frames={frames}",
                insert.id
            );
        }
        sink.request_block(frames as u32);
    }
}

/// Multi-out (Slice 2): after a bridged instrument's chain has run and read the
/// full plugin block into each insert's `scratch_multi`, scatter every child
/// route's channel pair into the destination "Out Ch" track's receive buffer.
/// The dest tracks are routing-style and process in pass 2, so by the time they
/// run their `recv_*` already holds the demuxed pair. No-op (cheap bail) for the
/// overwhelmingly common case of a track with no multi-out children.
///
/// Children read the *raw* plugin output (pre the instrument track's fader), so
/// each child strip is independently mixable — exactly the multi-out contract.
#[inline]
fn track_has_master_route(runtime: &RuntimeProject, track_index: usize) -> bool {
    let mut current = track_index;
    for _ in 0..runtime.tracks.len().saturating_add(1) {
        let Some(track) = runtime.tracks.get(current) else {
            return false;
        };
        let Some(next) = track.output_track_index else {
            return true;
        };
        if next >= runtime.tracks.len() || next == current {
            return true;
        }
        if !is_routing_type(&runtime.tracks[next].track_type) {
            return true;
        }
        current = next;
    }
    false
}

#[inline]
fn add_bus_pair_to_master_output(
    scratch: &[f32],
    scratch_channels: usize,
    ch_l: usize,
    ch_r: usize,
    frames: usize,
    output: &mut [f32],
    output_channels: usize,
) {
    if output_channels < 2 || scratch_channels == 0 {
        return;
    }
    let n = frames
        .min(scratch.len() / scratch_channels)
        .min(output.len() / output_channels);
    for i in 0..n {
        let base = i * scratch_channels;
        let out = &mut output[i * output_channels..i * output_channels + output_channels];
        out[0] += scratch[base + ch_l - 1];
        out[1] += scratch[base + ch_r - 1];
    }
}

pub(crate) fn scatter_vsti_output_children(
    runtime: &mut RuntimeProject,
    source_track_index: usize,
    frames: usize,
    output: &mut [f32],
    output_channels: usize,
) {
    if frames == 0 || source_track_index >= runtime.tracks.len() {
        return;
    }
    // This function runs on the device callback. Production routing must not
    // allocate, format strings, scan peaks, or write stderr. The detailed bus
    // trace remains available behind the explicit forensic flag.
    let trace = crate::forensic_trace::forensic_trace_enabled();
    let insert_count = runtime.tracks[source_track_index].inserts.len();
    for ins in 0..insert_count {
        let child_count = runtime.tracks[source_track_index].inserts[ins]
            .vsti_output_children
            .len();
        if child_count == 0 {
            continue;
        }
        // `scratch_multi` is sized to exactly `frames * channels` by the read
        // above, so the channel stride is recoverable here without extra state.
        let channels = {
            let len = runtime.tracks[source_track_index].inserts[ins]
                .scratch_multi
                .len();
            if len == 0 {
                continue;
            }
            len / frames
        };
        if channels == 0 {
            continue;
        }
        for c in 0..child_count {
            let (dest_idx, ch_l, ch_r, bus_index, channel_count, trace_ids) = {
                let child =
                    &runtime.tracks[source_track_index].inserts[ins].vsti_output_children[c];
                let ch_l = child.channel_l as usize;
                let ch_r = child.channel_r as usize;
                if !(1..=channels).contains(&ch_l) || !(1..=channels).contains(&ch_r) {
                    continue;
                }
                (
                    child.dest_track_index,
                    ch_l,
                    ch_r,
                    child.bus_index,
                    child.channel_count,
                    trace.then(|| {
                        (
                            child.dest_track_id.clone(),
                            runtime.tracks[source_track_index].inserts[ins].id.clone(),
                        )
                    }),
                )
            };
            let scratch_len = runtime.tracks[source_track_index].inserts[ins]
                .scratch_multi
                .len();
            let n = frames.min(scratch_len / channels);
            if n == 0 {
                continue;
            }
            let (source_peak_l, source_peak_r, sum_l, sum_r) = if trace {
                let mut source_peak_l = 0.0f32;
                let mut source_peak_r = 0.0f32;
                let mut sum_l = 0.0f64;
                let mut sum_r = 0.0f64;
                let scratch = &runtime.tracks[source_track_index].inserts[ins].scratch_multi;
                for i in 0..n {
                    let base = i * channels;
                    let l = scratch[base + ch_l - 1];
                    let r = scratch[base + ch_r - 1];
                    source_peak_l = source_peak_l.max(l.abs());
                    source_peak_r = source_peak_r.max(r.abs());
                    sum_l += (l as f64) * (l as f64);
                    sum_r += (r as f64) * (r as f64);
                }
                (source_peak_l, source_peak_r, sum_l, sum_r)
            } else {
                (0.0, 0.0, 0.0, 0.0)
            };
            let nonzero = trace && source_peak_l.max(source_peak_r) > 0.0001;
            let process_seq = if trace {
                static BUS_AUDIO_ROUTE_SEQ: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                BUS_AUDIO_ROUTE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            } else {
                0
            };
            let route_log = trace && (process_seq.is_multiple_of(30) || nonzero);
            let (dest_track_id, insert_id) = trace_ids
                .as_ref()
                .map(|(dest, insert)| (dest.as_str(), insert.as_str()))
                .unwrap_or(("", ""));
            if route_log {
                let rms_l = (sum_l / n as f64).sqrt();
                let rms_r = (sum_r / n as f64).sqrt();
                eprintln!(
                    "[VSTI PROCESS OUT]\nplugin_instance_id={insert_id}\nplugin_name={insert_id}\nprocess_seq={process_seq}\nbus_index={bus_index}\nchannel_count={channel_count}\npeak_l={source_peak_l:.6}\npeak_r={source_peak_r:.6}\nrms_l={rms_l:.6}\nrms_r={rms_r:.6}\nnonzero={nonzero}"
                );
            }
            let Some(dest_idx) = dest_idx else {
                let scratch = &runtime.tracks[source_track_index].inserts[ins].scratch_multi;
                add_bus_pair_to_master_output(
                    scratch,
                    channels,
                    ch_l,
                    ch_r,
                    frames,
                    output,
                    output_channels,
                );
                if trace {
                    eprintln!(
                        "[ROUTING ERROR]\nplugin_instance_id={insert_id}\nbus_index={bus_index}\nreason=destination mixer channel does not exist\ndestination_mixer_channel_id={dest_track_id}\nfallback_downmix=true"
                    );
                }
                if route_log {
                    eprintln!(
                        "[BUS TO MIXER WRITE]\nplugin_instance_id={insert_id}\nbus_index={bus_index}\nsource_peak_l={source_peak_l:.6}\nsource_peak_r={source_peak_r:.6}\ndestination_mixer_channel_id={dest_track_id}\ndestination_exists=false\nsamples_written=false\nwrite_peak_l=0.000000\nwrite_peak_r=0.000000"
                    );
                }
                continue;
            };
            if dest_idx >= runtime.tracks.len() || dest_idx == source_track_index {
                let scratch = &runtime.tracks[source_track_index].inserts[ins].scratch_multi;
                add_bus_pair_to_master_output(
                    scratch,
                    channels,
                    ch_l,
                    ch_r,
                    frames,
                    output,
                    output_channels,
                );
                if trace {
                    eprintln!(
                        "[ROUTING ERROR]\nplugin_instance_id={insert_id}\nbus_index={bus_index}\nreason=stale mixer channel index\ndestination_mixer_channel_id={dest_track_id}\nfallback_downmix=true"
                    );
                }
                continue;
            }
            let route_to_master_exists = track_has_master_route(runtime, dest_idx);
            {
                let (src_track, dst_track) =
                    two_mut(&mut runtime.tracks, source_track_index, dest_idx);
                let scratch = &src_track.inserts[ins].scratch_multi;
                let n = n.min(dst_track.recv_l.len()).min(dst_track.recv_r.len());
                for i in 0..n {
                    let base = i * channels;
                    dst_track.recv_l[i] += scratch[base + ch_l - 1];
                    dst_track.recv_r[i] += scratch[base + ch_r - 1];
                }
            }
            if !route_to_master_exists {
                let scratch = &runtime.tracks[source_track_index].inserts[ins].scratch_multi;
                add_bus_pair_to_master_output(
                    scratch,
                    channels,
                    ch_l,
                    ch_r,
                    frames,
                    output,
                    output_channels,
                );
                if trace {
                    eprintln!(
                        "[ROUTING ERROR]\nplugin_instance_id={insert_id}\nbus_index={bus_index}\nreason=destination mixer channel has no route to master\ndestination_mixer_channel_id={dest_track_id}\nfallback_downmix=true"
                    );
                }
            }
            if route_log {
                let dst_track = &runtime.tracks[dest_idx];
                let n = n.min(dst_track.recv_l.len()).min(dst_track.recv_r.len());
                let write_peak_l = dst_track.recv_l[..n]
                    .iter()
                    .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
                let write_peak_r = dst_track.recv_r[..n]
                    .iter()
                    .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
                let mute = dst_track.muted;
                let solo = dst_track.solo;
                let gain = dst_track.volume;
                eprintln!(
                    "[BUS TO MIXER WRITE]\nplugin_instance_id={insert_id}\nbus_index={bus_index}\nsource_peak_l={source_peak_l:.6}\nsource_peak_r={source_peak_r:.6}\ndestination_mixer_channel_id={dest_track_id}\ndestination_exists=true\nsamples_written={}\nwrite_peak_l={write_peak_l:.6}\nwrite_peak_r={write_peak_r:.6}",
                    write_peak_l.max(write_peak_r) > 0.0001
                );
                eprintln!(
                    "[MIXER CHANNEL AFTER STRIP]\nmixer_channel_id={dest_track_id}\nroute_node_id={dest_track_id}\nbus_index={bus_index}\nmute={mute}\nsolo={solo}\ngain={gain:.6}\npost_strip_peak_l={write_peak_l:.6}\npost_strip_peak_r={write_peak_r:.6}\nroute_to_master_exists={route_to_master_exists}"
                );
            }
            if route_log && nonzero {
                let rms_l = (sum_l / n as f64).sqrt();
                let rms_r = (sum_r / n as f64).sqrt();
                eprintln!(
                    "[BUS AUDIO ROUTE]\nprocess_seq={process_seq}\nplugin_instance_id={insert_id}\nbus_index={bus_index}\nsource_processdata_output_index={bus_index}\nsource_channel_count={channel_count}\ndestination_mixer_channel_id={dest_track_id}\npeak_l={source_peak_l:.6}\npeak_r={source_peak_r:.6}\nrms_l={rms_l:.6}\nrms_r={rms_r:.6}\ncopied_to_parent_track=false\nfallback_downmix={}",
                    !route_to_master_exists
                );
            }
        }
    }
}

#[inline]
pub fn apply_preview_mode(l: f32, r: f32, mode: RuntimePreviewMode) -> (f32, f32) {
    match mode {
        RuntimePreviewMode::Stereo => (l, r),
        RuntimePreviewMode::Mono | RuntimePreviewMode::Mid => {
            let m = (l + r) * 0.5;
            (m, m)
        }
        RuntimePreviewMode::Side => {
            let s = (l - r) * 0.5;
            (s, s)
        }
    }
}

#[inline]
pub fn apply_insert(l: f32, r: f32, insert: &mut RuntimeInsert) -> (f32, f32) {
    if insert.kind_tag == crate::runtime::RuntimeInsertKind::NativePlugin {
        if !insert.enabled {
            if !insert.callback_process_log_done {
                insert.callback_process_log_done = true;
                // params lookup only inside the once-per-insert log branch.
                let format = insert
                    .params
                    .get("format")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                eprintln!(
                    "[SphereAudio callback] insert={} format={} bypass=true beforePeakL={:.6} beforePeakR={:.6} afterPeakL={:.6} afterPeakR={:.6}",
                    insert.id,
                    format,
                    l.abs(),
                    r.abs(),
                    l.abs(),
                    r.abs()
                );
            }
            return (l, r);
        }
        if let Some(vst3) = insert.vst3.as_mut() {
            let processed = vst3.process_stereo_sample(l, r);
            let (out_l, out_r) = processed.unwrap_or((l, r));
            if !insert.callback_process_log_done {
                insert.callback_process_log_done = true;
                let format = insert
                    .params
                    .get("format")
                    .and_then(|value| value.as_str())
                    .unwrap_or("");
                eprintln!(
                    "[SphereAudio callback] insert={} format={} processorHandle=0x{:x} bypass=false processOk={} beforePeakL={:.6} beforePeakR={:.6} afterPeakL={:.6} afterPeakR={:.6}",
                    insert.id,
                    format,
                    vst3.handle_value(),
                    processed.is_some(),
                    l.abs(),
                    r.abs(),
                    out_l.abs(),
                    out_r.abs()
                );
            }
            return (out_l, out_r);
        }
        if !insert.callback_process_log_done {
            insert.callback_process_log_done = true;
            let format = insert
                .params
                .get("format")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            eprintln!(
                "[SphereAudio callback] insert={} format={} processorHandle=0x0 bypass=false processOk=false beforePeakL={:.6} beforePeakR={:.6} afterPeakL={:.6} afterPeakR={:.6}",
                insert.id,
                format,
                l.abs(),
                r.abs(),
                l.abs(),
                r.abs()
            );
        }
        return (l, r);
    }

    let plugin_id = canonical_plugin_id(&insert.kind);
    process_stereo_sample(
        plugin_id,
        insert.enabled,
        &insert.params,
        &mut insert.dsp,
        l,
        r,
    )
}

pub fn apply_insert_block(
    block_l: &mut [f32],
    block_r: &mut [f32],
    insert: &mut RuntimeInsert,
    midi_events: Option<&[crate::vst3_processor::Vst3MidiEvent]>,
    transport: RuntimeTransportContext,
) {
    if block_l.is_empty() || block_r.is_empty() {
        return;
    }
    if insert.kind_tag != crate::runtime::RuntimeInsertKind::NativePlugin {
        let plugin_id = canonical_plugin_id(&insert.kind);
        insert.dsp.refresh_process_params(plugin_id, &insert.params);
        for i in 0..block_l.len().min(block_r.len()) {
            let (l, r) = apply_insert(block_l[i], block_r[i], insert);
            block_l[i] = l;
            block_r[i] = r;
        }
        return;
    }

    // Diagnostic-only: peak folds feed the once-per-insert process log and the
    // silent-block counter; skipped entirely once that log has fired so the
    // steady-state path stays branch + DSP only. The params "format" lookup
    // happens only inside the log branches.
    let diag = !insert.callback_process_log_done;
    let (before_peak_l, before_peak_r) = if diag {
        (
            block_l
                .iter()
                .fold(0.0f32, |peak, sample| peak.max(sample.abs())),
            block_r
                .iter()
                .fold(0.0f32, |peak, sample| peak.max(sample.abs())),
        )
    } else {
        (0.0, 0.0)
    };

    if !insert.enabled {
        if !insert.callback_process_log_done {
            insert.callback_process_log_done = true;
            let format = insert
                .params
                .get("format")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            eprintln!(
                "[SphereAudio callback] insert={} format={} bypass=true blockFrames={} beforePeakL={:.6} beforePeakR={:.6} afterPeakL={:.6} afterPeakR={:.6}",
                insert.id,
                format,
                block_l.len().min(block_r.len()),
                before_peak_l,
                before_peak_r,
                before_peak_l,
                before_peak_r
            );
        }
        return;
    }

    let Some(vst3) = insert.vst3.as_mut() else {
        if !insert.callback_process_log_done {
            insert.callback_process_log_done = true;
            let format = insert
                .params
                .get("format")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            eprintln!(
                "[SphereAudio callback] insert={} format={} processorHandle=0x0 bypass=false processOk=false blockFrames={} beforePeakL={:.6} beforePeakR={:.6} afterPeakL={:.6} afterPeakR={:.6}",
                insert.id,
                format,
                block_l.len().min(block_r.len()),
                before_peak_l,
                before_peak_r,
                before_peak_l,
                before_peak_r
            );
        }
        return;
    };

    // Guard: if the underlying C++ processor was destroyed (e.g., Arc dropped
    // on another thread racing with this callback), bypass and log once.
    if !vst3.is_processor_valid() {
        if !insert.callback_process_log_done {
            insert.callback_process_log_done = true;
            let format = insert
                .params
                .get("format")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            eprintln!(
                "[SphereAudio callback] insert={} format={} processorHandle=0x{:x} INVALID/DESTROYED bypass=true — insert bypassed to prevent use-after-free",
                insert.id,
                format,
                vst3.handle_value()
            );
        }
        return;
    }

    let frames = block_l.len().min(block_r.len());
    if insert.scratch_l.len() < frames {
        insert.scratch_l.resize(frames, 0.0);
        insert.scratch_r.resize(frames, 0.0);
    }
    insert.scratch_l[..frames].fill(0.0);
    insert.scratch_r[..frames].fill(0.0);

    // Real transport ProcessContext for this block, immediately before the
    // plugin processes it (same thread, no race with process()).
    vst3.set_process_context(&transport);

    let handle = vst3.handle_value();
    let process_ok = if let Some(events) = midi_events.filter(|e| !e.is_empty()) {
        vst3.process_stereo_block_with_midi(
            &block_l[..frames],
            &block_r[..frames],
            &mut insert.scratch_l[..frames],
            &mut insert.scratch_r[..frames],
            events,
        )
    } else {
        vst3.process_stereo_block(
            &block_l[..frames],
            &block_r[..frames],
            &mut insert.scratch_l[..frames],
            &mut insert.scratch_r[..frames],
        )
    };
    if process_ok {
        block_l[..frames].copy_from_slice(&insert.scratch_l[..frames]);
        block_r[..frames].copy_from_slice(&insert.scratch_r[..frames]);
    }

    if diag && before_peak_l <= 0.000001 && before_peak_r <= 0.000001 {
        insert.silent_process_blocks = insert.silent_process_blocks.saturating_add(1);
    }

    if diag
        && (before_peak_l > 0.000001
            || before_peak_r > 0.000001
            || insert.silent_process_blocks >= 200)
    {
        insert.callback_process_log_done = true;
        let format = insert
            .params
            .get("format")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let after_peak_l = block_l[..frames]
            .iter()
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
        let after_peak_r = block_r[..frames]
            .iter()
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
        eprintln!(
            "[SphereAudio callback] insert={} format={} processorHandle=0x{:x} bypass=false processOk={} blockFrames={} silentBlocks={} beforePeakL={:.6} beforePeakR={:.6} afterPeakL={:.6} afterPeakR={:.6}",
            insert.id,
            format,
            handle,
            process_ok,
            frames,
            insert.silent_process_blocks,
            before_peak_l,
            before_peak_r,
            after_peak_l,
            after_peak_r
        );
    }
}

/// Feed one track's post-fader block to its Audio Jam publish slot.
///
/// Realtime-safe by construction: no allocation, no lock, no key lookup — the
/// slot index was resolved on the control thread and shipped as a command.
#[inline]
fn publish_track_to_jam(
    runtime: &RuntimeProject,
    track_index: usize,
    frames: usize,
    jam: Option<&crate::jam_bus::JamAudioBus>,
) {
    let Some(bus) = jam else {
        return;
    };
    let Some(track) = runtime.tracks.get(track_index) else {
        return;
    };
    let Some(slot) = track.jam_publish_slot else {
        return;
    };
    let Some(slot) = bus.publish(slot as usize) else {
        return;
    };
    slot.write_planar(&track.block_l, &track.block_r, frames, runtime.sample_rate);
}

/// Keep this track's post-fader block for a track that loops it back as input.
///
/// Realtime-safe in the steady state: two `copy_from_slice`s into buffers that
/// were grown on the first block a source published and never shrink. A track
/// nobody reads pays one `bool`.
#[inline]
fn capture_track_loopback(runtime: &mut RuntimeProject, track_index: usize, frames: usize) {
    let Some(track) = runtime.tracks.get_mut(track_index) else {
        return;
    };
    if !track.loopback_publish {
        // Release the buffers of a source nobody reads any more. Done here
        // rather than on the control thread because the control thread does not
        // own the graph the callback is holding, and a `Vec` that is already
        // empty costs nothing to clear.
        if !track.loopback_out_l.is_empty() {
            track.loopback_out_l.clear();
            track.loopback_out_r.clear();
        }
        return;
    }
    if track.loopback_out_l.len() < frames {
        track.loopback_out_l.resize(frames, 0.0);
        track.loopback_out_r.resize(frames, 0.0);
    }
    let frames = frames.min(track.block_l.len()).min(track.block_r.len());
    track.loopback_out_l[..frames].copy_from_slice(&track.block_l[..frames]);
    track.loopback_out_r[..frames].copy_from_slice(&track.block_r[..frames]);
}

/// Assemble one block of the Audio Jam multitrack stream.
///
/// Every shared track has already rendered its post-fader block by the time
/// this runs, so the whole stream is staged and published in one pass at the
/// end of the graph rather than a pair at a time as each track finishes. That
/// ordering is the point: one head advance means one capture base and one
/// sequence for the whole arrangement, which is what lets a receiving Studio
/// drop the take onto its timeline as a single aligned multitrack recording.
///
/// Realtime-safe: bounds-checked indexing and atomic stores into rings that
/// were allocated when the bus was built. Pairs nobody staged are zeroed by
/// [`crate::jam_bus::JamPublishSlot::commit`], so a track that stopped sharing
/// leaves silence rather than a repeating block.
#[inline]
fn publish_multitrack_to_jam(
    runtime: &RuntimeProject,
    frames: usize,
    jam: Option<&crate::jam_bus::JamAudioBus>,
) {
    let Some(bus) = jam else {
        return;
    };
    let Some(slot) = bus.multitrack_publish() else {
        return;
    };
    for track in runtime.tracks.iter() {
        let Some(pair) = track.jam_multitrack_pair else {
            continue;
        };
        slot.stage_pair(pair as usize, &track.block_l, &track.block_r, frames);
    }
    slot.commit(frames, runtime.sample_rate);
}

/// Channel pan law for the track fader stage — see
/// [`crate::dsp::gain::pan_gains`]. One law for the live callback and the
/// offline bounce so an export sits exactly where the mix was heard.
#[inline]
pub fn pan_gains(pan: f32) -> (f32, f32) {
    crate::dsp::gain::pan_gains(pan)
}

#[cfg(test)]
mod jam_input_tests {
    use super::render_project_block_interleaved_with_inputs;
    use crate::jam_bus::{jam_device_id, JamAudioBus};
    use crate::runtime::{RuntimeProject, RuntimeTrackInputSource};
    use crate::types::{
        EngineProjectSnapshot, EngineRoutingSnapshot, EngineTrackInputSourceSnapshot,
        EngineTrackSnapshot,
    };
    use std::collections::HashMap;

    const FRAMES: usize = 64;
    const STREAM: &str = "str_guitar";

    fn track(
        id: &str,
        track_type: &str,
        input: EngineTrackInputSourceSnapshot,
    ) -> EngineTrackSnapshot {
        EngineTrackSnapshot {
            id: id.to_string(),
            track_type: track_type.to_string(),
            volume: 1.0,
            pan: 0.0,
            muted: false,
            solo: false,
            armed: false,
            input_monitor: track_type == "audio",
            input_source: input,
            preview_mode: "stereo".to_string(),
            output_track_id: None,
            inserts: Vec::new(),
            sends: Vec::new(),
            automation_lanes: Vec::new(),
            builtin_soundfont_player: false,
            soundfont_path: None,
            soundfont_preset_bank: None,
            soundfont_preset_patch: None,
            soundfont_volume: 1.0,
            soundfont_reverb_chorus: true,
            soundfont_polyphony: 64,
            soundfont_envelope: Default::default(),
            soundfont_quality: Default::default(),
            solfege_engine: None,
        }
    }

    fn snapshot(input: EngineTrackInputSourceSnapshot) -> EngineProjectSnapshot {
        EngineProjectSnapshot {
            project_id: "jam-test".to_string(),
            project_root: None,
            preferred_input_device: None,
            bpm: 120.0,
            tempo_points: Vec::new(),
            time_signature: [4, 4],
            sample_rate: 48_000,
            tracks: vec![
                track("audio-1", "audio", input),
                track("master", "master", Default::default()),
            ],
            clips: Vec::new(),
            midi_clips: Vec::new(),
            pdc_enabled: true,
            latency_graph_version: 1,
            routing: EngineRoutingSnapshot {
                master_output_device: None,
                sample_rate: 48_000,
                buffer_size: 256,
            },
        }
    }

    fn jam_route() -> EngineTrackInputSourceSnapshot {
        EngineTrackInputSourceSnapshot {
            device_id: Some(jam_device_id(STREAM)),
            channels: vec![0, 1],
        }
    }

    fn build(snapshot: &EngineProjectSnapshot, bus: &JamAudioBus) -> RuntimeProject {
        let mut runtime = RuntimeProject::build(snapshot, 48_000, &mut HashMap::new(), None, true)
            .expect("jam runtime");
        runtime.resolve_jam_inputs(snapshot, bus);
        runtime.resolve_loopback_inputs(snapshot);
        runtime
    }

    fn render(
        runtime: &mut RuntimeProject,
        bus: &JamAudioBus,
        live_input: Option<(&[f32], &[f32])>,
    ) -> (f32, f32) {
        let mut output = [0.0f32; FRAMES * 2];
        render_project_block_interleaved_with_inputs(
            runtime,
            0,
            1.0,
            &mut output,
            2,
            false,
            4,
            4,
            None,
            live_input,
            Some(bus),
        );
        let mut peak_l = 0.0f32;
        let mut peak_r = 0.0f32;
        for frame in output.chunks(2) {
            peak_l = peak_l.max(frame[0].abs());
            peak_r = peak_r.max(frame[1].abs());
        }
        (peak_l, peak_r)
    }

    fn remote_block(left: f32, right: f32) -> Vec<f32> {
        (0..FRAMES).flat_map(|_| [left, right]).collect()
    }

    /// The first milestone in one test: a remote stream lands in the bus and is
    /// audible on a track routed to it.
    #[test]
    fn a_remote_stream_is_heard_on_the_track_routed_to_it() {
        let bus = JamAudioBus::new();
        let snapshot = snapshot(jam_route());
        let mut runtime = build(&snapshot, &bus);

        let slot_index = bus
            .input_slot_for(STREAM)
            .expect("the route claimed a slot");
        assert!(matches!(
            runtime.tracks[0].input_source,
            RuntimeTrackInputSource::Jam { .. }
        ));

        // Silence first: nothing has arrived from the network yet.
        assert_eq!(render(&mut runtime, &bus, None), (0.0, 0.0));

        // One block of remote audio, captured at a session tick of its own.
        bus.input(slot_index).expect("in range").write_interleaved(
            &remote_block(0.5, -0.25),
            2,
            20_000_000,
            48_000,
        );
        bus.input(slot_index).expect("in range").assume_primed();

        let (peak_l, peak_r) = render(&mut runtime, &bus, None);
        assert!((peak_l - 0.5).abs() < 1e-6, "left was {peak_l}");
        assert!((peak_r - 0.25).abs() < 1e-6, "right was {peak_r}");
    }

    /// A jam track must not also pick up whatever is plugged into the audio
    /// interface: they are two different performers.
    #[test]
    fn a_jam_track_does_not_double_with_the_hardware_input() {
        let bus = JamAudioBus::new();
        let snapshot = snapshot(jam_route());
        let mut runtime = build(&snapshot, &bus);
        let slot_index = bus.input_slot_for(STREAM).expect("bound");

        bus.input(slot_index).expect("in range").write_interleaved(
            &remote_block(0.5, 0.5),
            2,
            0,
            48_000,
        );
        bus.input(slot_index).expect("in range").assume_primed();

        let hardware = [0.5f32; FRAMES];
        let (peak_l, _) = render(&mut runtime, &bus, Some((&hardware, &hardware)));
        assert!(
            (peak_l - 0.5).abs() < 1e-6,
            "the hardware input summed into the jam track: {peak_l}"
        );
    }

    /// The inverse: a hardware-routed track is untouched by a jam that happens
    /// to be running.
    #[test]
    fn a_hardware_track_is_unaffected_by_a_live_jam() {
        let bus = JamAudioBus::new();
        let snapshot = snapshot(EngineTrackInputSourceSnapshot {
            device_id: Some("asio:test".to_string()),
            channels: vec![0, 1],
        });
        let mut runtime = build(&snapshot, &bus);
        assert!(!runtime.tracks[0].input_source.is_jam());

        // Another participant's stream is arriving, bound to a slot this track
        // does not reference.
        let slot_index = bus.bind_input("str_other").expect("bound");
        bus.input(slot_index).expect("in range").write_interleaved(
            &remote_block(1.0, 1.0),
            2,
            0,
            48_000,
        );
        bus.input(slot_index).expect("in range").assume_primed();

        let hardware = [0.25f32; FRAMES];
        let (peak_l, _) = render(&mut runtime, &bus, Some((&hardware, &hardware)));
        assert!((peak_l - 0.25).abs() < 1e-6, "left was {peak_l}");
    }

    /// A performer leaving mid-session must not leave the callback reading a
    /// slot that has been handed to somebody else.
    #[test]
    fn a_released_stream_goes_silent_rather_than_playing_the_next_tenant() {
        let bus = JamAudioBus::new();
        let snapshot = snapshot(jam_route());
        let mut runtime = build(&snapshot, &bus);
        let slot_index = bus.input_slot_for(STREAM).expect("bound");

        bus.input(slot_index).expect("in range").write_interleaved(
            &remote_block(1.0, 1.0),
            2,
            0,
            48_000,
        );
        bus.input(slot_index).expect("in range").assume_primed();
        assert!(render(&mut runtime, &bus, None).0 > 0.5);

        bus.release_input(STREAM);
        // The runtime snapshot still points at the old index for one block.
        assert_eq!(render(&mut runtime, &bus, None), (0.0, 0.0));
    }

    /// A jam route with no slot left captures nothing rather than being pointed
    /// at an unrelated stream.
    #[test]
    fn a_route_that_cannot_be_bound_resolves_to_no_input() {
        let bus = JamAudioBus::new();
        for index in 0..crate::jam_bus::MAX_JAM_INPUT_SLOTS {
            bus.bind_input(&format!("filler_{index}"));
        }
        let snapshot = snapshot(jam_route());
        let runtime = build(&snapshot, &bus);
        assert_eq!(
            runtime.tracks[0].input_source,
            RuntimeTrackInputSource::None
        );
    }

    /// The publish half: a track's post-fader block reaches the jam bus slot it
    /// was bound to, and only that one.
    #[test]
    fn a_shared_track_writes_its_post_fader_block_to_its_publish_slot() {
        let bus = JamAudioBus::new();
        let snapshot = snapshot(jam_route());
        let mut runtime = build(&snapshot, &bus);
        let input_slot = bus.input_slot_for(STREAM).expect("bound");

        // Feed the track from the jam input so it has something post-fader.
        bus.input(input_slot).expect("in range").write_interleaved(
            &remote_block(0.5, 0.5),
            2,
            0,
            48_000,
        );
        bus.input(input_slot).expect("in range").assume_primed();

        // Nothing bound yet: the tap must not run.
        render(&mut runtime, &bus, None);
        assert!(bus.publish_slot_for("track:audio-1").is_none());

        let publish_slot = bus.bind_publish("track:audio-1").expect("a slot is free");
        runtime.update_track_jam_publish(0, Some(publish_slot as u32));
        bus.input(input_slot).expect("in range").write_interleaved(
            &remote_block(0.5, 0.5),
            2,
            0,
            48_000,
        );
        bus.input(input_slot).expect("in range").assume_primed();
        render(&mut runtime, &bus, None);

        let mut out = Vec::new();
        let (frames, _channels, _) = bus
            .publish(publish_slot)
            .expect("in range")
            .read_interleaved(&mut out, FRAMES)
            .expect("the tap wrote frames");
        assert_eq!(frames, FRAMES);
        let peak = out
            .iter()
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
        assert!((peak - 0.5).abs() < 1e-6, "the tap captured {peak}");
    }

    /// The multitrack half: every shared track lands in its own channel pair of
    /// one stream, and the whole stream advances on a single head — which is
    /// what makes the arriving take alignable as one recording.
    #[test]
    fn a_multitrack_stream_carries_each_shared_track_in_its_own_pair() {
        let bus = JamAudioBus::new();
        let snapshot = snapshot(jam_route());
        let mut runtime = build(&snapshot, &bus);
        let input_slot = bus.input_slot_for(STREAM).expect("bound");

        bus.bind_multitrack_publish(4)
            .expect("the wide slot is free");
        // Only one track exists in this fixture, so it takes pair 0 and pair 1
        // is left for a track that is not sharing.
        runtime.update_jam_multitrack_pairs(&[0]);

        bus.input(input_slot).expect("in range").write_interleaved(
            &remote_block(0.5, 0.5),
            2,
            0,
            48_000,
        );
        bus.input(input_slot).expect("in range").assume_primed();
        render(&mut runtime, &bus, None);

        let mut out = Vec::new();
        let (frames, channels, _) = bus
            .multitrack_publish()
            .expect("claimed")
            .read_interleaved(&mut out, FRAMES)
            .expect("the tap wrote frames");
        assert_eq!((frames, channels), (FRAMES, 4));

        let peak_first = out
            .chunks(channels)
            .fold(0.0f32, |peak, frame| peak.max(frame[0].abs()));
        let peak_second = out
            .chunks(channels)
            .fold(0.0f32, |peak, frame| peak.max(frame[2].abs()));
        assert!(
            (peak_first - 0.5).abs() < 1e-6,
            "pair 0 captured {peak_first}"
        );
        assert_eq!(
            peak_second, 0.0,
            "an unassigned pair is silence, not the neighbouring track"
        );
    }

    /// Clearing the assignment stops the tap, the same way unsharing one track
    /// does. A stream that kept advancing would fill a ring nobody drains.
    #[test]
    fn clearing_the_multitrack_assignment_stops_the_stream() {
        let bus = JamAudioBus::new();
        let snapshot = snapshot(jam_route());
        let mut runtime = build(&snapshot, &bus);
        let input_slot = bus.input_slot_for(STREAM).expect("bound");
        bus.bind_multitrack_publish(2).expect("free");
        runtime.update_jam_multitrack_pairs(&[0]);

        bus.input(input_slot).expect("in range").write_interleaved(
            &remote_block(0.5, 0.5),
            2,
            0,
            48_000,
        );
        bus.input(input_slot).expect("in range").assume_primed();
        render(&mut runtime, &bus, None);
        let mut out = Vec::new();
        assert!(bus
            .multitrack_publish()
            .expect("claimed")
            .read_interleaved(&mut out, FRAMES)
            .is_some());

        bus.release_publish(crate::jam_bus::PUBLISH_KEY_MULTITRACK);
        runtime.update_jam_multitrack_pairs(&[]);
        render(&mut runtime, &bus, None);
        assert!(
            bus.multitrack_publish().is_none(),
            "the released slot is not reachable, so nothing can be written to it"
        );
    }

    /// Unsharing stops the tap. A slot that keeps filling after a publish ends
    /// would hold audio nobody drains until the ring wrapped.
    #[test]
    fn unsharing_a_track_stops_the_tap() {
        let bus = JamAudioBus::new();
        let snapshot = snapshot(jam_route());
        let mut runtime = build(&snapshot, &bus);
        let input_slot = bus.input_slot_for(STREAM).expect("bound");
        let publish_slot = bus.bind_publish("track:audio-1").expect("a slot is free");
        runtime.update_track_jam_publish(0, Some(publish_slot as u32));

        bus.input(input_slot).expect("in range").write_interleaved(
            &remote_block(0.5, 0.5),
            2,
            0,
            48_000,
        );
        bus.input(input_slot).expect("in range").assume_primed();
        render(&mut runtime, &bus, None);
        let mut out = Vec::new();
        assert!(bus
            .publish(publish_slot)
            .expect("in range")
            .read_interleaved(&mut out, FRAMES)
            .is_some());

        runtime.update_track_jam_publish(0, None);
        bus.input(input_slot).expect("in range").write_interleaved(
            &remote_block(0.5, 0.5),
            2,
            0,
            48_000,
        );
        bus.input(input_slot).expect("in range").assume_primed();
        render(&mut runtime, &bus, None);
        assert!(
            bus.publish(publish_slot)
                .expect("in range")
                .read_interleaved(&mut out, FRAMES)
                .is_none(),
            "the tap kept writing after the publish ended"
        );
    }

    /// An unmonitored track is silent whatever is arriving for it, exactly as a
    /// hardware input would be.
    #[test]
    fn an_unmonitored_jam_track_stays_silent() {
        let bus = JamAudioBus::new();
        let mut snapshot = snapshot(jam_route());
        snapshot.tracks[0].input_monitor = false;
        let mut runtime = build(&snapshot, &bus);
        let slot_index = bus.input_slot_for(STREAM).expect("bound");

        bus.input(slot_index).expect("in range").write_interleaved(
            &remote_block(1.0, 1.0),
            2,
            0,
            48_000,
        );
        bus.input(slot_index).expect("in range").assume_primed();
        assert_eq!(render(&mut runtime, &bus, None), (0.0, 0.0));
    }
}

#[cfg(test)]
mod live_input_monitor_tests {
    use super::{
        render_project_block_interleaved, render_project_block_interleaved_with_live_input,
    };
    use crate::runtime::RuntimeProject;
    use crate::types::{
        EngineInsertSnapshot, EngineProjectSnapshot, EngineRoutingSnapshot,
        EngineTrackInputSourceSnapshot, EngineTrackSnapshot,
    };
    use std::collections::HashMap;

    fn track(id: &str, track_type: &str) -> EngineTrackSnapshot {
        EngineTrackSnapshot {
            id: id.to_string(),
            track_type: track_type.to_string(),
            volume: 1.0,
            pan: 0.0,
            muted: false,
            solo: false,
            armed: false,
            input_monitor: track_type == "audio",
            input_source: if track_type == "audio" {
                EngineTrackInputSourceSnapshot {
                    device_id: Some("asio:test".to_string()),
                    channels: vec![0, 1],
                }
            } else {
                Default::default()
            },
            preview_mode: "stereo".to_string(),
            output_track_id: None,
            inserts: Vec::new(),
            sends: Vec::new(),
            automation_lanes: Vec::new(),
            builtin_soundfont_player: false,
            soundfont_path: None,
            soundfont_preset_bank: None,
            soundfont_preset_patch: None,
            soundfont_volume: 1.0,
            soundfont_reverb_chorus: true,
            soundfont_polyphony: 64,
            soundfont_envelope: Default::default(),
            soundfont_quality: Default::default(),
            solfege_engine: None,
        }
    }

    fn runtime() -> RuntimeProject {
        let snapshot = EngineProjectSnapshot {
            project_id: "monitor-test".to_string(),
            project_root: None,
            preferred_input_device: None,
            bpm: 120.0,
            tempo_points: Vec::new(),
            time_signature: [4, 4],
            sample_rate: 48_000,
            tracks: vec![track("audio-1", "audio"), track("master", "master")],
            clips: Vec::new(),
            midi_clips: Vec::new(),
            pdc_enabled: true,
            latency_graph_version: 1,
            routing: EngineRoutingSnapshot {
                master_output_device: None,
                sample_rate: 48_000,
                buffer_size: 256,
            },
        };
        RuntimeProject::build(&snapshot, 48_000, &mut HashMap::new(), None, true)
            .expect("monitor runtime")
    }

    const FRAMES: usize = 64;

    fn render_monitored_block(runtime: &mut RuntimeProject, base_sample: u64) -> (f32, f32) {
        let input = [0.5f32; FRAMES];
        let mut output = [0.0f32; FRAMES * 2];
        render_project_block_interleaved_with_live_input(
            runtime,
            base_sample,
            1.0,
            &mut output,
            2,
            false,
            4,
            4,
            None,
            &input,
            &input,
        );
        let mut peak_l = 0.0f32;
        let mut peak_r = 0.0f32;
        for frame in output.chunks(2) {
            peak_l = peak_l.max(frame[0].abs());
            peak_r = peak_r.max(frame[1].abs());
        }
        (peak_l, peak_r)
    }

    #[test]
    fn monitored_input_obeys_track_pan_and_mute() {
        let mut runtime = runtime();
        // Exact per-block gains (no first-block ramp) so pan asserts are strict.
        runtime.fader_smoothing = false;
        runtime.tracks[0].pan = -1.0;
        let (left, right) = render_monitored_block(&mut runtime, 0);
        assert!(left > 0.0);
        assert!(right.abs() < 1.0e-6);

        runtime.tracks[0].muted = true;
        let (muted_l, muted_r) = render_monitored_block(&mut runtime, FRAMES as u64);
        assert!(muted_l.abs() < 1.0e-6);
        assert!(muted_r.abs() < 1.0e-6);
    }

    /// The channel fader has to attenuate what actually leaves the engine, and
    /// `SetTrackVolume` (which is all `update_track_volume` is) has to be what
    /// moves it. Reported as "master works, the channel strip does not": the
    /// master gain is an atomic applied to the interleaved output, while a track
    /// fader travels the command queue into `RuntimeTrack::volume` and is applied
    /// in `apply_fader`, so the two halves fail independently.
    #[test]
    fn track_fader_attenuates_the_rendered_block() {
        let mut runtime = runtime();
        // Exact per-block gain, no first-block ramp, so the ratio is strict.
        runtime.fader_smoothing = false;

        let (unity_l, unity_r) = render_monitored_block(&mut runtime, 0);
        assert!(unity_l > 0.1, "test signal must be audible at unity");

        // -11 dB, the level from the bug report.
        let minus_11_db = 10.0f32.powf(-11.0 / 20.0);
        assert!(
            runtime.update_track_volume("audio-1", minus_11_db),
            "the command must find the track by id"
        );
        let (faded_l, faded_r) = render_monitored_block(&mut runtime, FRAMES as u64);

        let expected_l = unity_l * minus_11_db;
        let expected_r = unity_r * minus_11_db;
        assert!(
            (faded_l - expected_l).abs() < 1.0e-4 && (faded_r - expected_r).abs() < 1.0e-4,
            "fader must scale the output: unity=({unity_l:.6}, {unity_r:.6}) \
             faded=({faded_l:.6}, {faded_r:.6}) expected=({expected_l:.6}, {expected_r:.6})"
        );
    }

    #[test]
    fn unmonitored_track_receives_no_live_input() {
        let mut runtime = runtime();
        runtime.fader_smoothing = false;
        runtime.tracks[0].monitor_enabled = false;
        let (left, right) = render_monitored_block(&mut runtime, 0);
        assert!(left.abs() < 1.0e-6);
        assert!(right.abs() < 1.0e-6);
    }

    #[test]
    fn stress_1000_tracks_and_128_bypassed_inserts_render_only_active_source() {
        let mut tracks: Vec<_> = (0..1_000)
            .map(|index| track(&format!("midi-{index}"), "midi"))
            .collect();
        tracks[500].inserts = (0..128)
            .map(|index| EngineInsertSnapshot {
                id: format!("bypassed-{index}"),
                kind: "gain".to_string(),
                enabled: false,
                params: HashMap::new(),
                state: None,
            })
            .collect();
        tracks.push(track("master", "master"));
        let snapshot = EngineProjectSnapshot {
            project_id: "stress-1k-tracks".to_string(),
            project_root: None,
            preferred_input_device: None,
            bpm: 120.0,
            tempo_points: Vec::new(),
            time_signature: [4, 4],
            sample_rate: 48_000,
            tracks,
            clips: Vec::new(),
            midi_clips: Vec::new(),
            pdc_enabled: true,
            latency_graph_version: 1,
            routing: EngineRoutingSnapshot {
                master_output_device: None,
                sample_rate: 48_000,
                buffer_size: 256,
            },
        };
        let mut runtime = RuntimeProject::build(&snapshot, 48_000, &mut HashMap::new(), None, true)
            .expect("large runtime");
        assert_eq!(runtime.tracks.len(), 1_001);
        assert_eq!(
            runtime
                .audio_graph
                .active_source_mask
                .iter()
                .filter(|active| **active)
                .count(),
            1,
            "only the source with an insert chain should touch audio buffers"
        );

        let mut output = [0.0f32; FRAMES * 2];
        render_project_block_interleaved(&mut runtime, 0, 1.0, &mut output, 2, true, 4, 4, None);
        assert!(output.iter().all(|sample| *sample == 0.0));
    }
}

/// End-to-end coverage for the built-in Soundfont Player as a track
/// instrument: a real `.sf2` is loaded through the runtime graph, MIDI reaches
/// it the same way the piano roll and the Soundfont Player window send it, and
/// the rendered block is checked for actual audio.
#[cfg(test)]
mod soundfont_instrument_tests {
    use super::render_project_block_interleaved;
    use crate::runtime::RuntimeProject;
    use crate::types::{
        EngineMidiClipSnapshot, EngineMidiNoteSnapshot, EngineProjectSnapshot,
        EngineRoutingSnapshot, EngineTrackSnapshot,
    };
    use sphere_soundfont_player::{
        test_font, SoundfontEnvelope, SoundfontRenderQuality, DECIMATOR_LATENCY_SAMPLES,
    };
    use std::collections::HashMap;
    use std::path::PathBuf;

    const SAMPLE_RATE: u32 = 48_000;
    const FRAMES: usize = 512;

    /// A per-test `.sf2` on disk, removed when the guard drops.
    struct FontFile {
        dir: PathBuf,
        path: PathBuf,
    }

    impl FontFile {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "futureboard-soundfont-engine-{}-{name}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("temp dir");
            let path = dir.join("test.sf2");
            test_font::write_sf2(&path).expect("write test soundfont");
            Self { dir, path }
        }
    }

    impl Drop for FontFile {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    fn soundfont_track(id: &str, font: &FontFile, preset: (i32, i32)) -> EngineTrackSnapshot {
        EngineTrackSnapshot {
            id: id.to_string(),
            track_type: "instrument".to_string(),
            volume: 1.0,
            pan: 0.0,
            muted: false,
            solo: false,
            armed: false,
            input_monitor: false,
            input_source: Default::default(),
            preview_mode: "stereo".to_string(),
            output_track_id: None,
            inserts: Vec::new(),
            sends: Vec::new(),
            automation_lanes: Vec::new(),
            builtin_soundfont_player: true,
            soundfont_path: Some(font.path.to_string_lossy().into_owned()),
            soundfont_preset_bank: Some(preset.0),
            soundfont_preset_patch: Some(preset.1),
            soundfont_volume: 1.0,
            soundfont_reverb_chorus: true,
            soundfont_polyphony: 64,
            soundfont_envelope: Default::default(),
            soundfont_quality: Default::default(),
            solfege_engine: None,
        }
    }

    fn master_track() -> EngineTrackSnapshot {
        EngineTrackSnapshot {
            id: "master".to_string(),
            track_type: "master".to_string(),
            volume: 1.0,
            pan: 0.0,
            muted: false,
            solo: false,
            armed: false,
            input_monitor: false,
            input_source: Default::default(),
            preview_mode: "stereo".to_string(),
            output_track_id: None,
            inserts: Vec::new(),
            sends: Vec::new(),
            automation_lanes: Vec::new(),
            builtin_soundfont_player: false,
            soundfont_path: None,
            soundfont_preset_bank: None,
            soundfont_preset_patch: None,
            soundfont_volume: 1.0,
            soundfont_reverb_chorus: true,
            soundfont_polyphony: 64,
            soundfont_envelope: Default::default(),
            soundfont_quality: Default::default(),
            solfege_engine: None,
        }
    }

    fn runtime(font: &FontFile, preset: (i32, i32)) -> RuntimeProject {
        runtime_with_shaping(font, preset, Default::default(), Default::default())
    }

    fn runtime_with_shaping(
        font: &FontFile,
        preset: (i32, i32),
        envelope: SoundfontEnvelope,
        quality: SoundfontRenderQuality,
    ) -> RuntimeProject {
        let mut track = soundfont_track("sf-1", font, preset);
        track.soundfont_envelope = envelope;
        track.soundfont_quality = quality;
        build_runtime(vec![track, master_track()])
    }

    fn build_runtime(tracks: Vec<EngineTrackSnapshot>) -> RuntimeProject {
        let snapshot = EngineProjectSnapshot {
            project_id: "soundfont-test".to_string(),
            project_root: None,
            preferred_input_device: None,
            bpm: 120.0,
            tempo_points: Vec::new(),
            time_signature: [4, 4],
            sample_rate: SAMPLE_RATE,
            tracks,
            clips: Vec::new(),
            midi_clips: Vec::new(),
            pdc_enabled: true,
            latency_graph_version: 1,
            routing: EngineRoutingSnapshot {
                master_output_device: None,
                sample_rate: SAMPLE_RATE,
                buffer_size: FRAMES as u32,
            },
        };
        RuntimeProject::build(&snapshot, SAMPLE_RATE, &mut HashMap::new(), None, true)
            .expect("soundfont runtime builds")
    }

    /// Renders one stopped-transport block — the state the Soundfont Player
    /// window's Test button and the piano roll audition run in — and returns
    /// its peak level.
    fn render_peak(runtime: &mut RuntimeProject) -> f32 {
        let mut output = vec![0.0f32; FRAMES * 2];
        render_project_block_interleaved(runtime, 0, 1.0, &mut output, 2, false, 4, 4, None);
        output.iter().fold(0.0f32, |peak, s| peak.max(s.abs()))
    }

    #[test]
    fn snapshot_load_attaches_a_player_with_the_requested_preset() {
        let font = FontFile::new("load");
        let runtime = runtime(&font, test_font::MELODIC_PRESET);
        let soundfont = runtime.tracks[0]
            .soundfont_player
            .as_ref()
            .expect("track carries a soundfont player");
        let player = soundfont.player.as_ref().expect("font loaded");
        assert_eq!(player.bank_name(), test_font::BANK_NAME);
        assert_eq!(soundfont.preset, Some(test_font::MELODIC_PRESET));
    }

    #[test]
    fn preview_note_renders_without_an_instrument_insert() {
        let font = FontFile::new("preview");
        let mut runtime = runtime(&font, test_font::MELODIC_PRESET);
        assert!(
            runtime.tracks[0].midi_instrument_insert_ix.is_none(),
            "the built-in player is a track instrument, not an insert"
        );
        assert_eq!(render_peak(&mut runtime), 0.0, "silent before any note");

        runtime.midi_preview_note_on("sf-1", 0, 60, 100);
        assert!(
            runtime.has_active_midi_preview(),
            "a soundfont preview note must register so the callback keeps rendering"
        );
        assert!(
            render_peak(&mut runtime) > 0.001,
            "preview note should be audible"
        );

        runtime.midi_preview_note_off("sf-1", 0, 60);
        assert!(!runtime.has_active_midi_preview());
    }

    #[test]
    fn a_drum_bank_preset_sounds_for_a_note_on_the_tracks_own_channel() {
        // Regression: choosing a bank-128 kit program-changed only MIDI channel
        // 10, but an instrument track's notes carry channel 1, so the track was
        // silent (or played a leftover melodic preset) no matter what the panel
        // showed. The player now routes the track's notes to the percussion
        // channel that kit lives on.
        let font = FontFile::new("drum-routing");
        let mut runtime = runtime(&font, test_font::DRUM_PRESET);
        let player = runtime.tracks[0]
            .soundfont_player
            .as_ref()
            .and_then(|sf| sf.player.as_ref())
            .expect("font loaded");
        assert_eq!(player.selected_preset(), Some(test_font::DRUM_PRESET));
        assert_eq!(
            player.routed_channel(0),
            sphere_soundfont_player::PERCUSSION_CHANNEL
        );

        runtime.midi_preview_note_on("sf-1", 0, 60, 100);
        assert!(
            render_peak(&mut runtime) > 0.001,
            "a drum kit must sound for a note written on channel 1"
        );
    }

    #[test]
    fn a_melodic_preset_sounds_for_a_note_written_on_channel_ten() {
        let font = FontFile::new("melodic-routing");
        let mut runtime = runtime(&font, test_font::MELODIC_PRESET);
        runtime.midi_preview_note_on("sf-1", sphere_soundfont_player::PERCUSSION_CHANNEL, 60, 100);
        assert!(
            render_peak(&mut runtime) > 0.001,
            "a melodic preset must still play a note that arrives on channel 10"
        );
    }

    #[test]
    fn the_track_envelope_reaches_the_engine_player_and_shapes_playback() {
        let font = FontFile::new("envelope");
        let envelope = SoundfontEnvelope {
            attack_ms: 400.0,
            ..SoundfontEnvelope::default()
        };
        let mut shaped = runtime_with_shaping(
            &font,
            test_font::MELODIC_PRESET,
            envelope,
            SoundfontRenderQuality::Standard,
        );
        assert_eq!(
            shaped.tracks[0]
                .soundfont_player
                .as_ref()
                .and_then(|sf| sf.player.as_ref())
                .expect("font loaded")
                .envelope(),
            envelope,
            "the snapshot's envelope must reach the audible player, not just the window"
        );

        let mut plain = runtime(&font, test_font::MELODIC_PRESET);
        shaped.midi_preview_note_on("sf-1", 0, 60, 100);
        plain.midi_preview_note_on("sf-1", 0, 60, 100);
        // One 512-frame block at 48 kHz is ~11 ms, far inside a 400 ms attack.
        let shaped_peak = render_peak(&mut shaped);
        let plain_peak = render_peak(&mut plain);
        assert!(
            shaped_peak < plain_peak * 0.25,
            "attack should fade playback in: shaped={shaped_peak} plain={plain_peak}"
        );
    }

    #[test]
    fn an_oversampled_quality_still_plays_and_reports_its_latency() {
        let font = FontFile::new("quality");
        let mut runtime = runtime_with_shaping(
            &font,
            test_font::MELODIC_PRESET,
            SoundfontEnvelope::default(),
            SoundfontRenderQuality::High,
        );
        let soundfont = runtime.tracks[0]
            .soundfont_player
            .as_ref()
            .expect("track carries a soundfont player");
        assert_eq!(soundfont.quality, SoundfontRenderQuality::High);
        let player = soundfont.player.as_ref().expect("font loaded");
        assert_eq!(player.sample_rate(), SAMPLE_RATE as i32, "output rate");
        assert_eq!(player.internal_sample_rate(), SAMPLE_RATE as i32 * 2);
        assert_eq!(soundfont.latency_samples(), DECIMATOR_LATENCY_SAMPLES);
        assert_eq!(
            runtime.tracks[0].plugin_latency_samples, DECIMATOR_LATENCY_SAMPLES,
            "the decimation delay has to reach delay compensation"
        );

        runtime.midi_preview_note_on("sf-1", 0, 60, 100);
        // The filter's 16-sample delay lands well inside the first block.
        assert!(
            render_peak(&mut runtime) > 0.001,
            "an oversampled player must still be audible"
        );
    }

    #[test]
    fn standard_quality_adds_no_latency() {
        let font = FontFile::new("quality-standard");
        let runtime = runtime(&font, test_font::MELODIC_PRESET);
        assert_eq!(runtime.tracks[0].plugin_latency_samples, 0);
    }

    #[test]
    fn preview_note_plays_on_a_channel_other_than_the_first() {
        // Tracks can put each note on its own MIDI channel, so the selected
        // preset has to be live on every melodic channel, not only channel 1.
        let font = FontFile::new("channels");
        let mut runtime = runtime(&font, test_font::MELODIC_PRESET);
        runtime.midi_preview_note_on("sf-1", 7, 64, 110);
        assert!(render_peak(&mut runtime) > 0.001);
    }

    #[test]
    fn muted_track_renders_no_soundfont_audio() {
        let font = FontFile::new("muted");
        let mut runtime = runtime(&font, test_font::MELODIC_PRESET);
        runtime.tracks[0].muted = true;
        runtime.fader_smoothing = false;
        runtime.midi_preview_note_on("sf-1", 0, 60, 100);
        assert!(render_peak(&mut runtime) < 1.0e-6);
    }

    /// One block plus the per-callback `midi_block_events` clear the real
    /// backends do after rendering, so a queued preview note is delivered once
    /// rather than re-triggered every block.
    fn render_peak_consuming_events(runtime: &mut RuntimeProject) -> f32 {
        let peak = render_peak(runtime);
        for track in &mut runtime.tracks {
            track.midi_block_events.clear();
        }
        peak
    }

    #[test]
    fn a_muted_instrument_track_keeps_its_voices_running_underneath() {
        // Mute/solo silence the output, not the instrument. Under the old skip
        // the muted track never processed its block, so the note-on was
        // dropped with that block's events and unmuting produced silence until
        // the next note. Now the voice runs (and its attack advances) under the
        // mute, so lifting it lands mid-note.
        let font = FontFile::new("mute-continuity");
        let envelope = SoundfontEnvelope {
            attack_ms: 250.0,
            ..SoundfontEnvelope::default()
        };
        let mut muted = runtime_with_shaping(
            &font,
            test_font::MELODIC_PRESET,
            envelope,
            SoundfontRenderQuality::Standard,
        );
        muted.fader_smoothing = false;
        muted.tracks[0].muted = true;
        muted.midi_preview_note_on("sf-1", 0, 60, 100);

        // ~0.25 s of muted playback: nothing audible, but the note is running.
        let blocks = (SAMPLE_RATE as f32 * 0.25 / FRAMES as f32).ceil() as usize;
        for _ in 0..blocks {
            assert!(
                render_peak_consuming_events(&mut muted) < 1.0e-6,
                "a muted track must stay silent"
            );
        }
        muted.tracks[0].muted = false;
        let resumed = render_peak_consuming_events(&mut muted);
        assert!(resumed > 0.001, "unmuting must resume the sounding note");

        // Same note started fresh at that moment would still be inside its
        // attack, so "resumed mid-note" has to be clearly louder than "restarted".
        let mut restarted = runtime_with_shaping(
            &font,
            test_font::MELODIC_PRESET,
            envelope,
            SoundfontRenderQuality::Standard,
        );
        restarted.fader_smoothing = false;
        restarted.midi_preview_note_on("sf-1", 0, 60, 100);
        let first_block = render_peak_consuming_events(&mut restarted);
        assert!(
            resumed > first_block * 2.0,
            "the muted voice must have advanced its attack: resumed={resumed} restart={first_block}"
        );
    }

    #[test]
    fn a_soloed_track_does_not_stop_another_tracks_notes() {
        let font = FontFile::new("solo-continuity");
        let mut runtime = build_runtime(vec![
            soundfont_track("sf-1", &font, test_font::MELODIC_PRESET),
            soundfont_track("sf-2", &font, test_font::MELODIC_PRESET),
            master_track(),
        ]);
        runtime.fader_smoothing = false;

        // Solo the *other* track, then start a note on this one — exactly what
        // happens when the transport rolls into a new phrase while something
        // else is soloed. The old skip threw the note-on away with the block's
        // events, so releasing solo produced silence until the next note.
        runtime.update_track_solo("sf-2", true);
        runtime.midi_preview_note_on("sf-1", 0, 60, 100);
        assert!(
            render_peak_consuming_events(&mut runtime) < 1.0e-6,
            "an unsoloed track must be silent"
        );

        runtime.update_track_solo("sf-2", false);
        assert!(
            render_peak_consuming_events(&mut runtime) > 0.001,
            "releasing solo must reveal the note that started under it"
        );
    }

    #[test]
    fn graph_clone_reuses_the_parsed_font_and_still_plays() {
        let font = FontFile::new("clone");
        let runtime = runtime(&font, test_font::MELODIC_PRESET);
        let original = runtime.tracks[0]
            .soundfont_player
            .as_ref()
            .and_then(|sf| sf.player.as_ref())
            .expect("font loaded")
            .sound_font();

        let mut cloned = runtime.clone();
        let clone_font = cloned.tracks[0]
            .soundfont_player
            .as_ref()
            .and_then(|sf| sf.player.as_ref())
            .expect("clone keeps a loaded player")
            .sound_font();
        assert!(
            std::sync::Arc::ptr_eq(&original, &clone_font),
            "a graph swap must not re-parse the SoundFont"
        );

        cloned.midi_preview_note_on("sf-1", 0, 60, 100);
        assert!(render_peak(&mut cloned) > 0.001);
    }

    /// Renders a whole bar of transport playback offline and reports the peak
    /// level in each 0.25-beat slice, so a test can assert *when* the notes of
    /// a MIDI clip sound rather than only that something did.
    fn render_bar_envelope(runtime: &mut RuntimeProject, beats: f64, bpm: f64) -> Vec<f32> {
        let samples_per_beat = SAMPLE_RATE as f64 * 60.0 / bpm;
        let total = (samples_per_beat * beats) as u64;
        let mut envelope = Vec::new();
        let mut position = 0u64;
        let slice = (samples_per_beat * 0.25) as u64;
        while position < total {
            let mut slice_peak = 0.0f32;
            let slice_end = (position + slice).min(total);
            while position < slice_end {
                let frames = FRAMES.min((slice_end - position) as usize);
                let mut output = vec![0.0f32; frames * 2];
                crate::engine::schedule_midi_render_block(runtime, position, frames as u64, None);
                render_project_block_interleaved(
                    runtime,
                    position,
                    1.0,
                    &mut output,
                    2,
                    true,
                    4,
                    4,
                    None,
                );
                slice_peak = output.iter().fold(slice_peak, |peak, s| peak.max(s.abs()));
                position += frames as u64;
            }
            envelope.push(slice_peak);
        }
        envelope
    }

    #[test]
    fn midi_clip_notes_play_through_the_soundfont_during_transport() {
        // The arrangement path: a MIDI clip on a Soundfont Player track, played
        // by the transport rather than auditioned. Two notes a bar apart, so
        // the rendered envelope has to be silent before the first, loud on each
        // note, and quiet in the gap between them.
        let font = FontFile::new("clip");
        let bpm = 120.0;
        let mut snapshot = EngineProjectSnapshot {
            project_id: "soundfont-clip".to_string(),
            project_root: None,
            preferred_input_device: None,
            bpm,
            tempo_points: Vec::new(),
            time_signature: [4, 4],
            sample_rate: SAMPLE_RATE,
            tracks: vec![
                soundfont_track("sf-1", &font, test_font::MELODIC_PRESET),
                master_track(),
            ],
            clips: Vec::new(),
            midi_clips: vec![EngineMidiClipSnapshot {
                id: "clip-1".to_string(),
                track_id: "sf-1".to_string(),
                start_beat: 1.0,
                length_beats: 4.0,
                notes: vec![
                    EngineMidiNoteSnapshot {
                        id: 1,
                        pitch: 60,
                        start_beat: 0.0,
                        length_beats: 0.5,
                        velocity: 100,
                        channel: 0,
                        pitch_points: Vec::new(),
                        articulation: None,
                    },
                    EngineMidiNoteSnapshot {
                        id: 2,
                        pitch: 67,
                        start_beat: 3.0,
                        length_beats: 0.5,
                        velocity: 100,
                        channel: 0,
                        pitch_points: Vec::new(),
                        articulation: None,
                    },
                ],
                controllers: Vec::new(),
            }],
            pdc_enabled: true,
            latency_graph_version: 1,
            routing: EngineRoutingSnapshot {
                master_output_device: None,
                sample_rate: SAMPLE_RATE,
                buffer_size: FRAMES as u32,
            },
        };
        snapshot.midi_clips[0].length_beats = 4.0;

        let mut runtime =
            RuntimeProject::build(&snapshot, SAMPLE_RATE, &mut HashMap::new(), None, true)
                .expect("runtime builds");
        let envelope = render_bar_envelope(&mut runtime, 6.0, bpm);

        // Beat 0..1 is before the clip starts.
        let before = envelope[..4].iter().fold(0.0f32, |a, b| a.max(*b));
        assert!(before < 1.0e-6, "silent before the clip: {before}");

        // The clip's first note lands on beat 1, the second on beat 4.
        let first = envelope[4..6].iter().fold(0.0f32, |a, b| a.max(*b));
        let second = envelope[16..18].iter().fold(0.0f32, |a, b| a.max(*b));
        assert!(first > 0.001, "first note should sound: {first}");
        assert!(second > 0.001, "second note should sound: {second}");

        // The two-beat gap in the middle decays well below the note attacks.
        let gap = envelope[10..15].iter().fold(0.0f32, |a, b| a.max(*b));
        assert!(
            gap < first * 0.5,
            "gap between notes should decay: gap={gap} first={first}"
        );
    }

    #[test]
    fn missing_font_leaves_the_track_silent_instead_of_failing_the_graph() {
        let font = FontFile::new("missing");
        let mut track = soundfont_track("sf-1", &font, test_font::MELODIC_PRESET);
        track.soundfont_path = Some("/definitely/not/a/soundfont.sf2".to_string());
        let snapshot = EngineProjectSnapshot {
            project_id: "soundfont-missing".to_string(),
            project_root: None,
            preferred_input_device: None,
            bpm: 120.0,
            tempo_points: Vec::new(),
            time_signature: [4, 4],
            sample_rate: SAMPLE_RATE,
            tracks: vec![track, master_track()],
            clips: Vec::new(),
            midi_clips: Vec::new(),
            pdc_enabled: true,
            latency_graph_version: 1,
            routing: EngineRoutingSnapshot {
                master_output_device: None,
                sample_rate: SAMPLE_RATE,
                buffer_size: FRAMES as u32,
            },
        };
        let mut runtime =
            RuntimeProject::build(&snapshot, SAMPLE_RATE, &mut HashMap::new(), None, true)
                .expect("graph still builds without the font");
        runtime.midi_preview_note_on("sf-1", 0, 60, 100);
        assert_eq!(render_peak(&mut runtime), 0.0);
    }

    /// Regression: a DSP Fx dropped on a Soundfont track used to be picked as
    /// the track's "MIDI instrument" (any bridged insert on an Instrument track
    /// qualified), so notes were pushed at the effect's bridge sink and the
    /// player itself was starved — silence with an Fx, sound without one. The
    /// player owns the notes; the effect only sees audio.
    #[test]
    fn an_effect_insert_does_not_take_the_notes_away_from_the_player() {
        let font = FontFile::new("effect-insert");
        let mut track = soundfont_track("sf-1", &font, test_font::MELODIC_PRESET);
        let mut params = HashMap::new();
        params.insert("format".to_string(), serde_json::json!("BuiltIn"));
        params.insert("classId".to_string(), serde_json::json!("equz8"));
        params.insert("role".to_string(), serde_json::json!("effect"));
        track.inserts.push(crate::types::EngineInsertSnapshot {
            id: "insert-sf-1-eq".to_string(),
            kind: "external-bridge-plugin".to_string(),
            enabled: true,
            params,
            state: None,
        });
        let mut runtime = build_runtime(vec![track, master_track()]);
        assert!(
            runtime.tracks[0].midi_instrument_insert_ix.is_none(),
            "an effect insert must not become the note destination"
        );
        runtime.midi_preview_note_on("sf-1", 0, 60, 100);
        assert!(
            render_peak(&mut runtime) > 0.001,
            "the player must still sound with an effect in the chain"
        );
    }
}

#[cfg(test)]
mod bridge_bypass_tests {
    use super::apply_bridge_insert_output;

    #[test]
    fn effect_with_fresh_block_replaces_dry_signal() {
        let mut block_l = vec![1.0, 1.0, 1.0, 1.0];
        let mut block_r = vec![1.0, 1.0, 1.0, 1.0];
        let scratch_l = vec![0.5, 0.25, 0.0, -0.75];
        let scratch_r = vec![-0.5, 0.0, 0.25, 0.75];
        apply_bridge_insert_output(true, 4, &mut block_l, &mut block_r, &scratch_l, &scratch_r);
        assert_eq!(block_l, scratch_l, "effect output replaces the dry block");
        assert_eq!(block_r, scratch_r);
    }

    #[test]
    fn effect_not_ready_passes_dry_signal_through() {
        // got == 0: the host produced no fresh block. An effect must BYPASS —
        // the dry input is left untouched, never silenced, never a stale replay.
        let mut block_l = vec![0.3, -0.4, 0.5, -0.6];
        let mut block_r = vec![-0.3, 0.4, -0.5, 0.6];
        let dry_l = block_l.clone();
        let dry_r = block_r.clone();
        let scratch_l = vec![9.0, 9.0, 9.0, 9.0]; // must be ignored
        let scratch_r = vec![9.0, 9.0, 9.0, 9.0];
        let (pl, pr) =
            apply_bridge_insert_output(true, 0, &mut block_l, &mut block_r, &scratch_l, &scratch_r);
        assert_eq!(block_l, dry_l, "effect keeps the dry signal when not ready");
        assert_eq!(block_r, dry_r);
        assert_eq!((pl, pr), (0.0, 0.0));
    }

    #[test]
    fn instrument_not_ready_contributes_silence() {
        // got == 0: an instrument must add nothing — the accumulator block is
        // left as-is (whatever else summed into it), i.e. this insert is silent.
        let mut block_l = vec![0.2, 0.2, 0.2, 0.2];
        let mut block_r = vec![0.1, 0.1, 0.1, 0.1];
        let before_l = block_l.clone();
        let before_r = block_r.clone();
        let scratch_l = vec![9.0, 9.0, 9.0, 9.0];
        let scratch_r = vec![9.0, 9.0, 9.0, 9.0];
        let (pl, pr) = apply_bridge_insert_output(
            false,
            0,
            &mut block_l,
            &mut block_r,
            &scratch_l,
            &scratch_r,
        );
        assert_eq!(block_l, before_l, "instrument adds silence when not ready");
        assert_eq!(block_r, before_r);
        assert_eq!((pl, pr), (0.0, 0.0));
    }

    #[test]
    fn instrument_with_fresh_block_sums_into_accumulator() {
        let mut block_l = vec![0.2, 0.2, 0.2];
        let mut block_r = vec![0.1, 0.1, 0.1];
        let scratch_l = vec![0.5, -0.3, 0.0];
        let scratch_r = vec![0.0, 0.4, -0.6];
        apply_bridge_insert_output(false, 3, &mut block_l, &mut block_r, &scratch_l, &scratch_r);
        let approx = |a: &[f32], b: &[f32]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-5);
        assert!(approx(&block_l, &[0.7, -0.1, 0.2]), "got {block_l:?}");
        assert!(approx(&block_r, &[0.1, 0.5, -0.5]), "got {block_r:?}");
    }

    #[test]
    fn effect_partial_block_keeps_full_dry() {
        // got < frames: refuse the wet|dry splice — leave the whole dry block.
        let mut block_l = vec![1.0, 1.0, 1.0, 1.0];
        let mut block_r = vec![1.0, 1.0, 1.0, 1.0];
        let scratch_l = vec![0.5, 0.5, 0.0, 0.0];
        let scratch_r = vec![0.5, 0.5, 0.0, 0.0];
        apply_bridge_insert_output(true, 2, &mut block_l, &mut block_r, &scratch_l, &scratch_r);
        assert_eq!(
            block_l,
            vec![1.0, 1.0, 1.0, 1.0],
            "partial effect must not splice"
        );
        assert_eq!(block_r, vec![1.0, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn instrument_partial_block_only_sums_read_frames() {
        let mut block_l = vec![1.0, 1.0, 1.0, 1.0];
        let mut block_r = vec![1.0, 1.0, 1.0, 1.0];
        let scratch_l = vec![0.5, 0.5, 9.0, 9.0];
        let scratch_r = vec![0.25, 0.25, 9.0, 9.0];
        apply_bridge_insert_output(false, 2, &mut block_l, &mut block_r, &scratch_l, &scratch_r);
        assert_eq!(block_l, vec![1.5, 1.5, 1.0, 1.0]);
        assert_eq!(block_r, vec![1.25, 1.25, 1.0, 1.0]);
    }
}

#[cfg(test)]
#[path = "control_room_tests.rs"]
mod control_room_tests;

#[cfg(test)]
#[path = "solfege_pitch_tests.rs"]
mod solfege_pitch_tests;

/// The crossfade laws, which are the whole point of letting a clip choose one.
///
/// A crossfade is a *pair* of ramps, so what matters is not the shape of one
/// but what the two sum to across the overlap. These pin both sums, because
/// picking the wrong law is audible as a 3 dB hole or a 3 dB bulge in the
/// middle of every edit — and until now the choice was carried in the project
/// file and ignored by the renderer, so it could not be heard at all.
#[cfg(test)]
mod fade_curve_tests {
    use super::clip_fade_gain_with_curves;
    use crate::runtime::FadeCurve;

    const LEN: u64 = 1_000;

    /// Gain of a clip fading *in* over the whole of its length, at `t`.
    fn rising(curve: FadeCurve, t: f32) -> f32 {
        clip_fade_gain_with_curves((t * LEN as f32) as u64, LEN, LEN, 0, curve, curve)
    }

    /// Gain of a clip fading *out* over the whole of its length, at `t`.
    fn falling(curve: FadeCurve, t: f32) -> f32 {
        clip_fade_gain_with_curves((t * LEN as f32) as u64, LEN, 0, LEN, curve, curve)
    }

    /// Two uncorrelated takes: their *powers* add, so the power sum has to stay
    /// flat or the middle of every crossfade drops about 3 dB.
    #[test]
    fn equal_power_holds_the_power_sum_flat() {
        for step in 0..=10 {
            let t = step as f32 / 10.0;
            let a = falling(FadeCurve::EqualPower, t);
            let b = rising(FadeCurve::EqualPower, t);
            let power = a * a + b * b;
            assert!(
                (power - 1.0).abs() < 0.01,
                "power sum {power} at t={t} is not flat"
            );
        }
        // The signature of the law: -3 dB, not -6, at the midpoint.
        let mid = rising(FadeCurve::EqualPower, 0.5);
        assert!(
            (mid - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.01,
            "equal-power midpoint {mid} should be ≈0.707"
        );
    }

    /// Two halves of one continuous performance are perfectly correlated, so
    /// their *amplitudes* add. Linear is the law that keeps that sum flat.
    #[test]
    fn linear_holds_the_amplitude_sum_flat() {
        for step in 0..=10 {
            let t = step as f32 / 10.0;
            let sum = falling(FadeCurve::Linear, t) + rising(FadeCurve::Linear, t);
            assert!(
                (sum - 1.0).abs() < 0.01,
                "amplitude sum {sum} at t={t} is not flat"
            );
        }
        let mid = rising(FadeCurve::Linear, 0.5);
        assert!(
            (mid - 0.5).abs() < 0.01,
            "linear midpoint {mid} should be 0.5"
        );
    }

    /// The two edges of one clip are independent: a tail crossfading into the
    /// next take can be equal power while the head fading up from silence is
    /// linear.
    #[test]
    fn the_two_edges_carry_their_own_curve() {
        let gain = clip_fade_gain_with_curves(
            LEN / 4,
            LEN,
            LEN / 2,
            LEN / 2,
            FadeCurve::Linear,
            FadeCurve::EqualPower,
        );
        // Half way through a linear fade-in, and clear of the fade-out.
        assert!(
            (gain - 0.5).abs() < 0.01,
            "fade-in edge used the wrong law: {gain}"
        );
    }

    #[test]
    fn a_clip_with_no_fades_is_untouched() {
        for curve in [FadeCurve::EqualPower, FadeCurve::Linear] {
            assert_eq!(
                clip_fade_gain_with_curves(LEN / 2, LEN, 0, 0, curve, curve),
                1.0
            );
        }
    }

    /// The tag crosses a serialization boundary, so an unknown one has to play
    /// rather than fail — and it must land on the default, not on silence.
    #[test]
    fn an_unknown_curve_tag_falls_back_to_equal_power() {
        assert_eq!(FadeCurve::from_tag("linear"), FadeCurve::Linear);
        assert_eq!(FadeCurve::from_tag("Equal_Power"), FadeCurve::EqualPower);
        assert_eq!(
            FadeCurve::from_tag("s-curve-from-2027"),
            FadeCurve::EqualPower
        );
        assert_eq!(FadeCurve::from_tag(""), FadeCurve::EqualPower);
        // And the tag round-trips, so a project saved by this build reloads
        // with the curve it was given.
        for curve in [FadeCurve::EqualPower, FadeCurve::Linear] {
            assert_eq!(FadeCurve::from_tag(curve.tag()), curve);
        }
    }
}
