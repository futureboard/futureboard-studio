//! Canvas painting for the Audio Editor.
//!
//! Pure functions of a frame description: no project access, no decoding.
//! Every x comes from [`Viewport::x_at`] over a clip-relative beat, and every
//! waveform column asks [`ClipMap`] which source frames play there, so the
//! picture, the hit-testing in `mod.rs` and the edit ranges share one
//! transform. Hit-testing reuses the envelope and handle geometry below.

use std::sync::Arc;

use gpui::{fill, point, px, Bounds, PathBuilder, Pixels, Rgba, Window};

use super::geometry::{ClipMap, Viewport};
use super::EditorAudio;
use sphere_audio_editor::ClipEnvelope;

/// Height of the strip along the top of the lanes that holds the clip's own
/// handles (trim edges and fade handles), kept clear of the selection gesture.
pub const HANDLE_BAND: f32 = 14.0;
/// Size of a fade handle square.
pub const FADE_HANDLE: f32 = 8.0;

/// Gain envelope range drawn over the lanes, in dB.
pub const ENVELOPE_MIN_DB: f32 = -60.0;
pub const ENVELOPE_MAX_DB: f32 = 12.0;

/// Envelope dB → y inside `[top, top + height]`. Square-law so the region
/// around 0 dB, where most envelope work happens, gets most of the height.
pub fn envelope_y(db: f32, top: f32, height: f32) -> f32 {
    let t = ((db.clamp(ENVELOPE_MIN_DB, ENVELOPE_MAX_DB) - ENVELOPE_MIN_DB)
        / (ENVELOPE_MAX_DB - ENVELOPE_MIN_DB))
        .sqrt();
    top + (1.0 - t) * height
}

