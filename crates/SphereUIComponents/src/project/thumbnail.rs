//! The project thumbnail: `{project}/Preview/Thumb.png`, the arrangement
//! view the Welcome screen shows on the project's card.
//!
//! Only projects created with thumbnails get one. A new project's folder is
//! made with `Preview/` in it ([`enable_thumbnails`]), and a save writes the
//! thumbnail only where that folder exists
//! ([`write_thumbnail_image_if_enabled`]), so saving a project made before
//! thumbnails existed never adds files to it.
//!
//! The picture is the arrangement view itself: the studio draws its window's
//! last frame again and reads it back (`StudioLayout::write_project_thumbnail`),
//! and [`thumbnail_from_frame`] cuts the arrangement out of it. Never a
//! capture of the screen. Where a frame cannot be read back,
//! [`render_thumbnail`] draws an overview from the project data instead: one
//! lane per arrangement track in its colour, each clip a block in that
//! colour, MIDI clips with their notes.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use image::{Rgba, RgbaImage};

use super::{ClipSource, FutureboardProject, ProjectTrackType};

/// The folder beside the `.fbproj` that holds the thumbnail.
pub const PREVIEW_DIR: &str = "Preview";
/// The thumbnail's file name inside [`PREVIEW_DIR`].
pub const THUMB_FILE: &str = "Thumb.png";
/// 16:9, twice the Welcome card's picture, so it stays sharp on a HiDPI
/// display.
pub const THUMB_WIDTH: u32 = 480;
pub const THUMB_HEIGHT: u32 = 270;

/// Largest thumbnail the Welcome screen will read, so a stray huge file in
/// the folder never stalls it.
pub const THUMB_MAX_BYTES: u64 = 4 * 1024 * 1024;

const BACKGROUND: [u8; 3] = [0x14, 0x15, 0x19];
const LANE_EVEN: [u8; 3] = [0x1a, 0x1b, 0x20];
const LANE_ODD: [u8; 3] = [0x17, 0x18, 0x1d];
const BAR_LINE: [u8; 3] = [0x24, 0x26, 0x2d];
const PAD: u32 = 10;
/// The most lanes drawn; more than this would leave each a line or two tall.
const MAX_LANES: usize = 16;
/// A lane's height at most, so a project of two or three tracks reads as
/// lanes under open space rather than as a few slabs.
const MAX_LANE_H: i64 = 44;

/// Where a project's thumbnail lives, given its root folder.
pub fn thumbnail_path(project_root: &Path) -> PathBuf {
    project_root.join(PREVIEW_DIR).join(THUMB_FILE)
}

/// Mark a new project's folder as one that keeps a thumbnail.
pub fn enable_thumbnails(project_root: &Path) -> std::io::Result<()> {
    fs::create_dir_all(project_root.join(PREVIEW_DIR))
}

