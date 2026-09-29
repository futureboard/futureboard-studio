//! Runtime audio graph planning and validation (Phase O / Audio Graph).
//!
//! Builds a declarative view of the stereo processing graph from runtime
//! tracks, detects routing cycles before the graph reaches the audio callback,
//! and produces a topological Pass-2 order for bus/return tracks.

use std::collections::{HashMap, VecDeque};

use crate::runtime::RuntimeTrack;

/// Node kinds from `tasks/native/audio-system-plan.md` §8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioGraphNodeKind {
    AudioInput,
    AudioClip,
    MidiClip,
    Instrument,
    InsertPlugin,
    TrackMixer,
    Send,
    ReturnTrack,
    BusTrack,
    GroupTrack,
    Master,
    Output,
    Meter,
}

impl AudioGraphNodeKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::AudioInput => "AudioInput",
            Self::AudioClip => "AudioClip",
            Self::MidiClip => "MidiClip",
            Self::Instrument => "Instrument",
            Self::InsertPlugin => "InsertPlugin",
            Self::TrackMixer => "TrackMixer",
            Self::Send => "Send",
            Self::ReturnTrack => "ReturnTrack",
            Self::BusTrack => "BusTrack",
            Self::GroupTrack => "GroupTrack",
            Self::Master => "Master",
            Self::Output => "Output",
            Self::Meter => "Meter",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioGraphNode {
    pub id: String,
    pub kind: AudioGraphNodeKind,
    /// Index into `RuntimeProject.tracks` when this node maps to a track strip.
    pub track_index: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphRouteKind {
    Send,
    MainOutput,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphRouteIssue {
    pub from_track_id: String,
    pub to_track_id: String,
    pub kind: GraphRouteKind,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphValidationError {
    pub message: String,
    pub cycles: Vec<Vec<String>>,
    pub rejected_routes: Vec<GraphRouteIssue>,
}

impl std::fmt::Display for GraphValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for GraphValidationError {}

/// Immutable runtime audio graph snapshot built off the audio thread.
#[derive(Debug, Clone, Default)]
pub struct RuntimeAudioGraph {
    pub nodes: Vec<AudioGraphNode>,
    /// Pass-1 source track indices (audio / midi / instrument), array order.
    pub pass1_source_indices: Vec<usize>,
    /// Source tracks that can produce audio without live input in the current
    /// runtime snapshot. Filled after clip and MIDI routes are resolved.
    pub active_source_mask: Vec<bool>,
    /// Round-robin cursor for bounded bridge-latency observation.
    pub latency_scan_cursor: usize,
    /// Pass-2 routing tracks in topological order (bus / return).
    pub pass2_routing_indices: Vec<usize>,
    pub master_index: Option<usize>,
    pub rejected_routes: Vec<GraphRouteIssue>,
    /// Scratch for the render pass: the tracks whose insert chains run in the
    /// current batch. Reserved here, on the control thread, for every track,
    /// so the callback only clears and pushes within capacity.
    pub chain_jobs: Vec<ChainJob>,
}

/// One track whose insert chain runs in a render batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainJob {
    pub track_index: usize,
    /// `false` for a muted or solo-silenced instrument track: its chain still
    /// runs, so its notes keep moving, but its block is discarded.
    pub audible: bool,
}

pub fn is_routing_track_type(track_type: &str) -> bool {
    matches!(track_type, "bus" | "return" | "group")
}

/// True when adding `src → tgt` would close a cycle in the current adjacency.
fn edge_would_cycle(adjacency: &[Vec<usize>], src: usize, tgt: usize) -> bool {
    if src == tgt {
        return true;
    }
    // Reachability from tgt back to src means src→tgt closes a loop.
    let mut stack = vec![tgt];
    let mut seen = vec![false; adjacency.len()];
    while let Some(node) = stack.pop() {
        if node == src {
            return true;
        }
        if seen[node] {
            continue;
        }
        seen[node] = true;
        stack.extend(adjacency[node].iter().copied());
    }
    false
}

pub fn is_master_track_type(track_type: &str) -> bool {
    track_type == "master"
}

fn track_mixer_kind(track_type: &str) -> AudioGraphNodeKind {
    match track_type {
        "return" => AudioGraphNodeKind::ReturnTrack,
        "bus" => AudioGraphNodeKind::BusTrack,
        "group" => AudioGraphNodeKind::GroupTrack,
        "master" => AudioGraphNodeKind::Master,
        "instrument" => AudioGraphNodeKind::Instrument,
        _ => AudioGraphNodeKind::TrackMixer,
    }
}

/// Build the graph plan from prepared runtime tracks. Fails when a routing
/// cycle is detected among bus/return/group edges.
pub fn plan_runtime_audio_graph(
    tracks: &[RuntimeTrack],
) -> Result<RuntimeAudioGraph, GraphValidationError> {
    let mut nodes = Vec::new();
    let mut id_to_index: HashMap<String, usize> = HashMap::new();
    let mut pass1_source_indices = Vec::new();
    let mut routing_indices = Vec::new();
    let mut master_index = None;
    let mut rejected_routes = Vec::new();

    for (idx, track) in tracks.iter().enumerate() {
        id_to_index.insert(track.id.clone(), idx);
        if is_master_track_type(&track.track_type) {
            master_index = Some(idx);
        }
        nodes.push(AudioGraphNode {
            id: track.id.clone(),
            kind: track_mixer_kind(&track.track_type),
            track_index: Some(idx),
        });
        for insert in &track.inserts {
            nodes.push(AudioGraphNode {
                id: format!("{}:insert:{}", track.id, insert.id),
                kind: AudioGraphNodeKind::InsertPlugin,
                track_index: Some(idx),
            });
        }
        nodes.push(AudioGraphNode {
            id: format!("{}:meter", track.id),
            kind: AudioGraphNodeKind::Meter,
            track_index: Some(idx),
        });
        for send in &track.sends {
            nodes.push(AudioGraphNode {
                id: format!("{}:send:{}", track.id, send.id),
                kind: AudioGraphNodeKind::Send,
                track_index: Some(idx),
            });
        }

        if is_master_track_type(&track.track_type) {
            continue;
        }
        if is_routing_track_type(&track.track_type) {
            routing_indices.push(idx);
        } else {
            pass1_source_indices.push(idx);
        }
    }

    nodes.push(AudioGraphNode {
        id: "master-output".to_string(),
        kind: AudioGraphNodeKind::Output,
        track_index: master_index,
    });

    let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); tracks.len()];

    for (src_idx, track) in tracks.iter().enumerate() {
        if is_master_track_type(&track.track_type) {
            continue;
        }
        let src_routing = is_routing_track_type(&track.track_type);

        for send in &track.sends {
            if !send.enabled {
                continue;
            }
            let Some(tgt_idx) = id_to_index.get(&send.return_track_id).copied() else {
                rejected_routes.push(GraphRouteIssue {
                    from_track_id: track.id.clone(),
                    to_track_id: send.return_track_id.clone(),
                    kind: GraphRouteKind::Send,
                    reason: "send target track not found".to_string(),
                });
                continue;
            };
            if tgt_idx == src_idx {
                rejected_routes.push(GraphRouteIssue {
                    from_track_id: track.id.clone(),
                    to_track_id: send.return_track_id.clone(),
                    kind: GraphRouteKind::Send,
                    reason: "return self-send".to_string(),
                });
                continue;
            }
            if !is_routing_track_type(&tracks[tgt_idx].track_type) {
                rejected_routes.push(GraphRouteIssue {
                    from_track_id: track.id.clone(),
                    to_track_id: send.return_track_id.clone(),
                    kind: GraphRouteKind::Send,
                    reason: "send target is not a bus/return/group track".to_string(),
                });
                continue;
            }
            if src_routing && edge_would_cycle(&adjacency, src_idx, tgt_idx) {
                rejected_routes.push(GraphRouteIssue {
                    from_track_id: track.id.clone(),
                    to_track_id: send.return_track_id.clone(),
                    kind: GraphRouteKind::Send,
                    reason: "send would create a routing cycle".to_string(),
                });
                continue;
            }
            adjacency[src_idx].push(tgt_idx);
        }

        if let Some(output_id) = track.output_track_id.as_deref() {
            if output_id.is_empty() || output_id.eq_ignore_ascii_case("master") {
                continue;
            }
            let Some(tgt_idx) = id_to_index.get(output_id).copied() else {
                rejected_routes.push(GraphRouteIssue {
                    from_track_id: track.id.clone(),
                    to_track_id: output_id.to_string(),
                    kind: GraphRouteKind::MainOutput,
                    reason: "output target track not found".to_string(),
                });
                continue;
            };
            if tgt_idx == src_idx {
                rejected_routes.push(GraphRouteIssue {
                    from_track_id: track.id.clone(),
                    to_track_id: output_id.to_string(),
                    kind: GraphRouteKind::MainOutput,
                    reason: "track cannot route output to itself".to_string(),
                });
                continue;
            }
            if !is_routing_track_type(&tracks[tgt_idx].track_type) {
                rejected_routes.push(GraphRouteIssue {
                    from_track_id: track.id.clone(),
                    to_track_id: output_id.to_string(),
                    kind: GraphRouteKind::MainOutput,
                    reason: "output target is not a bus/return/group track".to_string(),
                });
                continue;
            }
            if src_routing && edge_would_cycle(&adjacency, src_idx, tgt_idx) {
                rejected_routes.push(GraphRouteIssue {
                    from_track_id: track.id.clone(),
                    to_track_id: output_id.to_string(),
                    kind: GraphRouteKind::MainOutput,
                    reason: "output route would create a routing cycle".to_string(),
                });
                continue;
            }
            adjacency[src_idx].push(tgt_idx);
        }
    }

    let cycles = find_cycles(&adjacency, tracks);
    if !cycles.is_empty() {
        return Err(GraphValidationError {
            message: format!("routing graph contains {} cycle(s)", cycles.len()),
            cycles,
            rejected_routes,
        });
    }

    let pass2_routing_indices = topological_sort_routing(&routing_indices, &adjacency);

    Ok(RuntimeAudioGraph {
        nodes,
        pass1_source_indices,
        active_source_mask: vec![false; tracks.len()],
        latency_scan_cursor: 0,
        pass2_routing_indices,
        master_index,
        rejected_routes,
        chain_jobs: Vec::with_capacity(tracks.len()),
    })
}

