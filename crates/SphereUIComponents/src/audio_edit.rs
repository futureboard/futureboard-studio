//! Destructive audio edits with a version history.
//!
//! Every edit that changes a clip's audio (a cut, a process, a repair) renders
//! a **new** file and points the clip at it. Nothing is ever overwritten:
//!
//! * renders go to the project's `Assets/Audio/Edits` folder (or an `Edits`
//!   folder beside the source when the project has not been saved yet), under
//!   a fresh `<name>-edit-NNN.wav`, so the original and every earlier version
//!   stay on disk and undo can always go back to them;
//! * files are 32-bit float at the source's rate and channel count
//!   ([`SphereAudioProcessor::write_wav_f32`]), so an edit costs no resolution
//!   and keeps peaks above 0 dBFS;
//! * [`apply_new_source`] swaps the clip onto the render **without** wiping the
//!   clip's own settings (gain, fades, stretch, pitch, channel, DC, de-hum,
//!   de-noise, envelope). A render is made from the raw source, so those
//!   settings still apply on top — exactly as they did before the edit.
//!
//! Off the realtime path: file I/O and allocation are fine here; callers run
//! the render on the background executor.

use std::path::{Path, PathBuf};

use crate::components::timeline::timeline_state::{AudioImportState, ClipState, ClipType};

/// Where rendered versions of `source` are written.
pub fn edit_output_dir(project_folder: Option<&Path>, source: &Path) -> PathBuf {
    match project_folder {
        Some(root) => crate::paths::ProjectFolderLayout::from_root(root.to_path_buf())
            .media_audio
            .join("Edits"),
        None => source
            .parent()
            .map(|dir| dir.join("Edits"))
            .unwrap_or_else(|| PathBuf::from("Edits")),
    }
}

/// The source's base name with any earlier edit/processed suffix removed, so
/// successive edits of one file read `vox-edit-001`, `vox-edit-002`, … rather
/// than `vox-edit-001-edit-001`.
fn base_stem(source: &Path) -> String {
    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("audio")
        .to_string();
    // `<stem>-edit-NNN`
    if let Some((head, tail)) = stem.rsplit_once("-edit-") {
        if !head.is_empty() && !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
            return head.to_string();
        }
    }
    // Legacy `<stem>.<clip>.processed`
    if let Some(head) = stem.strip_suffix(".processed") {
        if let Some((base, _clip)) = head.rsplit_once('.') {
            if !base.is_empty() {
                return base.to_string();
            }
        }
    }
    stem
}

/// A path in `dir` that does not exist yet: `<base>-edit-NNN.wav`, with NNN
/// the lowest free number. Never returns an existing file.
pub fn next_version_path(dir: &Path, source: &Path) -> PathBuf {
    let base = base_stem(source);
    let mut n = 1u32;
    loop {
        let candidate = dir.join(format!("{base}-edit-{n:03}.wav"));
        if !candidate.exists() && !candidate.with_extension("wav.tmp").exists() {
            return candidate;
        }
        n += 1;
    }
}

/// A rendered file and how it relates to the clip's old source.
#[derive(Debug, Clone, PartialEq)]
pub struct NewSource {
    pub path: PathBuf,
    pub sample_rate: u32,
    pub frames: u64,
    /// The old source window `[start, end)` (in old source frames) that the
    /// render was made from. The new file starts at the window's first frame.
    pub old_window: (u64, u64),
    pub old_sample_rate: u32,
    /// Beats to move the clip's start by, so audio that survived the edit
    /// stays where it was on the timeline (a crop keeps the selection in
    /// place instead of pulling it back to the clip's old start).
    pub start_shift_beats: f32,
}

impl NewSource {
    /// New length relative to the old window, in time (not frames). `1.0` for
    /// every edit except time-changing ones (Time & Pitch).
    pub fn time_scale(&self) -> f64 {
        let old_frames = self.old_window.1.saturating_sub(self.old_window.0).max(1) as f64;
        let old_seconds = old_frames / self.old_sample_rate.max(1) as f64;
        let new_seconds = self.frames as f64 / self.sample_rate.max(1) as f64;
        (new_seconds / old_seconds.max(1e-9)).max(1e-6)
    }
}

