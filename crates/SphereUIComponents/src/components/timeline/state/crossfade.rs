//! Clip fades and the automatic crossfades between overlapping audio clips.
//!
//! One answer shared by everything that draws, edits or plays a fade — the
//! arrangement's clips and crossfade overlays, the Inspector, the Audio Editor
//! and the engine snapshot — so what is drawn is what plays:
//!
//! - A clip's fades are lengths of the clip's own playing time, in
//!   milliseconds. [`TimelineState::clip_beat_at_local_seconds`] and its
//!   inverse put that time on the beat axis every lane is drawn on, through
//!   the tempo map, so a fade is drawn and grabbed where it is heard.
//! - Two audio clips on one track crossfade over their overlap when they are
//!   staggered: the later one starts *and* ends later. On such an edge the
//!   effective fade is exactly the overlap; the manual fade stored there is
//!   kept but does not apply while the overlap exists.
//! - Equal starts and containment (one clip inside another) make no
//!   crossfade: a fade-in at the start and a fade-out at the end cannot
//!   describe them.
//! - The engine also crossfades overlapping reference-video clips, whose
//!   sound it plays ([`TimelineState::engine_crossfades`]); the arrangement
//!   draws audio crossfades only.

use super::*;

/// Shortest crossfade the planner leaves, so a drag cannot remove one.
pub const MIN_CROSSFADE_SECONDS: f64 = 0.001;

/// Overlaps shorter than this are rounding between abutting clips (a split's
/// halves meet on a whole sample, not on an exact beat), not crossfades.
pub const CROSSFADE_MIN_OVERLAP_SECONDS: f64 = 0.0005;

/// Length `audio:create-crossfade` gives a new crossfade, and the length a
/// double-click on a crossfade handle resets to.
pub const DEFAULT_CROSSFADE_SECONDS: f64 = 0.020;

/// Largest gap between two clips that `audio:create-crossfade` still treats
/// as touching.
pub const CROSSFADE_NEAR_ABUT_SECONDS: f64 = 0.050;

/// Which fade of a clip: the fade-in at its start or the fade-out at its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FadeEdge {
    In,
    Out,
}

/// One automatic crossfade: the staggered overlap of two audio clips on a
/// track. `left` starts first.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioCrossfade {
    pub left_id: String,
    pub right_id: String,
    pub start_beat: f64,
    pub end_beat: f64,
    /// Length of the overlap in real time, through the tempo map.
    pub seconds: f64,
    /// The overlap in the left clip's own playing time: its fade-out.
    pub left_seconds: f64,
    /// The overlap in the right clip's own playing time: its fade-in.
    pub right_seconds: f64,
}

/// Every crossfade on one track, resolved once per lane render or snapshot.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackCrossfades {
    pub crossfades: Vec<AudioCrossfade>,
}

impl TrackCrossfades {
    /// The crossfade length that replaces `clip_id`'s fade-in, if an earlier
    /// clip overlaps its start. With several, the longest covers the edge.
    pub fn fade_in_override(&self, clip_id: &str) -> Option<f64> {
        self.crossfades
            .iter()
            .filter(|xf| xf.right_id == clip_id)
            .map(|xf| xf.right_seconds)
            .reduce(f64::max)
    }

    /// The crossfade length that replaces `clip_id`'s fade-out.
    pub fn fade_out_override(&self, clip_id: &str) -> Option<f64> {
        self.crossfades
            .iter()
            .filter(|xf| xf.left_id == clip_id)
            .map(|xf| xf.left_seconds)
            .reduce(f64::max)
    }

    pub fn is_empty(&self) -> bool {
        self.crossfades.is_empty()
    }
}

/// The fades a clip actually plays, in seconds of its own playing time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EffectiveFades {
    pub played_seconds: f64,
    pub in_seconds: f64,
    pub out_seconds: f64,
    /// The fade-in is a crossfade's (the manual one does not apply).
    pub in_crossfade: bool,
    pub out_crossfade: bool,
}

impl EffectiveFades {
    pub fn seconds(&self, edge: FadeEdge) -> f64 {
        match edge {
            FadeEdge::In => self.in_seconds,
            FadeEdge::Out => self.out_seconds,
        }
    }

    pub fn crossfaded(&self, edge: FadeEdge) -> bool {
        match edge {
            FadeEdge::In => self.in_crossfade,
            FadeEdge::Out => self.out_crossfade,
        }
    }

    /// Longest the manual fade on `edge` may be: what the other edge leaves.
    pub fn max_manual_seconds(&self, edge: FadeEdge) -> f64 {
        let other = match edge {
            FadeEdge::In => self.out_seconds,
            FadeEdge::Out => self.in_seconds,
        };
        (self.played_seconds - other).max(0.0)
    }

    pub fn any(&self) -> bool {
        self.in_crossfade || self.out_crossfade
    }
}

/// What the Inspector shows for a clip's fades.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClipFadeSummary {
    pub fades: EffectiveFades,
    /// The clip is on an ARA track, whose plug-in renders the audio: the
    /// engine applies no clip fades, crossfades or clip gain there.
    pub ara: bool,
}

/// Resolve the fade lengths a clip plays, the way the engine clamps them
/// (fade-in first, then the fade-out in what is left), except that a
/// crossfaded edge is fixed first: an overlap is exactly as long as it is,
/// and the manual fade on the other edge gives way to it.
pub fn resolve_effective_fades(
    manual_in: f64,
    manual_out: f64,
    crossfade_in: Option<f64>,
    crossfade_out: Option<f64>,
    played: f64,
) -> (f64, f64) {
    let played = played.max(0.0);
    let manual_in = manual_in.max(0.0);
    let manual_out = manual_out.max(0.0);
    match (crossfade_in, crossfade_out) {
        (Some(xf_in), Some(xf_out)) => {
            let fade_in = fade_within(xf_in, played);
            (fade_in, fade_within(xf_out, played - fade_in))
        }
        (Some(xf_in), None) => {
            let fade_in = fade_within(xf_in, played);
            (fade_in, fade_within(manual_out, played - fade_in))
        }
        (None, Some(xf_out)) => {
            let fade_out = fade_within(xf_out, played);
            (fade_within(manual_in, played - fade_out), fade_out)
        }
        (None, None) => {
            let fade_in = fade_within(manual_in, played);
            (fade_in, fade_within(manual_out, played - fade_in))
        }
    }
}

/// `seconds` kept within `0..=room`. Unlike `f64::clamp` it cannot panic:
/// rounding may leave `room` a hair below zero, and a NaN reads as zero.
fn fade_within(seconds: f64, room: f64) -> f64 {
    seconds.max(0.0).min(room.max(0.0))
}

/// Clamp a requested manual fade so the two fades never overlap:
/// `other_seconds` is the effective fade on the other edge.
pub fn clamp_fade_seconds(requested: f64, other_seconds: f64, played: f64) -> f64 {
    let requested = if requested.is_finite() {
        requested
    } else {
        0.0
    };
    requested.clamp(0.0, (played - other_seconds.max(0.0)).max(0.0))
}

/// Whether `clip` takes part in automatic crossfades: an unmuted audio clip
/// with a real source, the same clips the engine renders.
pub fn crossfade_candidate(clip: &ClipState) -> bool {
    matches!(
        &clip.clip_type,
        ClipType::Audio {
            source_path: Some(path),
            ..
        } if !clip.muted && !path.trim().is_empty()
    )
}

/// Whether `clip` is an unmuted reference video with a source. The engine
/// plays a video's own sound (`engine_snapshot::clip_media_source`), and
/// overlapping video clips have always crossfaded it, so the engine keeps
/// doing so ([`TimelineState::engine_crossfades`]). The arrangement draws no
/// crossfade for them: a Video track has no fade handles or curves.
pub fn video_crossfade_candidate(clip: &ClipState) -> bool {
    matches!(
        &clip.clip_type,
        ClipType::Video {
            source_path: Some(path),
            ..
        } if !clip.muted && !path.trim().is_empty()
    )
}

/// Why [`TimelineState::plan_crossfade`] cannot resize a crossfade, so it
/// keeps its length: its handle is shown disabled with
/// [`Self::handle_tooltip`], and `audio:create-crossfade` answers with
/// [`Self::command_message`]. The crossfade over an existing overlap still
/// plays; only resizing it is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrossfadeBlock {
    /// Not an audio clip.
    NotAudio,
    /// Warp markers pin the clip's source window to beats.
    Warp,
    /// Reversed audio plays its window backwards, so revealing source at an
    /// edge would shift all of the clip's audio instead of extending it.
    Reversed,
    /// The source is not decoded yet, so its window is unknown.
    Undecoded,
    /// The trim lives only in a legacy beat offset: moving the window would
    /// make the clip jump to the start of its file.
    LegacyOffset,
    /// Both clips can be trimmed, but less than [`MIN_CROSSFADE_SECONDS`]
    /// fits between how far their edges reach into their files and the body
    /// each keeps outside the overlap. Only the pair has this block
    /// ([`TimelineState::crossfade_adjust_block`]), never one clip.
    NoRoom,
}

