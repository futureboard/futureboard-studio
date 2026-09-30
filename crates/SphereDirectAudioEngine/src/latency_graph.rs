//! Runtime latency graph planning and PDC delays (Phase V/W).
//!
//! Built on the control thread alongside `RuntimeAudioGraph`. The audio callback
//! reads precomputed per-track delay sample counts and applies ring-buffer delay
//! lines before routing so parallel paths align at the master summing bus.

use std::collections::HashMap;

use crate::audio_graph::{is_master_track_type, is_routing_track_type, RuntimeAudioGraph};
use crate::runtime::RuntimeTrack;

/// Precomputed latency / PDC data for a runtime project snapshot.
#[derive(Debug, Clone, Default)]
pub struct RuntimeLatencyGraph {
    /// Sum of enabled native-plugin insert latencies on each track strip.
    pub track_plugin_latency: Vec<u32>,
    /// Latency at each track's output tap (includes upstream feed for routing tracks).
    pub track_output_latency: Vec<u32>,
    /// Delay applied to post-fader **main output** after sends, so dry/wet
    /// paths that diverge through return FX still align at the master bus.
    pub track_pdc_delay: Vec<u32>,
    /// Maximum path latency to the master summing bus (before master inserts).
    pub max_path_latency_samples: u32,
    pub master_plugin_latency: u32,
}

#[inline]
pub fn strip_plugin_latency_samples(track: &RuntimeTrack) -> u32 {
    // In-process VST3 insert latency, summed directly from the live processors.
    let from_inserts: u32 = track
        .inserts
        .iter()
        .filter(|insert| insert.enabled)
        .map(|insert| {
            insert
                .vst3
                .as_ref()
                .filter(|vst3| vst3.is_ready())
                .map(|vst3| vst3.get_latency_samples().max(0) as u32)
                .unwrap_or(0)
        })
        .sum();
    // `from_inserts` only sees in-process VST3 inserts — it does NOT account for
    // external-bridge inserts (their latency = reported + the one-block
    // handshake, tracked in `plugin_latency_samples` by
    // `RuntimeProject::track_insert_latency_samples`). Taking the max keeps both
    // sources: a track with only VST3 inserts uses the direct sum, and a track
    // that also has bridged inserts keeps the complete stored value instead of
    // silently dropping the bridge latency.
    //
    // The previous `if from_inserts > 0 { from_inserts } else { field }` dropped
    // the bridge component whenever a mixed track also had an in-process insert,
    // which both under-compensated the PDC path AND made
    // `refresh_runtime_latency_graph` see a permanent mismatch (perpetual graph
    // rebuild, never converging). `build` seeds `plugin_latency_samples = 0`
    // before its first `strip` call, so the max is a no-op there and only the
    // refresh path — which stores the complete observed latency — benefits.
    from_inserts.max(track.plugin_latency_samples)
}

/// Resolve the routing indices latency planning reads: every track's
/// main-output target and every send's return target.
///
/// Builds a track-id map, so this is a control-thread call — `RuntimeProject`
/// runs it before the first plan and again from `resolve_indices` on every
/// graph swap. Planning itself then works purely on indices, which is what lets
/// the realtime refresh path recompute latency without hashing a track id.
pub fn resolve_latency_routing_indices(tracks: &mut [RuntimeTrack]) {
    let id_to_index: HashMap<String, usize> = tracks
        .iter()
        .enumerate()
        .map(|(index, track)| (track.id.clone(), index))
        .collect();
    for track in tracks.iter_mut() {
        track.output_track_index = track
            .output_track_id
            .as_deref()
            .filter(|id| !crate::engine::is_master_output(id))
            .and_then(|id| id_to_index.get(id).copied());
        for send in &mut track.sends {
            send.return_track_index = id_to_index.get(&send.return_track_id).copied();
        }
    }
}

/// The track a main output feeds, or `None` when it reaches the master sum
/// directly. This is the same index the render path routes on, so compensation
/// is planned against the routing that actually runs.
#[inline]
fn resolve_output_target_index(track_index: usize, tracks: &[RuntimeTrack]) -> Option<usize> {
    tracks[track_index].output_track_index
}

