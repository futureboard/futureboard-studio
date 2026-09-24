//! Paint-ready clip visuals, resolved from clip data.
//!
//! This is the single source of truth for *what a clip looks like*, shared by
//! the interactive GPUI element path and the snapshot painter. Keeping one
//! implementation is what lets the two backends stay pixel-identical while the
//! arrangement moves from an element tree to a painted surface.
//!
//! Everything here is pure: clip data in, geometry out. No GPUI, no theme
//! lookups, no allocation per note — so it is unit-testable and cheap enough to
//! run on every repaint.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use crate::components::timeline::timeline_state::{
    midi_edit_revision, MidiControllerKind, MidiControllerLane, MidiNoteState, TimeWarp,
};

/// Clip-local beat ↔ clip-local px for content drawn inside a clip.
///
/// The arrangement's x axis follows the tempo map (see [`TimeWarp`]), so a
/// note's x inside a clip is not `beat × ppb` once the tempo varies across the
/// clip: it is the warp taken relative to the clip's start. With a constant
/// tempo this is exactly `beat × ppb`.
#[derive(Debug, Clone)]
pub struct ClipAxis {
    ppb: f32,
    pps: f32,
    clip_start: f64,
    origin: f64,
    warp: TimeWarp,
}

impl ClipAxis {
    pub fn new(
        warp: TimeWarp,
        clip_start: f32,
        pixels_per_second: f32,
        pixels_per_beat: f32,
    ) -> Self {
        let origin = warp.content_x(clip_start as f64, pixels_per_second, pixels_per_beat);
        Self {
            ppb: pixels_per_beat,
            pps: pixels_per_second,
            clip_start: clip_start as f64,
            origin,
            warp,
        }
    }

    /// Constant tempo at `pixels_per_beat`.
    pub fn linear(pixels_per_beat: f32) -> Self {
        Self::new(TimeWarp::default(), 0.0, pixels_per_beat, pixels_per_beat)
    }

    /// Nominal pixels per beat (the zoom), for density decisions.
    pub fn pixels_per_beat(&self) -> f32 {
        self.ppb
    }

    #[inline]
    pub fn x(&self, local_beat: f32) -> f32 {
        if self.warp.is_linear() {
            return local_beat * self.ppb;
        }
        (self
            .warp
            .content_x(self.clip_start + local_beat as f64, self.pps, self.ppb)
            - self.origin) as f32
    }

    #[inline]
    pub fn beat(&self, local_x: f32) -> f32 {
        if self.warp.is_linear() {
            return local_x / self.ppb.max(0.0001);
        }
        (self
            .warp
            .beat_at_content_x(self.origin + local_x as f64, self.pps, self.ppb)
            - self.clip_start) as f32
    }

    /// What a cached preview built against this axis depends on.
    fn cache_key(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.ppb.to_bits().hash(&mut hasher);
        if !self.warp.is_linear() {
            self.warp.key().hash(&mut hasher);
            self.pps.to_bits().hash(&mut hasher);
            self.clip_start.to_bits().hash(&mut hasher);
        }
        hasher.finish()
    }
}

// ── Preview cache ─────────────────────────────────────────────────────────────
//
// Building a preview is a pass over every note in the clip, and the arrangement
// asks for one per visible clip on every repaint — which is every scroll, every
// zoom and every selection. That makes a frame cost `visible clips × notes`
// while producing an output bounded by `visible clips × pixels`: a session with
// dense imported parts was measured at 40 ms a frame for 24,000 painted quads.
//
// So the pass runs once per (content, geometry) pair and is reused until one of
// them moves. Validity is a single integer compare against the global MIDI edit
// revision — see [`midi_edit_revision`], which exists for exactly this and is
// bumped by the mutable accessors themselves, so no edit path has to remember.
// The note count is in the key as well, which catches a clip whose payload was
// replaced wholesale rather than edited in place.

