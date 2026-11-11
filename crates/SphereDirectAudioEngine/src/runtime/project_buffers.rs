//! RuntimeProject: preallocating block buffers and retargeting the sample rate.

use super::*;

impl RuntimeProject {
    /// Grow every track's block and receive buffers to hold `frames`, off the
    /// audio thread. The render pass still grows them if a device hands it a
    /// larger block than announced, but a graph prepared here never allocates
    /// on its first blocks.
    pub fn reserve_track_blocks(&mut self, frames: usize) {
        let frames = frames.min(DEFAULT_AUDIO_BLOCK_CAPACITY);
        for track in &mut self.tracks {
            for buffer in [
                &mut track.block_l,
                &mut track.block_r,
                &mut track.recv_l,
                &mut track.recv_r,
            ] {
                if buffer.len() < frames {
                    buffer.resize(frames, 0.0);
                }
            }
        }
        // After `retarget_sample_rate` on a stream's graph: a lower rate packs
        // the same MIDI into fewer frames.
        self.reserve_midi_block_events();
    }

    /// Size every instrument track's per-block MIDI list for the densest block
    /// of its schedule ([`midi_block_event_capacity`]), off the audio thread.
    /// A schedule swapped in later brings its own larger list when it needs
    /// one ([`Self::adopt_midi_block_event_buffers`]).
    pub fn reserve_midi_block_events(&mut self) {
        // Every track holds at least the base list (live preview reaches
        // tracks with no schedule; a cloned graph's lists come back empty).
        for track in &mut self.tracks {
            let events = &mut track.midi_block_events;
            if events.capacity() < MIDI_BLOCK_EVENT_BASE_CAPACITY {
                events.reserve(MIDI_BLOCK_EVENT_BASE_CAPACITY - events.len());
            }
        }
        for midi_track in &self.midi_tracks {
            let Some(track) = midi_track
                .track_index
                .and_then(|index| self.tracks.get_mut(index))
            else {
                continue;
            };
            let need = midi_block_event_capacity(&midi_track.events, MIDI_BLOCK_WINDOW);
            let events = &mut track.midi_block_events;
            if events.capacity() < need {
                events.reserve(need - events.len());
            }
        }
    }

    /// Audio thread, right after a MIDI schedule is swapped in: take each
    /// track's preallocated block-event list where it is larger than the one
    /// the graph has. Anything already queued this block moves with it (it
    /// fits: the new list is the larger). The smaller list stays in the
    /// schedule and leaves with it through the graveyard. No allocation.
    pub(crate) fn adopt_midi_block_event_buffers(&mut self) {
        for midi_track in &mut self.midi_tracks {
            let Some(track) = midi_track
                .track_index
                .and_then(|index| self.tracks.get_mut(index))
            else {
                continue;
            };
            let spare = &mut midi_track.block_events_spare;
            if spare.capacity() > track.midi_block_events.capacity() {
                spare.clear();
                spare.extend_from_slice(&track.midi_block_events);
                std::mem::swap(spare, &mut track.midi_block_events);
            }
        }
    }

    /// Rebuild all sample-rate-derived runtime state for an already-cached graph.
    ///
    /// This is the safety net for device reopen/sample-rate changes: cached runtime
    /// graphs keep canonical beats, params, and sources, but clip sample positions,
    /// MIDI event samples, plugin DSP coefficients, PDC buffers, and VST3
    /// `setupProcessing` instances must match the active stream sample rate.
    pub fn retarget_sample_rate(&mut self, sample_rate: u32) {
        let sample_rate = sample_rate.max(1);
        if self.sample_rate == sample_rate {
            return;
        }
        let old_sample_rate = self.sample_rate.max(1);
        let ratio = sample_rate as f64 / old_sample_rate as f64;
        self.sample_rate = sample_rate;
        let sr = sample_rate as f64;

        for clip in &mut self.clips {
            let old_duration = clip.duration_samples;
            let old_fade_in = clip.fade_in_samples;
            let old_fade_out = clip.fade_out_samples;
            clip.start_sample = self.tempo_map.samples_at_beat(clip.start_beat, sr);
            clip.duration_samples = ((old_duration as f64) * ratio).round().max(1.0) as u64;
            // Warp segments are in output samples too; left alone they would
            // end short of (or past) the rescaled clip.
            for segment in &mut clip.warp_segments {
                segment.out_start = ((segment.out_start as f64) * ratio).round() as u64;
                segment.out_end = ((segment.out_end as f64) * ratio).round() as u64;
            }
            clip.gain_step = clip_gain_step(sample_rate);
            clip.fade_in_samples = ((old_fade_in as f64) * ratio).round() as u64;
            clip.fade_out_samples = ((old_fade_out as f64) * ratio).round() as u64;
            clip.fade_in_samples = clip.fade_in_samples.min(clip.duration_samples);
            clip.fade_out_samples = clip
                .fade_out_samples
                .min(clip.duration_samples.saturating_sub(clip.fade_in_samples));
            clip.stretch_next_project_sample = None;
        }

        for clip in &mut self.midi_clips {
            for event in &mut clip.events {
                event.sample = self.tempo_map.samples_at_beat(event.beat, sr);
            }
            sort_midi_events(&mut clip.events);
        }
        for track in &mut self.midi_tracks {
            for event in &mut track.events {
                event.sample = self.tempo_map.samples_at_beat(event.beat, sr);
            }
            sort_midi_events(&mut track.events);
            track.cursor = 0;
            track.active.clear();
        }

        for track in &mut self.tracks {
            if let Some(soundfont) = track.soundfont_player.as_mut() {
                let font = soundfont.player.as_ref().map(SoundfontPlayer::sound_font);
                soundfont.rebuild(sample_rate, font);
            }
            // The built-in Soundfont Player is a track instrument rather than an
            // insert, so its decimation delay has to be seeded here before the
            // insert latencies accumulate on top.
            track.plugin_latency_samples = track
                .soundfont_player
                .as_ref()
                .map(RuntimeSoundfontPlayer::latency_samples)
                .unwrap_or(0);
            for insert in &mut track.inserts {
                insert.dsp.rebuild(
                    canonical_plugin_id(&insert.kind),
                    &insert.params,
                    sample_rate,
                );
                if insert.kind_tag == RuntimeInsertKind::NativePlugin {
                    let needs_recreate = insert
                        .vst3
                        .as_ref()
                        .map(|processor| processor.sample_rate() != sample_rate)
                        .unwrap_or(false);
                    if needs_recreate {
                        if let Some(old) = insert.vst3.take() {
                            old.set_destroy_reason("sample-rate-change");
                        }
                        insert.vst3 =
                            Vst3RuntimeProcessor::from_params(&insert.params, sample_rate);
                    }
                }
                if let Some(vst3) = insert.vst3.as_ref().filter(|vst3| vst3.is_ready()) {
                    track.plugin_latency_samples = track
                        .plugin_latency_samples
                        .saturating_add(vst3.get_latency_samples().max(0) as u32);
                }
            }
        }

        self.plan_latency_and_pdc();
        self.reset_pdc_delay_lines();
    }
}
