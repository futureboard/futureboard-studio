//! Ordered track clips and the derived data used by the arrangement.

use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex, OnceLock};

use super::{
    AudioCrossfade, ClipState, ClipTimeAxis, ClipType, EffectiveFades, TimelineState, TrackState,
    CLIP_MIN_DRAWN_WIDTH,
};

/// The owning clip order is also the arrangement's stacking order and the
/// project/undo order. Every mutable borrow clears the derived data before it
/// can change the clip list or anything inside a clip.
#[derive(Debug, Default)]
pub struct TrackClips {
    clips: Vec<ClipState>,
    last_end: OnceLock<Option<f32>>,
    slots: OnceLock<HashMap<String, usize>>,
    arrangement: Mutex<Option<(u64, Arc<ClipIntervals>)>>,
    fades: Mutex<Option<(u64, Arc<ArrangementCrossfades>)>>,
}

impl TrackClips {
    pub(crate) fn last_end_beat(&self) -> Option<f32> {
        *self.last_end.get_or_init(|| {
            self.iter()
                .map(|clip| clip.start_beat + clip.duration_beats)
                .reduce(f32::max)
        })
    }

    pub(crate) fn index_of(&self, id: &str) -> Option<usize> {
        let slots = self.slots.get_or_init(|| {
            let mut slots = HashMap::with_capacity(self.len());
            for (index, clip) in self.iter().enumerate() {
                slots.entry(clip.id.clone()).or_insert(index);
            }
            slots
        });
        slots.get(id).copied()
    }
}

impl Clone for TrackClips {
    fn clone(&self) -> Self {
        Self::from(self.clips.clone())
    }
}

impl PartialEq for TrackClips {
    fn eq(&self, other: &Self) -> bool {
        self.clips == other.clips
    }
}

impl From<Vec<ClipState>> for TrackClips {
    fn from(clips: Vec<ClipState>) -> Self {
        Self {
            clips,
            ..Self::default()
        }
    }
}

impl FromIterator<ClipState> for TrackClips {
    fn from_iter<T: IntoIterator<Item = ClipState>>(iter: T) -> Self {
        Self::from(iter.into_iter().collect::<Vec<_>>())
    }
}

impl Deref for TrackClips {
    type Target = Vec<ClipState>;

    fn deref(&self) -> &Self::Target {
        &self.clips
    }
}

impl DerefMut for TrackClips {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.last_end.take();
        self.slots.take();
        self.arrangement = Mutex::default();
        self.fades = Mutex::default();
        &mut self.clips
    }
}

impl<'a> IntoIterator for &'a TrackClips {
    type Item = &'a ClipState;
    type IntoIter = std::slice::Iter<'a, ClipState>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a> IntoIterator for &'a mut TrackClips {
    type Item = &'a mut ClipState;
    type IntoIter = std::slice::IterMut<'a, ClipState>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl IntoIterator for TrackClips {
    type Item = ClipState;
    type IntoIter = std::vec::IntoIter<ClipState>;

    fn into_iter(self) -> Self::IntoIter {
        self.clips.into_iter()
    }
}

#[derive(Debug, Clone, Copy)]
struct BeatSpan {
    start: f32,
    end: f32,
    index: usize,
}

#[derive(Debug, Default)]
struct IntervalIndex {
    root: Option<Box<IntervalNode>>,
    unbounded: Vec<usize>,
    len: usize,
}

/// Each interval lives at the first median it crosses. A query follows the
/// two boundary paths and visits only matching buckets between them.
#[derive(Debug)]
struct IntervalNode {
    center: f32,
    by_start: Vec<BeatSpan>,
    by_end: Vec<BeatSpan>,
    left: Option<Box<Self>>,
    right: Option<Box<Self>>,
}

impl IntervalIndex {
    fn new(spans: impl Iterator<Item = (f32, f32)>) -> Self {
        let mut index = Self::default();
        let mut finite = Vec::new();
        for (slot, (start, end)) in spans.enumerate() {
            index.len += 1;
            if start.is_finite() && end.is_finite() {
                let start = start.max(0.0);
                finite.push(BeatSpan {
                    start,
                    end: end.max(start),
                    index: slot,
                });
            } else {
                index.unbounded.push(slot);
            }
        }
        finite.sort_unstable_by(|a, b| a.start.total_cmp(&b.start));
        index.root = IntervalNode::build(finite);
        index
    }