/// Bounded, insertion-ordered map. Same shape as the waveform geometry cache:
/// the working set is the visible clips times a handful of recent zoom/scroll
/// states, and evicting the oldest key keeps that resident without unbounded
/// growth on a long scroll.
///
/// Values are cloned out on a hit, so they are stored already wrapped — a hit
/// is a reference-count bump, never a copy of the geometry.
struct PreviewCache<T: Clone> {
    map: HashMap<u64, T>,
    order: VecDeque<u64>,
    cap: usize,
}

impl<T: Clone> PreviewCache<T> {
    fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap,
        }
    }

    fn get(&self, key: u64) -> Option<T> {
        self.map.get(&key).cloned()
    }

    fn insert(&mut self, key: u64, value: T) {
        if self.map.insert(key, value).is_none() {
            self.order.push_back(key);
            while self.order.len() > self.cap {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
        }
    }
}

/// `None` is cached as well as `Some`: a clip that genuinely draws nothing must
/// not be re-walked every frame to rediscover that.
type CachedNotePreview = Option<Arc<NotePreview>>;
type CachedControllerPreview = Option<Arc<ControllerPreview>>;

fn note_preview_cache() -> &'static Mutex<PreviewCache<CachedNotePreview>> {
    static CACHE: OnceLock<Mutex<PreviewCache<CachedNotePreview>>> = OnceLock::new();
    // Sized for the live working set, which is the visible clips times the two
    // or three scroll tiles each has recently occupied. A dense arrangement
    // shows a couple of hundred clips at once, so a few hundred entries is the
    // floor — under it a wide session evicts its own previous tile and pays a
    // rebuild for scrolling back the way it came. A preview is bounded by the
    // pixels it covers, so this is single-digit megabytes at worst.
    CACHE.get_or_init(|| Mutex::new(PreviewCache::new(2048)))
}

fn controller_preview_cache() -> &'static Mutex<PreviewCache<CachedControllerPreview>> {
    static CACHE: OnceLock<Mutex<PreviewCache<CachedControllerPreview>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(PreviewCache::new(256)))
}