/// Encode to a temporary file beside the target and rename it into place, so
/// a reader never sees half a PNG.
fn write_png(image: &RgbaImage, target: &Path) -> std::io::Result<()> {
    let mut bytes = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .map_err(std::io::Error::other)?;
    let temp = target.with_extension("png.tmp");
    {
        let mut file = fs::File::create(&temp)?;
        file.write_all(&bytes)?;
        file.flush()?;
    }
    fs::rename(&temp, target).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

/// Write `image` as the thumbnail of the project at `project_file`, when its
/// folder keeps one. Background thread only: it checks the folder and writes.
pub fn write_thumbnail_image_if_enabled(image: &RgbaImage, project_file: &Path) {
    let Some(root) = project_file.parent() else {
        return;
    };
    if !root.join(PREVIEW_DIR).is_dir() {
        return;
    }
    let target = thumbnail_path(root);
    if let Err(error) = write_png(image, &target) {
        eprintln!(
            "[ProjectThumb] could not write {}: {error}",
            target.display()
        );
    }
}

/// Turn a frame of the studio window into a thumbnail: the arrangement's
/// rectangle (`rect`, in physical pixels of `frame`) cut to 16:9 from its top
/// left — the track headers and the start of the song, which stay put while
/// the rest scrolls — and scaled to [`THUMB_WIDTH`] × [`THUMB_HEIGHT`].
/// `None` when the rectangle is not inside the frame.
pub fn thumbnail_from_frame(
    frame: &RgbaImage,
    (x, y, width, height): (u32, u32, u32, u32),
) -> Option<RgbaImage> {
    let width = width.min(frame.width().saturating_sub(x));
    let height = height.min(frame.height().saturating_sub(y));
    if width < 16 || height < 9 {
        return None;
    }
    let aspect = THUMB_WIDTH as f64 / THUMB_HEIGHT as f64;
    let (crop_w, crop_h) = if width as f64 / height as f64 > aspect {
        (((height as f64) * aspect).round() as u32, height)
    } else {
        (width, ((width as f64) / aspect).round() as u32)
    };
    let region = image::imageops::crop_imm(frame, x, y, crop_w.max(1), crop_h.max(1)).to_image();
    Some(image::imageops::resize(
        &region,
        THUMB_WIDTH,
        THUMB_HEIGHT,
        image::imageops::FilterType::Triangle,
    ))
}

/// The tracks that have a lane in the arrangement.
fn has_lane(track_type: ProjectTrackType) -> bool {
    !matches!(
        track_type,
        ProjectTrackType::Bus | ProjectTrackType::Return | ProjectTrackType::Master
    )
}

/// `#RRGGBB` or `#RRGGBBAA` to RGB; a grey for anything else.
fn parse_color(hex: &str) -> [u8; 3] {
    let digits = hex.trim().trim_start_matches('#');
    let channel = |at: usize| {
        digits
            .get(at..at + 2)
            .and_then(|pair| u8::from_str_radix(pair, 16).ok())
    };
    match (channel(0), channel(2), channel(4)) {
        (Some(r), Some(g), Some(b)) if digits.len() >= 6 => [r, g, b],
        _ => [0x8a, 0x8f, 0x99],
    }
}

fn mix(base: [u8; 3], over: [u8; 3], amount: f32) -> [u8; 3] {
    let amount = amount.clamp(0.0, 1.0);
    let channel =
        |i: usize| (base[i] as f32 + (over[i] as f32 - base[i] as f32) * amount).round() as u8;
    [channel(0), channel(1), channel(2)]
}

fn fill(image: &mut RgbaImage, x0: i64, y0: i64, x1: i64, y1: i64, color: [u8; 3]) {
    let (w, h) = (image.width() as i64, image.height() as i64);
    let (x0, x1) = (x0.clamp(0, w), x1.clamp(0, w));
    let (y0, y1) = (y0.clamp(0, h), y1.clamp(0, h));
    for y in y0..y1 {
        for x in x0..x1 {
            image.put_pixel(
                x as u32,
                y as u32,
                Rgba([color[0], color[1], color[2], 255]),
            );
        }
    }
}

/// Draw the arrangement overview.
pub fn render_thumbnail(project: &FutureboardProject) -> RgbaImage {
    let mut image = RgbaImage::from_pixel(
        THUMB_WIDTH,
        THUMB_HEIGHT,
        Rgba([BACKGROUND[0], BACKGROUND[1], BACKGROUND[2], 255]),
    );
    let tracks: Vec<_> = project
        .tracks
        .iter()
        .filter(|track| has_lane(track.track_type))
        .take(MAX_LANES)
        .collect();
    if tracks.is_empty() {
        return image;
    }

    // The span shown: to the end of the last clip, at least four bars.
    let beats_per_bar = project.settings.time_sig_num.max(1) as f64;
    let end_beat = tracks
        .iter()
        .flat_map(|track| track.clips.iter())
        .map(|clip| clip.start_beat + clip.duration_beats)
        .fold(beats_per_bar * 4.0, f64::max);

    let left = PAD as f64;
    let width = (THUMB_WIDTH - PAD * 2) as f64;
    let x_of = |beat: f64| (left + beat / end_beat * width).round() as i64;
    let top = PAD as i64;
    let lane_h = ((THUMB_HEIGHT - PAD * 2) as i64 / tracks.len() as i64).clamp(2, MAX_LANE_H);

    // Lanes, then bar lines across them.
    for (index, _) in tracks.iter().enumerate() {
        let y = top + index as i64 * lane_h;
        let shade = if index % 2 == 0 { LANE_EVEN } else { LANE_ODD };
        fill(
            &mut image,
            PAD as i64,
            y,
            (THUMB_WIDTH - PAD) as i64,
            y + lane_h,
            shade,
        );
    }
    let lanes_bottom = top + lane_h * tracks.len() as i64;
    let bars = (end_beat / beats_per_bar).ceil() as i64;
    let bar_step = (bars / 16).max(1);
    for bar in (0..=bars).step_by(bar_step as usize) {
        let x = x_of(bar as f64 * beats_per_bar);
        fill(&mut image, x, top, x + 1, lanes_bottom, BAR_LINE);
    }

    for (index, track) in tracks.iter().enumerate() {
        let color = parse_color(&track.color_hex);
        let lane_top = top + index as i64 * lane_h;
        // A one-pixel gap between lanes once they are tall enough to spare it.
        let inset = if lane_h >= 6 { 1 } else { 0 };
        let (y0, y1) = (lane_top + inset, lane_top + lane_h - inset);
        // The track's colour down the left edge, as in the arrangement.
        fill(&mut image, 0, y0, 4, y1, color);
        for clip in &track.clips {
            let x0 = x_of(clip.start_beat);
            let x1 = x_of(clip.start_beat + clip.duration_beats).max(x0 + 1);
            let strength = if clip.muted || track.muted {
                0.18
            } else {
                0.42
            };
            let body = mix(LANE_EVEN, color, strength);
            fill(&mut image, x0, y0, x1, y1, body);
            // A brighter top edge, so neighbouring clips stay apart.
            fill(&mut image, x0, y0, x1, y0 + 1, mix(body, color, 0.8));
            if let ClipSource::Midi { notes, .. } = &clip.source {
                draw_notes(
                    &mut image,
                    notes,
                    clip.start_beat,
                    (y0 + 1, y1),
                    color,
                    &x_of,
                );
            }
        }
    }
    image
}

/// MIDI notes inside a clip, over the pitch range the clip uses.
fn draw_notes(
    image: &mut RgbaImage,
    notes: &[super::MidiNote],
    clip_start: f64,
    (y0, y1): (i64, i64),
    color: [u8; 3],
    x_of: &impl Fn(f64) -> i64,
) {
    let Some(low) = notes.iter().map(|note| note.pitch).min() else {
        return;
    };
    let high = notes.iter().map(|note| note.pitch).max().unwrap_or(low);
    let span = (high - low) as i64 + 1;
    let height = (y1 - y0).max(1);
    let row = (height / span.max(1)).clamp(1, 4);
    let ink = mix(color, [0xff, 0xff, 0xff], 0.35);
    for note in notes.iter().filter(|note| !note.muted) {
        let start = clip_start + f64::from(note.start_beats);
        let x0 = x_of(start);
        let x1 = x_of(start + f64::from(note.duration_beats)).max(x0 + 1);
        let from_top = (high - note.pitch) as i64;
        let y = y0 + from_top * (height - row) / (span - 1).max(1);
        fill(image, x0, y, x1, y + row, ink);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_parse_with_or_without_alpha() {
        assert_eq!(parse_color("#56C7C9"), [0x56, 0xc7, 0xc9]);
        assert_eq!(parse_color("#56C7C9FF"), [0x56, 0xc7, 0xc9]);
        assert_eq!(parse_color("nonsense"), [0x8a, 0x8f, 0x99]);
    }

    #[test]
    fn a_project_without_a_preview_folder_gets_no_thumbnail() {
        let dir = std::env::temp_dir().join(format!("fb-thumb-old-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let project = FutureboardProject::new("Old Song");
        write_thumbnail_image_if_enabled(&render_thumbnail(&project), &dir.join("Old Song.fbproj"));
        assert!(!dir.join(PREVIEW_DIR).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_frame_is_cut_from_the_top_left_at_16_by_9() {
        // A wide arrangement in a 2000 x 1000 frame: red where the headers
        // are, blue far to the right.
        let mut frame = RgbaImage::from_pixel(2000, 1000, Rgba([0, 0, 255, 255]));
        for y in 0..1000 {
            for x in 0..300 {
                frame.put_pixel(x, y, Rgba([255, 0, 0, 255]));
            }
        }
        let thumb = thumbnail_from_frame(&frame, (0, 100, 2000, 800)).expect("a thumbnail");
        assert_eq!((thumb.width(), thumb.height()), (THUMB_WIDTH, THUMB_HEIGHT));
        // 800 tall at 16:9 keeps the left 1422 px: the headers are in it.
        assert_eq!(thumb.get_pixel(5, 135)[0], 255);
        assert!(thumbnail_from_frame(&frame, (1990, 0, 400, 400)).is_none());
    }

    #[test]
    fn a_new_project_writes_its_thumbnail() {
        let dir = std::env::temp_dir().join(format!("fb-thumb-new-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        enable_thumbnails(&dir).unwrap();
        let project = FutureboardProject::new("New Song");
        write_thumbnail_image_if_enabled(&render_thumbnail(&project), &dir.join("New Song.fbproj"));
        let written = image::open(thumbnail_path(&dir)).expect("a readable PNG");
        assert_eq!(
            (written.width(), written.height()),
            (THUMB_WIDTH, THUMB_HEIGHT)
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