    fn candidates(
        &self,
        state: &TimelineState,
        left: f32,
        right: f32,
        min_width: f32,
    ) -> Vec<usize> {
        // The span uses beats, independent of zoom. Pad the inverse transform
        // for the minimum drawn width, pixel rounding and f32 precision at
        // large scroll offsets. The caller then checks exact pixel geometry.
        let magnitude = state.viewport.scroll_x.abs() + left.abs().max(right.abs());
        let pad = 4.0 + magnitude * (16.0 * f32::EPSILON);
        let from = state.x_to_beats(left - min_width - pad);
        let to = state.x_to_beats(right + pad);
        if !from.is_finite() || !to.is_finite() || !pad.is_finite() {
            return (0..self.len).collect();
        }
        let mut hits = self.unbounded.clone();
        if let Some(root) = &self.root {
            root.query(from, to, &mut hits);
        }
        // Preserve clip order. On a dense result the exact scan is cheaper
        // than sorting the result and naturally produces that order.
        if hits.len() > 1 && hits.len().saturating_mul(hits.len().ilog2() as usize) >= self.len {
            return (0..self.len).collect();
        }
        hits.sort_unstable();
        hits
    }
}

impl IntervalNode {
    fn build(sorted: Vec<BeatSpan>) -> Option<Box<Self>> {
        let center = sorted.get(sorted.len() / 2)?.start;
        // Small leaves keep disjoint sessions from allocating a node and two
        // vectors for every clip. A leaf examines at most this many spans.
        const LEAF_CAPACITY: usize = 16;
        if sorted.len() <= LEAF_CAPACITY {
            return Some(Box::new(Self {
                center,
                by_start: sorted,
                by_end: Vec::new(),
                left: None,
                right: None,
            }));
        }
        let mut left = Vec::new();
        let mut right = Vec::new();
        let mut by_start = Vec::new();
        for span in sorted {
            if span.end < center {
                left.push(span);
            } else if span.start > center {
                right.push(span);
            } else {
                by_start.push(span);
            }
        }
        let mut by_end = by_start.clone();
        by_end.sort_unstable_by(|a, b| b.end.total_cmp(&a.end));
        Some(Box::new(Self {
            center,
            by_start,
            by_end,
            left: Self::build(left),
            right: Self::build(right),
        }))
    }

    fn query(&self, from: f32, to: f32, hits: &mut Vec<usize>) {
        if self.by_end.is_empty() {
            hits.extend(
                self.by_start
                    .iter()
                    .filter(|span| span.start <= to && span.end >= from)
                    .map(|span| span.index),
            );
            return;
        }
        if to < self.center {
            hits.extend(
                self.by_start
                    .iter()
                    .take_while(|span| span.start <= to)
                    .map(|s| s.index),
            );
        } else if from > self.center {
            hits.extend(
                self.by_end
                    .iter()
                    .take_while(|span| span.end >= from)
                    .map(|s| s.index),
            );
        } else {
            hits.extend(self.by_start.iter().map(|s| s.index));
        }
        if let Some(left) = self.left.as_ref().filter(|_| from < self.center) {
            left.query(from, to, hits);
        }
        if let Some(right) = self.right.as_ref().filter(|_| to > self.center) {
            right.query(from, to, hits);
        }
    }
}

#[derive(Debug)]
struct ClipIntervals {
    ends: Vec<f32>,
    index: IntervalIndex,
}

/// One tempo-key lookup for a frame or gesture, shared by all lane queries.
pub(crate) struct ArrangementTempo {
    key: u64,
    map: Arc<DirectAudio::TempoMap>,
}

impl ArrangementTempo {
    pub(crate) fn new(state: &TimelineState) -> Self {
        let key = state.tempo_cache_key();
        Self {
            key,
            map: state.resolved_tempo_map_for_key(key),
        }
    }

    pub(crate) fn lookup<'a>(&self, state: &'a TimelineState) -> super::TempoLookup<'a> {
        state.tempo_lookup_in(&self.map)
    }

    pub(crate) fn map(&self) -> &DirectAudio::TempoMap {
        &self.map
    }
}

impl TimelineState {
    /// Exact drawn spans intersecting the requested lane interval, in storage order.
    pub(crate) fn clips_in_lane_range(
        &self,
        track: &TrackState,
        tempo: &ArrangementTempo,
        left: f32,
        right: f32,
        live_edit: bool,
    ) -> Vec<(usize, f32, f32)> {
        let intervals = (!live_edit).then(|| {
            let mut cache = track
                .clips
                .arrangement
                .lock()
                .expect("clip interval cache poisoned");
            if let Some((_, intervals)) = cache.as_ref().filter(|(key, _)| *key == tempo.key) {
                return intervals.clone();
            }
            let intervals = Arc::new(self.build_clip_intervals(track, tempo));
            *cache = Some((tempo.key, intervals.clone()));
            intervals
        });
        let candidates = intervals.as_ref().map_or_else(
            || (0..track.clips.len()).collect(),
            |intervals| {
                intervals
                    .index
                    .candidates(self, left, right, CLIP_MIN_DRAWN_WIDTH)
            },
        );
        let hidden: HashSet<&str> = track
            .takes
            .iter()
            .filter(|take| !take.active)
            .map(|take| take.clip_id.as_str())
            .collect();
        candidates
            .into_iter()
            .filter_map(|index| {
                let clip = &track.clips[index];
                if hidden.contains(clip.id.as_str()) {
                    return None;
                }
                let end = intervals.as_ref().map_or_else(
                    || self.clip_end_in(clip, tempo),
                    |intervals| intervals.ends[index],
                );
                let (x, width) = self.clip_lane_x_span_with_end(clip, end);
                (x + width >= left && x <= right).then_some((index, x, width))
            })
            .collect()
    }