/// Tail latency from a routing track's output toward the master summing bus,
/// excluding the track's own `track_output_latency` (already counted separately).
fn routing_tail_to_master(
    mut track_index: usize,
    tracks: &[RuntimeTrack],
    plugin_latency: &[u32],
    master_index: Option<usize>,
) -> u32 {
    let mut tail = 0u32;
    let mut hops = 0usize;
    while hops < tracks.len() {
        hops += 1;
        let Some(next) = resolve_output_target_index(track_index, tracks) else {
            break;
        };
        if Some(next) == master_index {
            break;
        }
        if !is_routing_track_type(&tracks[next].track_type) {
            break;
        }
        tail = tail.saturating_add(plugin_latency[next]);
        track_index = next;
    }
    tail
}

fn path_to_master_sum(
    track_index: usize,
    tracks: &[RuntimeTrack],
    output_latency: &[u32],
    plugin_latency: &[u32],
    master_index: Option<usize>,
) -> u32 {
    if Some(track_index) == master_index {
        return 0;
    }
    output_latency
        .get(track_index)
        .copied()
        .unwrap_or(0)
        .saturating_add(routing_tail_to_master(
            track_index,
            tracks,
            plugin_latency,
            master_index,
        ))
}

fn effective_path_to_master(
    track_index: usize,
    tracks: &[RuntimeTrack],
    output_latency: &[u32],
    plugin_latency: &[u32],
    master_index: Option<usize>,
) -> u32 {
    let mut path = main_path_to_master(
        track_index,
        tracks,
        output_latency,
        plugin_latency,
        master_index,
    );

    for send in &tracks[track_index].sends {
        if !send.enabled {
            continue;
        }
        if let Some(ret_idx) = send.return_track_index {
            let via_return = path_to_master_sum(
                ret_idx,
                tracks,
                output_latency,
                plugin_latency,
                master_index,
            );
            path = path.max(via_return);
        }
    }
    path
}

/// Latency of the track's main-output (dry) path only — excludes send/return
/// branches. Used for PDC delay amounts so dry can catch up to wet without
/// delaying the send feed itself.
fn main_path_to_master(
    track_index: usize,
    tracks: &[RuntimeTrack],
    output_latency: &[u32],
    plugin_latency: &[u32],
    master_index: Option<usize>,
) -> u32 {
    resolve_output_target_index(track_index, tracks)
        .filter(|&target| is_routing_track_type(&tracks[target].track_type))
        .map(|target| {
            path_to_master_sum(target, tracks, output_latency, plugin_latency, master_index)
        })
        .unwrap_or_else(|| {
            path_to_master_sum(
                track_index,
                tracks,
                output_latency,
                plugin_latency,
                master_index,
            )
        })
}

/// Build latency metadata and per-track PDC delays from runtime tracks and the
/// audio graph plan. When `pdc_enabled` is false, delays are zeroed but path
/// latencies are still computed for reporting.
///
/// Takes the tracks mutably because it resolves their routing indices first:
/// planning reads those, and a caller that had to remember to resolve them
/// separately would get a silently mis-planned graph for forgetting. Control
/// thread only — allocates.
pub fn plan_runtime_latency_graph(
    tracks: &mut [RuntimeTrack],
    audio_graph: &RuntimeAudioGraph,
    pdc_enabled: bool,
) -> RuntimeLatencyGraph {
    let n = tracks.len();
    if n == 0 {
        return RuntimeLatencyGraph::default();
    }
    resolve_latency_routing_indices(tracks);

    let mut graph = RuntimeLatencyGraph {
        track_plugin_latency: vec![0; n],
        track_output_latency: vec![0; n],
        track_pdc_delay: vec![0; n],
        max_path_latency_samples: 0,
        master_plugin_latency: 0,
    };
    recompute_runtime_latency_graph(&mut graph, tracks, audio_graph, pdc_enabled);
    graph
}