/// Inverse of [`envelope_y`].
pub fn envelope_db(y: f32, top: f32, height: f32) -> f32 {
    let t = (1.0 - (y - top) / height.max(1.0)).clamp(0.0, 1.0);
    ENVELOPE_MIN_DB + t * t * (ENVELOPE_MAX_DB - ENVELOPE_MIN_DB)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridKind {
    Bar,
    Beat,
    Sub,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GridLine {
    /// Clip-relative beat.
    pub rel: f64,
    pub kind: GridKind,
    pub label: Option<String>,
}

/// Colours resolved once per render, never inside a paint loop.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub lane_bg: Rgba,
    pub outside_bg: Rgba,
    pub lane_divider: Rgba,
    pub center_line: Rgba,
    pub grid_bar: Rgba,
    pub grid_beat: Rgba,
    pub grid_sub: Rgba,
    pub wave: Rgba,
    pub wave_dim: Rgba,
    pub selection: Rgba,
    pub selection_edge: Rgba,
    pub cursor: Rgba,
    pub fade_shade: Rgba,
    pub fade_line: Rgba,
    pub handle: Rgba,
    pub envelope: Rgba,
    pub envelope_point: Rgba,
    pub warp: Rgba,
    pub warp_locked: Rgba,
    pub viewport_frame: Rgba,
    pub viewport_fill: Rgba,
}

/// What the lanes show this frame.
pub struct LaneFrame {
    pub view: Viewport,
    pub map: Option<ClipMap>,
    pub duration: f64,
    pub audio: Option<Arc<EditorAudio>>,
    pub lanes: usize,
    pub amp_zoom: f32,
    pub grid: Arc<Vec<GridLine>>,
    pub selection: Option<(f64, f64)>,
    pub cursor: Option<f64>,
    pub fade_in_end: f64,
    pub fade_out_start: f64,
    /// Whether each fade (in, out) offers its manual handle. An edge a
    /// crossfade covers draws its curve but no handle.
    pub fade_handles: [bool; 2],
    pub envelope: Option<(ClipEnvelope, bool)>,
    pub warp: Vec<(u64, f64, bool)>,
    pub warp_emphasized: bool,
    pub palette: Palette,
}

fn rect(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
    Bounds::new(
        point(px(x), px(y)),
        gpui::size(px(w.max(0.0)), px(h.max(0.0))),
    )
}

/// Vertical extent of lane `index` inside `height` (below the handle band).
pub fn lane_rect(index: usize, lanes: usize, top: f32, height: f32) -> (f32, f32) {
    let lanes = lanes.max(1) as f32;
    let body = (height - HANDLE_BAND).max(1.0);
    let h = body / lanes;
    (top + HANDLE_BAND + h * index as f32, h)
}

pub fn paint_lanes(bounds: Bounds<Pixels>, frame: &LaneFrame, window: &mut Window) {
    let ox: f32 = bounds.origin.x.into();
    let oy: f32 = bounds.origin.y.into();
    let w: f32 = bounds.size.width.into();
    let h: f32 = bounds.size.height.into();
    let p = &frame.palette;
    let view = frame.view;

    window.paint_quad(fill(bounds, p.outside_bg));
    let clip_x0 = (view.x_at(0.0) as f32).clamp(0.0, w);
    let clip_x1 = (view.x_at(frame.duration) as f32).clamp(0.0, w);
    window.paint_quad(fill(
        rect(ox + clip_x0, oy, clip_x1 - clip_x0, h),
        p.lane_bg,
    ));

    // Grid: bars lead, beats support, subdivisions recede.
    for line in frame.grid.iter() {
        let x = view.x_at(line.rel) as f32;
        if !(0.0..=w).contains(&x) {
            continue;
        }
        let color = match line.kind {
            GridKind::Bar => p.grid_bar,
            GridKind::Beat => p.grid_beat,
            GridKind::Sub => p.grid_sub,
        };
        window.paint_quad(fill(
            rect(ox + x.floor(), oy + HANDLE_BAND, 1.0, h - HANDLE_BAND),
            color,
        ));
    }

    // Selection sits under the waveform so the audio stays readable.
    if let Some((a, b)) = frame.selection {
        let x0 = (view.x_at(a.min(b)) as f32).clamp(-1.0, w + 1.0);
        let x1 = (view.x_at(a.max(b)) as f32).clamp(-1.0, w + 1.0);
        window.paint_quad(fill(rect(ox + x0, oy, x1 - x0, h), p.selection));
        for x in [x0, x1] {
            if (0.0..=w).contains(&x) {
                window.paint_quad(fill(rect(ox + x - 0.5, oy, 1.0, h), p.selection_edge));
            }
        }
    }

    for lane in 0..frame.lanes.max(1) {
        let (top, lane_h) = lane_rect(lane, frame.lanes, oy, h);
        let center = top + lane_h * 0.5;
        window.paint_quad(fill(
            rect(ox + clip_x0, center, clip_x1 - clip_x0, 1.0),
            p.center_line,
        ));
        if lane > 0 {
            window.paint_quad(fill(rect(ox, top, w, 1.0), p.lane_divider));
        }
        if let (Some(map), Some(audio)) = (frame.map.as_ref(), frame.audio.as_ref()) {
            let channel = lane.min(audio.pcm.channels.max(1) - 1);
            paint_channel(
                (ox, top, w, lane_h),
                (clip_x0, clip_x1),
                view,
                map,
                audio,
                channel,
                frame.amp_zoom,
                p.wave,
                window,
            );
        }
    }

    paint_fades(bounds, frame, clip_x0, clip_x1, window);

    if let Some((envelope, emphasized)) = frame.envelope.as_ref() {
        paint_envelope(
            bounds,
            frame,
            envelope,
            *emphasized,
            clip_x0,
            clip_x1,
            window,
        );
    }

    for &(_, rel, locked) in &frame.warp {
        let x = view.x_at(rel) as f32;
        if !(0.0..=w).contains(&x) {
            continue;
        }
        let color = if locked { p.warp_locked } else { p.warp };
        let alpha = if frame.warp_emphasized { 1.0 } else { 0.45 };
        let color = Rgba {
            a: color.a * alpha,
            ..color
        };
        window.paint_quad(fill(
            rect(ox + x - 0.5, oy + HANDLE_BAND, 1.0, h - HANDLE_BAND),
            color,
        ));
        let mut flag = PathBuilder::fill();
        flag.move_to(point(px(ox + x - 5.0), px(oy)));
        flag.line_to(point(px(ox + x + 5.0), px(oy)));
        flag.line_to(point(px(ox + x), px(oy + HANDLE_BAND - 2.0)));
        flag.close();
        if let Ok(path) = flag.build() {
            window.paint_path(path, color);
        }
    }

    if let Some(cursor) = frame.cursor {
        let x = view.x_at(cursor) as f32;
        if (0.0..=w).contains(&x) {
            window.paint_quad(fill(rect(ox + x - 0.5, oy, 1.0, h), p.cursor));
        }
    }

    // Trim grips at the clip's edges, in the handle band.
    for x in [view.x_at(0.0) as f32, view.x_at(frame.duration) as f32] {
        if (-2.0..=w + 2.0).contains(&x) {
            window.paint_quad(fill(rect(ox + x - 1.5, oy, 3.0, HANDLE_BAND), p.handle));
        }
    }
}

/// One channel's waveform: a filled min/max envelope while several frames
/// share a pixel, and the sample polyline once each frame has room.
#[allow(clippy::too_many_arguments)]
fn paint_channel(
    (ox, top, w, lane_h): (f32, f32, f32, f32),
    (clip_x0, clip_x1): (f32, f32),
    view: Viewport,
    map: &ClipMap,
    audio: &EditorAudio,
    channel: usize,
    amp_zoom: f32,
    color: Rgba,
    window: &mut Window,
) {
    let half = lane_h * 0.5 - 2.0;
    let center = top + lane_h * 0.5;
    let y_of = |v: f32| center - (v * amp_zoom).clamp(-1.0, 1.0) * half;
    let pcm = &audio.pcm;
    let channels = pcm.channels.max(1);
    let frames = pcm.frames();
    if frames == 0 || clip_x1 <= clip_x0 {
        return;
    }
    let mid = view.beat_at(((clip_x0 + clip_x1) * 0.5) as f64);
    let frames_per_px = map.frames_per_beat(mid.clamp(0.0, map.duration)) / view.pixels_per_beat;

    if frames_per_px < 1.0 {
        // Samples: one vertex per frame, dots once they are far apart.
        let a = view.beat_at(clip_x0 as f64).clamp(0.0, map.duration);
        let b = view.beat_at(clip_x1 as f64).clamp(0.0, map.duration);
        let (f0, f1) = map.source_range(a, b);
        let f0 = f0.saturating_sub(1) as usize;
        let f1 = (f1 as usize + 1).min(frames);
        if f1 <= f0 {
            return;
        }
        let mut path = PathBuilder::stroke(px(1.25));
        let px_per_frame = 1.0 / frames_per_px;
        for (i, frame) in (f0..f1).enumerate() {
            let x = view.x_at(map.beat_at(frame as f64 + 0.5)) as f32;
            let y = y_of(pcm.samples[frame * channels + channel]);
            let at = point(px(ox + x), px(y));
            if i == 0 {
                path.move_to(at);
            } else {
                path.line_to(at);
            }
            if px_per_frame >= 6.0 {
                window.paint_quad(fill(rect(ox + x - 1.5, y - 1.5, 3.0, 3.0), color));
            }
        }
        if let Ok(path) = path.build() {
            window.paint_path(path, color);
        }
        return;
    }

    let x_start = clip_x0.floor().max(0.0) as i32;
    let x_end = clip_x1.ceil().min(w) as i32;
    let mut tops: Vec<(f32, f32)> = Vec::with_capacity((x_end - x_start).max(0) as usize);
    let mut bottoms: Vec<(f32, f32)> = Vec::with_capacity(tops.capacity());
    for x in x_start..x_end {
        let a = view.beat_at(x as f64).clamp(0.0, map.duration);
        let b = view.beat_at(x as f64 + 1.0).clamp(0.0, map.duration);
        let (f0, mut f1) = map.source_range(a, b);
        if f1 <= f0 {
            f1 = f0 + 1;
        }
        let Some(mm) = audio
            .peaks
            .range(&pcm.samples, channel, f0 as usize, f1 as usize)
        else {
            continue;
        };
        let (mut y_top, mut y_bot) = (y_of(mm.max), y_of(mm.min));
        if y_bot - y_top < 1.0 {
            let c = (y_top + y_bot) * 0.5;
            y_top = c - 0.5;
            y_bot = c + 0.5;
        }
        tops.push((ox + x as f32, y_top));
        bottoms.push((ox + x as f32, y_bot));
    }
    if tops.is_empty() {
        return;
    }
    let mut path = PathBuilder::fill();
    path.move_to(point(px(tops[0].0), px(tops[0].1)));
    for &(x, y) in tops.iter().skip(1) {
        path.line_to(point(px(x + 1.0), px(y)));
    }
    for &(x, y) in bottoms.iter().rev() {
        path.line_to(point(px(x + 1.0), px(y)));
    }
    path.line_to(point(px(bottoms[0].0), px(bottoms[0].1)));
    path.close();
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

fn paint_fades(
    bounds: Bounds<Pixels>,
    frame: &LaneFrame,
    clip_x0: f32,
    clip_x1: f32,
    window: &mut Window,
) {
    let ox: f32 = bounds.origin.x.into();
    let oy: f32 = bounds.origin.y.into();
    let w: f32 = bounds.size.width.into();
    let h: f32 = bounds.size.height.into();
    let p = &frame.palette;
    let view = frame.view;
    let top = oy + HANDLE_BAND;
    let bottom = oy + h;

    let fade_in_x = view.x_at(frame.fade_in_end) as f32;
    let fade_out_x = view.x_at(frame.fade_out_start) as f32;
    let start_x = view.x_at(0.0) as f32;
    let end_x = view.x_at(frame.duration) as f32;

    // Shade the part of each fade the curve removes, and draw the curve.
    let mut curve = |from: (f32, f32), to: (f32, f32), rising: bool| {
        if (to.0 - from.0).abs() < 1.0 || to.0 < 0.0 || from.0 > w {
            return;
        }
        let steps = ((to.0 - from.0).abs() / 3.0).clamp(4.0, 96.0) as usize;
        let mut shade = PathBuilder::fill();
        let mut line = PathBuilder::stroke(px(1.25));
        shade.move_to(point(px(ox + from.0), px(top)));
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            let gain = if rising { t } else { 1.0 - t };
            // Equal-power look, matching how the fade sounds.
            let gain = (gain * std::f32::consts::FRAC_PI_2).sin();
            let x = ox + from.0 + (to.0 - from.0) * t;
            let y = bottom - (bottom - top) * gain;
            shade.line_to(point(px(x), px(y)));
            if i == 0 {
                line.move_to(point(px(x), px(y)));
            } else {
                line.line_to(point(px(x), px(y)));
            }
        }
        shade.line_to(point(px(ox + to.0), px(top)));
        shade.close();
        if let Ok(path) = shade.build() {
            window.paint_path(path, p.fade_shade);
        }
        if let Ok(path) = line.build() {
            window.paint_path(path, p.fade_line);
        }
    };
    if frame.fade_in_end > 0.0 {
        curve((start_x, bottom), (fade_in_x, top), true);
    }
    if frame.fade_out_start < frame.duration {
        curve((fade_out_x, top), (end_x, bottom), false);
    }
    let _ = (clip_x0, clip_x1);

    // Handles always show, so a fade of zero length can still be pulled out —
    // except on an edge a crossfade covers, which no manual fade changes.
    for (x, shown) in [fade_in_x, fade_out_x].into_iter().zip(frame.fade_handles) {
        if shown && (-FADE_HANDLE..=w + FADE_HANDLE).contains(&x) {
            let r = rect(
                ox + x - FADE_HANDLE * 0.5,
                oy + (HANDLE_BAND - FADE_HANDLE) * 0.5,
                FADE_HANDLE,
                FADE_HANDLE,
            );
            window.paint_quad(fill(r, p.handle));
        }
    }
}

fn paint_envelope(
    bounds: Bounds<Pixels>,
    frame: &LaneFrame,
    envelope: &ClipEnvelope,
    emphasized: bool,
    clip_x0: f32,
    clip_x1: f32,
    window: &mut Window,
) {
    let ox: f32 = bounds.origin.x.into();
    let oy: f32 = bounds.origin.y.into();
    let h: f32 = bounds.size.height.into();
    let p = &frame.palette;
    let top = oy + HANDLE_BAND;
    let height = h - HANDLE_BAND;
    let color = if emphasized {
        p.envelope
    } else {
        Rgba {
            a: p.envelope.a * 0.5,
            ..p.envelope
        }
    };
    let duration = frame.duration.max(1.0e-9);
    let mut line = PathBuilder::stroke(px(if emphasized { 1.5 } else { 1.0 }));
    let mut started = false;
    let mut x = clip_x0;
    while x <= clip_x1 {
        let rel = frame.view.beat_at(x as f64);
        let time = (rel / duration).clamp(0.0, 1.0) as f32;
        let y = envelope_y(envelope.value_db_at(time), top, height);
        let at = point(px(ox + x), px(y));
        if started {
            line.line_to(at);
        } else {
            line.move_to(at);
            started = true;
        }
        x += 2.0;
    }
    if let Ok(path) = line.build() {
        window.paint_path(path, color);
    }
    if emphasized {
        for point_ in &envelope.points {
            let x = frame.view.x_at(point_.time as f64 * duration) as f32;
            let y = envelope_y(point_.value_db, top, height);
            window.paint_quad(fill(
                rect(ox + x - 3.5, y - 3.5, 7.0, 7.0),
                p.envelope_point,
            ));
        }
    }
}

/// Whole-clip overview with the visible range framed.
pub fn paint_overview(
    bounds: Bounds<Pixels>,
    map: Option<&ClipMap>,
    audio: Option<&EditorAudio>,
    duration: f64,
    view: Viewport,
    palette: &Palette,
    window: &mut Window,
) {
    let ox: f32 = bounds.origin.x.into();
    let oy: f32 = bounds.origin.y.into();
    let w: f32 = bounds.size.width.into();
    let h: f32 = bounds.size.height.into();
    window.paint_quad(fill(bounds, palette.lane_bg));
    let duration = duration.max(1.0e-9);
    if let (Some(map), Some(audio)) = (map, audio) {
        let pcm = &audio.pcm;
        let center = oy + h * 0.5;
        let half = h * 0.5 - 2.0;
        let columns = w.max(1.0) as usize;
        for x in 0..columns {
            let a = duration * x as f64 / columns as f64;
            let b = duration * (x + 1) as f64 / columns as f64;
            let (f0, mut f1) = map.source_range(a, b);
            if f1 <= f0 {
                f1 = f0 + 1;
            }
            let mut peak = 0.0f32;
            for ch in 0..pcm.channels.max(1) {
                if let Some(mm) = audio
                    .peaks
                    .range(&pcm.samples, ch, f0 as usize, f1 as usize)
                {
                    peak = peak.max(mm.max.abs()).max(mm.min.abs());
                }
            }
            let bar = (peak.min(1.0) * half).max(0.5);
            window.paint_quad(fill(
                rect(ox + x as f32, center - bar, 1.0, bar * 2.0),
                palette.wave_dim,
            ));
        }
    }
    let x0 = (view.scroll / duration * w as f64) as f32;
    let x1 = ((view.scroll + view.visible_beats()) / duration * w as f64) as f32;
    let x0 = x0.clamp(0.0, w);
    let x1 = x1.clamp(x0, w);
    window.paint_quad(fill(rect(ox + x0, oy, x1 - x0, h), palette.viewport_fill));
    for (x, width) in [(x0, 1.0), (x1 - 1.0, 1.0)] {
        window.paint_quad(fill(rect(ox + x, oy, width, h), palette.viewport_frame));
    }
    window.paint_quad(fill(
        rect(ox + x0, oy, x1 - x0, 1.0),
        palette.viewport_frame,
    ));
    window.paint_quad(fill(
        rect(ox + x0, oy + h - 1.0, x1 - x0, 1.0),
        palette.viewport_frame,
    ));
}

/// Ruler tick marks (labels are laid out as text by the view).
pub fn paint_ruler(
    bounds: Bounds<Pixels>,
    grid: &[GridLine],
    view: Viewport,
    duration: f64,
    palette: &Palette,
    window: &mut Window,
) {
    let ox: f32 = bounds.origin.x.into();
    let oy: f32 = bounds.origin.y.into();
    let w: f32 = bounds.size.width.into();
    let h: f32 = bounds.size.height.into();
    let clip_x0 = (view.x_at(0.0) as f32).clamp(0.0, w);
    let clip_x1 = (view.x_at(duration) as f32).clamp(0.0, w);
    window.paint_quad(fill(rect(ox, oy + h - 1.0, w, 1.0), palette.lane_divider));
    // The clip's extent along the bottom of the ruler.
    window.paint_quad(fill(
        rect(ox + clip_x0, oy + h - 3.0, clip_x1 - clip_x0, 2.0),
        palette.wave_dim,
    ));
    for line in grid {
        let x = view.x_at(line.rel) as f32;
        if !(0.0..=w).contains(&x) {
            continue;
        }
        let (len, color) = match line.kind {
            GridKind::Bar => (h, palette.grid_bar),
            GridKind::Beat => (h * 0.4, palette.grid_beat),
            GridKind::Sub => (h * 0.2, palette.grid_sub),
        };
        window.paint_quad(fill(rect(ox + x.floor(), oy + h - len, 1.0, len), color));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_mapping_round_trips() {
        for db in [-60.0f32, -24.0, -6.0, 0.0, 6.0, 12.0] {
            let y = envelope_y(db, 10.0, 200.0);
            assert!((envelope_db(y, 10.0, 200.0) - db).abs() < 1e-3, "{db}");
        }
        // 0 dB sits in the upper part of the lane, where the detail is.
        assert!(envelope_y(0.0, 0.0, 100.0) < 25.0);
    }

    #[test]
    fn lanes_split_the_body_below_the_handle_band() {
        let (top0, h0) = lane_rect(0, 2, 0.0, 214.0);
        let (top1, h1) = lane_rect(1, 2, 0.0, 214.0);
        assert_eq!(top0, HANDLE_BAND);
        assert_eq!(h0, 100.0);
        assert_eq!(top1, HANDLE_BAND + 100.0);
        assert_eq!(h1, 100.0);
    }
}