/// Point `clip` at a rendered version of its audio.
///
/// Keeps every clip setting: the render was made from the raw source, so
/// gain, fades, stretch, pitch, channel/DC/de-hum/de-noise and the envelope
/// still apply on top of it, as they did before. Only what the file itself
/// changes is adjusted:
///
/// * the source window becomes the whole new file;
/// * warp markers move with the audio (shifted by the old window start,
///   rescaled for a new sample rate), and are dropped when the render changed
///   the audio's length, since their musical positions no longer hold;
/// * a length change scales the clip's duration so it still plays all of it.
pub fn apply_new_source(clip: &mut ClipState, source: &NewSource) {
    let path = source.path.to_string_lossy().into_owned();
    if let ClipType::Audio {
        file_id,
        source_path,
    } = &mut clip.clip_type
    {
        *file_id = path.clone();
        *source_path = Some(path);
    }
    clip.audio_import = AudioImportState::Pending;
    clip.source_duration_seconds = Some(source.frames as f64 / source.sample_rate.max(1) as f64);

    let time_scale = source.time_scale();
    let length_changed = (time_scale - 1.0).abs() > 1e-6;
    let rate_scale = source.sample_rate.max(1) as f64 / source.old_sample_rate.max(1) as f64;
    let (window_start, window_end) = source.old_window;

    let stretch = &mut clip.stretch;
    stretch.original_sample_rate = source.sample_rate;
    stretch.original_duration_samples = source.frames;
    stretch.source_start_samples = 0;
    stretch.source_end_samples = source.frames;
    if length_changed {
        stretch.warp_markers.clear();
    } else {
        stretch.warp_markers.retain(|m| {
            m.source_sample >= window_start && m.source_sample < window_end.max(window_start + 1)
        });
        for marker in &mut stretch.warp_markers {
            let local = (marker.source_sample - window_start) as f64 * rate_scale;
            marker.source_sample = (local.round() as u64).min(source.frames.saturating_sub(1));
        }
    }
    stretch.dirty = true;

    if length_changed {
        clip.duration_beats = (clip.duration_beats as f64 * time_scale) as f32;
    }
    if source.start_shift_beats != 0.0 {
        clip.start_beat = (clip.start_beat + source.start_shift_beats).max(0.0);
    }
}

/// Run `process` on each channel of interleaved `samples` separately and
/// re-interleave. `process` gets the channel index and that channel's samples
/// and must return the same number of samples.
///
/// Processors written for one signal (a spectral denoiser, a notch bank) must
/// not be fed a mono downmix and have the result copied to every channel: that
/// collapses a stereo recording to mono.
pub fn process_channels_independently(
    samples: &[f32],
    channels: usize,
    mut process: impl FnMut(usize, &[f32]) -> Vec<f32>,
) -> Vec<f32> {
    let channels = channels.max(1);
    let frames = samples.len() / channels;
    let mut out = vec![0.0f32; frames * channels];
    let mut plane = Vec::with_capacity(frames);
    for ch in 0..channels {
        plane.clear();
        plane.extend((0..frames).map(|f| samples[f * channels + ch]));
        let processed = process(ch, &plane);
        for (f, value) in processed.iter().take(frames).enumerate() {
            out[f * channels + ch] = *value;
        }
    }
    out
}

/// Interleaved PCM: what an edit reads, writes and the clipboard holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Pcm {
    pub sample_rate: u32,
    pub channels: usize,
    pub samples: Vec<f32>,
}

impl Pcm {
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1)
    }

    /// Frames `[start, end)`, clamped to what exists.
    pub fn slice(&self, start: usize, end: usize) -> Pcm {
        let channels = self.channels.max(1);
        let end = end.min(self.frames());
        let start = start.min(end);
        Pcm {
            sample_rate: self.sample_rate,
            channels,
            samples: self.samples[start * channels..end * channels].to_vec(),
        }
    }

    /// The same audio at `channels` and `sample_rate`, for pasting into a clip
    /// whose source differs from where the audio was copied. Mono spreads to
    /// every channel, a wider source folds down by averaging, and the rate is
    /// converted with the offline sinc resampler.
    pub fn conformed(&self, channels: usize, sample_rate: u32) -> Result<Pcm, String> {
        let from = self.channels.max(1);
        let to = channels.max(1);
        let frames = self.frames();
        let samples = if from == to {
            self.samples.clone()
        } else {
            let mut out = Vec::with_capacity(frames * to);
            for frame in self.samples.chunks(from) {
                if from == 1 {
                    out.extend(std::iter::repeat(frame[0]).take(to));
                } else if to == 1 {
                    out.push(frame.iter().sum::<f32>() / from as f32);
                } else {
                    out.extend((0..to).map(|ch| frame[ch % from]));
                }
            }
            out
        };
        let samples = if self.sample_rate != sample_rate {
            SphereAudioProcessor::resample_interleaved(&samples, to, self.sample_rate, sample_rate)
                .map_err(|e| e.to_string())?
        } else {
            samples
        };
        Ok(Pcm {
            sample_rate,
            channels: to,
            samples,
        })
    }
}