thread_local! {
    /// Note previews actually built on this thread.
    ///
    /// The arrangement's whole scaling claim is that this counts *geometry* —
    /// one build per clip per window per content revision — and never grows
    /// with the number of notes behind the clip. A test can assert that without
    /// a stopwatch; a wall-clock comparison would only be measuring the machine.
    ///
    /// Per thread rather than global, for the same reason the arrangement is:
    /// the builds a caller cares about are the ones it caused. A process-wide
    /// counter would also be counting whatever another window — or another
    /// test running beside this one — happened to draw.
    static NOTE_PREVIEWS_BUILT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// See [`NOTE_PREVIEWS_BUILT`].
pub fn note_previews_built() -> u64 {
    NOTE_PREVIEWS_BUILT.with(|built| built.get())
}

/// Everything a preview's shape depends on, as one integer.
///
/// `clip_id` identifies the clip, the edit revision identifies its content, and
/// the geometry values identify the window it is being drawn into. A miss on
/// any of them rebuilds; there is no path that reuses geometry built for
/// different notes or a different zoom.
fn preview_key(clip_id: &str, content_len: usize, axis: u64, geometry: &[f32]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    clip_id.hash(&mut hasher);
    axis.hash(&mut hasher);
    midi_edit_revision().hash(&mut hasher);
    content_len.hash(&mut hasher);
    for value in geometry {
        value.to_bits().hash(&mut hasher);
    }
    hasher.finish()
}

/// Scroll granularity the note preview is cached at, in clip-local pixels.
///
/// A cache keyed on the exact visible window is a cache that misses on every
/// frame of a scroll — which is the one gesture whose cost the user feels. So
/// the window is snapped out to a tile boundary: the geometry covers a little
/// more than is on screen, and stays valid until the scroll crosses a tile.
///
/// 256 px is roughly a sixth of an editing viewport: the extra columns built
/// and painted are a rounding error against re-walking every note, and one
/// entry survives a quarter-screen of travel.
const PREVIEW_TILE_PX: f32 = 256.0;

/// Snap a visible window out to tile boundaries, clamped to the clip.
///
/// Clamping matters: the preview's columns are painted at clip-local x inside
/// the clip's own element, so a window that ran past the clip's edge would draw
/// note mass outside the clip it belongs to.
fn tiled_window(px_start: f32, px_end: f32, clip_width_px: f32) -> (f32, f32) {
    let limit = clip_width_px.max(0.0);
    let start = (px_start / PREVIEW_TILE_PX).floor() * PREVIEW_TILE_PX;
    let end = (px_end / PREVIEW_TILE_PX).ceil() * PREVIEW_TILE_PX;
    (start.max(0.0), end.min(limit).max(px_end.min(limit)))
}

/// [`build_note_preview`], reused across frames and across a scroll.
///
/// The uncached builder stays public and is what the geometry tests exercise:
/// the cache is a memo over it, never a second implementation.
pub fn note_preview_cached(
    clip_id: &str,
    notes: &[MidiNoteState],
    clip_len: f32,
    axis: &ClipAxis,
    px_start: f32,
    px_end: f32,
) -> Option<Arc<NotePreview>> {
    let clip_width_px = axis.x(clip_len.max(0.0)).max(px_end);
    let (px_start, px_end) = tiled_window(px_start, px_end, clip_width_px);
    let key = preview_key(
        clip_id,
        notes.len(),
        axis.cache_key(),
        &[clip_len, px_start, px_end],
    );
    if let Some(hit) = note_preview_cache()
        .lock()
        .ok()
        .and_then(|cache| cache.get(key))
    {
        crate::perf::count("midi_preview_cache_hit", 1);
        return hit;
    }
    crate::perf::count("midi_preview_cache_miss", 1);
    NOTE_PREVIEWS_BUILT.with(|built| built.set(built.get().saturating_add(1)));
    let built = build_note_preview(notes, clip_len, axis, px_start, px_end).map(Arc::new);
    if let Ok(mut cache) = note_preview_cache().lock() {
        cache.insert(key, built.clone());
    }
    built
}

/// [`build_controller_preview`], reused across frames.
pub fn controller_preview_cached(
    clip_id: &str,
    lanes: &[MidiControllerLane],
    clip_len: f32,
    axis: &ClipAxis,
    width: f32,
) -> Option<Arc<ControllerPreview>> {
    let content_len: usize = lanes.iter().map(|lane| lane.points.len()).sum();
    let key = preview_key(
        clip_id,
        content_len,
        axis.cache_key(),
        &[clip_len, width, -1.0],
    );
    if let Some(hit) = controller_preview_cache()
        .lock()
        .ok()
        .and_then(|cache| cache.get(key))
    {
        return hit;
    }
    let built = build_controller_preview(lanes, clip_len, axis, width).map(Arc::new);
    if let Ok(mut cache) = controller_preview_cache().lock() {
        cache.insert(key, built.clone());
    }
    built
}

/// Clip-local pixel window that is actually on screen, or `None` when the clip
/// is fully scrolled out. `left` is the clip's x in lane coordinates.
pub fn visible_clip_px_range(left: f32, width: f32, viewport_width: f32) -> Option<(f32, f32)> {
    let lane_w = viewport_width.max(1.0);
    let start = (-left).max(0.0);
    let end = (lane_w - left).min(width);
    (end > start).then_some((start, end))
}

/// One horizontal pixel column of coalesced note mass. Pitches are normalized
/// (0 = bottom of the clip's pitch span, 1 = top) so the paint pass can map them
/// against the real canvas height without re-reading the notes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NoteColumn {
    pub x: f32,
    pub lowest_norm: f32,
    pub highest_norm: f32,
}

/// A single note quad, used at zoom levels where notes are individually visible.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NoteQuad {
    pub x: f32,
    pub width: f32,
    pub norm_pitch: f32,
}

/// Resolved note-preview geometry for one clip. Size is bounded by the clip's
/// visible pixel width, never by its note count.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NotePreview {
    pub columns: Vec<NoteColumn>,
    pub quads: Vec<NoteQuad>,
    /// Notes inside the clip bounds, for diagnostics only.
    pub note_count: usize,
}

