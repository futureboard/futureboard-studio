//! Lossless paint runs for immutable waveform tiles. The peak/zoom cache still
//! decides which samples belong to a column; this only reduces scene emission.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::sync::Arc;

use super::WaveformBars;

#[derive(Clone, Copy)]
pub(super) struct PaintStyle {
    pub height: f32,
    pub gain: f32,
    pub zoom: f32,
    pub origin_y: f32,
    pub scale: f32,
}

impl PaintStyle {
    /// Same arithmetic and one-pixel minimum as the original column painter.
    pub fn column(self, min: f32, max: f32) -> (f32, f32, bool) {
        let center = self.height / 2.0;
        let max = max * self.gain;
        let min = min * self.gain;
        let top = center - (max * self.zoom).clamp(-1.0, 1.0) * center;
        let bottom = center - (min * self.zoom).clamp(-1.0, 1.0) * center;
        (top, (bottom - top).max(1.0), max > 1.0 || min < -1.0)
    }
}

#[derive(Debug)]
pub(super) struct PaintRun {
    pub columns: Range<usize>,
    pub top: f32,
    pub height: f32,
    pub clipped: bool,
}

fn build_runs(
    bars: &WaveformBars,
    style: PaintStyle,
    snap: impl Fn(f32) -> f32,
) -> Arc<[PaintRun]> {
    let _scope = crate::perf::PerfScope::enter("Waveform:prepare_paint");
    let mut runs: Vec<PaintRun> = Vec::new();
    let mut previous_edges = None;
    for (index, &(x, min, max)) in bars.iter().enumerate() {
        let (top, height, clipped) = style.column(min, max);
        let absolute_top = style.origin_y + top;
        let edges = (snap(absolute_top), snap(absolute_top + height));
        if let Some(last) = runs.last_mut() {
            if previous_edges == Some(edges)
                && last.clipped == clipped
                && bars[index - 1].0 + 1.0 == x
            {
                last.columns.end = index + 1;
                continue;
            }
        }
        runs.push(PaintRun {
            columns: index..index + 1,
            top,
            height,
            clipped,
        });
        previous_edges = Some(edges);
    }
    runs.into()
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct PaintKey {
    source: usize,
    height: u32,
    gain: u32,
    zoom: u32,
    origin_y: u32,
    scale: u32,
}

impl PaintKey {
    fn new(source: &Arc<WaveformBars>, style: PaintStyle) -> Self {
        Self {
            source: Arc::as_ptr(source) as usize,
            height: style.height.to_bits(),
            gain: style.gain.to_bits(),
            zoom: style.zoom.to_bits(),
            // Keep the absolute y, not just its pixel phase: f32 addition can
            // round a near-half-pixel differently at a different magnitude.
            // Horizontal pan does not change y, so it still reuses every tile.
            origin_y: style.origin_y.to_bits(),
            scale: style.scale.to_bits(),
        }
    }
}

struct PaintEntry {
    // Retaining the allocation makes pointer identity safe: a revised tile
    // cannot reuse this address while the cached runs are still alive.
    _source: Arc<WaveformBars>,
    runs: Arc<[PaintRun]>,
}

struct PaintCache {
    entries: HashMap<PaintKey, PaintEntry>,
    order: VecDeque<PaintKey>,
    capacity: usize,
}

impl PaintCache {
    fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            capacity,
        }
    }

    fn get(&self, key: &PaintKey) -> Option<Arc<[PaintRun]>> {
        self.entries.get(key).map(|entry| Arc::clone(&entry.runs))
    }

    fn insert(&mut self, key: PaintKey, source: Arc<WaveformBars>, runs: Arc<[PaintRun]>) {
        if self
            .entries
            .insert(
                key,
                PaintEntry {
                    _source: source,
                    runs,
                },
            )
            .is_none()
        {
            self.order.push_back(key);
        }
        while self.order.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }
}

thread_local! {
    // 4096 fixed 256-column tiles, matching the geometry cache's working set.
    // Live takes bypass this cache, so it cannot retain an unbounded live span.
    static PAINT_CACHE: RefCell<PaintCache> = RefCell::new(PaintCache::new(4096));
}