/// Recompute `graph` in place from the tracks' current latencies.
///
/// This is the whole planning algorithm; [`plan_runtime_latency_graph`] is just
/// this behind a fresh allocation. Splitting it out is what makes the realtime
/// refresh path safe: routing comes from the resolved index fields and every
/// vector is already sized for `tracks`, so a recompute triggered from the audio
/// callback by a bridged plugin's reported latency moving does not allocate,
/// hash a track id, or touch the environment.
///
/// Only the numbers change here. Topology changes go through a graph swap, which
/// rebuilds on the control thread via [`plan_runtime_latency_graph`], so a
/// mismatched vector length means the caller skipped that swap — the recompute
/// is skipped rather than resized, since growing a vector is exactly what this
/// path exists to avoid.
pub fn recompute_runtime_latency_graph(
    graph: &mut RuntimeLatencyGraph,
    tracks: &[RuntimeTrack],
    audio_graph: &RuntimeAudioGraph,
    pdc_enabled: bool,
) {
    let n = tracks.len();
    debug_assert_eq!(graph.track_plugin_latency.len(), n);
    debug_assert_eq!(graph.track_output_latency.len(), n);
    debug_assert_eq!(graph.track_pdc_delay.len(), n);
    if n == 0
        || graph.track_plugin_latency.len() != n
        || graph.track_output_latency.len() != n
        || graph.track_pdc_delay.len() != n
    {
        return;
    }

    // Destructured so the passes below can read one vector while writing
    // another without the borrow checker seeing the whole graph as borrowed.
    let RuntimeLatencyGraph {
        track_plugin_latency: plugin_latency,
        track_output_latency: output_latency,
        track_pdc_delay,
        max_path_latency_samples,
        master_plugin_latency,
    } = graph;

    for (idx, track) in tracks.iter().enumerate() {
        plugin_latency[idx] = strip_plugin_latency_samples(track);
    }
    let master_index = audio_graph.master_index;
    *master_plugin_latency = master_index
        .and_then(|idx| plugin_latency.get(idx).copied())
        .unwrap_or(0);

    output_latency.copy_from_slice(plugin_latency);

    for &idx in &audio_graph.pass2_routing_indices {
        let mut feed_max = 0u32;
        for (src_idx, track) in tracks.iter().enumerate() {
            if is_master_track_type(&track.track_type) {
                continue;
            }
            for send in &track.sends {
                if !send.enabled {
                    continue;
                }
                if send.return_track_index == Some(idx) {
                    feed_max = feed_max.max(output_latency[src_idx]);
                }
            }
            // Main-output → bus/return feeds also contribute upstream latency.
            if resolve_output_target_index(src_idx, tracks) == Some(idx) {
                feed_max = feed_max.max(output_latency[src_idx]);
            }
        }
        output_latency[idx] = plugin_latency[idx].saturating_add(feed_max);
    }

    *max_path_latency_samples = 0;
    for idx in 0..n {
        if Some(idx) == master_index {
            continue;
        }
        if is_master_track_type(&tracks[idx].track_type) {
            continue;
        }
        let path =
            effective_path_to_master(idx, tracks, output_latency, plugin_latency, master_index);
        *max_path_latency_samples = (*max_path_latency_samples).max(path);
    }

    // A recompute reuses the previous plan's vector, so the delays every branch
    // below skips have to be cleared rather than left at their old values.
    track_pdc_delay.fill(0);
    if pdc_enabled && *max_path_latency_samples > 0 {
        for idx in 0..n {
            if Some(idx) == master_index || is_master_track_type(&tracks[idx].track_type) {
                continue;
            }
            // Main-output → bus/return feeders must not take PDC on the feeder
            // hop. The bus already carries upstream latency in its path and
            // compensates once before summing to master. Delaying both the
            // feeder and the bus double-compensates and makes bus-routed audio
            // late vs direct-to-master tracks by exactly that extra delay.
            // Send→return is different: the dry main still goes to master, so
            // the source keeps its delay (applied after the pre-PDC send tap).
            if resolve_output_target_index(idx, tracks)
                .is_some_and(|target| is_routing_track_type(&tracks[target].track_type))
            {
                continue;
            }
            // PDC delay is relative to the *main/dry* path so return-FX latency
            // can pull dry forward without also delaying the send feed.
            let path =
                main_path_to_master(idx, tracks, output_latency, plugin_latency, master_index);
            track_pdc_delay[idx] = (*max_path_latency_samples).saturating_sub(path);
        }
    }
}