/// Collapse a clip's notes into paintable geometry.
///
/// One allocation-free pass over the notes. Raw MIDI pitches are accumulated
/// into the output slots and normalized afterwards against the clip's full
/// pitch span, which keeps the whole thing single-pass while still mapping
/// pitch from the *entire* clip — so the preview does not shift vertically as
/// the clip scrolls in and out of view.
pub fn build_note_preview(
    notes: &[MidiNoteState],
    clip_len: f32,
    axis: &ClipAxis,
    px_start: f32,
    px_end: f32,
) -> Option<NotePreview> {
    let ppb = axis.pixels_per_beat();
    if notes.is_empty() || ppb <= 0.0 || px_end <= px_start {
        return None;
    }

    let visible_start_beat = axis.beat(px_start).max(0.0);
    let visible_end_beat = axis.beat(px_end).min(clip_len).max(0.0);
    let visible_width = px_end - px_start;

    // Very dense / zoomed-out MIDI maps many notes to the same pixel. Coalesce to
    // one vertical span per x-column so paint calls stay bounded by clip width
    // rather than note count while preserving the musical mass. `notes.len()` is
    // the upper bound on how many land in the window; this is a density heuristic,
    // so the bound is as good as the exact count and costs no extra pass.
    let dense = ppb < 5.0 || notes.len() > (visible_width as usize).saturating_mul(3);
    let columns = visible_width.ceil().clamp(1.0, 2400.0) as usize;
    let mut spans: Vec<Option<(u8, u8)>> = if dense {
        vec![None; columns]
    } else {
        Vec::new()
    };
    let mut raw_quads: Vec<(f32, f32, u8)> = Vec::new();
    let min_note_w = if ppb < 3.0 { 1.0 } else { 2.0 };

    let mut lo = u8::MAX;
    let mut hi = 0u8;
    let mut in_bounds = 0usize;
    for note in notes {
        let start = note.start.max(0.0);
        let end = (note.start + note.duration).min(clip_len);
        if note.start >= clip_len || note.start + note.duration <= 0.0 || end <= start {
            continue;
        }
        // Pitch span covers the whole clip, not just the visible window.
        in_bounds += 1;
        lo = lo.min(note.pitch);
        hi = hi.max(note.pitch);

        if start >= visible_end_beat || end <= visible_start_beat {
            continue;
        }
        if dense {
            let x0 = (axis.x(start) - px_start)
                .floor()
                .clamp(0.0, (columns - 1) as f32) as usize;
            let x1 = (axis.x(end) - px_start)
                .ceil()
                .clamp(x0 as f32, (columns - 1) as f32) as usize;
            for cell in &mut spans[x0..=x1] {
                *cell = Some(match *cell {
                    Some((low, high)) => (low.min(note.pitch), high.max(note.pitch)),
                    None => (note.pitch, note.pitch),
                });
            }
        } else {
            let x = axis.x(start);
            raw_quads.push((x, (axis.x(end) - x).max(min_note_w), note.pitch));
        }
    }
    if in_bounds == 0 {
        return None;
    }

    let top_pitch = hi.saturating_add(2).min(127);
    let bottom_pitch = lo.saturating_sub(2);
    let pitch_range = (top_pitch as i32 - bottom_pitch as i32).max(12) as f32;
    let norm_of = |pitch: u8| (pitch as i32 - bottom_pitch as i32) as f32 / pitch_range;

    let columns: Vec<NoteColumn> = spans
        .into_iter()
        .enumerate()
        .filter_map(|(col, span)| {
            span.map(|(low, high)| NoteColumn {
                x: px_start + col as f32,
                lowest_norm: norm_of(low),
                highest_norm: norm_of(high),
            })
        })
        .collect();
    let quads: Vec<NoteQuad> = raw_quads
        .into_iter()
        .map(|(x, width, pitch)| NoteQuad {
            x,
            width,
            norm_pitch: norm_of(pitch),
        })
        .collect();
    if columns.is_empty() && quads.is_empty() {
        return None;
    }
    Some(NotePreview {
        columns,
        quads,
        note_count: in_bounds,
    })
}

