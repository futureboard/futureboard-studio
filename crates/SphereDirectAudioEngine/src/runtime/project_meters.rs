//! RuntimeProject: per-block meters and meter snapshots.

use super::*;

impl RuntimeProject {
    #[inline]
    pub fn begin_meter_block(&mut self) {
        for track in &mut self.tracks {
            track.meter_peak_l = 0.0;
            track.meter_peak_r = 0.0;
            track.meter_sum_sq_l = 0.0;
            track.meter_sum_sq_r = 0.0;
        }
    }

    #[inline]
    pub fn accumulate_track_meter(&mut self, track_index: usize, l: f32, r: f32) {
        let Some(track) = self.tracks.get_mut(track_index) else {
            return;
        };
        let abs_l = l.abs();
        let abs_r = r.abs();
        track.meter_peak_l = track.meter_peak_l.max(abs_l);
        track.meter_peak_r = track.meter_peak_r.max(abs_r);
        track.meter_sum_sq_l += l * l;
        track.meter_sum_sq_r += r * r;
    }

    #[inline]
    pub fn accumulate_live_input_meters(
        &mut self,
        latest_l: f32,
        latest_r: f32,
        monitor_source: (u32, u32),
    ) {
        if latest_l == 0.0 && latest_r == 0.0 {
            return;
        }
        for track in &mut self.tracks {
            if track.track_type != "audio" {
                continue;
            }
            if !track.record_armed && !track.monitor_enabled {
                continue;
            }
            if !track.input_source.is_routable() {
                continue;
            }
            let (l, r) =
                track
                    .input_source
                    .sample_from_monitor_pair(latest_l, latest_r, monitor_source);
            let abs_l = l.abs();
            let abs_r = r.abs();
            track.meter_peak_l = track.meter_peak_l.max(abs_l);
            track.meter_peak_r = track.meter_peak_r.max(abs_r);
            track.meter_sum_sq_l += l * l;
            track.meter_sum_sq_r += r * r;
        }
    }

    #[inline]
    pub fn end_meter_block(&mut self, frames: u64) {
        let frame_count = frames.max(1) as f32;
        for track in &mut self.tracks {
            let rms_l = (track.meter_sum_sq_l / frame_count).sqrt();
            let rms_r = (track.meter_sum_sq_r / frame_count).sqrt();
            track
                .meter
                .store(track.meter_peak_l, track.meter_peak_r, rms_l, rms_r);
        }
    }

    /// Every track's meter since the last call, which starts the next window
    /// (see [`RuntimeTrackMeter::store`]). One display reader calls this.
    pub fn meter_snapshots(&self) -> Vec<RuntimeTrackMeterSnapshot> {
        self.tracks
            .iter()
            .map(|track| track.meter.take(&track.id))
            .collect()
    }

    pub fn plugin_output_meter_snapshots(&self) -> Vec<RuntimePluginOutputMeterSnapshot> {
        let mut snapshots = Vec::new();
        for track in &self.tracks {
            for insert in &track.inserts {
                let Some(sink) = insert.bridge_sink.as_ref() else {
                    continue;
                };
                let channels = sink
                    .plugin_output_channels()
                    .clamp(0, MAX_VSTI_OUTPUT_CHANNELS as u32) as u8;
                for channel in 1..=channels {
                    snapshots.push(RuntimePluginOutputMeterSnapshot {
                        track_id: track.id.clone(),
                        insert_id: insert.id.clone(),
                        channel,
                        peak: sink.output_channel_peak(channel),
                    });
                }
            }
        }
        snapshots
    }
}
