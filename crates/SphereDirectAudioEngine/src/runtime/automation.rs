//! Automation lanes, curves, targets and plug-in parameter bindings.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeAutomationCurve {
    Linear,
    Hold,
    Smooth,
}

impl RuntimeAutomationCurve {
    #[inline]
    pub(super) fn from_tag(tag: u8) -> Self {
        match tag {
            1 => Self::Hold,
            2 => Self::Smooth,
            _ => Self::Linear,
        }
    }

    #[inline]
    pub fn to_tag(self) -> u8 {
        match self {
            Self::Linear => 0,
            Self::Hold => 1,
            Self::Smooth => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeAutomationTarget {
    TrackVolume,
    TrackPan,
    TrackMute,
    PluginParameter {
        insert_id: String,
        parameter_id: String,
    },
    SendGain {
        send_id: String,
    },
    /// A send's on/off switch: 0.5 and above is on.
    SendEnabled {
        send_id: String,
    },
    /// An insert's bypass: 0.5 and above is bypassed.
    InsertBypass {
        insert_id: String,
    },
    Unresolved,
}

impl RuntimeAutomationTarget {
    pub(super) fn from_snapshot(lane: &EngineAutomationLaneSnapshot) -> Self {
        match lane.target.tag {
            0 => Self::TrackVolume,
            1 => Self::TrackPan,
            2 => Self::TrackMute,
            3 if !lane.target.insert_id.is_empty() && !lane.target.parameter_id.is_empty() => {
                Self::PluginParameter {
                    insert_id: lane.target.insert_id.clone(),
                    parameter_id: lane.target.parameter_id.clone(),
                }
            }
            4 if !lane.target.send_id.is_empty() => Self::SendGain {
                send_id: lane.target.send_id.clone(),
            },
            5 if !lane.target.send_id.is_empty() => Self::SendEnabled {
                send_id: lane.target.send_id.clone(),
            },
            6 if !lane.target.insert_id.is_empty() => Self::InsertBypass {
                insert_id: lane.target.insert_id.clone(),
            },
            _ => Self::Unresolved,
        }
    }

    #[inline]
    pub fn default_value(&self) -> f32 {
        match self {
            Self::TrackVolume => volume_db_to_norm(0.0),
            Self::TrackPan => 0.5,
            Self::TrackMute => 0.0,
            Self::PluginParameter { .. } => 0.5,
            Self::SendGain { .. } => volume_db_to_norm(0.0),
            Self::SendEnabled { .. } => 1.0,
            Self::InsertBypass { .. } => 0.0,
            Self::Unresolved => 0.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeAutomationPoint {
    pub beat: f64,
    pub value: f32,
    pub curve: RuntimeAutomationCurve,
    /// Per-segment tension in `-1.0..=1.0` (see [`automation_curve_factor`]).
    pub tension: f32,
}

#[derive(Debug, Clone)]
pub struct RuntimeAutomationLane {
    pub id: String,
    pub name: String,
    pub target: RuntimeAutomationTarget,
    pub enabled: bool,
    pub points: Vec<RuntimeAutomationPoint>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct RuntimeTrackAutomationValues {
    pub volume: Option<f32>,
    pub pan: Option<f32>,
    pub muted: Option<bool>,
}

/// One resolved plugin-parameter automation route, built off the audio thread
/// by [`RuntimeProject::resolve_indices`] so the render path never parses a
/// string param id or searches for an insert by id per block. `lane_ix` /
/// `insert_ix` index into the owning track's `automation_lanes` / `inserts`.
#[derive(Debug, Clone)]
pub struct RuntimePluginParamBinding {
    pub insert_ix: usize,
    pub lane_ix: usize,
    /// Real VST3 `ParamID`.
    pub param_id: u32,
    /// Last normalized value pushed to the plugin; starts `NaN` so the first
    /// block after a (re)build always emits. Dedupe avoids flooding the param
    /// ring with identical values every block.
    pub last_value: f32,
}

/// Values that differ by less than this are treated as unchanged for plugin
/// parameter automation dedupe (well below VST3's typical step resolution).
pub const PLUGIN_PARAM_AUTOMATION_EPS: f32 = 1e-5;

impl RuntimeAutomationLane {
    pub(super) fn from_snapshot(lane: &EngineAutomationLaneSnapshot) -> Self {
        let mut points: Vec<RuntimeAutomationPoint> = lane
            .points
            .iter()
            .map(|point| RuntimeAutomationPoint {
                beat: point.beat.max(0.0),
                value: point.value.clamp(0.0, 1.0),
                curve: RuntimeAutomationCurve::from_tag(point.curve),
                tension: point.tension.clamp(-1.0, 1.0),
            })
            .collect();
        points.sort_by(|a, b| a.beat.total_cmp(&b.beat));
        Self {
            id: lane.id.clone(),
            name: lane.name.clone(),
            target: RuntimeAutomationTarget::from_snapshot(lane),
            enabled: lane.enabled,
            points,
        }
    }

    #[inline]
    /// The lane's value at `beat`, or `None` when the lane has nothing to say.
    ///
    /// An **empty** lane says nothing. It used to answer with
    /// [`RuntimeAutomationTarget::default_value`], and for a track lane that
    /// answer *wins*: `apply_fader` reads
    /// `automation.volume.unwrap_or(track.volume)`, so an enabled Track Volume
    /// lane carrying no points pinned the channel at its default 0 dB and threw
    /// the fader away. `SetTrackVolume` still landed — `update_track_volume`
    /// reported `applied=true` with the right linear gain — it was simply never
    /// read again, which is why the fader moved, the meter followed the lane and
    /// nothing got quieter. Pan and Mute lanes had the same hole: an empty lane
    /// forced centre and un-muted.
    ///
    /// [`build_plugin_param_bindings`] already skipped empty lanes for exactly
    /// this reason ("a freshly added empty lane never forces the parameter to its
    /// default value"); the track lanes now agree. A lane with points still
    /// overrides the fader, which is what Read mode is supposed to do.
    pub fn evaluate_normalized(&self, beat: f64) -> Option<f32> {
        if !self.enabled
            || self.points.is_empty()
            || matches!(self.target, RuntimeAutomationTarget::Unresolved)
        {
            return None;
        }
        Some(evaluate_automation_points(
            &self.points,
            beat,
            self.target.default_value(),
        ))
    }
}

/// Shape factor for one automation segment — the single source of truth for
/// curve math. Maps a normalized position `t` in `[0, 1]` between a segment's
/// left point and right point to an eased interpolation factor in `[0, 1]`, so
/// the value is `a.value + (b.value - a.value) * factor`. Realtime playback, the
/// offline exporter, and the UI lane renderer all call this, so the heard curve,
/// the bounced curve, and the drawn curve agree exactly (no visual-only curves).
///
/// `curve_tag`: `1` = Hold (stepped, holds the left value), `2` = Smooth
/// (S-curve), anything else = Linear shaped by `tension`.
///
/// `tension` in `[-1, 1]`:
/// - Linear: `0` = straight line; `> 0` eases in (exponential, slow start);
///   `< 0` eases out (logarithmic, fast start).
/// - Smooth: `0` = the classic smoothstep S; `> 0` sharpens it (flatter ends,
///   steeper middle, towards a step at the midpoint); `< 0` softens it
///   (steeper ends, a flatter middle, an inverted S). Always symmetric about
///   the segment's midpoint, always monotonic.
/// - Hold: ignored.
#[inline]
pub fn automation_curve_factor(curve_tag: u8, tension: f32, t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    match curve_tag {
        // Hold: stay at the left value for the whole segment (stepped).
        1 => 0.0,
        // Smooth: smoothstep over a symmetric warp of `t`. The warp is the
        // identity at tension 0, so a pre-tension project draws and plays
        // exactly the S it always did.
        2 => {
            let k = tension.clamp(-1.0, 1.0);
            let t = if k.abs() < 1.0e-4 {
                t
            } else {
                // Each half is a power curve mirrored about (0.5, 0.5):
                // exponent > 1 pulls the ends flat (sharper S), < 1 pushes
                // them steep (softer, inverted S).
                let p = 2f32.powf(k * 1.5);
                if t < 0.5 {
                    0.5 * (2.0 * t).powf(p)
                } else {
                    1.0 - 0.5 * (2.0 * (1.0 - t)).powf(p)
                }
            };
            t * t * (3.0 - 2.0 * t)
        }
        // Linear / curved: a power curve driven by tension.
        _ => {
            let k = tension.clamp(-1.0, 1.0);
            if k.abs() < 1.0e-4 {
                t
            } else {
                // exponent > 1 for k > 0 (ease-in), < 1 for k < 0 (ease-out);
                // symmetric in log space, so +k and -k are mirror curves.
                t.powf(2f32.powf(k * 2.5))
            }
        }
    }
}

/// Evaluate a sorted automation point list without allocating. Empty lanes use
/// `default`; before/after the authored range, the nearest point is held. The
/// segment shape comes from the left point's curve/tension via
/// [`automation_curve_factor`].
pub fn evaluate_automation_points(
    points: &[RuntimeAutomationPoint],
    beat: f64,
    default: f32,
) -> f32 {
    if points.is_empty() {
        return default.clamp(0.0, 1.0);
    }
    let beat = beat.max(0.0);
    if beat <= points[0].beat {
        return points[0].value;
    }
    let last = points.len() - 1;
    if beat >= points[last].beat {
        return points[last].value;
    }
    // Binary search for the segment: a recorded lane holds thousands of
    // points, and this runs for every lane of every track on every block, so
    // a linear scan made playback dearer the further it got into the song.
    // `ix` is the first point after `beat`; both ends were handled above, so
    // `1 <= ix <= last`.
    let ix = points
        .partition_point(|point| point.beat <= beat)
        .clamp(1, last);
    let a = &points[ix - 1];
    let b = &points[ix];
    let span = (b.beat - a.beat).max(f64::EPSILON);
    let t = ((beat - a.beat) / span).clamp(0.0, 1.0) as f32;
    let factor = automation_curve_factor(a.curve.to_tag(), a.tension, t);
    a.value + (b.value - a.value) * factor
}

/// Where on the timeline each automation stage of one track is this block.
///
/// A track's audio reaches its fader `track_output_latency` samples after the
/// timeline position it came from (its plug-ins delay it), so reading its
/// lanes at the block's own position played every move early by that much.
/// The render pass fills this per track at the start of each block
/// (`stamp_automation_clocks`) and every automation read goes through it.
/// Kept as offsets from the block's beat, so the default (a graph nobody
/// stamped: tests, a stopped transport) reads every stage at the block beat,
/// exactly as before.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct RuntimeAutomationClock {
    /// Beats the audio at the fader, sends and mute lags the block's
    /// position by (the track's output latency).
    pub fader_lag_beats: f64,
    /// Beats the audio entering the insert chain lags it by (what a bus's
    /// feeds were delayed by): plug-in parameter and bypass lanes.
    pub chain_lag_beats: f64,
    /// From the block's start to its end, in beats — negative when a loop
    /// wraps inside the block. The fader ramps to the lane's value at the
    /// end, so a ramp is followed across the block instead of a block late.
    pub block_end_beats: f64,
}

impl RuntimeAutomationClock {
    /// Where the fader, sends and mute read their lanes this block.
    #[inline]
    pub fn fader_beat(&self, block_beat: f64) -> f64 {
        (block_beat - self.fader_lag_beats).max(0.0)
    }

    /// What the fader ramps toward: its lanes at the block's end.
    #[inline]
    pub fn fader_end_beat(&self, block_beat: f64) -> f64 {
        (block_beat + self.block_end_beats - self.fader_lag_beats).max(0.0)
    }

    /// Where plug-in parameter and bypass lanes are read this block.
    #[inline]
    pub fn chain_beat(&self, block_beat: f64) -> f64 {
        (block_beat - self.chain_lag_beats).max(0.0)
    }
}

/// A track's automation, prepared on the control thread so the callback
/// only swaps it in ([`RuntimeProject::install_track_automation`]): the lanes,
/// their plug-in bindings and the lane each send and insert reads, resolved to
/// indices. What it replaces goes back into this box and leaves through the
/// graveyard.
#[derive(Debug, Default)]
pub struct TrackAutomationChange {
    pub track_index: usize,
    pub lanes: Vec<RuntimeAutomationLane>,
    pub bindings: Vec<RuntimePluginParamBinding>,
    /// Per send, in the track's send order: `(level lane, on/off lane)`.
    pub send_lanes: Vec<(Option<usize>, Option<usize>)>,
    /// Per insert, in the track's insert order: its bypass lane.
    pub bypass_lanes: Vec<Option<usize>>,
}

impl TrackAutomationChange {
    /// Resolve `lanes` against `track`'s inserts and sends. Control thread.
    pub fn prepare(
        track_index: usize,
        track: &RuntimeTrack,
        lanes: &[EngineAutomationLaneSnapshot],
    ) -> Self {
        let lanes: Vec<RuntimeAutomationLane> = lanes
            .iter()
            .map(RuntimeAutomationLane::from_snapshot)
            .collect();
        let bindings = plugin_param_bindings(&lanes, &track.inserts);
        let (send_lanes, bypass_lanes) = lane_indices(&lanes, &track.sends, &track.inserts);
        Self {
            track_index,
            lanes,
            bindings,
            send_lanes,
            bypass_lanes,
        }
    }
}

/// Which lane each send (`(level, on/off)`) and each insert (bypass) reads,
/// as indices into `lanes`. Off the audio thread.
pub(crate) fn lane_indices(
    lanes: &[RuntimeAutomationLane],
    sends: &[RuntimeSend],
    inserts: &[RuntimeInsert],
) -> (Vec<(Option<usize>, Option<usize>)>, Vec<Option<usize>>) {
    let mut send_lanes = vec![(None, None); sends.len()];
    let mut bypass_lanes = vec![None; inserts.len()];
    for (lane_ix, lane) in lanes.iter().enumerate() {
        match &lane.target {
            RuntimeAutomationTarget::SendGain { send_id } => {
                if let Some(s) = sends.iter().position(|s| &s.id == send_id) {
                    send_lanes[s].0 = Some(lane_ix);
                }
            }
            RuntimeAutomationTarget::SendEnabled { send_id } => {
                if let Some(s) = sends.iter().position(|s| &s.id == send_id) {
                    send_lanes[s].1 = Some(lane_ix);
                }
            }
            RuntimeAutomationTarget::InsertBypass { insert_id } => {
                if let Some(i) = inserts.iter().position(|i| &i.id == insert_id) {
                    bypass_lanes[i] = Some(lane_ix);
                }
            }
            _ => {}
        }
    }
    (send_lanes, bypass_lanes)
}

/// Resolve plug-in parameter lanes into compact bindings. Off the audio
/// thread, so allocation and string parsing are fine. Lanes with no points
/// are skipped so a freshly added empty lane never forces the parameter to
/// its default value.
pub(crate) fn plugin_param_bindings(
    lanes: &[RuntimeAutomationLane],
    inserts: &[RuntimeInsert],
) -> Vec<RuntimePluginParamBinding> {
    let mut out: Vec<RuntimePluginParamBinding> = Vec::new();
    for (lane_ix, lane) in lanes.iter().enumerate() {
        if !lane.enabled || lane.points.is_empty() {
            continue;
        }
        let RuntimeAutomationTarget::PluginParameter {
            insert_id,
            parameter_id,
        } = &lane.target
        else {
            continue;
        };
        let Some(insert_ix) = inserts.iter().position(|i| &i.id == insert_id) else {
            continue;
        };
        let Ok(param_id) = parameter_id.parse::<u32>() else {
            continue;
        };
        out.push(RuntimePluginParamBinding {
            insert_ix,
            lane_ix,
            param_id,
            last_value: f32::NAN,
        });
    }
    out
}

impl RuntimeProject {
    /// Swap a prepared [`TrackAutomationChange`] into its track. Realtime-safe:
    /// moves and index writes only; the replaced lanes and bindings go back
    /// into `change` for the caller to retire. A change prepared against a
    /// different shape of track (the graph was rebuilt meanwhile) is ignored —
    /// the rebuild carried the lanes in already.
    pub fn install_track_automation(&mut self, change: &mut TrackAutomationChange) -> bool {
        let Some(track) = self.tracks.get_mut(change.track_index) else {
            return false;
        };
        if track.sends.len() != change.send_lanes.len()
            || track.inserts.len() != change.bypass_lanes.len()
            || change
                .bindings
                .iter()
                .any(|binding| binding.insert_ix >= track.inserts.len())
        {
            return false;
        }
        std::mem::swap(&mut track.automation_lanes, &mut change.lanes);
        std::mem::swap(&mut track.plugin_param_automation, &mut change.bindings);
        for (send, &(level, enabled)) in track.sends.iter_mut().zip(&change.send_lanes) {
            send.level_lane = level;
            send.enabled_lane = enabled;
        }
        for (insert, &lane) in track.inserts.iter_mut().zip(&change.bypass_lanes) {
            insert.bypass_lane = lane;
            if lane.is_none() {
                insert.automation_bypass = None;
            }
        }
        true
    }
}

pub const AUTOMATION_VOLUME_MIN_DB: f32 = -60.0;

pub const AUTOMATION_VOLUME_MAX_DB: f32 = 6.0;

#[inline]
pub fn volume_db_to_norm(db: f32) -> f32 {
    ((db - AUTOMATION_VOLUME_MIN_DB) / (AUTOMATION_VOLUME_MAX_DB - AUTOMATION_VOLUME_MIN_DB))
        .clamp(0.0, 1.0)
}

#[inline]
pub fn volume_norm_to_linear(norm: f32) -> f32 {
    let norm = norm.clamp(0.0, 1.0);
    let db =
        AUTOMATION_VOLUME_MIN_DB + norm * (AUTOMATION_VOLUME_MAX_DB - AUTOMATION_VOLUME_MIN_DB);
    if norm <= 0.001 || db <= AUTOMATION_VOLUME_MIN_DB + 0.05 {
        0.0
    } else {
        10.0_f32.powf(db / 20.0).clamp(0.0, 2.0)
    }
}