/// Resolved controller-lane geometry: one normalized value per sampled column,
/// so the paint pass never touches the (potentially very dense) point lists.
#[derive(Debug, Clone, PartialEq)]
pub struct ControllerPreview {
    pub lane_kinds: Vec<MidiControllerKind>,
    /// Per lane, `columns + 1` normalized values sampled left to right.
    pub lane_values: Vec<Vec<f32>>,
    pub columns: usize,
    pub step_px: f32,
    pub width: f32,
}

pub fn build_controller_preview(
    lanes: &[MidiControllerLane],
    clip_len: f32,
    axis: &ClipAxis,
    width: f32,
) -> Option<ControllerPreview> {
    let ppb = axis.pixels_per_beat();
    if width <= 1.0 {
        return None;
    }
    let columns = width.ceil().clamp(1.0, 1200.0) as usize;
    let step_px = (width / columns as f32).max(1.0);

    let mut lane_kinds = Vec::new();
    let mut lane_values = Vec::new();
    for lane in lanes
        .iter()
        .filter(|lane| lane.visible && !lane.points.is_empty())
        .take(3)
    {
        let default_value = midi_controller_default_value(lane.kind);
        let mut values = Vec::with_capacity(columns + 1);
        let mut point_index = 0usize;
        for col in 0..=columns {
            let x = (col as f32 * step_px).min(width);
            let beat = if ppb <= 0.0 {
                0.0
            } else {
                axis.beat(x).clamp(0.0, clip_len.max(0.0))
            };
            values.push(evaluate_midi_controller_points_cursor(
                &lane.points,
                beat,
                default_value,
                &mut point_index,
            ));
        }
        lane_kinds.push(lane.kind);
        lane_values.push(values);
    }

    (!lane_kinds.is_empty()).then_some(ControllerPreview {
        lane_kinds,
        lane_values,
        columns,
        step_px,
        width,
    })
}

pub fn controller_preview_band_h(height: f32, lane_count: usize) -> f32 {
    let min_needed = (lane_count as f32 * 6.0).max(8.0);
    (height * 0.44).clamp(min_needed, 30.0).min(height.max(1.0))
}

pub fn midi_controller_default_value(kind: MidiControllerKind) -> f32 {
    match kind {
        MidiControllerKind::PitchBend => 0.5,
        MidiControllerKind::CC(_)
        | MidiControllerKind::ChannelPressure
        | MidiControllerKind::PolyPressure => 0.0,
    }
}

/// Sample a controller lane at `beat`, advancing `point_index` as a cursor.
///
/// The cursor makes a left-to-right sweep linear in the number of points rather
/// than `columns * points`, which matters for imported CC lanes with thousands
/// of events.
pub fn evaluate_midi_controller_points_cursor(
    points: &[crate::components::timeline::timeline_state::MidiControllerPoint],
    beat: f32,
    default_value: f32,
    point_index: &mut usize,
) -> f32 {
    if points.is_empty() {
        return default_value.clamp(0.0, 1.0);
    }
    let beat = beat.max(0.0);
    if beat <= points[0].beat {
        *point_index = 0;
        return points[0].value.clamp(0.0, 1.0);
    }
    let last = points.len() - 1;
    if beat >= points[last].beat {
        *point_index = last.saturating_sub(1);
        return points[last].value.clamp(0.0, 1.0);
    }

    while *point_index + 1 < points.len() && beat > points[*point_index + 1].beat {
        *point_index += 1;
    }
    while *point_index > 0 && beat < points[*point_index].beat {
        *point_index -= 1;
    }
    let next = (*point_index + 1).min(last);
    let a = &points[*point_index];
    let b = &points[next];
    let span = (b.beat - a.beat).max(1.0e-6);
    let t = ((beat - a.beat) / span).clamp(0.0, 1.0);
    (a.value + (b.value - a.value) * t).clamp(0.0, 1.0)
}