impl CrossfadeBlock {
    /// The status-bar answer when `audio:create-crossfade` meets this clip.
    pub fn command_message(self) -> &'static str {
        match self {
            Self::NotAudio => "Crossfades need two audio clips",
            Self::Warp => "Cannot crossfade Warp clips: warp markers pin their audio",
            Self::Reversed => "Cannot crossfade reversed clips",
            Self::Undecoded => "Cannot crossfade until the clips' audio has loaded",
            Self::LegacyOffset => {
                "Cannot crossfade a clip trimmed by an older project's beat offset"
            }
            Self::NoRoom => "No audio beyond the clip edges to crossfade over",
        }
    }

    /// Why a crossfade handle on this clip is disabled.
    pub fn handle_tooltip(self) -> &'static str {
        match self {
            Self::NotAudio => "Crossfade length is fixed: one side is not an audio clip",
            Self::Warp => "Crossfade length is fixed on Warp clips: warp markers pin their audio",
            Self::Reversed => "Crossfade length is fixed on reversed clips",
            Self::Undecoded => "Crossfade length can change once the clips' audio has loaded",
            Self::LegacyOffset => {
                "Crossfade length is fixed: a clip is trimmed by an older project's beat offset"
            }
            Self::NoRoom => "Crossfade length is fixed: the clips leave no room to resize it",
        }
    }
}

/// Why `clip`'s edge cannot move to reveal or hide its own source audio, or
/// `None` when it can: an audio clip that is not Warp, not reversed, decoded,
/// and trimmed by its sample window rather than a legacy beat offset.
pub fn crossfade_trim_block(clip: &ClipState) -> Option<CrossfadeBlock> {
    if !matches!(clip.clip_type, ClipType::Audio { .. }) {
        return Some(CrossfadeBlock::NotAudio);
    }
    let stretch = &clip.stretch;
    if stretch.mode == StretchMode::Warp {
        return Some(CrossfadeBlock::Warp);
    }
    if stretch.reverse {
        return Some(CrossfadeBlock::Reversed);
    }
    if stretch.source_sample_rate() == 0 || stretch.source_len_samples() == 0 {
        return Some(CrossfadeBlock::Undecoded);
    }
    if stretch.source_start_samples == 0 && clip.offset_beats > 0.0 {
        return Some(CrossfadeBlock::LegacyOffset);
    }
    None
}

/// Why the crossfade between `left` and `right` cannot be planned, the left
/// clip's reason first. `None` when [`TimelineState::plan_crossfade`] can
/// re-trim both, so a crossfade handle on them is live.
pub fn crossfade_pair_block(left: &ClipState, right: &ClipState) -> Option<CrossfadeBlock> {
    crossfade_trim_block(left).or_else(|| crossfade_trim_block(right))
}

/// The overlap `length_seconds` long whose centre is as near `center` as
/// `low..=high` allows: `(start, end, length)`, the length shrunk to the room
/// there is. `None` when less than [`MIN_CROSSFADE_SECONDS`] fits.
///
/// The latest centre is kept at or after the earliest: when the length is
/// the whole room, rounding can put `high - length / 2` an ulp before
/// `low + length / 2`, and `f64::clamp` panics on crossed bounds.
fn centred_overlap(
    center: f64,
    length_seconds: f64,
    low: f64,
    high: f64,
) -> Option<(f64, f64, f64)> {
    // `f64::min` ignores a NaN, so a NaN bound would pass as a finite length
    // and reach the clamp below as a NaN bound, which panics too.
    if !(low.is_finite() && high.is_finite() && center.is_finite()) {
        return None;
    }
    let length = length_seconds.min(high - low);
    if !length.is_finite() || length < MIN_CROSSFADE_SECONDS {
        return None;
    }
    let earliest = low + length * 0.5;
    let latest = (high - length * 0.5).max(earliest);
    let center = center.clamp(earliest, latest);
    Some((center - length * 0.5, center + length * 0.5, length))
}

/// Whether a gesture that started at `before` and ended at `after` changed
/// nothing a project keeps. `dirty` is a transient re-render flag (it always
/// loads `false`), so flipping it alone is no edit: no undo entry, no engine
/// reload.
pub fn clip_edit_is_noop(before: &ClipState, after: &ClipState) -> bool {
    if before.stretch.dirty == after.stretch.dirty {
        return before == after;
    }
    let mut after = after.clone();
    after.stretch.dirty = before.stretch.dirty;
    *before == after
}

/// Where a crossfade between two clips can lie: `low..=high` in real time,
/// at least [`MIN_CROSSFADE_SECONDS`] wide, with what the planner needs to
/// turn a length into source windows.
struct CrossfadeRoom {
    low: f64,
    high: f64,
    left_fps: f64,
    right_fps: f64,
    left_total: u64,
}

/// A crossfade the planner can build: both clips re-trimmed so they overlap
/// by `seconds`.
#[derive(Debug, Clone, PartialEq)]
pub struct CrossfadePlan {
    pub left: ClipState,
    pub right: ClipState,
    pub seconds: f64,
}

/// What `audio:create-crossfade` should do: crossfade `left_id` into
/// `right_id` over `length_seconds` centred on `center_seconds` (real time).
#[derive(Debug, Clone, PartialEq)]
pub struct CrossfadeRequest {
    pub left_id: String,
    pub right_id: String,
    pub center_seconds: f64,
    pub length_seconds: f64,
}

/// A clip's own playing time on the arrangement's beat axis, with the tempo
/// map resolved once: a fade curve samples it per pixel column, and looking
/// the cached map up per sample hashed every tempo point and took a lock each
/// time.
///
/// A clip that keeps its wall-clock length plays through the tempo map; a
/// Tempo Sync or Warp clip is defined in beats and plays at the project
/// tempo — the same split [`TimelineState::audio_clip_end_beat`] makes, so
/// the fades and the drawn clip share one axis.
///
/// [`TimelineState::clip_local_seconds_at_beat`] and its siblings build one
/// of these per call, so drawing through an axis held for a whole clip and
/// hit-testing through the per-call methods are the same arithmetic. A lane
/// builds one per clip per render, all from one [`TempoLookup`]
/// ([`TimelineState::clip_time_axis_in`]).
pub struct ClipTimeAxis<'a> {
    state: &'a TimelineState,
    tempo: TempoLookup<'a>,
    start_beat: f64,
    /// The project tempo, for a clip that plays at it.
    project_bpm: Option<f64>,
    /// The clip's start in real time, for a clip that plays through the
    /// tempo map.
    start_seconds: f64,
}

impl ClipTimeAxis<'_> {
    /// Seconds of the clip's playing time at timeline `beat` (negative before
    /// its start).
    pub fn local_seconds_at_beat(&self, beat: f64) -> f64 {
        match self.project_bpm {
            Some(bpm) => (beat - self.start_beat) * 60.0 / bpm,
            None => self.tempo.seconds_at_beat(beat) - self.start_seconds,
        }
    }

    /// Timeline beat `seconds` into the clip's playing time.
    pub fn beat_at_local_seconds(&self, seconds: f64) -> f64 {
        match self.project_bpm {
            Some(bpm) => (self.start_beat + seconds * bpm / 60.0).max(0.0),
            None => self.tempo.beat_at_seconds(self.start_seconds + seconds),
        }
    }

    /// Lane x of `seconds` into the clip's playing time.
    pub fn lane_x_at_local_seconds(&self, seconds: f64) -> f32 {
        self.state
            .beats_to_x(self.beat_at_local_seconds(seconds) as f32)
    }

    /// Seconds into the clip's playing time at lane x.
    pub fn local_seconds_at_lane_x(&self, lane_x: f32) -> f64 {
        self.local_seconds_at_beat(self.state.x_to_beat(lane_x))
    }

    /// Lane x is a straight scale of the clip's time: no tempo automation
    /// bends the axis, so a curve needs only its two ends placed.
    pub fn is_linear(&self) -> bool {
        !self.state.tempo_map.has_automation()
    }
}

impl TimelineState {
    fn follows_tempo_axis(clip: &ClipState) -> bool {
        matches!(clip.clip_type, ClipType::Audio { .. }) && clip.stretch.follows_project_tempo()
    }

