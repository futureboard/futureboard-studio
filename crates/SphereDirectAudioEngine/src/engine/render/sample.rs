//! The per-sample render path, master automation and beat/tempo helpers.

use super::*;

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
    let master_volume =
        master_automation_volume(&runtime.tracks, master_index, beat).unwrap_or(master_volume);

    for clip_index in 0..runtime.clips.len() {
        let (
            clip_muted,
            clip_start_sample,
            clip_duration_samples,
            rel_params,
            source,
            track_index,
            processor,
            effective_time_ratio,
        ) = {
            let clip = &runtime.clips[clip_index];
            (
                clip.muted,
                clip.start_sample,
                clip.duration_samples,
                (
                    clip.offset_seconds,
                    clip.source_read_rate,
                    clip.reverse,
                    clip.fade_in_samples,
                    clip.fade_out_samples,
                    clip.fade_in_curve,
                    clip.fade_out_curve,
                ),
                Arc::clone(&clip.source),
                clip.track_index,
                clip.processor,
                clip.effective_time_ratio,
            )
        };
        if clip_muted {
            continue;
        }
        if project_sample < clip_start_sample {
            continue;
        }
        let rel = project_sample - clip_start_sample;
        if rel >= clip_duration_samples {
            continue;
        }

        let (
            clip_offset_seconds,
            clip_source_read_rate,
            clip_reverse,
            clip_fade_in,
            clip_fade_out,
            clip_fade_in_curve,
            clip_fade_out_curve,
        ) = rel_params;

        // Resolved at build time — no id lookup or String clone per sample.
        let Some(track_index) = track_index.filter(|&ti| ti < runtime.tracks.len()) else {
            continue;
        };
        if Some(track_index) == master_index {
            continue;
        }
        let has_solo = runtime.has_solo;
        if effective_track_muted(&runtime.tracks[track_index], beat)
            || (has_solo
                && !runtime.tracks[track_index].solo
                && !solo_keeps_source_audible(runtime, track_index))
        {
            continue;
        }

        let (source_pos, dry_source_pos, processor) = {
            let clip = &runtime.clips[clip_index];
            let warped = !clip.warp_segments.is_empty();
            let source_pos = resolved_clip_source_frame(
                rel,
                clip_duration_samples,
                clip_offset_seconds,
                runtime.sample_rate,
                source.sample_rate() as f64,
                if matches!(processor, ClipDspProcessor::PhaseVocoderBasic) && !warped {
                    1.0 / effective_time_ratio.max(0.01)
                } else {
                    clip_source_read_rate
                },
                clip_reverse,
                &clip.warp_segments,
            );
            let dry_source_pos = if warped {
                source_pos
            } else {
                resolved_clip_source_frame(
                    rel,
                    clip_duration_samples,
                    clip_offset_seconds,
                    runtime.sample_rate,
                    source.sample_rate() as f64,
                    clip_source_read_rate,
                    clip_reverse,
                    &clip.warp_segments,
                )
            };
            let processor = if warped {
                ClipDspProcessor::Resample
            } else {
                processor
            };
            (source_pos, dry_source_pos, processor)
        };
        let (mut l, mut r) = sample_clip_processor_stereo(
            &source,
            source_pos,
            dry_source_pos,
            effective_time_ratio,
            processor,
        );
        let fade = clip_fade_gain_with_curves(
            rel,
            clip_duration_samples,
            clip_fade_in,
            clip_fade_out,
            clip_fade_in_curve,
            clip_fade_out_curve,
        );
        (l, r) = process_clip_frame(&mut runtime.clips[clip_index], rel, fade, l, r);
        if l == 0.0 && r == 0.0 {
            continue;
        }

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
                let track = &runtime.tracks[track_index];
                let (enabled, level) = track.send_state_at(s, beat);
                (enabled, level, track.sends[s].return_track_index)
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

/// The master's linear gain at `beat` from its Volume lane, or `None` when it
/// has none with points (the fader stands). Lane walk only, no allocation.
#[inline]
pub(crate) fn master_automation_volume(
    tracks: &[RuntimeTrack],
    master_index: Option<usize>,
    beat: f64,
) -> Option<f32> {
    let master = tracks.get(master_index?)?;
    master.automation_values_at_beat(beat).volume
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
                    tension: crate::tempo_map::clamp_tension(p.tension),
                })
                .collect(),
        )
    }
}

pub(super) fn sample_to_beat(runtime: &RuntimeProject, sample: u64) -> f64 {
    runtime
        .tempo_map
        .beat_at_samples(sample, runtime.sample_rate.max(1) as f64)
}
