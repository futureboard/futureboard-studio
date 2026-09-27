use gpui::KeyDownEvent;

use crate::components::add_track_dialog::AddTrackKind;
use crate::components::timeline::timeline_state::TrackState;

use super::studio_state::TransportCommand;

/// Keep in sync with `DirectAudio::probe_audio_file`,
/// `waveform_cache::decode_file_uncached`, and
/// `file_browser::FileBrowserEntry::is_audio` — any divergence between
/// these lists creates "imports but never plays" or "looks pending
/// forever" bugs.
pub(super) fn is_supported_audio_ext(ext: &str) -> bool {
    matches!(
        ext,
        "wav" | "wave" | "mp3" | "flac" | "ogg" | "oga" | "m4a" | "aiff" | "aif"
    )
}

/// Resolve a shared menu command ID to a transport action.
/// Returns `None` for commands the unified dispatcher should log as
/// unsupported. Keep in lock-step with `apps/web/src/menu/actionRunner.ts`
/// and `packages/shared/generated/native-menu.json`.
pub(super) fn transport_command_from_id(command_id: &str) -> Option<TransportCommand> {
    match command_id {
        "transport:play-pause" => Some(TransportCommand::PlayPause),
        "transport:stop" => Some(TransportCommand::Stop),
        "transport:go-to-start" => Some(TransportCommand::ReturnToStart),
        "transport:toggle-loop" => Some(TransportCommand::ToggleLoop),
        "transport:toggle-metronome" => Some(TransportCommand::ToggleMetronome),
        "transport:toggle-follow-playhead" | "transport:toggle-autoscroll" => {
            Some(TransportCommand::ToggleFollowPlayhead)
        }
        "transport:toggle-autoscroll-mode" => Some(TransportCommand::ToggleAutoScrollMode),
        "transport:record" => Some(TransportCommand::Record),
        _ => None,
    }
}

/// Focus-relevant snapshot used to decide whether a global transport shortcut
/// (Space, Enter, ...) should be handled by the workspace or left to the focused
/// widget. Captured on the UI thread at the moment a key arrives.
pub(super) struct FocusContext {
    /// A live Futureboard text field (search / rename / numeric edit) owns
    /// keyboard capture. Closed popovers with stale focus handles do not count.
    pub text_input_focused: bool,
}

/// Whether the workspace should claim a global transport shortcut.
///
/// - Text field focused -> keep the keystroke (Space types a space).
/// - Otherwise -> the workspace handles it (Space toggles playback).
///
/// Note: when the native plugin editor window is the active OS window this
/// code path is never reached - Windows delivers the key to the plugin's HWND,
/// not the GPUI workspace window - so "plugin editor focused" implicitly means
/// the plugin consumes the key, matching the current policy.
pub(super) fn should_handle_global_transport_shortcut(focus: &FocusContext) -> bool {
    !focus.text_input_focused
}

pub(super) fn is_tap_tempo_command(command_id: &str) -> bool {
    matches!(
        command_id,
        "tempo:tap" | "tempo:reset-tap" | "tempo:add-tap-marker"
    )
}

pub(super) fn key_debug() -> bool {
    std::env::var_os("FUTUREBOARD_KEY_DEBUG").is_some()
}

/// `FUTUREBOARD_EDIT_COMMAND_DEBUG=1` traces edit-command routing (resolved
/// command, target editor, no-op reason). Cached on first read.
pub(super) fn edit_command_debug() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_EDIT_COMMAND_DEBUG").is_some())
}

/// The Ctrl/Cmd+A/C/V/X and Delete/Backspace command family that both the
/// timeline and the MIDI editor implement. When the MIDI editor holds focus the
/// `StudioLayout` global handler must NOT dispatch these as timeline commands —
/// it lets the event bubble to the piano roll's own `on_key_down` instead, so
/// Ctrl+A selects notes (not clips) and Delete removes notes (not tracks/clips).
pub(super) fn is_midi_routable_edit_command(command_id: &str) -> bool {
    matches!(
        command_id,
        "edit:select-all"
            | "edit:copy"
            | "edit:cut"
            | "edit:paste"
            | "edit:duplicate"
            | "edit:delete"
            | "edit:delete-backspace"
            | "clip:delete"
            | "clip:duplicate"
    )
}