pub fn midi_controller_kind_label(kind: MidiControllerKind) -> String {
    match kind {
        MidiControllerKind::CC(number) => format!("CC{}", number),
        MidiControllerKind::PitchBend => "PB".to_string(),
        MidiControllerKind::ChannelPressure => "AT".to_string(),
        MidiControllerKind::PolyPressure => "PAT".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(pitch: u8, start: f32, duration: f32) -> MidiNoteState {
        MidiNoteState::new(pitch, start, duration, 100)
    }

    #[test]
    fn offscreen_clips_build_no_preview_geometry() {
        assert_eq!(visible_clip_px_range(-500.0, 200.0, 1000.0), None);
        assert_eq!(visible_clip_px_range(1200.0, 200.0, 1000.0), None);
    }

    #[test]
    fn partially_visible_clip_is_clipped_to_the_lane() {
        let (start, end) = visible_clip_px_range(-100.0, 400.0, 300.0).expect("partially visible");
        assert_eq!((start, end), (100.0, 400.0));
        let (start, end) = visible_clip_px_range(200.0, 400.0, 300.0).expect("partially visible");
        assert_eq!((start, end), (0.0, 100.0));
    }

    #[test]
    fn dense_preview_stays_bounded_by_visible_pixels_not_note_count() {
        let notes: Vec<MidiNoteState> = (0..20_000)
            .map(|i| note(48 + (i % 24) as u8, i as f32 * 0.01, 0.05))
            .collect();
        let preview = build_note_preview(&notes, 200.0, &ClipAxis::linear(1.0), 0.0, 200.0)
            .expect("notes produce a preview");
        assert!(
            preview.quads.is_empty(),
            "dense zoom coalesces into columns"
        );
        assert!(
            preview.columns.len() <= 200,
            "columns bounded by visible width, got {}",
            preview.columns.len()
        );
        assert_eq!(preview.note_count, 20_000);
    }

    #[test]
    fn zoomed_in_preview_draws_one_quad_per_visible_note() {
        let notes = vec![note(60, 0.0, 1.0), note(64, 1.0, 1.0), note(67, 2.0, 1.0)];
        let preview = build_note_preview(&notes, 4.0, &ClipAxis::linear(40.0), 0.0, 160.0)
            .expect("notes produce a preview");
        assert!(preview.columns.is_empty());
        assert_eq!(preview.quads.len(), 3);
        assert_eq!(preview.quads[0].width, 40.0);
    }

    #[test]
    fn scrolling_culls_notes_without_shifting_the_pitch_mapping() {
        let notes = vec![note(36, 0.0, 1.0), note(96, 100.0, 1.0)];
        let full = build_note_preview(&notes, 200.0, &ClipAxis::linear(10.0), 0.0, 2000.0)
            .expect("preview");
        let scrolled = build_note_preview(&notes, 200.0, &ClipAxis::linear(10.0), 990.0, 1020.0)
            .expect("preview");
        let high_in_full = full
            .quads
            .iter()
            .map(|q| q.norm_pitch)
            .fold(f32::MIN, f32::max);
        assert_eq!(scrolled.quads.len(), 1, "only the high note is on screen");
        assert!(
            (scrolled.quads[0].norm_pitch - high_in_full).abs() < 1.0e-6,
            "pitch mapping must not depend on the scroll window"
        );
    }

    #[test]
    fn empty_and_degenerate_inputs_produce_no_preview() {
        assert!(build_note_preview(&[], 4.0, &ClipAxis::linear(40.0), 0.0, 160.0).is_none());
        assert!(build_note_preview(
            &[note(60, 0.0, 1.0)],
            4.0,
            &ClipAxis::linear(0.0),
            0.0,
            160.0
        )
        .is_none());
        assert!(build_note_preview(
            &[note(60, 8.0, 1.0)],
            4.0,
            &ClipAxis::linear(40.0),
            0.0,
            160.0
        )
        .is_none());
    }
}