    /// `clip`'s playing time on the beat axis, resolved once for a run of
    /// conversions. See [`ClipTimeAxis`].
    pub fn clip_time_axis(&self, clip: &ClipState) -> ClipTimeAxis<'_> {
        self.clip_time_axis_in(self.tempo_lookup(), clip)
    }

    /// [`Self::clip_time_axis`] through a tempo map already resolved, so a
    /// lane of clips resolves it once.
    pub fn clip_time_axis_in<'a>(
        &'a self,
        tempo: TempoLookup<'a>,
        clip: &ClipState,
    ) -> ClipTimeAxis<'a> {
        let start_beat = clip.start_beat.max(0.0) as f64;
        let project_bpm = Self::follows_tempo_axis(clip).then(|| self.bpm.max(1.0) as f64);
        let start_seconds = if project_bpm.is_some() {
            0.0
        } else {
            tempo.seconds_at_beat(start_beat)
        };
        ClipTimeAxis {
            state: self,
            tempo,
            start_beat,
            project_bpm,
            start_seconds,
        }
    }

    /// Seconds of `clip`'s own playing time at timeline `beat` (negative
    /// before its start). See [`ClipTimeAxis`].
    pub fn clip_local_seconds_at_beat(&self, clip: &ClipState, beat: f64) -> f64 {
        self.clip_time_axis(clip).local_seconds_at_beat(beat)
    }

    /// Timeline beat `seconds` into `clip`'s playing time. Inverse of
    /// [`Self::clip_local_seconds_at_beat`].
    pub fn clip_beat_at_local_seconds(&self, clip: &ClipState, seconds: f64) -> f64 {
        self.clip_time_axis(clip).beat_at_local_seconds(seconds)
    }

    /// Lane x of `seconds` into `clip`'s playing time: the one transform a
    /// fade is drawn *and* hit-tested through.
    pub fn clip_lane_x_at_local_seconds(&self, clip: &ClipState, seconds: f64) -> f32 {
        self.clip_time_axis(clip).lane_x_at_local_seconds(seconds)
    }

    /// Seconds into `clip`'s playing time at lane x. Inverse of
    /// [`Self::clip_lane_x_at_local_seconds`].
    pub fn clip_local_seconds_at_lane_x(&self, clip: &ClipState, lane_x: f32) -> f64 {
        self.clip_time_axis(clip).local_seconds_at_lane_x(lane_x)
    }

    /// How long `clip` plays, in seconds: the decoded source window through
    /// its stretch, or its beat span while the source is still pending.
    pub fn clip_played_seconds(&self, clip: &ClipState) -> f64 {
        clip.stretch
            .played_seconds_for_project_bpm(self.bpm.max(1.0) as f64)
            .unwrap_or_else(|| {
                let end = (clip.start_beat + clip.duration_beats.max(0.0)) as f64;
                self.clip_local_seconds_at_beat(clip, end)
            })
            .max(0.0)
    }

    /// Beat an audio clip ends on, as drawn.
    fn crossfade_clip_end_beat(&self, clip: &ClipState) -> f64 {
        self.audio_clip_end_beat(clip)
            .unwrap_or((clip.start_beat + clip.duration_beats.max(0.0)) as f64)
    }

    /// Every automatic crossfade between the audio clips on `track`: the ones
    /// the arrangement draws, the Inspector and the Audio Editor show, and the
    /// engine plays.
    ///
    /// Clips are sorted once by start (ties by their order on the track) and
    /// each is paired with every later clip that starts before it ends. Only a
    /// staggered pair crossfades — `a` starts before `b` and ends before `b`
    /// does — and the overlap is timed through the tempo map.
    pub fn audio_crossfades(&self, track: &TrackState) -> TrackCrossfades {
        self.crossfades_among(track, crossfade_candidate)
    }

    /// Every crossfade the engine plays on `track`: the audio crossfades, and
    /// the same rule between reference-video clips, whose sound the engine
    /// plays too ([`video_crossfade_candidate`]). Audio and video clips never
    /// pair with each other, so the audio crossfades are exactly the ones the
    /// arrangement draws.
    pub fn engine_crossfades(&self, track: &TrackState) -> TrackCrossfades {
        let mut crossfades = self.audio_crossfades(track);
        crossfades.crossfades.extend(
            self.crossfades_among(track, video_crossfade_candidate)
                .crossfades,
        );
        crossfades
    }

    fn crossfades_among(
        &self,
        track: &TrackState,
        candidate: fn(&ClipState) -> bool,
    ) -> TrackCrossfades {
        let mut spans: Vec<(f64, f64, usize)> = track
            .clips
            .iter()
            .enumerate()
            .filter(|(_, clip)| candidate(clip))
            .map(|(index, clip)| {
                (
                    clip.start_beat.max(0.0) as f64,
                    self.crossfade_clip_end_beat(clip),
                    index,
                )
            })
            .collect();
        if spans.len() < 2 {
            return TrackCrossfades::default();
        }
        spans.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.2.cmp(&b.2)));

        let tempo = self.tempo_lookup();
        let mut crossfades = Vec::new();
        for (i, &(a_start, a_end, a_index)) in spans.iter().enumerate() {
            for &(b_start, b_end, b_index) in &spans[i + 1..] {
                // Sorted by start: nothing later reaches back into `a`.
                if b_start >= a_end {
                    break;
                }
                // Equal starts and containment make no crossfade.
                if !(a_start < b_start && a_end < b_end) {
                    continue;
                }
                let seconds = tempo.seconds_at_beat(a_end) - tempo.seconds_at_beat(b_start);
                if seconds < CROSSFADE_MIN_OVERLAP_SECONDS {
                    continue;
                }
                let left = &track.clips[a_index];
                let right = &track.clips[b_index];
                let left_time = self.clip_time_axis_in(tempo.clone(), left);
                let left_seconds = (left_time.local_seconds_at_beat(a_end)
                    - left_time.local_seconds_at_beat(b_start))
                .max(0.0);
                let right_seconds = self
                    .clip_time_axis_in(tempo.clone(), right)
                    .local_seconds_at_beat(a_end)
                    .max(0.0);
                crossfades.push(AudioCrossfade {
                    left_id: left.id.clone(),
                    right_id: right.id.clone(),
                    start_beat: b_start,
                    end_beat: a_end,
                    seconds,
                    left_seconds,
                    right_seconds,
                });
            }
        }
        TrackCrossfades { crossfades }
    }

    /// The fades `clip` plays, with `crossfades` resolved for its track.
    pub fn effective_clip_fades(
        &self,
        clip: &ClipState,
        crossfades: &TrackCrossfades,
    ) -> EffectiveFades {
        let played = self.clip_played_seconds(clip);
        let crossfade_in = crossfades.fade_in_override(&clip.id);
        let crossfade_out = crossfades.fade_out_override(&clip.id);
        let (in_seconds, out_seconds) = resolve_effective_fades(
            clip.stretch.fade_in_ms as f64 / 1000.0,
            clip.stretch.fade_out_ms as f64 / 1000.0,
            crossfade_in,
            crossfade_out,
            played,
        );
        EffectiveFades {
            played_seconds: played,
            in_seconds,
            out_seconds,
            in_crossfade: crossfade_in.is_some(),
            out_crossfade: crossfade_out.is_some(),
        }
    }

    /// The Inspector's view of `clip_id`'s fades; `None` unless it is audio.
    pub fn clip_fade_summary(&self, clip_id: &str) -> Option<ClipFadeSummary> {
        let (track, clip) = self.find_clip(clip_id)?;
        if !matches!(clip.clip_type, ClipType::Audio { .. }) {
            return None;
        }
        let crossfades = self.audio_crossfades(track);
        Some(ClipFadeSummary {
            fades: self.effective_clip_fades(clip, &crossfades),
            ara: track.ara.is_some(),
        })
    }

    /// Set `clip_id`'s manual fade on `edge` to `seconds`, clamped so the two
    /// fades never overlap (against the other edge's *effective* fade, a
    /// crossfade included). Returns whether anything changed.
    ///
    /// Leaves the transient `dirty` flag alone, so a gesture that ends where it
    /// started is no edit at all.
    pub fn set_clip_fade_seconds(&mut self, clip_id: &str, edge: FadeEdge, seconds: f64) -> bool {
        let Some((track, clip)) = self.find_clip(clip_id) else {
            return false;
        };
        if !matches!(clip.clip_type, ClipType::Audio { .. }) {
            return false;
        }
        let crossfades = self.audio_crossfades(track);
        let fades = self.effective_clip_fades(clip, &crossfades);
        let other = match edge {
            FadeEdge::In => fades.out_seconds,
            FadeEdge::Out => fades.in_seconds,
        };
        let ms = (clamp_fade_seconds(seconds, other, fades.played_seconds) * 1000.0) as f32;
        let mut stretch = clip.stretch.clone();
        match edge {
            FadeEdge::In => stretch.fade_in_ms = ms,
            FadeEdge::Out => stretch.fade_out_ms = ms,
        }
        self.set_clip_stretch(clip_id, stretch)
    }

    /// Set `clip_id`'s manual fade on `edge` so it ends (fade-in) or starts
    /// (fade-out) at timeline `beat` — the pointer's beat, already snapped.
    pub fn set_clip_fade_at_beat(&mut self, clip_id: &str, edge: FadeEdge, beat: f64) -> bool {
        let Some((_, clip)) = self.find_clip(clip_id) else {
            return false;
        };
        let local = self.clip_local_seconds_at_beat(clip, beat);
        let seconds = match edge {
            FadeEdge::In => local,
            FadeEdge::Out => self.clip_played_seconds(clip) - local,
        };
        self.set_clip_fade_seconds(clip_id, edge, seconds)
    }

    /// Put `clip` back in place of the clip with its id, keeping its slot on
    /// its track. For gesture previews, which must not reorder clips.
    pub fn replace_clip_in_place(&mut self, clip: &ClipState) -> bool {
        for track in &mut self.tracks {
            if let Some(slot) = track.clips.iter_mut().find(|c| c.id == clip.id) {
                if *slot == *clip {
                    return false;
                }
                *slot = clip.clone();
                return true;
            }
        }
        false
    }

    /// The real-time point a crossfade between `left` and `right` centres on:
    /// midway between where `left` ends and `right` starts. For clips that
    /// already overlap, that is the middle of the overlap.
    pub fn crossfade_center_seconds(&self, left: &ClipState, right: &ClipState) -> f64 {
        let left_end = self.seconds_at_beat(self.crossfade_clip_end_beat(left));
        let right_start = self.seconds_at_beat(right.start_beat.max(0.0) as f64);
        (left_end + right_start) * 0.5
    }

    /// Why [`Self::plan_crossfade`] cannot resize the crossfade between
    /// `left` and `right` to any length, or `None` when it can. The reason is
    /// either a clip that cannot be trimmed ([`crossfade_pair_block`]) or
    /// [`CrossfadeBlock::NoRoom`]. The planner and this answer come from one
    /// computation, so the arrangement's handle, which is live only when this
    /// is `None`, never offers a drag the planner refuses at every length.
    pub fn crossfade_adjust_block(
        &self,
        left: &ClipState,
        right: &ClipState,
    ) -> Option<CrossfadeBlock> {
        self.crossfade_room(left, right).err()
    }

    /// Whether [`Self::plan_crossfade`] can resize the crossfade between
    /// `left` and `right`: some length of at least [`MIN_CROSSFADE_SECONDS`]
    /// plans. A drag on a crossfade handle starts only then.
    pub fn can_adjust_crossfade(&self, left: &ClipState, right: &ClipState) -> bool {
        self.crossfade_room(left, right).is_ok()
    }

    /// Where a crossfade between `left` and `right` can lie, in real time,
    /// or why no crossfade of at least [`MIN_CROSSFADE_SECONDS`] can:
    /// [`Self::plan_crossfade`] needs no more than this to succeed for every
    /// length of at least that, and refuses every length without it.
    fn crossfade_room(
        &self,
        left: &ClipState,
        right: &ClipState,
    ) -> Result<CrossfadeRoom, CrossfadeBlock> {
        if let Some(block) = crossfade_pair_block(left, right) {
            return Err(block);
        }
        let project_bpm = self.bpm.max(1.0) as f64;
        let frames_per_second = |clip: &ClipState| -> f64 {
            let ratio = clip.stretch.effective_time_ratio(project_bpm);
            let ratio = if ratio.is_finite() && ratio > 1.0e-6 {
                ratio
            } else {
                1.0
            };
            clip.stretch.source_sample_rate() as f64 / ratio
        };
        let left_fps = frames_per_second(left);
        let right_fps = frames_per_second(right);

        let left_start = self.seconds_at_beat(left.start_beat.max(0.0) as f64);
        let right_start = self.seconds_at_beat(right.start_beat.max(0.0) as f64);
        let right_end = self.seconds_at_beat(self.crossfade_clip_end_beat(right));
        if right_start <= left_start {
            return Err(CrossfadeBlock::NoRoom);
        }

        // How far each edge can reach: `left`'s end to the end of its file,
        // `right`'s start back to the start of its file.
        let left_total = if left.stretch.original_duration_samples > 0 {
            left.stretch.original_duration_samples
        } else {
            left.stretch.source_end_samples
        };
        let left_max_local =
            left_total.saturating_sub(left.stretch.source_start_samples) as f64 / left_fps;
        let latest_end =
            self.seconds_at_beat(self.clip_beat_at_local_seconds(left, left_max_local));
        let right_min_local = -(right.stretch.source_start_samples as f64) / right_fps;
        let earliest_start =
            self.seconds_at_beat(self.clip_beat_at_local_seconds(right, right_min_local));

        let min_body = self.beats_to_seconds(MIN_AUDIO_CLIP_BEATS) as f64;
        let low = earliest_start.max(left_start + min_body);
        let high = latest_end.min(right_end - min_body);
        // Exactly what `centred_overlap` needs for a length of at least the
        // minimum: finite bounds at least that far apart.
        if !(low.is_finite() && high.is_finite() && high - low >= MIN_CROSSFADE_SECONDS) {
            return Err(CrossfadeBlock::NoRoom);
        }
        Ok(CrossfadeRoom {
            low,
            high,
            left_fps,
            right_fps,
            left_total,
        })
    }

    /// Re-trim `left` and `right` so they overlap by `length_seconds` around
    /// `center_seconds` (real time): `left`'s end and `right`'s start move,
    /// each revealing or hiding its own source audio, and nothing else does.
    ///
    /// Planned in seconds through the tempo map. When one side has too little
    /// source audio beyond its edge the crossfade shifts toward the other; when
    /// both together cannot cover `length_seconds` it shrinks. Each clip keeps
    /// a minimum body outside the overlap, so the pair stays staggered. `None`
    /// when `length_seconds` is under [`MIN_CROSSFADE_SECONDS`], and at every
    /// length when [`Self::crossfade_adjust_block`] names a reason: either
    /// clip cannot be trimmed ([`crossfade_pair_block`]: not audio, Warp,
    /// reversed, undecoded, or a legacy offset-only trim), or no crossfade of
    /// the minimum fits ([`CrossfadeBlock::NoRoom`]), for example because
    /// there is no source audio beyond the edges.
    pub fn plan_crossfade(
        &self,
        left: &ClipState,
        right: &ClipState,
        center_seconds: f64,
        length_seconds: f64,
    ) -> Option<CrossfadePlan> {
        let CrossfadeRoom {
            low,
            high,
            left_fps,
            right_fps,
            left_total,
        } = self.crossfade_room(left, right).ok()?;
        let (overlap_start, overlap_end, length) =
            centred_overlap(center_seconds, length_seconds, low, high)?;

        // `left`: the start stays, the end moves to `overlap_end`.
        let mut next_left = left.clone();
        let end_beat = self.beat_at_seconds(overlap_end);
        let left_local = self.clip_local_seconds_at_beat(left, end_beat).max(0.0);
        let source_start = left.stretch.source_start_samples;
        let source_end = (source_start as f64 + left_local * left_fps)
            .round()
            .max(0.0) as u64;
        next_left.stretch.apply_trim(
            source_start,
            source_end.min(left_total).max(source_start + 1),
        );
        next_left.stretch.dirty = left.stretch.dirty;
        let left_end_beat = self.audio_clip_end_beat(&next_left).unwrap_or(end_beat);
        next_left.duration_beats = (left_end_beat - left.start_beat.max(0.0) as f64)
            .max(MIN_AUDIO_CLIP_BEATS as f64) as f32;

        // `right`: the end stays, the start moves to `overlap_start`.
        let mut next_right = right.clone();
        let right_end_beat = self.crossfade_clip_end_beat(right);
        let start_beat = self.beat_at_seconds(overlap_start);
        let shift_local = self.clip_local_seconds_at_beat(right, start_beat);
        let source_end = right.stretch.source_end_samples;
        let source_start = (right.stretch.source_start_samples as f64 + shift_local * right_fps)
            .round()
            .clamp(0.0, source_end.saturating_sub(1) as f64) as u64;
        next_right.stretch.apply_trim(source_start, source_end);
        next_right.stretch.dirty = right.stretch.dirty;
        if source_start == 0 {
            // The engine reads a zero window start as "use the legacy offset".
            next_right.offset_beats = 0.0;
        }
        next_right.start_beat = start_beat as f32;
        let right_end_beat = self
            .audio_clip_end_beat(&next_right)
            .unwrap_or(right_end_beat);
        next_right.duration_beats =
            (right_end_beat - start_beat).max(MIN_AUDIO_CLIP_BEATS as f64) as f32;

        Some(CrossfadePlan {
            left: next_left,
            right: next_right,
            seconds: length,
        })
    }

    /// What `audio:create-crossfade` targets, or why it cannot run.
    ///
    /// An arrangement range that spans exactly one boundary between two audio
    /// clips on its track gets a crossfade as long as the range. Otherwise two
    /// selected audio clips on one track that touch (or overlap by less than
    /// the default length, or leave a gap of at most
    /// [`CROSSFADE_NEAR_ABUT_SECONDS`]) get the default length centred on
    /// their boundary. Clips that already overlap by the default or more keep
    /// their crossfade — X adds one, it never shortens one — and clips
    /// [`plan_crossfade`](Self::plan_crossfade) cannot re-trim are refused
    /// with the reason ([`CrossfadeBlock::command_message`]).
    pub fn crossfade_request(&self) -> Result<CrossfadeRequest, String> {
        if let Some(range) = self.arrangement_range.as_ref() {
            if range.end_beat > range.start_beat {
                return self.crossfade_request_for_range(range);
            }
        }
        let mut picked: Vec<(&TrackState, &ClipState)> = self
            .selection
            .selected_clip_ids
            .iter()
            .filter_map(|id| self.find_clip(id))
            .filter(|(_, clip)| matches!(clip.clip_type, ClipType::Audio { .. }))
            .collect();
        if picked.len() != 2 || picked[0].0.id != picked[1].0.id {
            return Err("Select two touching audio clips on one track to crossfade".to_string());
        }
        let track = picked[0].0;
        if track.ara.is_some() {
            return Err(
                "Crossfades do not play on ARA tracks: the ARA plug-in renders this track"
                    .to_string(),
            );
        }
        picked.sort_by(|a, b| a.1.start_beat.total_cmp(&b.1.start_beat));
        let (left, right) = (picked[0].1, picked[1].1);
        if !crossfade_candidate(left) || !crossfade_candidate(right) {
            return Err("Crossfades need two unmuted audio clips".to_string());
        }
        let left_end = self.crossfade_clip_end_beat(left);
        let right_end = self.crossfade_clip_end_beat(right);
        let staggered = left.start_beat < right.start_beat && left_end < right_end;
        let gap =
            self.seconds_at_beat(right.start_beat.max(0.0) as f64) - self.seconds_at_beat(left_end);
        if !staggered || gap > CROSSFADE_NEAR_ABUT_SECONDS {
            return Err("The two clips must touch to crossfade".to_string());
        }
        // A tolerance for the round trip through beats, so a second X on a
        // crossfade the first one made is refused too.
        if -gap >= DEFAULT_CROSSFADE_SECONDS - 1.0e-6 {
            return Err("These clips already crossfade over their overlap".to_string());
        }
        if let Some(block) = crossfade_pair_block(left, right) {
            return Err(block.command_message().to_string());
        }
        Ok(CrossfadeRequest {
            left_id: left.id.clone(),
            right_id: right.id.clone(),
            center_seconds: self.crossfade_center_seconds(left, right),
            length_seconds: DEFAULT_CROSSFADE_SECONDS,
        })
    }

    fn crossfade_request_for_range(
        &self,
        range: &TimelineRangeSelection,
    ) -> Result<CrossfadeRequest, String> {
        let track_ids: Vec<&str> = if range.track_ids.is_empty() {
            self.selection
                .selected_track_id
                .iter()
                .map(String::as_str)
                .collect()
        } else {
            range.track_ids.iter().map(String::as_str).collect()
        };
        let mut found = Vec::new();
        for track in self
            .tracks
            .iter()
            .filter(|t| track_ids.contains(&t.id.as_str()))
        {
            let mut clips: Vec<&ClipState> = track
                .clips
                .iter()
                .filter(|c| crossfade_candidate(c))
                .collect();
            clips.sort_by(|a, b| a.start_beat.total_cmp(&b.start_beat));
            for pair in clips.windows(2) {
                let (left, right) = (pair[0], pair[1]);
                let left_end = self.crossfade_clip_end_beat(left);
                let boundary_start = (right.start_beat as f64).min(left_end);
                let boundary_end = (right.start_beat as f64).max(left_end);
                let inside = boundary_start >= range.start_beat && boundary_end <= range.end_beat;
                if inside && left_end < self.crossfade_clip_end_beat(right) {
                    found.push((track, left, right));
                }
            }
        }
        let [(track, left, right)] = found[..] else {
            return Err("Select a range across one boundary between two audio clips".to_string());
        };
        if track.ara.is_some() {
            return Err(
                "Crossfades do not play on ARA tracks: the ARA plug-in renders this track"
                    .to_string(),
            );
        }
        if let Some(block) = crossfade_pair_block(left, right) {
            return Err(block.command_message().to_string());
        }
        let start = self.seconds_at_beat(range.start_beat);
        let end = self.seconds_at_beat(range.end_beat);
        Ok(CrossfadeRequest {
            left_id: left.id.clone(),
            right_id: right.id.clone(),
            center_seconds: (start + end) * 0.5,
            length_seconds: end - start,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio_clip(id: &str, start_beat: f32, seconds: f64) -> ClipState {
        let mut clip = ClipState {
            id: id.to_string(),
            name: id.to_string(),
            start_beat,
            duration_beats: (seconds * 2.0) as f32,
            source_duration_seconds: Some(seconds),
            offset_beats: 0.0,
            gain: 1.0,
            clip_type: ClipType::Audio {
                file_id: id.to_string(),
                source_path: Some(format!("{id}.wav")),
            },
            muted: false,
            audio_import: AudioImportState::Ready,
            stretch: AudioClipStretchState::default(),
        };
        let frames = (seconds * 48_000.0).round() as u64;
        clip.stretch.original_sample_rate = 48_000;
        clip.stretch.project_sample_rate = 48_000;
        clip.stretch.original_duration_samples = frames;
        clip.stretch.source_start_samples = 0;
        clip.stretch.source_end_samples = frames;
        clip
    }

    /// A clip `seconds` long playing the middle of a longer file, so it has
    /// `handle` seconds of source on both sides.
    fn clip_with_handles(id: &str, start_beat: f32, seconds: f64, handle: f64) -> ClipState {
        let mut clip = audio_clip(id, start_beat, seconds + 2.0 * handle);
        let rate = 48_000.0;
        clip.stretch.source_start_samples = (handle * rate).round() as u64;
        clip.stretch.source_end_samples = ((handle + seconds) * rate).round() as u64;
        clip.duration_beats = (seconds * 2.0) as f32;
        clip
    }

    fn state_with(clips: Vec<ClipState>) -> TimelineState {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        state.tracks.clear();
        let track_id = state.create_audio_track();
        let track = state.tracks.iter_mut().find(|t| t.id == track_id).unwrap();
        track.clips = clips;
        state.reconcile_audio_clip_lengths();
        state
    }

    fn close(a: f64, b: f64, tolerance: f64) -> bool {
        (a - b).abs() <= tolerance
    }

    fn ramp(state: &mut TimelineState) {
        state
            .tempo_map
            .add_or_update_point(0.0, 60.0, TempoCurve::Linear);
        state
            .tempo_map
            .add_or_update_point(8.0, 180.0, TempoCurve::Hold);
        state.reconcile_audio_clip_lengths();
    }

    /// The fade transform goes seconds -> x -> seconds and back through the
    /// same tempo map the waveform uses, flat or ramped, so the handle is
    /// grabbed exactly where its fade is drawn.
    #[test]
    fn the_fade_transform_round_trips_through_a_tempo_ramp() {
        for ramped in [false, true] {
            let mut state = state_with(vec![audio_clip("a", 1.0, 6.0)]);
            // About a millisecond per pixel.
            state.viewport.pixels_per_second = 960.0;
            state.sync_pixels_per_beat();
            if ramped {
                ramp(&mut state);
            }
            state.sync_time_warp();
            let clip = state.tracks[0].clips[0].clone();
            let played = state.clip_played_seconds(&clip);
            assert!(close(played, 6.0, 1.0e-6));
            for ms in [0.0, 5.0, 250.0, 1_000.0, 3_700.0] {
                let seconds = ms / 1000.0;
                let x = state.clip_lane_x_at_local_seconds(&clip, seconds);
                let back = state.clip_local_seconds_at_lane_x(&clip, x);
                // Drawing rounds to whole pixels: half a pixel of time at most,
                // which here is well inside a millisecond.
                let half_px = (state.clip_local_seconds_at_lane_x(&clip, x + 0.5)
                    - state.clip_local_seconds_at_lane_x(&clip, x - 0.5))
                .abs()
                    * 0.5;
                assert!(half_px < 0.001, "half a pixel is {half_px} s");
                assert!(
                    close(back, seconds, half_px + 1.0e-6),
                    "ramped={ramped} {ms} ms came back as {back} s"
                );
                let beat = state.clip_beat_at_local_seconds(&clip, seconds);
                // The drawn x is also exactly where hit-testing lands.
                assert_eq!(state.clip_lane_x_at_local_seconds(&clip, back), x);
                // Pure beat round trip, no pixels in between.
                assert!(close(
                    state.clip_local_seconds_at_beat(&clip, beat),
                    seconds,
                    1.0e-6
                ));
            }
        }
    }

    /// Under a ramp a fade of fixed length covers fewer beats where the tempo
    /// is slow: the drawn fade follows real time, not a straight scale.
    #[test]
    fn a_fade_under_a_ramp_follows_real_time() {
        let mut state = state_with(vec![audio_clip("a", 0.0, 6.0)]);
        ramp(&mut state);
        let clip = state.tracks[0].clips[0].clone();
        let beat = state.clip_beat_at_local_seconds(&clip, 1.0);
        assert!(close(state.seconds_at_beat(beat), 1.0, 1.0e-6));
        // At 60 BPM rising, one second covers just over one beat.
        assert!(beat > 1.0 && beat < 1.2, "1 s ends on beat {beat}");
    }

    #[test]
    fn fades_are_clamped_so_they_never_overlap() {
        assert_eq!(clamp_fade_seconds(3.0, 0.5, 2.0), 1.5);
        assert_eq!(clamp_fade_seconds(-1.0, 0.0, 2.0), 0.0);
        // A fade may pass half the clip when the other one is zero.
        assert_eq!(clamp_fade_seconds(1.8, 0.0, 2.0), 1.8);
        assert_eq!(clamp_fade_seconds(f64::NAN, 0.0, 2.0), 0.0);

        // The engine's own order: fade-in first, the fade-out in what is left.
        assert_eq!(
            resolve_effective_fades(1.5, 1.5, None, None, 2.0),
            (1.5, 0.5)
        );
        let (fade_in, fade_out) = resolve_effective_fades(0.4, 0.3, None, None, 2.0);
        assert!(fade_in + fade_out <= 2.0);
        assert_eq!((fade_in, fade_out), (0.4, 0.3));
    }

    /// A crossfaded edge is exactly the overlap: a longer manual fade does not
    /// stretch it and a shorter one does not shorten it, and the manual fade on
    /// the other edge gives way to it.
    #[test]
    fn an_overlapped_edge_plays_exactly_the_overlap() {
        assert_eq!(
            resolve_effective_fades(3.0, 0.0, Some(0.5), None, 4.0),
            (0.5, 0.0)
        );
        assert_eq!(
            resolve_effective_fades(0.1, 0.0, Some(0.5), None, 4.0),
            (0.5, 0.0)
        );
        assert_eq!(
            resolve_effective_fades(3.9, 0.0, None, Some(0.5), 4.0),
            (3.5, 0.5)
        );
        assert_eq!(
            resolve_effective_fades(0.0, 9.0, Some(0.5), Some(1.0), 4.0),
            (0.5, 1.0)
        );
    }

    fn pairs(crossfades: &TrackCrossfades) -> Vec<(&str, &str)> {
        crossfades
            .crossfades
            .iter()
            .map(|xf| (xf.left_id.as_str(), xf.right_id.as_str()))
            .collect()
    }

    /// A[0,8) B[2,4) C[3,10): B sits inside A (no crossfade), A and C are
    /// staggered over [3,8), B and C over [3,4). C's fade-in is the longest
    /// overlap on it.
    #[test]
    fn the_resolver_pairs_every_staggered_overlap() {
        // 120 BPM: two beats a second.
        let state = state_with(vec![
            audio_clip("a", 0.0, 4.0),
            audio_clip("b", 2.0, 1.0),
            audio_clip("c", 3.0, 3.5),
        ]);
        let crossfades = state.audio_crossfades(&state.tracks[0]);
        assert_eq!(pairs(&crossfades), vec![("a", "c"), ("b", "c")]);
        let a_c = &crossfades.crossfades[0];
        assert!(close(a_c.start_beat, 3.0, 1.0e-6) && close(a_c.end_beat, 8.0, 1.0e-4));
        assert!(close(a_c.seconds, 2.5, 1.0e-4));
        assert!(close(
            crossfades.fade_out_override("a").unwrap(),
            2.5,
            1.0e-4
        ));
        assert!(close(
            crossfades.fade_out_override("b").unwrap(),
            0.5,
            1.0e-4
        ));
        assert!(close(
            crossfades.fade_in_override("c").unwrap(),
            2.5,
            1.0e-4
        ));
        assert_eq!(crossfades.fade_in_override("b"), None);
        assert_eq!(crossfades.fade_in_override("a"), None);
    }

    #[test]
    fn equal_starts_and_containment_make_no_crossfade() {
        let tie = state_with(vec![audio_clip("a", 0.0, 2.0), audio_clip("b", 0.0, 4.0)]);
        assert!(tie.audio_crossfades(&tie.tracks[0]).is_empty());

        let nested = state_with(vec![audio_clip("a", 0.0, 4.0), audio_clip("b", 1.0, 1.0)]);
        assert!(nested.audio_crossfades(&nested.tracks[0]).is_empty());

        // Abutting clips meet; they do not overlap.
        let abut = state_with(vec![audio_clip("a", 0.0, 1.0), audio_clip("b", 2.0, 1.0)]);
        assert!(abut.audio_crossfades(&abut.tracks[0]).is_empty());

        // Muted clips and clips without a source play nothing to cross.
        let mut muted = state_with(vec![audio_clip("a", 0.0, 2.0), audio_clip("b", 3.0, 2.0)]);
        muted.tracks[0].clips[1].muted = true;
        assert!(muted.audio_crossfades(&muted.tracks[0]).is_empty());
    }

    /// The order clips sit in on the track does not change the answer.
    #[test]
    fn the_resolver_does_not_depend_on_track_order() {
        let forward = state_with(vec![audio_clip("a", 0.0, 2.0), audio_clip("b", 3.0, 2.0)]);
        let backward = state_with(vec![audio_clip("b", 3.0, 2.0), audio_clip("a", 0.0, 2.0)]);
        assert_eq!(
            forward.audio_crossfades(&forward.tracks[0]),
            backward.audio_crossfades(&backward.tracks[0])
        );
        assert_eq!(
            pairs(&forward.audio_crossfades(&forward.tracks[0])),
            vec![("a", "b")]
        );
    }

    /// Under a tempo map the crossfade is the real time between its edges.
    #[test]
    fn crossfade_seconds_follow_the_tempo_map() {
        let mut state = state_with(vec![audio_clip("a", 0.0, 3.0), audio_clip("b", 2.0, 3.0)]);
        ramp(&mut state);
        let crossfades = state.audio_crossfades(&state.tracks[0]);
        let xf = &crossfades.crossfades[0];
        let expected = state.seconds_at_beat(xf.end_beat) - state.seconds_at_beat(xf.start_beat);
        assert!(close(xf.seconds, expected, 1.0e-9));
        // A ends after 3 s of real time; B started at beat 2.
        assert!(close(xf.seconds, 3.0 - state.seconds_at_beat(2.0), 1.0e-6));
        assert!(close(xf.left_seconds, xf.seconds, 1.0e-6));
        assert!(close(xf.right_seconds, xf.seconds, 1.0e-6));
    }

    /// A fade drag that ends where it started changes nothing, so its release
    /// records no undo entry and reloads nothing in the engine.
    #[test]
    fn a_fade_gesture_that_changes_nothing_is_no_edit() {
        let mut state = state_with(vec![audio_clip("a", 2.0, 2.0)]);
        let before = state.tracks[0].clips[0].clone();
        // Dragging left of the clip start clamps to the zero it started at.
        assert!(!state.set_clip_fade_at_beat("a", FadeEdge::In, 0.0));
        assert!(!state.set_clip_fade_at_beat("a", FadeEdge::Out, 9.0));
        let after = state.tracks[0].clips[0].clone();
        assert!(clip_edit_is_noop(&before, &after));
        assert_eq!(before, after, "the preview leaves the dirty flag alone");

        // A flipped transient flag alone is still no edit.
        let mut flagged = after.clone();
        flagged.stretch.dirty = !flagged.stretch.dirty;
        assert!(clip_edit_is_noop(&before, &flagged));

        // A real change is one.
        assert!(state.set_clip_fade_at_beat("a", FadeEdge::In, 3.0));
        let changed = &state.tracks[0].clips[0];
        assert!(close(changed.stretch.fade_in_ms as f64, 500.0, 0.01));
        assert!(!clip_edit_is_noop(&before, changed));
    }

    #[test]
    fn a_fade_gesture_is_clamped_by_the_other_edge() {
        let mut clip = audio_clip("a", 0.0, 2.0);
        clip.stretch.fade_out_ms = 1_500.0;
        let mut state = state_with(vec![clip]);
        // Past half the clip is fine; into the fade-out is not.
        assert!(state.set_clip_fade_seconds("a", FadeEdge::In, 1.8));
        assert!(close(
            state.tracks[0].clips[0].stretch.fade_in_ms as f64,
            500.0,
            0.01
        ));

        // `b`'s fade-in is its crossfade with `a` (0.5 s): a fade-out on `b`
        // stops where that crossfade ends, whatever `a`'s manual fade says.
        let mut state = state_with(vec![audio_clip("a", 0.0, 2.0), audio_clip("b", 3.0, 2.0)]);
        assert!(state.set_clip_fade_seconds("b", FadeEdge::Out, 5.0));
        assert!(close(
            state.tracks[0].clips[1].stretch.fade_out_ms as f64,
            1_500.0,
            0.01
        ));
    }

    /// A split keeps the clip's outer fades and gives the cut none; copying
    /// them put a fade-out and a fade-in, a dip, on every cut.
    #[test]
    fn a_split_keeps_the_outer_fades_and_zeroes_the_inner_ones() {
        let mut clip = audio_clip("a", 0.0, 4.0);
        clip.stretch.fade_in_ms = 120.0;
        clip.stretch.fade_out_ms = 340.0;
        let state = state_with(vec![clip]);
        let clip = state.tracks[0].clips[0].clone();
        let (left, right) = state.plan_audio_clip_split(&clip, 4.0).unwrap();
        assert_eq!(left.stretch.fade_in_ms, 120.0);
        assert_eq!(left.stretch.fade_out_ms, 0.0);
        assert_eq!(right.stretch.fade_in_ms, 0.0);
        assert_eq!(right.stretch.fade_out_ms, 340.0);
    }

    /// Under a tempo ramp the halves meet on the audio at the split beat, the
    /// sample the waveform draws under the razor line.
    #[test]
    fn a_split_under_a_tempo_ramp_cuts_the_audio_at_the_split_beat() {
        let mut state = state_with(vec![audio_clip("a", 0.0, 6.0)]);
        ramp(&mut state);
        let clip = state.tracks[0].clips[0].clone();
        let (left, right) = state.plan_audio_clip_split(&clip, 3.0).unwrap();
        let expected = (state.seconds_at_beat(3.0) * 48_000.0).round() as u64;
        assert_eq!(left.stretch.source_end_samples, expected);
        assert_eq!(right.stretch.source_start_samples, expected);
    }

    #[test]
    fn the_effective_fade_of_a_clip_uses_its_crossfades() {
        let mut state = state_with(vec![audio_clip("a", 0.0, 2.0), audio_clip("b", 3.0, 2.0)]);
        state.tracks[0].clips[0].stretch.fade_out_ms = 1_500.0;
        state.tracks[0].clips[0].stretch.fade_in_ms = 200.0;
        let crossfades = state.audio_crossfades(&state.tracks[0]);
        let a = state.effective_clip_fades(&state.tracks[0].clips[0], &crossfades);
        assert!(a.out_crossfade && !a.in_crossfade);
        assert!(close(a.out_seconds, 0.5, 1.0e-4), "exactly the overlap");
        assert!(close(a.in_seconds, 0.2, 1.0e-9));
        assert!(close(a.max_manual_seconds(FadeEdge::In), 1.5, 1.0e-4));
    }

    /// Two abutting 3 s clips meeting at 3 s (beat 6): the left one has
    /// `handle_left` seconds of file past its end, the right one
    /// `handle_right` seconds before its start.
    fn split_halves(handle_left: f64, handle_right: f64) -> (TimelineState, ClipState, ClipState) {
        let rate = 48_000.0;
        let three = (3.0 * rate) as u64;
        let mut left = audio_clip("l", 0.0, 3.0);
        left.stretch.original_duration_samples = three + (handle_left * rate).round() as u64;
        let mut right = audio_clip("r", 6.0, 3.0);
        let start = (handle_right * rate).round() as u64;
        right.stretch.source_start_samples = start;
        right.stretch.source_end_samples = start + three;
        right.stretch.original_duration_samples = start + three;
        let state = state_with(vec![left, right]);
        let left = state.tracks[0].clips[0].clone();
        let right = state.tracks[0].clips[1].clone();
        (state, left, right)
    }

    /// Two abutting halves of one file: the crossfade is centred on the cut,
    /// each half reveals half of it.
    #[test]
    fn plan_crossfade_centres_on_the_boundary() {
        let (state, left, right) = split_halves(1.0, 1.0);
        let center = state.crossfade_center_seconds(&left, &right);
        assert!(close(center, 3.0, 1.0e-6));
        let plan = state
            .plan_crossfade(&left, &right, center, DEFAULT_CROSSFADE_SECONDS)
            .expect("both halves have audio beyond the cut");
        assert!(close(plan.seconds, 0.020, 1.0e-9));
        let left_end = state.seconds_at_beat(state.audio_clip_end_beat(&plan.left).unwrap());
        let right_start = state.seconds_at_beat(plan.right.start_beat as f64);
        assert!(close(left_end, 3.010, 1.0e-4), "left ends at {left_end}");
        assert!(
            close(right_start, 2.990, 1.0e-4),
            "right starts at {right_start}"
        );
        // Each half reveals 10 ms of its own file, and nothing else moves.
        assert_eq!(
            plan.left.stretch.source_end_samples - left.stretch.source_end_samples,
            480
        );
        assert_eq!(
            right.stretch.source_start_samples - plan.right.stretch.source_start_samples,
            480
        );
        assert_eq!(plan.left.start_beat, left.start_beat);
        let right_end_before = state.audio_clip_end_beat(&right).unwrap();
        let right_end_after = state.audio_clip_end_beat(&plan.right).unwrap();
        assert!(close(right_end_before, right_end_after, 1.0e-4));

        // Applied, the pair resolves to exactly that crossfade.
        let mut applied = state.clone();
        applied.replace_clip_in_place(&plan.left);
        applied.replace_clip_in_place(&plan.right);
        let crossfades = applied.audio_crossfades(&applied.tracks[0]);
        assert_eq!(pairs(&crossfades), vec![("l", "r")]);
        assert!(close(crossfades.crossfades[0].seconds, 0.020, 1.0e-4));
    }

    /// With no audio past the left half's end, the crossfade shifts into the
    /// right half's handle instead.
    #[test]
    fn plan_crossfade_shifts_when_one_side_has_no_handle() {
        let (state, left, right) = split_halves(0.0, 1.0);
        let center = state.crossfade_center_seconds(&left, &right);
        let plan = state
            .plan_crossfade(&left, &right, center, 0.020)
            .expect("shifted");
        assert!(close(plan.seconds, 0.020, 1.0e-9));
        assert_eq!(
            plan.left.stretch.source_end_samples,
            left.stretch.source_end_samples
        );
        let right_start = state.seconds_at_beat(plan.right.start_beat as f64);
        assert!(
            close(right_start, 2.980, 1.0e-4),
            "right starts at {right_start}"
        );
    }

    #[test]
    fn plan_crossfade_shrinks_to_the_handles_and_refuses_without_any() {
        // 5 ms each side: a 20 ms request fits 10 ms.
        let (state, left, right) = split_halves(0.005, 0.005);
        let center = state.crossfade_center_seconds(&left, &right);
        let plan = state
            .plan_crossfade(&left, &right, center, 0.020)
            .expect("shrunk");
        assert!(close(plan.seconds, 0.010, 1.0e-4), "got {}", plan.seconds);

        let (state, left, right) = split_halves(0.0, 0.0);
        let center = state.crossfade_center_seconds(&left, &right);
        assert_eq!(state.plan_crossfade(&left, &right, center, 0.020), None);
    }

    /// A crossfade handle drag re-plans from the gesture's origin: longer and
    /// shorter about the same centre, never below the minimum.
    #[test]
    fn plan_crossfade_resizes_an_existing_overlap_about_its_centre() {
        let (state, left, right) = split_halves(1.0, 1.0);
        let center = state.crossfade_center_seconds(&left, &right);
        let wide = state.plan_crossfade(&left, &right, center, 0.5).unwrap();
        let mut applied = state.clone();
        applied.replace_clip_in_place(&wide.left);
        applied.replace_clip_in_place(&wide.right);
        let (l, r) = (
            applied.tracks[0].clips[0].clone(),
            applied.tracks[0].clips[1].clone(),
        );
        let again = applied.crossfade_center_seconds(&l, &r);
        assert!(close(again, center, 1.0e-4), "the centre stays put");
        let narrow = applied.plan_crossfade(&l, &r, again, 0.1).unwrap();
        assert!(close(narrow.seconds, 0.1, 1.0e-9));
        let tiny = applied.plan_crossfade(&l, &r, again, 0.0).is_none();
        assert!(tiny, "below the minimum is refused, not zero");
        let floor = applied
            .plan_crossfade(&l, &r, again, MIN_CROSSFADE_SECONDS)
            .unwrap();
        assert!(close(floor.seconds, MIN_CROSSFADE_SECONDS, 1.0e-12));
    }

    /// Escape, a tool change or focus loss puts a previewed fade or crossfade
    /// back exactly as the gesture found it, in its slot on the track.
    #[test]
    fn a_cancelled_preview_puts_the_clips_back_exactly() {
        let (mut state, left, right) = split_halves(1.0, 1.0);
        let before = state.tracks[0].clips.clone();

        assert!(state.set_clip_fade_at_beat("l", FadeEdge::In, 1.0));
        let center = state.crossfade_center_seconds(&left, &right);
        let plan = state.plan_crossfade(&left, &right, center, 0.3).unwrap();
        assert!(state.replace_clip_in_place(&plan.left));
        assert!(state.replace_clip_in_place(&plan.right));
        assert_ne!(state.tracks[0].clips, before);

        assert!(state.replace_clip_in_place(&left));
        assert!(state.replace_clip_in_place(&right));
        assert_eq!(state.tracks[0].clips, before);
        // Restoring what is already there changes nothing.
        assert!(!state.replace_clip_in_place(&left));
    }

    #[test]
    fn plan_crossfade_refuses_warp_and_legacy_offset_clips() {
        let (state, mut left, right) = split_halves(1.0, 1.0);
        left.stretch.mode = StretchMode::Warp;
        assert_eq!(state.plan_crossfade(&left, &right, 3.0, 0.02), None);
        assert_eq!(
            crossfade_pair_block(&left, &right),
            Some(CrossfadeBlock::Warp)
        );

        let (state, left, mut right) = split_halves(1.0, 1.0);
        right.stretch.source_start_samples = 0;
        right.offset_beats = 6.0;
        assert_eq!(state.plan_crossfade(&left, &right, 3.0, 0.02), None);
        assert_eq!(
            crossfade_pair_block(&left, &right),
            Some(CrossfadeBlock::LegacyOffset)
        );
    }

    /// A reversed clip plays its window backwards: revealing source before
    /// its start would move all of its audio earlier, not extend it into the
    /// overlap. The planner refuses it, on either side, and says why.
    #[test]
    fn plan_crossfade_refuses_reversed_clips() {
        let (state, left, mut right) = split_halves(1.0, 1.0);
        let center = state.crossfade_center_seconds(&left, &right);
        assert!(state.plan_crossfade(&left, &right, center, 0.02).is_some());
        right.stretch.reverse = true;
        assert_eq!(state.plan_crossfade(&left, &right, center, 0.02), None);
        assert_eq!(
            crossfade_pair_block(&left, &right),
            Some(CrossfadeBlock::Reversed)
        );

        let (state, mut left, right) = split_halves(1.0, 1.0);
        left.stretch.reverse = true;
        assert_eq!(state.plan_crossfade(&left, &right, center, 0.02), None);

        // X says so rather than blaming the handles.
        let (mut state, _, _) = split_halves(1.0, 1.0);
        state.tracks[0].clips[1].stretch.reverse = true;
        state.selection.selected_clip_ids = vec!["l".into(), "r".into()];
        assert_eq!(
            state.crossfade_request(),
            Err(CrossfadeBlock::Reversed.command_message().to_string())
        );
    }

    /// Every reason the planner refuses is named, and a pair it can plan has
    /// none — the gate the crossfade handle is shown live by.
    #[test]
    fn a_crossfade_block_names_why_a_clip_cannot_be_trimmed() {
        let (state, left, right) = split_halves(1.0, 1.0);
        assert_eq!(crossfade_pair_block(&left, &right), None);
        let center = state.crossfade_center_seconds(&left, &right);
        assert!(state.plan_crossfade(&left, &right, center, 0.02).is_some());

        let mut undecoded = right.clone();
        undecoded.stretch.source_start_samples = 0;
        undecoded.stretch.source_end_samples = 0;
        assert_eq!(
            crossfade_trim_block(&undecoded),
            Some(CrossfadeBlock::Undecoded)
        );
        assert_eq!(state.plan_crossfade(&left, &undecoded, center, 0.02), None);

        let mut video = right.clone();
        video.clip_type = ClipType::Video {
            file_id: "v".into(),
            source_path: Some("v.mp4".into()),
        };
        assert_eq!(crossfade_trim_block(&video), Some(CrossfadeBlock::NotAudio));

        // The left clip's reason comes first.
        let mut warp = left.clone();
        warp.stretch.mode = StretchMode::Warp;
        let mut reversed = right.clone();
        reversed.stretch.reverse = true;
        assert_eq!(
            crossfade_pair_block(&warp, &reversed),
            Some(CrossfadeBlock::Warp)
        );
        for block in [
            CrossfadeBlock::NotAudio,
            CrossfadeBlock::Warp,
            CrossfadeBlock::Reversed,
            CrossfadeBlock::Undecoded,
            CrossfadeBlock::LegacyOffset,
        ] {
            assert!(!block.command_message().is_empty());
            assert!(!block.handle_tooltip().is_empty());
        }
    }

    /// When the requested length is the whole room, the two centre bounds
    /// are equal in exact arithmetic but can cross by an ulp in `f64` —
    /// these are such bounds — and `f64::clamp` panics on crossed bounds.
    #[test]
    fn a_crossfade_as_long_as_its_room_does_not_panic() {
        let (low, high) = (0.0280018_f64, 0.12251580000000001_f64);
        let length = high - low;
        assert!(
            low + length * 0.5 > high - length * 0.5,
            "the bounds cross, so this exercises the edge"
        );
        for center in [0.0, low, 0.07, high, 1.0] {
            let (start, end, planned) =
                centred_overlap(center, 1.0, low, high).expect("the whole room fits");
            assert_eq!(planned, length);
            assert!(start >= low - 1.0e-12 && end <= high + 1.0e-12);
            let (start, end, _) = centred_overlap(center, length, low, high).unwrap();
            assert!(start >= low - 1.0e-12 && end <= high + 1.0e-12);
        }
        assert_eq!(centred_overlap(f64::NAN, 0.02, low, high), None);
        assert_eq!(centred_overlap(0.05, 0.02, high, low), None);
        // A non-finite room is no room, not a panic.
        assert_eq!(centred_overlap(0.05, 0.02, f64::NAN, high), None);
        assert_eq!(centred_overlap(0.05, 0.02, low, f64::INFINITY), None);

        // Through the planner: a request for more than the handles hold is
        // planned at exactly the room there is.
        let (state, left, right) = split_halves(0.005, 0.005);
        let center = state.crossfade_center_seconds(&left, &right);
        for request in [0.010, 0.0100001, 0.02, 10.0] {
            let plan = state.plan_crossfade(&left, &right, center, request);
            assert!(plan.is_some(), "{request} s");
        }
    }

    /// The cached per-clip map is the direct transform, number for number:
    /// flat and ramped tempo, a clip that plays through the tempo map and one
    /// that plays at the project tempo.
    #[test]
    fn a_cached_clip_time_axis_is_the_direct_transform() {
        for ramped in [false, true] {
            let mut synced = audio_clip("s", 1.5, 3.0);
            synced.stretch.mode = StretchMode::TempoSync;
            let mut state = state_with(vec![audio_clip("a", 1.5, 6.0), synced]);
            state.viewport.pixels_per_second = 240.0;
            state.sync_pixels_per_beat();
            if ramped {
                ramp(&mut state);
            }
            state.sync_time_warp();
            for clip in state.tracks[0].clips.clone() {
                let start = clip.start_beat.max(0.0) as f64;
                let synced = clip.stretch.follows_project_tempo();
                let bpm = state.bpm.max(1.0) as f64;
                let time = state.clip_time_axis(&clip);
                for seconds in [-0.25, 0.0, 0.001, 0.5, 1.75, 2.999, 4.0] {
                    let direct = if synced {
                        (start + seconds * bpm / 60.0).max(0.0)
                    } else {
                        state.beat_at_seconds(state.seconds_at_beat(start) + seconds)
                    };
                    assert_eq!(time.beat_at_local_seconds(seconds), direct);
                    assert_eq!(
                        time.lane_x_at_local_seconds(seconds),
                        state.beats_to_x(direct as f32)
                    );
                    assert_eq!(
                        time.lane_x_at_local_seconds(seconds),
                        state.clip_lane_x_at_local_seconds(&clip, seconds)
                    );
                }
                for beat in [0.0, 1.5, 2.0, 3.25, 7.0] {
                    let direct = if synced {
                        (beat - start) * 60.0 / bpm
                    } else {
                        state.seconds_at_beat(beat) - state.seconds_at_beat(start)
                    };
                    assert_eq!(time.local_seconds_at_beat(beat), direct);
                }
                for x in [0.0_f32, 120.5, 480.0] {
                    assert_eq!(
                        time.local_seconds_at_lane_x(x),
                        state.clip_local_seconds_at_lane_x(&clip, x)
                    );
                }
                assert_eq!(time.is_linear(), !ramped);
            }
        }
    }

    /// X adds a crossfade; it never shortens one. Clips that already overlap
    /// by the default length or more are left alone, with a reason, while a
    /// sliver of an overlap still grows to the default.
    #[test]
    fn the_crossfade_command_leaves_an_existing_crossfade_alone() {
        let (state, left, right) = split_halves(1.0, 1.0);
        let center = state.crossfade_center_seconds(&left, &right);
        let long = state.plan_crossfade(&left, &right, center, 0.5).unwrap();
        let mut overlapped = state.clone();
        overlapped.replace_clip_in_place(&long.left);
        overlapped.replace_clip_in_place(&long.right);
        overlapped.selection.selected_clip_ids = vec!["l".into(), "r".into()];
        let before = overlapped.tracks[0].clips.clone();
        assert_eq!(
            overlapped.crossfade_request(),
            Err("These clips already crossfade over their overlap".to_string())
        );
        assert_eq!(overlapped.tracks[0].clips, before);

        // A second X on the crossfade the first one made is refused too.
        let default = state
            .plan_crossfade(&left, &right, center, DEFAULT_CROSSFADE_SECONDS)
            .unwrap();
        let mut again = state.clone();
        again.replace_clip_in_place(&default.left);
        again.replace_clip_in_place(&default.right);
        again.selection.selected_clip_ids = vec!["l".into(), "r".into()];
        assert!(again.crossfade_request().is_err());

        // 5 ms of overlap: X lengthens it to the default about its centre.
        let short = state.plan_crossfade(&left, &right, center, 0.005).unwrap();
        let mut sliver = state.clone();
        sliver.replace_clip_in_place(&short.left);
        sliver.replace_clip_in_place(&short.right);
        sliver.selection.selected_clip_ids = vec!["l".into(), "r".into()];
        let request = sliver.crossfade_request().expect("a sliver grows");
        assert_eq!(request.length_seconds, DEFAULT_CROSSFADE_SECONDS);
    }

    /// X on a pair the planner cannot re-trim names the real reason instead
    /// of "no audio beyond the clip edges".
    #[test]
    fn the_crossfade_command_says_why_a_pair_cannot_be_trimmed() {
        let (mut state, _, _) = split_halves(1.0, 1.0);
        state.selection.selected_clip_ids = vec!["l".into(), "r".into()];
        assert!(state.crossfade_request().is_ok());

        state.tracks[0].clips[0].stretch.mode = StretchMode::Warp;
        assert_eq!(
            state.crossfade_request(),
            Err(CrossfadeBlock::Warp.command_message().to_string())
        );
    }

    /// The engine crossfades overlapping reference videos, whose sound it
    /// plays; the arrangement draws no crossfade for them.
    #[test]
    fn video_clips_crossfade_in_the_engine_only() {
        let mut state = TimelineState::default();
        state.bpm = 120.0;
        state.tracks.clear();
        let first = state.insert_video_clip("a.mp4".into(), "a".into(), 0.0);
        let second = state.insert_video_clip("b.mp4".into(), "b".into(), 12.0);
        let track = state
            .tracks
            .iter()
            .find(|track| track.track_type == TrackType::Video)
            .unwrap();
        assert!(state.audio_crossfades(track).is_empty());
        let crossfades = state.engine_crossfades(track);
        assert_eq!(pairs(&crossfades), vec![(first.as_str(), second.as_str())]);
        let expected = state.beats_to_seconds(4.0) as f64;
        assert!(close(crossfades.crossfades[0].seconds, expected, 1.0e-6));
        assert!(close(
            crossfades.fade_out_override(&first).unwrap(),
            expected,
            1.0e-6
        ));
        assert!(close(
            crossfades.fade_in_override(&second).unwrap(),
            expected,
            1.0e-6
        ));
    }

    #[test]
    fn the_crossfade_command_needs_two_touching_clips_on_one_track() {
        let (mut state, left, right) = split_halves(1.0, 1.0);
        state.selection.selected_clip_ids = vec![right.id.clone(), left.id.clone()];
        let request = state.crossfade_request().expect("two abutting halves");
        assert_eq!(request.left_id, "l");
        assert_eq!(request.right_id, "r");
        assert!(close(request.center_seconds, 3.0, 1.0e-6));
        assert_eq!(request.length_seconds, DEFAULT_CROSSFADE_SECONDS);

        state.selection.selected_clip_ids = vec![left.id.clone()];
        assert!(state.crossfade_request().is_err());

        // Far apart: not a boundary.
        let mut apart = state_with(vec![audio_clip("a", 0.0, 1.0), audio_clip("b", 8.0, 1.0)]);
        apart.selection.selected_clip_ids = vec!["a".into(), "b".into()];
        assert!(apart.crossfade_request().is_err());

        // On an ARA track the engine would ignore it.
        let (mut ara, left, right) = split_halves(1.0, 1.0);
        ara.tracks[0].ara = Some(AraTrackBinding {
            plugin_id: "melodyne".into(),
            plugin_path: "m.vst3".into(),
            class_id: "c".into(),
        });
        ara.selection.selected_clip_ids = vec![left.id, right.id];
        assert!(ara.crossfade_request().is_err());
    }

    #[test]
    fn a_range_across_one_boundary_sets_the_crossfade_length() {
        let (mut state, _, _) = split_halves(1.0, 1.0);
        let track_id = state.tracks[0].id.clone();
        // 2.9 s .. 3.1 s around the cut at beat 6.
        state.arrangement_range = Some(TimelineRangeSelection::new(5.8, 6.2, vec![track_id]));
        let request = state
            .crossfade_request()
            .expect("one boundary inside the range");
        assert!(close(request.length_seconds, 0.2, 1.0e-6));
        assert!(close(request.center_seconds, 3.0, 1.0e-6));

        state.arrangement_range = Some(TimelineRangeSelection::new(6.5, 7.0, vec![]));
        assert!(state.crossfade_request().is_err());
    }
}