/// In-place stereo delay line for PDC. `delay_l` / `delay_r` must be preallocated
/// with length >= `delay_samples + frames`.
#[inline]
pub fn apply_pdc_delay_block(
    block_l: &mut [f32],
    block_r: &mut [f32],
    delay_l: &mut [f32],
    delay_r: &mut [f32],
    write_pos: &mut usize,
    delay_samples: u32,
    frames: usize,
) {
    let delay = delay_samples as usize;
    if delay == 0 || frames == 0 {
        return;
    }
    let cap = delay_l.len();
    if cap <= delay {
        return;
    }

    for frame in 0..frames {
        let wp = *write_pos % cap;
        let rp = (wp + cap - delay) % cap;
        let out_l = delay_l[rp];
        let out_r = delay_r[rp];
        delay_l[wp] = block_l[frame];
        delay_r[wp] = block_r[frame];
        block_l[frame] = out_l;
        block_r[frame] = out_r;
        *write_pos = (wp + 1) % cap;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio_graph::plan_runtime_audio_graph;
    use crate::runtime::{RuntimePreviewMode, RuntimeSend, RuntimeTrack};

    fn track(id: &str, ty: &str, plugin_latency: u32, sends: Vec<RuntimeSend>) -> RuntimeTrack {
        RuntimeTrack {
            spatial: None,
            spatial_params: Default::default(),
            active_voices: 0,
            ara_renderers: Vec::new(),
            ara_l: Vec::new(),
            ara_r: Vec::new(),
            ara_silence: Vec::new(),
            ara_latency_samples: 0,
            listen: crate::monitor::ListenMode::Off,
            id: id.to_string(),
            track_type: ty.to_string(),
            volume: 1.0,
            pan: 0.0,
            muted: false,
            solo: false,
            record_armed: false,
            monitor_enabled: false,
            input_source: crate::runtime::RuntimeTrackInputSource::None,
            jam_publish_slot: None,
            loopback_publish: false,
            loopback_out_l: Vec::new(),
            loopback_out_r: Vec::new(),
            jam_multitrack_pair: None,
            preview_mode: RuntimePreviewMode::Stereo,
            output_track_id: None,
            output_track_index: None,
            inserts: Vec::new(),
            sends,
            automation_lanes: Vec::new(),
            plugin_param_automation: Vec::new(),
            meter: std::sync::Arc::new(crate::runtime::RuntimeTrackMeter::default()),
            meter_peak_l: 0.0,
            meter_peak_r: 0.0,
            meter_sum_sq_l: 0.0,
            meter_sum_sq_r: 0.0,
            callback_insert_log_done: false,
            callback_clip_route_log_done: false,
            block_l: vec![0.0; 64],
            block_r: vec![0.0; 64],
            recv_l: vec![0.0; 64],
            recv_r: vec![0.0; 64],
            soundfont_l: vec![0.0; 64],
            soundfont_r: vec![0.0; 64],
            midi_block_events: Vec::new(),
            solfege_pitch_events: Vec::new(),
            solfege_articulation_events: Vec::new(),
            midi_instrument_insert_ix: None,
            soundfont_player: None,
            solfege_engine: None,
            pdc_delay_l: Vec::new(),
            pdc_delay_r: Vec::new(),
            pdc_write_pos: 0,
            plugin_latency_samples: plugin_latency,
            smoothed_gain_l: 1.0,
            smoothed_gain_r: 1.0,
        }
    }

    fn send(id: &str, target: &str) -> RuntimeSend {
        RuntimeSend {
            id: id.to_string(),
            return_track_id: target.to_string(),
            return_track_index: None,
            level: 1.0,
            enabled: true,
            pre_fader: false,
        }
    }

    #[test]
    fn pdc_delays_shorter_track_to_match_longer_path() {
        let mut tracks = vec![
            track("fast", "audio", 0, vec![]),
            track("slow", "audio", 512, vec![]),
            track("master", "master", 0, vec![]),
        ];
        let audio_graph = plan_runtime_audio_graph(&tracks).unwrap();
        let latency = plan_runtime_latency_graph(&mut tracks, &audio_graph, true);
        assert_eq!(latency.max_path_latency_samples, 512);
        assert_eq!(latency.track_pdc_delay[0], 512);
        assert_eq!(latency.track_pdc_delay[1], 0);
    }

    #[test]
    fn return_feed_increases_path_latency() {
        let mut tracks = vec![
            track("src", "audio", 128, vec![send("s", "ret")]),
            track("ret", "return", 256, vec![]),
            track("master", "master", 0, vec![]),
        ];
        let audio_graph = plan_runtime_audio_graph(&tracks).unwrap();
        let latency = plan_runtime_latency_graph(&mut tracks, &audio_graph, true);
        assert_eq!(latency.track_output_latency[1], 128 + 256);
        assert_eq!(latency.max_path_latency_samples, 128 + 256);
        // Dry main path is 128; wet via return is 384. PDC delays dry by 256
        // (applied after the send tap) so dry and wet meet at the master.
        assert_eq!(latency.track_pdc_delay[0], 256);
        assert_eq!(latency.track_pdc_delay[1], 0);
    }

    #[test]
    fn main_to_bus_does_not_double_pdc_against_longer_direct_path() {
        // src → bus (0 insert lat) and slow → master (512). Without the feeder
        // exemption both src and bus got D = max − path(bus), so bus audio
        // arrived at max+D. Feeder delay must stay 0; only the bus (or other
        // master-bound hop) compensates.
        let mut src = track("src", "audio", 0, vec![]);
        src.output_track_id = Some("bus".to_string());
        let mut tracks = vec![
            src,
            track("bus", "bus", 0, vec![]),
            track("slow", "audio", 512, vec![]),
            track("master", "master", 0, vec![]),
        ];
        let audio_graph = plan_runtime_audio_graph(&tracks).unwrap();
        let latency = plan_runtime_latency_graph(&mut tracks, &audio_graph, true);
        assert_eq!(latency.max_path_latency_samples, 512);
        assert_eq!(
            latency.track_pdc_delay[0], 0,
            "main→bus feeder must not take PDC (would double with the bus)"
        );
        assert_eq!(
            latency.track_pdc_delay[1], 512,
            "bus compensates once to align with the longer direct path"
        );
        assert_eq!(latency.track_pdc_delay[2], 0);
        // Bus-path arrival at master = bus_output_lat + bus_pdc = 0 + 512 = max.
        let bus_arrival = latency.track_output_latency[1] + latency.track_pdc_delay[1];
        assert_eq!(bus_arrival, latency.max_path_latency_samples);
    }

    #[test]
    fn strip_latency_uses_stored_value_as_a_floor() {
        // A track carrying bridge latency in `plugin_latency_samples` (set by the
        // refresh path) must never have it dropped by `strip`. With no in-process
        // VST3 inserts the direct sum is 0, so the stored value is the result —
        // and the max means a mixed VST3+bridge track keeps the complete value
        // rather than collapsing to the VST3-only sum (the perpetual-rebuild /
        // PDC-undercount bug). Insert-level mixing needs live VST3/bridge
        // processors, so this locks the field-floor contract the fix relies on.
        let t = track("bridged", "audio", 900, vec![]);
        assert_eq!(strip_plugin_latency_samples(&t), 900);

        let zero = track("empty", "audio", 0, vec![]);
        assert_eq!(strip_plugin_latency_samples(&zero), 0);
    }

    #[test]
    fn apply_pdc_delay_block_shifts_samples() {
        let mut block_l = vec![1.0, 2.0, 3.0, 4.0];
        let mut block_r = vec![-1.0, -2.0, -3.0, -4.0];
        let mut delay_l = vec![0.0; 8];
        let mut delay_r = vec![0.0; 8];
        let mut pos = 0usize;
        apply_pdc_delay_block(
            &mut block_l,
            &mut block_r,
            &mut delay_l,
            &mut delay_r,
            &mut pos,
            2,
            4,
        );
        assert_eq!(block_l[0], 0.0);
        assert_eq!(block_l[1], 0.0);
        assert_eq!(block_l[2], 1.0);
        assert_eq!(block_l[3], 2.0);
    }
}