/// A destructive edit of a frame range.
#[derive(Debug, Clone)]
pub enum EditOp {
    /// Remove the range; later audio moves up to close the gap.
    Delete,
    /// Keep only the range.
    Crop,
    /// Replace the range with digital silence.
    Silence,
    /// Ramp the range up from silence.
    FadeIn,
    /// Ramp the range down to silence.
    FadeOut,
    /// Scale the range so its peak reaches `target_db` dBFS.
    Normalize { target_db: f32 },
    /// Play the range backwards.
    Reverse,
    /// Replace the range with this audio (insert when the range is empty).
    /// Must already match the edited audio's channels and rate.
    Paste(std::sync::Arc<Pcm>),
}

impl EditOp {
    pub fn label(&self) -> &'static str {
        match self {
            EditOp::Delete => "Delete",
            EditOp::Crop => "Crop",
            EditOp::Silence => "Silence",
            EditOp::FadeIn => "Fade In",
            EditOp::FadeOut => "Fade Out",
            EditOp::Normalize { .. } => "Normalize",
            EditOp::Reverse => "Reverse",
            EditOp::Paste(_) => "Paste",
        }
    }

    /// Whether the op can run on an empty range (a cursor).
    pub fn accepts_empty_range(&self) -> bool {
        matches!(self, EditOp::Paste(_))
    }

    /// The same edit as seen from a clip that plays its source backwards:
    /// a fade that should rise on the timeline must fall in the file.
    pub fn for_reversed_playback(self) -> Self {
        match self {
            EditOp::FadeIn => EditOp::FadeOut,
            EditOp::FadeOut => EditOp::FadeIn,
            EditOp::Paste(pcm) => {
                let mut reversed = (*pcm).clone();
                reverse_frames(&mut reversed.samples, reversed.channels);
                EditOp::Paste(std::sync::Arc::new(reversed))
            }
            other => other,
        }
    }
}

/// Length of the crossfade that hides a splice, in frames: 2 ms. Long enough
/// that no joint clicks, short enough to be inaudible as a fade.
pub fn splice_frames(sample_rate: u32) -> usize {
    (sample_rate as usize / 500).max(8)
}