    pub(crate) fn clip_end_in(&self, clip: &ClipState, tempo: &ArrangementTempo) -> f32 {
        let end = clip.start_beat + clip.duration_beats.max(0.0);
        if matches!(clip.clip_type, ClipType::Audio { .. }) {
            return self
                .audio_clip_end_beat_in(clip, &tempo.map)
                .map_or(end, |end| end as f32);
        }
        end
    }

    fn build_clip_intervals(&self, track: &TrackState, tempo: &ArrangementTempo) -> ClipIntervals {
        let ends: Vec<_> = track
            .clips
            .iter()
            .map(|clip| self.clip_end_in(clip, tempo))
            .collect();
        let index = IntervalIndex::new(
            track
                .clips
                .iter()
                .zip(&ends)
                .map(|(clip, end)| (clip.start_beat, *end)),
        );
        ClipIntervals { ends, index }
    }

    pub(crate) fn arrangement_crossfades(
        &self,
        track: &TrackState,
        tempo: &ArrangementTempo,
        live_edit: bool,
    ) -> Arc<ArrangementCrossfades> {
        let build = || Arc::new(ArrangementCrossfades::new(self, track, tempo, live_edit));
        if live_edit {
            return build();
        }
        let mut cache = track.clips.fades.lock().expect("crossfade cache poisoned");
        if let Some((_, fades)) = cache.as_ref().filter(|(key, _)| *key == tempo.key) {
            return fades.clone();
        }
        let fades = build();
        *cache = Some((tempo.key, fades.clone()));
        fades
    }
}

#[derive(Debug)]
pub(crate) struct ArrangementCrossfades {
    pairs: Vec<AudioCrossfade>,
    overlays: Option<IntervalIndex>,
    effective: HashMap<usize, EffectiveFades>,
    endpoints: Vec<(usize, usize)>,
}

impl ArrangementCrossfades {
    fn new(
        state: &TimelineState,
        track: &TrackState,
        tempo: &ArrangementTempo,
        live_edit: bool,
    ) -> Self {
        let fades = state.audio_crossfades_in(track, &tempo.map);
        let index = fades.index();
        let mut effective = HashMap::new();
        let mut endpoints = Vec::with_capacity(fades.crossfades.len());
        for pair in &fades.crossfades {
            let left = track
                .clips
                .index_of(&pair.left_id)
                .expect("resolved left clip");
            let right = track
                .clips
                .index_of(&pair.right_id)
                .expect("resolved right clip");
            endpoints.push((left, right));
            for slot in [left, right] {
                effective.entry(slot).or_insert_with(|| {
                    let clip = &track.clips[slot];
                    let axis = state.clip_time_axis_in(tempo.lookup(state), clip);
                    state.effective_clip_fades_indexed_in(clip, &index, &axis)
                });
            }
        }
        let pairs = fades.crossfades;
        let overlays = (!live_edit).then(|| {
            IntervalIndex::new(
                pairs
                    .iter()
                    .map(|pair| (pair.start_beat as f32, pair.end_beat as f32)),
            )
        });
        Self {
            pairs,
            overlays,
            effective,
            endpoints,
        }
    }

    pub(crate) fn visible_candidates(
        &self,
        state: &TimelineState,
        left: f32,
        right: f32,
    ) -> impl Iterator<Item = (&AudioCrossfade, usize, usize)> {
        self.overlays
            .as_ref()
            .map_or_else(
                || (0..self.pairs.len()).collect(),
                |index| index.candidates(state, left, right, 0.0),
            )
            .into_iter()
            .map(|index| {
                let (left, right) = self.endpoints[index];
                (&self.pairs[index], left, right)
            })
    }

    pub(crate) fn effective_fades(
        &self,
        state: &TimelineState,
        track: &TrackState,
        index: usize,
        axis: &ClipTimeAxis<'_>,
    ) -> EffectiveFades {
        self.effective
            .get(&index)
            .copied()
            .unwrap_or_else(|| state.effective_clip_fades_in(&track.clips[index], None, None, axis))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offscreen_clips_do_not_add_exact_geometry_candidates() {
        let mut state = TimelineState::default();
        state.viewport.pixels_per_second = 40.0;
        state.sync_pixels_per_beat();
        for count in [1_000, 10_000, 100_000] {
            let index = IntervalIndex::new((0..count).map(|index| {
                let start = index as f32 * 8.0;
                (start, start + 7.0)
            }));
            for frame in 0..20 {
                state.viewport.scroll_x = frame as f32 * 8.0;
                let candidates = index.candidates(&state, 0.0, 1600.0, CLIP_MIN_DRAWN_WIDTH);
                assert!(candidates.len() <= 12, "{count} clips: {candidates:?}");
            }
        }
    }
}