fn find_cycles(adjacency: &[Vec<usize>], tracks: &[RuntimeTrack]) -> Vec<Vec<String>> {
    let n = adjacency.len();
    let mut state = vec![0u8; n]; // 0=unseen, 1=stack, 2=done
    let mut stack = Vec::new();
    let mut cycles = Vec::new();

    for start in 0..n {
        if state[start] != 0 {
            continue;
        }
        dfs_cycle(
            start,
            adjacency,
            tracks,
            &mut state,
            &mut stack,
            &mut cycles,
        );
    }
    cycles
}

fn dfs_cycle(
    node: usize,
    adjacency: &[Vec<usize>],
    tracks: &[RuntimeTrack],
    state: &mut [u8],
    stack: &mut Vec<usize>,
    cycles: &mut Vec<Vec<String>>,
) {
    state[node] = 1;
    stack.push(node);
    for &next in &adjacency[node] {
        match state[next] {
            0 => dfs_cycle(next, adjacency, tracks, state, stack, cycles),
            1 => {
                let pos = stack.iter().position(|&v| v == next).unwrap_or(0);
                let cycle_ids: Vec<String> = stack[pos..]
                    .iter()
                    .chain(std::iter::once(&next))
                    .map(|&idx| tracks[idx].id.clone())
                    .collect();
                if !cycles.iter().any(|c| c == &cycle_ids) {
                    cycles.push(cycle_ids);
                }
            }
            _ => {}
        }
    }
    stack.pop();
    state[node] = 2;
}