/// Apply `op` to frames `[start, end)` of interleaved `samples`.
///
/// Every joint the edit creates is crossfaded against the audio that
/// naturally continues across it (see [`splice`]), so cutting, pasting,
/// silencing or reversing part of a waveform never leaves a step that clicks.
pub fn apply_edit(
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
    start: usize,
    end: usize,
    op: &EditOp,
) -> Result<Vec<f32>, String> {
    let channels = channels.max(1);
    let frames = samples.len() / channels;
    let end = end.min(frames);
    let start = start.min(end);
    if start == end && !op.accepts_empty_range() {
        return Err("Select some audio first".to_string());
    }
    let ramp = splice_frames(sample_rate);
    let mut out = match op {
        EditOp::Delete => splice(samples, channels, start, end, &[], ramp),
        EditOp::Paste(pcm) => {
            if pcm.channels.max(1) != channels {
                return Err("clipboard does not match the clip's channels".to_string());
            }
            splice(samples, channels, start, end, &pcm.samples, ramp)
        }
        EditOp::Crop => {
            let mut kept = samples[start * channels..end * channels].to_vec();
            let n = ramp.min((end - start) / 2);
            if start > 0 {
                ramp_edge(&mut kept, channels, 0, n, true);
            }
            if end < frames {
                let len = end - start;
                ramp_edge(&mut kept, channels, len - n, len, false);
            }
            kept
        }
        EditOp::Silence => {
            let mut out = samples.to_vec();
            let n = ramp.min((end - start) / 2);
            // Keep a ramp at each edge so the drop to silence does not click.
            let body_start = if start > 0 { start + n } else { start };
            let body_end = if end < frames { end - n } else { end };
            ramp_edge(&mut out, channels, start, body_start, false);
            ramp_edge(&mut out, channels, body_end, end, true);
            out[body_start * channels..body_end * channels].fill(0.0);
            out
        }
        EditOp::FadeIn | EditOp::FadeOut => {
            let mut out = samples.to_vec();
            let len = (end - start) as f32;
            let rising = matches!(op, EditOp::FadeIn);
            for frame in start..end {
                let t = (frame - start) as f32 / len.max(1.0);
                let t = if rising { t } else { 1.0 - t };
                let gain = (t * std::f32::consts::FRAC_PI_2).sin();
                for value in &mut out[frame * channels..(frame + 1) * channels] {
                    *value *= gain;
                }
            }
            out
        }
        EditOp::Normalize { target_db } => {
            let region = &samples[start * channels..end * channels];
            let peak = region
                .iter()
                .filter(|v| v.is_finite())
                .fold(0.0f32, |peak, v| peak.max(v.abs()));
            if peak <= 1.0e-9 {
                return Err("The selection is silent".to_string());
            }
            let gain = 10.0f32.powf(target_db / 20.0) / peak;
            let mut out = samples.to_vec();
            for value in &mut out[start * channels..end * channels] {
                *value *= gain;
            }
            out
        }
        EditOp::Reverse => {
            let mut reversed = samples[start * channels..end * channels].to_vec();
            reverse_frames(&mut reversed, channels);
            splice(samples, channels, start, end, &reversed, ramp)
        }
    };
    for value in &mut out {
        if !value.is_finite() {
            *value = 0.0;
        }
    }
    Ok(out)
}

/// Replace frames `[start, end)` of `samples` with `insert`, crossfading each
/// joint into the audio that would have continued across it:
///
/// * the head of the inserted audio fades in over `samples[start..]`, the
///   audio that followed the left side of the cut;
/// * its tail fades out into `samples[..end]`, the audio that led into the
///   right side.
///
/// With nothing inserted the two joints coincide, and the left side fades
/// straight into the audio that led up to `end`. Either way the waveform stays
/// continuous across every joint and the result is exactly
/// `frames - (end - start) + insert_frames` long.
fn splice(
    samples: &[f32],
    channels: usize,
    start: usize,
    end: usize,
    insert: &[f32],
    ramp: usize,
) -> Vec<f32> {
    let frames = samples.len() / channels;
    let insert_frames = insert.len() / channels;
    let mut out = Vec::with_capacity((frames - (end - start) + insert_frames) * channels);
    out.extend_from_slice(&samples[..start * channels]);
    // A joint exists only where audio remains on both sides of it.
    if insert_frames == 0 {
        let n = if end < frames {
            ramp.min(start).min(end - start)
        } else {
            0
        };
        let join = start - n;
        for i in 0..n {
            let t = (i as f32 + 0.5) / n as f32;
            for ch in 0..channels {
                let left = samples[(join + i) * channels + ch];
                let lead_in = samples[(end - n + i) * channels + ch];
                out[(join + i) * channels + ch] = left * (1.0 - t) + lead_in * t;
            }
        }
    } else {
        let base = out.len();
        out.extend_from_slice(&insert[..insert_frames * channels]);
        let head = if start > 0 {
            ramp.min(insert_frames / 2).min(frames - start)
        } else {
            0
        };
        for i in 0..head {
            let t = (i as f32 + 0.5) / head as f32;
            for ch in 0..channels {
                let index = base + i * channels + ch;
                let follow = samples[(start + i) * channels + ch];
                out[index] = out[index] * t + follow * (1.0 - t);
            }
        }
        let tail = if end < frames {
            ramp.min(insert_frames / 2).min(end)
        } else {
            0
        };
        for i in 0..tail {
            let t = (i as f32 + 0.5) / tail as f32;
            for ch in 0..channels {
                let index = base + (insert_frames - tail + i) * channels + ch;
                let lead_in = samples[(end - tail + i) * channels + ch];
                out[index] = out[index] * (1.0 - t) + lead_in * t;
            }
        }
    }
    out.extend_from_slice(&samples[end * channels..]);
    out
}