pub(super) fn is_text_input_key(event: &KeyDownEvent) -> bool {
    let key = event.keystroke.key.as_str();
    let mods = event.keystroke.modifiers;
    if (mods.control || mods.platform) && !mods.alt && !mods.function {
        return matches!(key, "a" | "A" | "c" | "C" | "v" | "V" | "x" | "X");
    }
    if mods.control || mods.alt || mods.platform || mods.function {
        return false;
    }
    matches!(
        key,
        "backspace"
            | "delete"
            | "left"
            | "arrow_left"
            | "right"
            | "arrow_right"
            | "home"
            | "end"
            | "space"
    ) || key.chars().count() == 1
}

pub(super) fn normalize_command_id(command_id: &str) -> String {
    command_id.trim().replace('.', ":").replace('_', "-")
}

pub(super) fn cleaned_track_name(name: &str, kind: AddTrackKind) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        kind.label().to_string()
    } else {
        trimmed.to_string()
    }
}

pub(super) fn numbered_name_stem(name: &str) -> String {
    let stem = name
        .trim_end_matches(|c: char| c.is_ascii_digit())
        .trim_end();
    if stem.is_empty() {
        "Track".to_string()
    } else {
        stem.to_string()
    }
}

#[inline]
fn meter_visual_bucket(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0) as u8
}

/// Advance one smoothed meter value. The dirty verdict follows the same 8-bit
/// visual precision used by meter render signatures, so every visible step is
/// painted while sub-visible motion remains free of redundant notifications.
pub(super) fn smooth_meter_value(current: &mut f32, target: f32, dt_secs: f32) -> bool {
    let previous_bucket = meter_visual_bucket(*current);
    let target = target.clamp(0.0, 1.0);
    let dt_frames = (dt_secs.clamp(1.0 / 240.0, 0.1) * 60.0).max(0.0);
    let base_rate: f32 = if target > *current { 0.72 } else { 0.18 };
    let rate = 1.0 - (1.0 - base_rate).powf(dt_frames);
    let next = (*current + (target - *current) * rate).clamp(0.0, 1.0);
    *current = if next < 0.002 { 0.0 } else { next };
    previous_bucket != meter_visual_bucket(*current)
}

/// Update a peak-hold value: jump up instantly to a higher level, otherwise
/// release slowly so the held peak lingers above the decaying meter bar
/// (roughly a third of full scale per second, independent of display refresh).
pub(super) fn update_meter_hold(hold: &mut f32, level: f32, dt_secs: f32) -> bool {
    let previous_bucket = meter_visual_bucket(*hold);
    let level = level.clamp(0.0, 1.0);
    *hold = if level >= *hold {
        level
    } else {
        (*hold - 0.36 * dt_secs.clamp(1.0 / 240.0, 0.1)).max(level)
    };
    previous_bucket != meter_visual_bucket(*hold)
}

/// Latch / release the clip indicator. Sets it when either channel's raw
/// (pre-clamp) peak reached 0 dBFS, and clears it once the held peak has
/// fallen back below 0.6 (≈1 s after the last overload, via the hold release).
pub(super) fn update_meter_clip(
    clip: &mut bool,
    raw_peak_l: f64,
    raw_peak_r: f64,
    hold_max: f32,
) -> bool {
    let previous = *clip;
    if raw_peak_l >= 1.0 || raw_peak_r >= 1.0 {
        *clip = true;
    } else if hold_max < 0.6 {
        *clip = false;
    }
    previous != *clip
}

pub(super) fn find_clip_summary<'a>(
    tracks: &'a [TrackState],
    clip_id: Option<&str>,
    project_bpm: f64,
    selection_duration_beats: Option<f32>,
) -> Option<crate::components::panel::SelectedClipSummary<'a>> {
    let id = clip_id?;
    for t in tracks {
        if let Some(c) = t.clips.iter().find(|c| c.id == id) {
            let (kind, source_path, note_count) = match &c.clip_type {
                crate::components::timeline::timeline_state::ClipType::Audio {
                    source_path,
                    ..
                } => ("Audio", source_path.as_deref(), None),
                crate::components::timeline::timeline_state::ClipType::Midi { notes, .. } => {
                    ("MIDI", None, Some(notes.len()))
                }
                crate::components::timeline::timeline_state::ClipType::Video {
                    source_path,
                    ..
                } => ("Video", source_path.as_deref(), None),
            };
            return Some(crate::components::panel::SelectedClipSummary {
                clip_id: &c.id,
                track_id: &t.id,
                name: &c.name,
                start_beat: c.start_beat,
                duration_beats: c.duration_beats,
                muted: c.muted,
                gain: c.gain,
                source_duration_seconds: c.source_duration_seconds,
                source_path,
                note_count,
                kind,
                track_name: &t.name,
                stretch: &c.stretch,
                project_bpm,
                selection_duration_beats,
                fades: None,
            });
        }
    }
    None
}