fn topological_sort_routing(routing_indices: &[usize], adjacency: &[Vec<usize>]) -> Vec<usize> {
    let mut in_degree: HashMap<usize, usize> = routing_indices
        .iter()
        .copied()
        .map(|idx| (idx, 0usize))
        .collect();
    for &src in routing_indices {
        for &tgt in &adjacency[src] {
            if is_routing_track_index(tgt, routing_indices) {
                *in_degree.entry(tgt).or_insert(0) += 1;
            }
        }
    }

    let mut queue: VecDeque<usize> = routing_indices
        .iter()
        .copied()
        .filter(|idx| in_degree.get(idx).copied().unwrap_or(0) == 0)
        .collect();

    let mut order = Vec::with_capacity(routing_indices.len());
    while let Some(node) = queue.pop_front() {
        if !routing_indices.contains(&node) {
            continue;
        }
        order.push(node);
        for &tgt in &adjacency[node] {
            if let Some(deg) = in_degree.get_mut(&tgt) {
                *deg = deg.saturating_sub(1);
                if *deg == 0 {
                    queue.push_back(tgt);
                }
            }
        }
    }

    if order.len() != routing_indices.len() {
        // Should not happen once cycles are rejected; preserve stable fallback.
        let mut fallback = routing_indices.to_vec();
        fallback.sort_unstable();
        return fallback;
    }
    order
}