/// Linear gain ramp over frames `[from, to)`: up from silence when `rising`,
/// down to silence otherwise.
fn ramp_edge(samples: &mut [f32], channels: usize, from: usize, to: usize, rising: bool) {
    let n = to.saturating_sub(from);
    for i in 0..n {
        let t = (i as f32 + 0.5) / n as f32;
        let gain = if rising { t } else { 1.0 - t };
        for value in &mut samples[(from + i) * channels..(from + i + 1) * channels] {
            *value *= gain;
        }
    }
}

pub(crate) fn reverse_frames(samples: &mut [f32], channels: usize) {
    let channels = channels.max(1);
    let frames = samples.len() / channels;
    for i in 0..frames / 2 {
        let j = frames - 1 - i;
        for ch in 0..channels {
            samples.swap(i * channels + ch, j * channels + ch);
        }
    }
}

/// Audio copied or cut in the Audio Editor. One clipboard for the whole app,
/// so audio copied from one clip pastes into another.
pub mod clipboard {
    use std::sync::{Arc, Mutex, OnceLock};

    use super::Pcm;

    fn slot() -> &'static Mutex<Option<Arc<Pcm>>> {
        static SLOT: OnceLock<Mutex<Option<Arc<Pcm>>>> = OnceLock::new();
        SLOT.get_or_init(|| Mutex::new(None))
    }

    pub fn set(pcm: Pcm) {
        if let Ok(mut slot) = slot().lock() {
            *slot = Some(Arc::new(pcm));
        }
    }

    pub fn get() -> Option<Arc<Pcm>> {
        slot().lock().ok().and_then(|slot| slot.clone())
    }

    pub fn has_audio() -> bool {
        slot()
            .lock()
            .map(|slot| slot.as_ref().is_some_and(|pcm| pcm.frames() > 0))
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::timeline::timeline_state::{AudioClipStretchState, WarpMarker};

    fn audio_clip() -> ClipState {
        ClipState {
            id: "c1".to_string(),
            name: "vox".to_string(),
            start_beat: 0.0,
            duration_beats: 8.0,
            source_duration_seconds: None,
            offset_beats: 0.0,
            gain: 0.5,
            clip_type: ClipType::Audio {
                file_id: "/a/vox.wav".into(),
                source_path: Some("/a/vox.wav".into()),
            },
            muted: false,
            audio_import: AudioImportState::Ready,
            stretch: AudioClipStretchState {
                original_sample_rate: 48_000,
                source_start_samples: 48_000,
                source_end_samples: 144_000,
                pitch_shift_semitones: 2.0,
                dc_remove: true,
                denoise_amount: 0.4,
                warp_markers: vec![
                    WarpMarker {
                        id: 1,
                        source_sample: 24_000,
                        timeline_beat: 0.0,
                        locked: false,
                    },
                    WarpMarker {
                        id: 2,
                        source_sample: 96_000,
                        timeline_beat: 4.0,
                        locked: false,
                    },
                ],
                ..AudioClipStretchState::default()
            },
        }
    }

    fn render(frames: u64, rate: u32) -> NewSource {
        NewSource {
            path: PathBuf::from("/p/Assets/Audio/Edits/vox-edit-001.wav"),
            sample_rate: rate,
            frames,
            old_window: (48_000, 144_000),
            old_sample_rate: 48_000,
            start_shift_beats: 0.0,
        }
    }

    #[test]
    fn replacing_the_source_keeps_the_clip_settings() {
        let mut clip = audio_clip();
        apply_new_source(&mut clip, &render(96_000, 48_000));
        assert_eq!(clip.gain, 0.5);
        assert_eq!(clip.stretch.pitch_shift_semitones, 2.0);
        assert!(clip.stretch.dc_remove);
        assert_eq!(clip.stretch.denoise_amount, 0.4);
        assert_eq!(clip.duration_beats, 8.0);
        assert_eq!(
            (
                clip.stretch.source_start_samples,
                clip.stretch.source_end_samples
            ),
            (0, 96_000)
        );
        assert!(matches!(
            &clip.clip_type,
            ClipType::Audio { source_path: Some(p), .. } if p.ends_with("vox-edit-001.wav")
        ));
    }

    #[test]
    fn warp_markers_follow_the_cropped_audio() {
        let mut clip = audio_clip();
        apply_new_source(&mut clip, &render(96_000, 48_000));
        // The marker before the window is gone; the one inside moved with it.
        assert_eq!(clip.stretch.warp_markers.len(), 1);
        assert_eq!(clip.stretch.warp_markers[0].source_sample, 48_000);
    }

    #[test]
    fn a_resample_rescales_markers_and_keeps_the_length() {
        let mut clip = audio_clip();
        apply_new_source(&mut clip, &render(88_200, 44_100));
        assert_eq!(clip.duration_beats, 8.0);
        assert_eq!(clip.stretch.warp_markers[0].source_sample, 44_100);
    }

    #[test]
    fn a_time_change_scales_the_clip_and_drops_markers() {
        let mut clip = audio_clip();
        apply_new_source(&mut clip, &render(192_000, 48_000));
        assert_eq!(clip.duration_beats, 16.0);
        assert!(clip.stretch.warp_markers.is_empty());
    }

    #[test]
    fn versions_never_collide_and_do_not_stack_suffixes() {
        let dir = std::env::temp_dir().join(format!("fb-edits-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = next_version_path(&dir, Path::new("/src/vox.wav"));
        assert_eq!(first.file_name().unwrap(), "vox-edit-001.wav");
        std::fs::write(&first, b"x").unwrap();
        let second = next_version_path(&dir, &first);
        assert_eq!(second.file_name().unwrap(), "vox-edit-002.wav");
        let legacy = next_version_path(&dir, Path::new("/src/vox.clip-3.processed.wav"));
        assert_eq!(legacy.file_name().unwrap(), "vox-edit-002.wav");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn edits_go_to_the_project_media_folder() {
        assert_eq!(
            edit_output_dir(Some(Path::new("/p")), Path::new("/elsewhere/a.wav")),
            PathBuf::from("/p/Assets/Audio/Edits")
        );
        assert_eq!(
            edit_output_dir(None, Path::new("/elsewhere/a.wav")),
            PathBuf::from("/elsewhere/Edits")
        );
    }

    #[test]
    fn channels_are_processed_independently() {
        let stereo = [1.0f32, -1.0, 2.0, -2.0];
        let out = process_channels_independently(&stereo, 2, |ch, data| {
            data.iter()
                .map(|v| v * if ch == 0 { 10.0 } else { 100.0 })
                .collect()
        });
        assert_eq!(out, vec![10.0, -100.0, 20.0, -200.0]);
    }

    fn ramp(n: usize) -> Vec<f32> {
        (0..n).map(|i| i as f32).collect()
    }

    /// Largest jump between neighbouring samples of one channel.
    fn max_step(samples: &[f32], channels: usize, ch: usize) -> f32 {
        samples
            .chunks(channels)
            .map(|f| f[ch])
            .collect::<Vec<_>>()
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max)
    }

    fn sine(frames: usize, channels: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let v = (i as f32 * 0.05).sin();
                std::iter::repeat(v).take(channels)
            })
            .collect()
    }

    #[test]
    fn delete_removes_the_range_without_a_step() {
        let input = sine(4_000, 2);
        let out = apply_edit(&input, 2, 48_000, 1_000, 1_700, &EditOp::Delete).unwrap();
        assert_eq!(out.len(), (4_000 - 700) * 2);
        // The audio after the cut is untouched and starts right at the joint.
        assert_eq!(&out[1_000 * 2..], &input[1_700 * 2..]);
        // A sine at 0.05 rad/sample moves < 0.05 per sample; the joint must
        // not jump much more than that.
        assert!(max_step(&out, 2, 0) < 0.12, "{}", max_step(&out, 2, 0));
    }

    #[test]
    fn crop_keeps_only_the_range() {
        let input = ramp(100);
        let out = apply_edit(&input, 1, 48_000, 0, 40, &EditOp::Crop).unwrap();
        assert_eq!(out.len(), 40);
        assert_eq!(out[0], 0.0);
        // The file edge needs no ramp; the cut edge does.
        assert!(out[39] < 39.0);
    }

    #[test]
    fn silence_zeroes_the_body_and_keeps_the_length() {
        let input = vec![1.0f32; 10_000];
        let out = apply_edit(&input, 1, 48_000, 2_000, 8_000, &EditOp::Silence).unwrap();
        assert_eq!(out.len(), 10_000);
        assert_eq!(out[5_000], 0.0);
        assert_eq!(out[1_999], 1.0);
        assert_eq!(out[8_000], 1.0);
        assert!(max_step(&out, 1, 0) < 0.02);
    }

    #[test]
    fn fades_reach_silence_at_the_right_end() {
        let input = vec![1.0f32; 1_000];
        let fin = apply_edit(&input, 1, 48_000, 0, 1_000, &EditOp::FadeIn).unwrap();
        assert!(fin[0] < 1.0e-3 && fin[999] > 0.99);
        let fout = apply_edit(&input, 1, 48_000, 0, 1_000, &EditOp::FadeOut).unwrap();
        assert!(fout[0] > 0.99 && fout[999] < 3.0e-3);
    }

    #[test]
    fn normalize_hits_the_target_peak() {
        let input = vec![0.25f32, -0.5, 0.1, 0.9];
        let op = EditOp::Normalize { target_db: 0.0 };
        let out = apply_edit(&input, 2, 48_000, 0, 1, &op).unwrap();
        assert!((out[1] + 1.0).abs() < 1.0e-6);
        assert!((out[0] - 0.5).abs() < 1.0e-6);
        // Frames outside the range are untouched.
        assert_eq!(&out[2..], &input[2..]);
        let silent = apply_edit(&[0.0; 8], 2, 48_000, 0, 4, &op);
        assert!(silent.is_err());
    }

    #[test]
    fn reverse_flips_frames_and_keeps_channels_paired() {
        let input = vec![1.0, 10.0, 2.0, 20.0, 3.0, 30.0];
        let out = apply_edit(&input, 2, 8, 0, 3, &EditOp::Reverse).unwrap();
        assert_eq!(out, vec![3.0, 30.0, 2.0, 20.0, 1.0, 10.0]);
    }

    #[test]
    fn paste_inserts_at_a_cursor_and_replaces_a_range() {
        let input = sine(4_000, 1);
        let clip = Pcm {
            sample_rate: 48_000,
            channels: 1,
            samples: sine(500, 1),
        };
        let op = EditOp::Paste(std::sync::Arc::new(clip));
        let inserted = apply_edit(&input, 1, 48_000, 2_000, 2_000, &op).unwrap();
        assert_eq!(inserted.len(), 4_500);
        assert_eq!(&inserted[2_500..], &input[2_000..]);
        assert!(max_step(&inserted, 1, 0) < 0.12);
        let replaced = apply_edit(&input, 1, 48_000, 1_000, 3_000, &op).unwrap();
        assert_eq!(replaced.len(), 2_500);
    }

    #[test]
    fn an_empty_range_is_refused_except_for_paste() {
        assert!(apply_edit(&[0.5; 10], 1, 48_000, 3, 3, &EditOp::Delete).is_err());
    }

    #[test]
    fn a_reversed_clip_swaps_fades() {
        assert!(matches!(
            EditOp::FadeIn.for_reversed_playback(),
            EditOp::FadeOut
        ));
    }

    #[test]
    fn clipboard_audio_conforms_to_the_target() {
        let stereo = Pcm {
            sample_rate: 48_000,
            channels: 2,
            samples: vec![1.0, 0.0, 0.5, 0.5],
        };
        let mono = stereo.conformed(1, 48_000).unwrap();
        assert_eq!(mono.samples, vec![0.5, 0.5]);
        let wide = mono.conformed(2, 48_000).unwrap();
        assert_eq!(wide.samples, vec![0.5, 0.5, 0.5, 0.5]);
        let long = Pcm {
            sample_rate: 48_000,
            channels: 1,
            samples: vec![0.0; 48_000],
        };
        let resampled = long.conformed(1, 24_000).unwrap();
        assert!((resampled.frames() as i64 - 24_000).abs() < 64);
    }
}