pub(super) fn reveal_path(path: &std::path::Path) {
    #[cfg(target_os = "windows")]
    {
        if path.is_file() {
            let _ = std::process::Command::new("explorer")
                .arg(format!("/select,{}", path.display()))
                .spawn();
        } else {
            let _ = std::process::Command::new("explorer").arg(path).spawn();
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg(if path.is_file() { "-R" } else { "" })
            .arg(path)
            .spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let parent = if path.is_file() {
            path.parent().unwrap_or(path)
        } else {
            path
        };
        let _ = std::process::Command::new("xdg-open").arg(parent).spawn();
    }
}

#[cfg(test)]
mod transport_binding_tests {
    use super::*;

    /// The transport Record button (top toolbar) and the `R`/`F9` keymap entry
    /// both dispatch the literal command id `"transport:record"`. It must
    /// resolve to the transport Record action — never a project/save action.
    /// Regression guard for the "record button saves the project" bug.
    #[test]
    fn record_command_id_resolves_to_transport_record() {
        assert!(matches!(
            transport_command_from_id("transport:record"),
            Some(TransportCommand::Record)
        ));
    }

    /// Normalized id forms (web-style `.` namespace separator, surrounding
    /// whitespace) must resolve identically — the chrome/keymap dispatcher
    /// normalizes ids before lookup, so a Record click can't be lost to a
    /// separator mismatch. (`normalize_command_id` maps `.`→`:` and `_`→`-`.)
    #[test]
    fn record_command_id_resolves_after_normalization() {
        for raw in ["transport.record", " transport:record "] {
            let normalized = normalize_command_id(raw);
            assert!(
                matches!(
                    transport_command_from_id(&normalized),
                    Some(TransportCommand::Record)
                ),
                "id {raw:?} should resolve to TransportCommand::Record"
            );
        }
    }

    /// The Save commands are not transport actions: `transport_command_from_id`
    /// returns `None`, so the transport dispatcher leaves them to the explicit
    /// project-save path. If a refactor ever mapped a save id onto a transport
    /// action (or Record onto Save), clicking Save could toggle recording and
    /// clicking Record could save — the exact regression class this file guards.
    #[test]
    fn midi_focus_routes_both_delete_keys_to_the_editor() {
        assert!(is_midi_routable_edit_command("edit:delete"));
        assert!(is_midi_routable_edit_command("edit:delete-backspace"));
    }

    #[test]
    fn save_command_ids_are_not_transport_actions() {
        for save_id in ["project:save", "project:save-as", "project:save-copy"] {
            assert!(
                transport_command_from_id(save_id).is_none(),
                "save id {save_id:?} must not map to a transport action"
            );
            let normalized = normalize_command_id(save_id);
            assert!(
                transport_command_from_id(&normalized).is_none(),
                "normalized save id {normalized:?} must not map to a transport action"
            );
        }
    }

    #[test]
    fn meter_dirty_detection_tracks_visible_steps_and_final_zero() {
        let mut level = 0.5;
        assert!(smooth_meter_value(&mut level, 0.0, 1.0 / 60.0));

        level = 0.0041;
        assert!(smooth_meter_value(&mut level, 0.0, 0.1));
        assert_eq!(level, 0.0);
    }

    #[test]
    fn peak_hold_and_clip_report_visual_state_changes() {
        let mut hold = 0.8;
        assert!(update_meter_hold(&mut hold, 0.0, 1.0 / 60.0));

        let mut clip = false;
        assert!(update_meter_clip(&mut clip, 1.0, 0.0, hold));
        assert!(clip);
        assert!(!update_meter_clip(&mut clip, 0.0, 0.0, hold));
        hold = 0.0;
        assert!(update_meter_clip(&mut clip, 0.0, 0.0, hold));
        assert!(!clip);
    }
}