fn is_routing_track_index(idx: usize, routing_indices: &[usize]) -> bool {
    routing_indices.contains(&idx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{RuntimeSend, RuntimeTrack};

    fn track(id: &str, ty: &str, sends: Vec<RuntimeSend>, output: Option<&str>) -> RuntimeTrack {
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
            preview_mode: crate::runtime::RuntimePreviewMode::Stereo,
            output_track_id: output.map(str::to_string),
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
            plugin_latency_samples: 0,
            pdc_delay_l: Vec::new(),
            pdc_delay_r: Vec::new(),
            pdc_write_pos: 0,
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
    fn rejects_cyclic_bus_send() {
        let tracks = vec![
            track("a", "bus", vec![send("s1", "b")], Some("b")),
            track("b", "return", vec![send("s2", "a")], None),
        ];
        let graph = plan_runtime_audio_graph(&tracks).unwrap();
        // a→b is accepted; b→a closes a cycle and is rejected.
        assert!(graph.rejected_routes.iter().any(|r| {
            r.from_track_id == "b" && r.to_track_id == "a" && r.kind == GraphRouteKind::Send
        }));
        let ids: Vec<_> = graph
            .pass2_routing_indices
            .iter()
            .map(|&i| tracks[i].id.as_str())
            .collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn accepts_reverse_index_bus_chain_without_cycle() {
        // Return created before bus (lower index), bus sends into return.
        let tracks = vec![
            track("ret", "return", vec![], None),
            track("bus", "bus", vec![send("s", "ret")], None),
        ];
        let graph = plan_runtime_audio_graph(&tracks).unwrap();
        assert!(graph.rejected_routes.is_empty());
        let ids: Vec<_> = graph
            .pass2_routing_indices
            .iter()
            .map(|&i| tracks[i].id.as_str())
            .collect();
        assert_eq!(ids, vec!["bus", "ret"]);
    }

    #[test]
    fn find_cycles_detects_routing_loop() {
        let tracks = vec![
            track("a", "bus", vec![], None),
            track("b", "bus", vec![], None),
            track("c", "return", vec![], None),
        ];
        let adjacency = vec![vec![1], vec![2], vec![0]];
        let cycles = find_cycles(&adjacency, &tracks);
        assert!(!cycles.is_empty());
    }

    #[test]
    fn forward_bus_chain_topologically_sorted() {
        let tracks = vec![
            track("src", "audio", vec![send("s", "bus1")], None),
            track("bus1", "bus", vec![send("s2", "ret")], None),
            track("ret", "return", vec![], None),
        ];
        let graph = plan_runtime_audio_graph(&tracks).unwrap();
        let ids: Vec<_> = graph
            .pass2_routing_indices
            .iter()
            .map(|&i| tracks[i].id.as_str())
            .collect();
        assert_eq!(ids, vec!["bus1", "ret"]);
    }

    #[test]
    fn rejects_send_to_audio_track() {
        let tracks = vec![
            track("a", "audio", vec![send("s", "b")], None),
            track("b", "audio", vec![], None),
        ];
        let graph = plan_runtime_audio_graph(&tracks).unwrap();
        assert!(graph
            .rejected_routes
            .iter()
            .any(|r| r.kind == GraphRouteKind::Send));
    }

    #[test]
    fn pass1_lists_non_routing_sources() {
        let tracks = vec![
            track("a1", "audio", vec![], None),
            track("m1", "midi", vec![], None),
            track("bus", "bus", vec![], None),
            track("master", "master", vec![], None),
        ];
        let graph = plan_runtime_audio_graph(&tracks).unwrap();
        let pass1: Vec<_> = graph
            .pass1_source_indices
            .iter()
            .map(|&i| tracks[i].id.as_str())
            .collect();
        assert_eq!(pass1, vec!["a1", "m1"]);
        assert_eq!(graph.master_index, Some(3));
    }
}