pub(super) fn prepared_runs(
    source: &Arc<WaveformBars>,
    style: PaintStyle,
    cacheable: bool,
    snap: impl Fn(f32) -> f32,
) -> Arc<[PaintRun]> {
    let key = PaintKey::new(source, style);
    if cacheable {
        if let Some(runs) = PAINT_CACHE.with(|cache| cache.borrow().get(&key)) {
            crate::perf::count("waveform_paint_cache_hit", 1);
            return runs;
        }
    }
    crate::perf::count("waveform_paint_cache_miss", 1);
    let runs = build_runs(source, style, snap);
    if cacheable {
        PAINT_CACHE.with(|cache| {
            cache
                .borrow_mut()
                .insert(key, Arc::clone(source), Arc::clone(&runs));
        });
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    // GPUI's round-half-toward-zero snapping, in device pixels. Production
    // passes Window::pixel_snap directly instead of maintaining a second rule.
    fn snap(value: f32, scale: f32) -> f32 {
        let value = value * scale;
        (value.abs() - 0.5).ceil().copysign(value)
    }

    fn style() -> PaintStyle {
        PaintStyle {
            height: 64.0,
            gain: 1.0,
            zoom: 1.0,
            origin_y: 0.0,
            scale: 1.0,
        }
    }

    type Raster = BTreeMap<(i32, i32), bool>;

    fn raster_rect(out: &mut Raster, rect: [f32; 4], clipped: bool, scale: f32) {
        let [left, top, right, bottom] = rect.map(|v| snap(v, scale) as i32);
        // A viewport mask, including partially visible runs at either edge.
        for x in left.max(0)..right.min(100) {
            for y in top.max(0)..bottom.min(160) {
                out.insert((x, y), clipped);
            }
        }
    }

    #[test]
    fn runs_cover_the_same_pixels_and_overload_colors_as_individual_bars() {
        let bars: WaveformBars = (0..128)
            .filter(|i| !(25..29).contains(i))
            .map(|i| {
                let amplitude = match i % 13 {
                    0 => 0.0,
                    1 => 1.2,
                    _ => 0.8 + (i % 4) as f32 * 0.001,
                };
                (i as f32 + 0.25, -amplitude, amplitude * 0.9)
            })
            .collect();
        for scale in [0.75, 1.0, 1.25, 1.5, 2.0] {
            for origin_y in [-20.5, 0.0, 0.25, 17.5] {
                for origin_x in [-37.25, -3.0, 0.25, 45.5] {
                    for height in [1.0, 19.5, 64.0] {
                        for (gain, zoom) in [(0.0, 1.0), (0.7, 1.0), (1.0, 2.5), (2.0, 1.0)] {
                            let style = PaintStyle {
                                height,
                                gain,
                                zoom,
                                origin_y,
                                scale,
                            };
                            let visible = 2..bars.len() - 3;
                            let mut original = Raster::new();
                            for &(x, min, max) in &bars[visible.clone()] {
                                let (top, h, clipped) = style.column(min, max);
                                let top = origin_y + top;
                                raster_rect(
                                    &mut original,
                                    [origin_x + x, top, (origin_x + x) + 1.0, top + h],
                                    clipped,
                                    scale,
                                );
                            }
                            let runs = build_runs(&bars, style, |v| snap(v, scale));
                            let mut merged = Raster::new();
                            for run in runs.iter() {
                                let first = run.columns.start.max(visible.start);
                                let end = run.columns.end.min(visible.end);
                                if first >= end {
                                    continue;
                                }
                                let left = origin_x + bars[first].0;
                                let right = (origin_x + bars[end - 1].0) + 1.0;
                                let top = origin_y + run.top;
                                // Match Bounds' origin + size arithmetic as well.
                                raster_rect(
                                    &mut merged,
                                    [left, top, left + (right - left), top + run.height],
                                    run.clipped,
                                    scale,
                                );
                            }
                            assert_eq!(original, merged, "scale={scale} origin=({origin_x},{origin_y}) height={height} gain={gain} zoom={zoom}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn dense_columns_merge_but_silence_gaps_and_clipping_boundaries_remain() {
        let bars = vec![
            (0.0, -1.0, 1.0),
            (1.0, -0.999, 0.999),
            (3.0, -1.0, 1.0),
            (4.0, -1.1, 1.1),
        ];
        let runs = build_runs(&bars, style(), |v| snap(v, 1.0));
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].columns, 0..2);
        assert!(!runs[1].clipped);
        assert!(runs[2].clipped);
    }

    #[test]
    fn cache_reuses_only_the_same_source_and_paint_geometry_and_stays_bounded() {
        let source = Arc::new(vec![(0.0, -0.8, 0.8)]);
        let runs = build_runs(&source, style(), |v| snap(v, 1.0));
        let key = PaintKey::new(&source, style());
        let mut cache = PaintCache::new(2);
        cache.insert(key, Arc::clone(&source), Arc::clone(&runs));
        assert!(Arc::ptr_eq(&runs, &cache.get(&key).unwrap()));
        for changed in [
            PaintStyle {
                height: 65.0,
                ..style()
            },
            PaintStyle {
                gain: 1.1,
                ..style()
            },
            PaintStyle {
                zoom: 2.0,
                ..style()
            },
            PaintStyle {
                origin_y: 0.5,
                ..style()
            },
            PaintStyle {
                scale: 2.0,
                ..style()
            },
        ] {
            assert!(cache.get(&PaintKey::new(&source, changed)).is_none());
        }
        let revised = Arc::new(source.as_ref().clone());
        assert!(cache.get(&PaintKey::new(&revised, style())).is_none());
        cache.insert(PaintKey::new(&revised, style()), revised, Arc::clone(&runs));
        let third = Arc::new(vec![]);
        cache.insert(PaintKey::new(&third, style()), third, runs);
        assert_eq!(cache.entries.len(), 2);
        assert_eq!(cache.order.len(), 2);
        assert!(cache.get(&key).is_none());
        assert_eq!(
            Arc::strong_count(&source),
            1,
            "eviction releases the retained source"
        );
    }

    #[test]
    fn live_tiles_do_not_reuse_prepared_geometry() {
        let source = Arc::new(vec![(0.0, -0.8, 0.8)]);
        let get = |cacheable| prepared_runs(&source, style(), cacheable, |v| snap(v, 1.0));
        let first = get(true);
        assert!(Arc::ptr_eq(&first, &get(true)));
        assert!(!Arc::ptr_eq(&first, &get(false)));
    }
}
