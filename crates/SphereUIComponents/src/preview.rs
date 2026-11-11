//! Offscreen previews of native views, rendered to PNG by `bin/preview.rs`.
//!
//! A developer tool behind the `ui-preview` feature, never part of the app.
//! Each [`Scene`] opens one real GPUI window — the same view type, theme and
//! fonts Studio uses — whose frame the binary reads back from the renderer
//! with `Window::render_to_image`; nothing is captured from the screen.
//!
//! Plug-in editors are given their state through Studio's own state mirror
//! (`builtin_state_seed`), exactly as a loaded project would, and a host-ops
//! set whose telemetry is synthetic: a fixed sample rate and a still,
//! music-shaped analyser frame. That data exists only here, so a preview
//! shows the analyser without a running host.

use std::sync::Arc;

use gpui::{
    point, px, size, AnyWindowHandle, App, AppContext, Bounds, Context, Result, Window,
    WindowBackgroundAppearance, WindowBounds, WindowKind, WindowOptions,
};

use crate::components::band_model::BandKind;
use crate::components::builtin_plugin_editor::builtin_state_seed;
use crate::components::builtin_plugin_editor_window::{BuiltinEditorHostOps, PluginInstanceKey};
use crate::components::dyn_model::DynKind;
use crate::components::native_plugin_shell::ShellIdentity;
use crate::components::plate;
use crate::components::plugin_kit::{KitModel, KitWindow};
use crate::components::white_sharp_panel::DOCK_H as WHITESHARP_DOCK_H;
use crate::components::white_sharp_window::{Dock, WhiteSharpWindow, WHITESHARP_WINDOW_SIZE};

/// One view to render.
pub struct Scene {
    /// File stem of its PNG, and the name `--scene` filters on.
    pub name: &'static str,
    pub width: f32,
    pub height: f32,
    /// Opens the window with `options` (already sized and placed).
    open: fn(WindowOptions, &mut App) -> Result<AnyWindowHandle>,
}

impl Scene {
    /// Opens the scene's window at its size. GPUI keeps a window's bounds only
    /// when its centre is on a display, so it opens on one; the caller moves it
    /// off the desktop with [`tuck_away`] in the same update, before the
    /// message loop runs and it could be painted there.
    pub fn open(&self, cx: &mut App) -> Result<AnyWindowHandle> {
        let bounds = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(self.width), px(self.height)),
        };
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            focus: false,
            show: true,
            kind: WindowKind::PopUp,
            is_movable: false,
            window_background: WindowBackgroundAppearance::Opaque,
            ..Default::default()
        };
        (self.open)(options, cx)
    }
}

/// Where [`tuck_away`] puts a scene's window: off every desktop.
#[cfg(target_os = "windows")]
const OFF_DESKTOP: i32 = -24_000;

#[cfg(target_os = "windows")]
fn hwnd(window: &Window) -> std::result::Result<windows::Win32::Foundation::HWND, String> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = HasWindowHandle::window_handle(window)
        .map_err(|error| format!("no window handle: {error}"))?;
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return Err("not a Win32 window".to_string());
    };
    Ok(windows::Win32::Foundation::HWND(win32.hwnd.get() as *mut _))
}

/// Moves `window` off every desktop and shows it there without activating
/// it: it lays out and draws like any window — at the scale of the display
/// it opened on, unless [`set_scale`] changes it — while no one sees it and
/// focus stays where it was.
#[cfg(target_os = "windows")]
pub fn tuck_away(window: &Window) -> std::result::Result<(), String> {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, ShowWindow, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_SHOWNOACTIVATE,
    };
    let hwnd = hwnd(window)?;
    unsafe {
        SetWindowPos(
            hwnd,
            None,
            OFF_DESKTOP,
            OFF_DESKTOP,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
        .map_err(|error| format!("SetWindowPos: {error}"))?;
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    Ok(())
}

/// Renders the tucked-away `window` at `scale` instead of its display's: it
/// is told its DPI changed, as Windows does when a window moves to another
/// monitor, so GPUI keeps the logical layout, resizes the renderer to the
/// new device size, and rasterizes text and icons at that scale. Sent
/// outside any window update, so the resize reaches the GPUI window.
///
/// The window's frame goes first: Windows limits a captioned window to the
/// size of the screen, which a scaled window easily outgrows.
#[cfg(target_os = "windows")]
pub fn set_scale(
    cx: &mut gpui::AsyncApp,
    window: AnyWindowHandle,
    scale: f32,
) -> std::result::Result<(), String> {
    use windows::Win32::Foundation::{LPARAM, RECT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SendMessageW, SetWindowLongPtrW, SetWindowPos, GWL_STYLE,
        SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WM_DPICHANGED,
        WS_CAPTION, WS_POPUP, WS_THICKFRAME,
    };
    let (hwnd, logical) = cx
        .update_window(window, |_, window, _| {
            hwnd(window).map(|hwnd| (hwnd, window.viewport_size()))
        })
        .map_err(|error| format!("{error:#}"))??;
    let dpi = (96.0 * scale).round() as usize;
    let rect = RECT {
        left: OFF_DESKTOP,
        top: OFF_DESKTOP,
        right: OFF_DESKTOP + (logical.width.as_f32() * scale).round() as i32,
        bottom: OFF_DESKTOP + (logical.height.as_f32() * scale).round() as i32,
    };
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        let frameless = style & !(WS_CAPTION.0 | WS_THICKFRAME.0) | WS_POPUP.0;
        SetWindowLongPtrW(hwnd, GWL_STYLE, frameless as isize);
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
        .map_err(|error| format!("SetWindowPos: {error}"))?;
        SendMessageW(
            hwnd,
            WM_DPICHANGED,
            Some(WPARAM(dpi << 16 | dpi)),
            Some(LPARAM(&rect as *const RECT as isize)),
        );
    }
    Ok(())
}

/// What Studio sets up before its first window: the theme — `theme` by id,
/// or the one saved in Settings, as Studio picks it — and the fonts.
pub fn init(cx: &mut App, theme: Option<&str>) -> String {
    let report = crate::theme::initialize_theme_system();
    let wanted = theme.map(str::to_string).unwrap_or_else(|| {
        crate::settings::SettingsSchema::load_from_disk()
            .appearance
            .theme
    });
    let active = if crate::theme::activate_theme_by_id(&wanted) {
        wanted
    } else {
        report.active_id.to_string()
    };
    crate::assets::register_fonts(cx);
    active
}

/// Every scene, in the order they render.
pub fn scenes() -> Vec<Scene> {
    vec![
        Scene {
            name: "voxtune-editor",
            width: 1280.0,
            height: 720.0,
            open: voxtune_editor,
        },
        Scene {
            name: "voxtune-shape",
            width: 1280.0,
            height: 720.0,
            open: |o, cx| voxtune_scene(o, cx, "panel-shape"),
        },
        Scene {
            name: "voxtune-take",
            width: 1280.0,
            height: 720.0,
            open: |o, cx| voxtune_scene(o, cx, "panel-take"),
        },
        Scene {
            name: "voxtune-pitch-drag",
            width: 1280.0,
            height: 720.0,
            open: |o, cx| voxtune_scene(o, cx, "tool:pitch"),
        },
        Scene {
            name: "voxtune-time-tool",
            width: 1280.0,
            height: 720.0,
            open: |o, cx| voxtune_scene(o, cx, "tool:time"),
        },
        Scene {
            name: "voxtune-cut-tool",
            width: 1280.0,
            height: 720.0,
            open: |o, cx| voxtune_scene(o, cx, "tool:cut"),
        },
        Scene {
            name: "equz8-empty",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: equz8_empty,
        },
        Scene {
            name: "equz8-vocal",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: equz8_vocal,
        },
        Scene {
            name: "equzx-empty",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: equzx_empty,
        },
        Scene {
            name: "equzx-mix",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: equzx_mix,
        },
        Scene {
            name: "zapp",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: zapp_scene,
        },
        Scene {
            name: "zapp-fold",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: zapp_fold_scene,
        },
        Scene {
            name: "zapp-small",
            width: 760.0,
            height: 570.0 + plate::SHELL_CHROME_H,
            open: zapp_scene,
        },
        Scene {
            name: "duckstep",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: duckstep_scene,
        },
        Scene {
            name: "duckstep-shaper",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: duckstep_shaper_scene,
        },
        Scene {
            name: "airboost",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: airboost_scene,
        },
        Scene {
            name: "airboost-bell",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: airboost_bell_scene,
        },
        Scene {
            name: "deesser",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: deesser_scene,
        },
        Scene {
            name: "missing-media",
            width: crate::components::missing_media_window::MISSING_MEDIA_WINDOW_WIDTH,
            height: crate::components::missing_media_window::MISSING_MEDIA_WINDOW_HEIGHT,
            open: missing_media_scene,
        },
        Scene {
            name: "deesser-wide",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: deesser_wide_scene,
        },
        Scene {
            name: "verbspace-hall",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: verbspace_hall,
        },
        Scene {
            name: "verbspace-plate",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: verbspace_plate,
        },
        Scene {
            name: "echospace-default",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: echospace_default,
        },
        Scene {
            name: "echospace-synced",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: echospace_synced,
        },
        Scene {
            name: "echospace-ambient",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: echospace_ambient,
        },
        Scene {
            name: "whitesharp",
            width: WHITESHARP_WINDOW_SIZE.0,
            height: WHITESHARP_WINDOW_SIZE.1,
            open: whitesharp_scene,
        },
        Scene {
            name: "whitesharp-live",
            width: WHITESHARP_WINDOW_SIZE.0,
            height: WHITESHARP_WINDOW_SIZE.1 + WHITESHARP_DOCK_H,
            open: whitesharp_live_scene,
        },
        Scene {
            name: "whitesharp-keys",
            width: WHITESHARP_WINDOW_SIZE.0,
            height: WHITESHARP_WINDOW_SIZE.1 + WHITESHARP_DOCK_H,
            open: whitesharp_keys_scene,
        },
        Scene {
            name: "whitesharp-scale-menu",
            width: WHITESHARP_WINDOW_SIZE.0,
            height: WHITESHARP_WINDOW_SIZE.1,
            open: whitesharp_scale_menu_scene,
        },
        Scene {
            name: "whitesharp-vibrato",
            width: WHITESHARP_WINDOW_SIZE.0,
            height: WHITESHARP_WINDOW_SIZE.1 + WHITESHARP_DOCK_H,
            open: whitesharp_vibrato_scene,
        },
        Scene {
            name: "zcomp",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: zcomp_scene,
        },
        Scene {
            name: "zcomp-bus",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: zcomp_bus_scene,
        },
        Scene {
            name: "burnlimit",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: burnlimit_scene,
        },
        Scene {
            name: "clipper67",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: clipper67_scene,
        },
        Scene {
            name: "clipper67-limit",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: clipper67_limit_scene,
        },
        Scene {
            name: "transient",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: transient_scene,
        },
        Scene {
            name: "transient-snap",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: transient_snap_scene,
        },
        Scene {
            name: "waygate",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: waygate_scene,
        },
        Scene {
            name: "waygate-duck",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: waygate_duck_scene,
        },
        Scene {
            name: "compressor-single",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: compressor_single,
        },
        Scene {
            name: "compressor-multi",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: compressor_multi,
        },
        Scene {
            name: "imager",
            width: crate::components::imager_plate::WINDOW_SIZE.0,
            height: crate::components::imager_plate::WINDOW_SIZE.1,
            open: imager_scene,
        },
        Scene {
            name: "imager-stereoize",
            width: crate::components::imager_plate::WINDOW_SIZE.0,
            height: crate::components::imager_plate::WINDOW_SIZE.1,
            open: imager_stereoize,
        },
        Scene {
            name: "mixstation",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: mixstation_scene,
        },
        Scene {
            name: "midi-input-tree",
            width: 320.0,
            height: 420.0,
            open: midi_input_tree,
        },
        Scene {
            name: "mixstation-empty",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: mixstation_empty,
        },
        Scene {
            name: "rodhareist",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: rodhareist,
        },
        Scene {
            name: "rodhareist-users",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: rodhareist_users,
        },
        Scene {
            name: "rodhareist-lanes",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: rodhareist_lanes,
        },
        Scene {
            name: "rodhareist-explore",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: rodhareist_explore,
        },
        Scene {
            name: "rodhareist-explore-amp",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: rodhareist_explore_amp,
        },
        Scene {
            name: "rodhareist-delay-sync",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: rodhareist_delay_sync,
        },
        Scene {
            name: "rodhareist-footswitches",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: rodhareist_footswitches,
        },
        Scene {
            name: "rodhareist-knob-entry",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: rodhareist_knob_entry,
        },
        Scene {
            name: "rodhareist-tuner",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: rodhareist_tuner,
        },
        Scene {
            name: "rodhareist-tuner-in-tune",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: rodhareist_tuner_in_tune,
        },
        Scene {
            name: "rodhareist-pedalboard",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: rodhareist_pedalboard,
        },
        Scene {
            name: "rodhareist-nam",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: rodhareist_nam,
        },
        Scene {
            name: "rodhareist-stereo",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: rodhareist_stereo,
        },
        Scene {
            name: "rodhareist-settings",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: rodhareist_settings,
        },
        Scene {
            name: "settings-experimental",
            width: crate::components::settings_layout::SETTINGS_WINDOW_WIDTH,
            height: crate::components::settings_layout::SETTINGS_WINDOW_HEIGHT,
            open: settings_experimental,
        },
        Scene {
            name: "settings-audio-shared",
            width: crate::components::settings_layout::SETTINGS_WINDOW_WIDTH,
            height: crate::components::settings_layout::SETTINGS_WINDOW_HEIGHT,
            open: settings_audio_shared,
        },
        Scene {
            name: "wrapsynth",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: wrap_synth,
        },
        Scene {
            name: "wrapsynth-modulation",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: wrap_synth_modulation,
        },
        Scene {
            name: "wrapsynth-voice-fx",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: wrap_synth_voice_fx,
        },
        Scene {
            name: "drum-sampler",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: drum_sampler,
        },
        Scene {
            name: "drum-sampler-kit",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: drum_sampler_kit,
        },
        Scene {
            name: "drum-sampler-mixer",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: drum_sampler_mixer,
        },
        Scene {
            name: "quick-sampler",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: quick_sampler,
        },
        Scene {
            name: "quick-sampler-loop",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: quick_sampler_loop,
        },
        Scene {
            name: "slicer",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: slicer,
        },
        Scene {
            name: "slicer-loop",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: |o, cx| slicer_loop_by(o, cx, slicer::SliceMode::Transient),
        },
        Scene {
            name: "slicer-beat",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: |o, cx| slicer_loop_by(o, cx, slicer::SliceMode::Beat),
        },
        Scene {
            name: "mixer",
            width: 1_680.0,
            height: 760.0,
            open: mixer::mixer,
        },
        Scene {
            name: "midi-tools",
            width: 1_200.0,
            height: 760.0,
            open: midi_tools_scene,
        },
        Scene {
            name: "midi-stretch",
            width: 1_200.0,
            height: 760.0,
            open: midi_stretch_scene,
        },
        Scene {
            name: "midi-stretch-idle",
            width: 1_200.0,
            height: 760.0,
            open: midi_stretch_idle_scene,
        },
        Scene {
            name: "midi-drums",
            width: 1_200.0,
            height: 760.0,
            open: midi_drums_scene,
        },
        Scene {
            name: "midi-drums-used",
            width: 1_200.0,
            height: 760.0,
            open: midi_drums_used_scene,
        },
        Scene {
            name: "midi-tools-pitch",
            width: 1_200.0,
            height: 760.0,
            open: midi_tools_pitch_scene,
        },
        Scene {
            name: "midi-tools-perform",
            width: 1_200.0,
            height: 760.0,
            open: midi_tools_perform_scene,
        },
        Scene {
            name: "midi-tool-strum",
            width: crate::components::piano_roll::advanced_tools::AdvancedTool::Strum
                .window_size()
                .0,
            height: crate::components::piano_roll::advanced_tools::AdvancedTool::Strum
                .window_size()
                .1,
            open: midi_tool_strum_scene,
        },
        Scene {
            name: "midi-tool-humanize",
            width: crate::components::piano_roll::advanced_tools::AdvancedTool::Humanize
                .window_size()
                .0,
            height: crate::components::piano_roll::advanced_tools::AdvancedTool::Humanize
                .window_size()
                .1,
            open: midi_tool_humanize_scene,
        },
        Scene {
            name: "midi-tool-length",
            width: crate::components::piano_roll::advanced_tools::AdvancedTool::Length
                .window_size()
                .0,
            height: crate::components::piano_roll::advanced_tools::AdvancedTool::Length
                .window_size()
                .1,
            open: midi_tool_length_scene,
        },
        Scene {
            name: "midi-tool-velocity",
            width: crate::components::piano_roll::advanced_tools::AdvancedTool::Velocity
                .window_size()
                .0,
            height: crate::components::piano_roll::advanced_tools::AdvancedTool::Velocity
                .window_size()
                .1,
            open: midi_tool_velocity_scene,
        },
        Scene {
            name: "midi-tool-cleanup",
            width: crate::components::piano_roll::advanced_tools::AdvancedTool::CleanUp
                .window_size()
                .0,
            height: crate::components::piano_roll::advanced_tools::AdvancedTool::CleanUp
                .window_size()
                .1,
            open: midi_tool_cleanup_scene,
        },
        Scene {
            name: "inspector",
            width: crate::components::panel::INSPECTOR_WIDTH,
            height: 1_120.0,
            open: inspector_scene,
        },
        Scene {
            name: "inspector-folded",
            width: crate::components::panel::INSPECTOR_WIDTH,
            height: 560.0,
            open: inspector_folded_scene,
        },
        Scene {
            name: "arrangement-meters",
            width: 1_100.0,
            height: 520.0,
            open: arrangement_meters_scene,
        },
        Scene {
            name: "ara-notes",
            width: 1_100.0,
            height: 520.0,
            open: ara_notes_scene,
        },
        Scene {
            name: "system-overloaded",
            width: crate::components::message_box_dialog::MESSAGE_BOX_WIDTH,
            height: 380.0,
            open: system_overloaded_scene,
        },
        Scene {
            name: "automation",
            width: 1_100.0,
            height: 560.0,
            open: automation_scene,
        },
        Scene {
            name: "master-automation",
            width: 1_100.0,
            height: 560.0,
            open: master_automation_scene,
        },
        Scene {
            name: "automation-value-edit",
            width: 1_100.0,
            height: 420.0,
            open: automation_value_edit_scene,
        },
        Scene {
            name: "tool-bars",
            width: 1_100.0,
            height: 220.0,
            open: tool_bars_scene,
        },
        Scene {
            name: "icons",
            width: 980.0,
            height: 580.0,
            open: icons_scene,
        },
        Scene {
            name: "jam",
            width: crate::components::jam_window::JAM_WINDOW_WIDTH,
            height: crate::components::jam_window::JAM_WINDOW_HEIGHT,
            open: jam_idle_scene,
        },
        Scene {
            name: "jam-live",
            width: crate::components::jam_window::JAM_WINDOW_WIDTH,
            height: 1_180.0,
            open: jam_live_scene,
        },
        Scene {
            name: "audio-connections",
            width: 1_040.0,
            height: 460.0,
            open: audio_connections_scene,
        },
        Scene {
            name: "welcome",
            width: 1_180.0,
            height: 760.0,
            open: welcome_scene,
        },
        Scene {
            name: "process-explorer",
            width: 920.0,
            height: 560.0,
            open: process_explorer_scene,
        },
        Scene {
            name: "background-tasks",
            width: 520.0,
            height: 520.0,
            open: background_tasks_scene,
        },
        Scene {
            name: "virtual-keyboard",
            width: 760.0,
            height: 236.0,
            open: virtual_keyboard_scene,
        },
        Scene {
            name: "audio-connections-empty",
            width: 1_040.0,
            height: 460.0,
            open: audio_connections_empty_scene,
        },
        Scene {
            name: "audio-clips",
            width: 1_100.0,
            height: 260.0,
            open: audio_clips_scene,
        },
        Scene {
            name: "mixer-wide",
            width: 1_880.0,
            height: 760.0,
            open: mixer::mixer_wide,
        },
        Scene {
            name: "mixer-compact",
            width: 1_100.0,
            height: 560.0,
            open: mixer::mixer_compact,
        },
        Scene {
            name: "mixer-short",
            width: 1_100.0,
            height: 460.0,
            open: mixer::mixer,
        },
    ]
}

mod mixer;

// ── Plug-in editor plumbing ────────────────────────────────────────────────

fn key(insert: &str) -> PluginInstanceKey {
    PluginInstanceKey {
        track_id: "preview-track".to_string(),
        insert_id: format!("preview-{insert}"),
    }
}

fn identity(plugin: &str) -> ShellIdentity {
    ShellIdentity {
        plugin_name: plugin.to_string(),
        track_name: "Lead Vocal".to_string(),
        insert_number: 2,
        // So the strip shows the preset menu, as it does in Studio.
        plugin_id: format!("builtin:{}", plugin.to_lowercase()),
        preset_plugin_name: plugin.to_string(),
    }
}

fn no_close() -> Arc<dyn Fn(&mut Window, &mut App)> {
    Arc::new(|_, _| {})
}

/// A still analyser frame shaped like a mix: flat lows, a gentle tilt
/// down, a few resonances — synthetic, for previews only.
fn preview_spectrum() -> [f32; SpherePluginHost::spectrum::SPECTRUM_BINS] {
    use SpherePluginHost::spectrum::{MAX_HZ, MIN_HZ, SPECTRUM_BINS};
    std::array::from_fn(|i| {
        let hz = MIN_HZ * (MAX_HZ / MIN_HZ).powf((i as f32 + 0.5) / SPECTRUM_BINS as f32);
        let octaves_up = (hz / 150.0).log2().max(0.0);
        let bump =
            |centre: f32, width: f32, db: f32| db * (-((hz / centre).log2() / width).powi(2)).exp();
        let low_roll = if hz < 40.0 {
            -(40.0 / hz).log2() * 12.0
        } else {
            0.0
        };
        -28.0 - 4.2 * octaves_up
            + low_roll
            + bump(110.0, 0.35, 6.0)
            + bump(2_800.0, 0.5, 4.0)
            + bump(7_000.0, 0.3, 3.0)
            + (i as f32 * 1.7).sin() * 1.2
    })
}

/// Host ops for a preview: no host behind them, a fixed sample rate and the
/// still analyser frame above.
fn preview_host_ops() -> BuiltinEditorHostOps {
    BuiltinEditorHostOps {
        host_status_source: Some(Arc::new(|_| Some((48_000, 512, 0, 120.0)))),
        spectrum_source: Some(Arc::new(|_| Some((1, preview_spectrum())))),
        ..Default::default()
    }
}

/// Puts `params` in Studio's state mirror for the preview insert, where
/// the editor reads them on open.
fn seed(plugin: &str, insert: &str, json: String) {
    builtin_state_seed(plugin, &key(insert).insert_id, json.as_bytes());
}

// ── WhiteSharp ─────────────────────────────────────────────────────────────

/// A project opened on another machine: a CLAP the scan found under another
/// folder, takes on a drive that is not there, one take already relinked,
/// and a plug-in that is there but would not load.
fn missing_media_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    use crate::components::missing_media_window::{
        MissingKind, MissingMediaCallbacks, MissingMediaWindow, MissingRow, MissingStatus,
    };
    let row =
        |id: &str, kind, name: &str, detail: &str, used_on: &str, path: &str, status| MissingRow {
            id: id.to_string(),
            kind,
            name: name.to_string(),
            detail: detail.to_string(),
            used_on: used_on.to_string(),
            path: path.to_string(),
            status,
            installed_at: None,
        };
    let rows = vec![
        MissingRow {
            installed_at: Some(
                r"C:\Program Files\Common Files\CLAP\FabFilter Saturn 2.clap".to_string(),
            ),
            ..row(
                "plugin:saturn",
                MissingKind::Plugin,
                "Saturn 2",
                "CLAP",
                "har b",
                r"C:\Program Files\Common Files\CLAP\FabFilter\FabFilter Saturn 2.clap",
                MissingStatus::Missing,
            )
        },
        row(
            "plugin:kontakt",
            MissingKind::Plugin,
            "Kontakt 8",
            "VST3",
            "guitar strum",
            r"C:\Program Files\Common Files\VST3\Kontakt 8.vst3",
            MissingStatus::Failed("The plug-in refused the sample rate".to_string()),
        ),
        row(
            "audio:vocal",
            MissingKind::Audio,
            "Vocal Take 3.wav",
            "WAV",
            "main, har · 4 clips",
            r"D:\Sessions\011169\Recordings\Vocal Take 3.wav",
            MissingStatus::Missing,
        ),
        row(
            "audio:ad",
            MissingKind::Audio,
            "ad lib.wav",
            "WAV",
            "ad",
            r"D:\Sessions\011169\Recordings\ad lib.wav",
            MissingStatus::Missing,
        ),
        row(
            "audio:guitar",
            MissingKind::Audio,
            "Guitar DI.flac",
            "FLAC",
            "Audio Track 1",
            r"C:\Users\Studio\Music\011169\Guitar DI.flac",
            MissingStatus::Found,
        ),
    ];
    let collect_rows = rows.clone();
    let callbacks = MissingMediaCallbacks {
        collect: Arc::new(move |_cx: &mut App| collect_rows.clone()),
        relocate: Arc::new(|_row: &str, _window: &mut Window, _cx: &mut App| {}),
        use_installed: Arc::new(|_row: &str, _window: &mut Window, _cx: &mut App| {}),
        search_folder: Arc::new(|_window: &mut Window, _cx: &mut App| {}),
    };
    let notes = vec!["Timed out restoring Kontakt 8 on guitar strum after 120s".to_string()];
    let handle = cx.open_window(options, move |_, cx| {
        cx.new(|cx| MissingMediaWindow::new(rows, callbacks, notes, cx))
    })?;
    Ok(handle.into())
}

fn whitesharp_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    whitesharp_window(
        options,
        cx,
        "whitesharp",
        whitesharp::LatencyMode::Quality,
        None,
    )
}

/// The same insert on the live path, with the Voice dock open: no added
/// delay, formant controls unavailable.
fn whitesharp_live_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    whitesharp_window(
        options,
        cx,
        "whitesharp-live",
        whitesharp::LatencyMode::Live,
        Some(Dock::Voice),
    )
}

fn whitesharp_keys_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    whitesharp_window(
        options,
        cx,
        "whitesharp-keys",
        whitesharp::LatencyMode::Quality,
        Some(Dock::Keys),
    )
}

/// The scale menu open, on the plate's own key: each scale with its notes.
fn whitesharp_scale_menu_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    let handle = whitesharp_window(
        options,
        cx,
        "whitesharp-scale-menu",
        whitesharp::LatencyMode::Quality,
        None,
    )?;
    handle.update(cx, |view, _, cx| {
        if let Ok(window) = view.downcast::<WhiteSharpWindow>() {
            window.update(cx, |window, cx| {
                window.editor().update(cx, |editor, cx| {
                    editor.toggle_menu(crate::components::white_sharp_window::Menu::Scale, cx)
                })
            });
        }
    })?;
    Ok(handle)
}

fn whitesharp_vibrato_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    whitesharp_window(
        options,
        cx,
        "whitesharp-vibrato",
        whitesharp::LatencyMode::Quality,
        Some(Dock::Vibrato),
    )
}

/// `insert` is the scene's own: the state mirror keeps the first state
/// seeded for an insert.
fn whitesharp_window(
    options: WindowOptions,
    cx: &mut App,
    insert: &'static str,
    latency: whitesharp::LatencyMode,
    dock: Option<Dock>,
) -> Result<AnyWindowHandle> {
    let mut params = whitesharp::default_params();
    params.latency = latency;
    params.key = 2;
    params.scale = whitesharp::Scale::Major;
    params.retune_ms = 12.0;
    params.humanize = 25.0;
    params.input_type = whitesharp::InputType::Soprano;
    params.bypass_mask = 1 << 11; // B
    params.remove_mask = 1 << 4; // E
    params.vibrato_shape = whitesharp::VibratoShape::Sine;
    if let Ok(json) = whitesharp::ipc::WhiteSharpState::new(params).to_json() {
        seed("whitesharp", insert, json);
    }
    let handle = cx.open_window(options, move |_, cx| {
        cx.new(|cx| {
            let window = WhiteSharpWindow::new(
                key(insert),
                identity("WhiteSharp"),
                preview_host_ops(),
                no_close(),
                cx,
            );
            // A voice 22 cents sharp of A, being pulled down onto it —
            // synthetic, for previews only.
            window
                .live()
                .borrow_mut()
                .show(whitesharp::telemetry::Reading {
                    input: Some(69.22),
                    output: Some(69.0),
                    target: Some(69),
                });
            window.editor().update(cx, |editor, _| editor.dock = dock);
            window
        })
    })?;
    Ok(handle.into())
}

// ── Dynamics, band and rack scenes ────────────────────────────────────────

/// A meter reading at telemetry frame `t`: a phrase swelling and easing
/// every few seconds, with up to `reduction_db` taken off at its loudest —
/// synthetic, for previews only.
fn preview_frame(t: u32, reduction_db: f32) -> SpherePluginHost::audio_bridge::BuiltinMeterFrame {
    let phase = t as f32 * 0.09;
    let swell = 0.5 + 0.5 * phase.sin() * (phase * 0.37).cos();
    let hit = if t % 11 == 0 { 0.25 } else { 0.0 };
    let level = |db: f32| 10f32.powf(db / 20.0);
    let in_db = -22.0 + 16.0 * swell + hit * 8.0;
    let reduction = (reduction_db * (swell + hit).min(1.0)).max(0.0);
    let out_db = in_db - reduction + reduction_db * 0.25;
    SpherePluginHost::audio_bridge::BuiltinMeterFrame {
        in_peak: level(in_db),
        in_rms: level(in_db - 8.0),
        out_peak: level(out_db),
        out_rms: level(out_db - 8.0),
        gain_reduction_db: reduction,
        slot_in_peak: [0.18, 0.2, 0.22, 0.16, 0.2, 0.24],
        slot_out_peak: [0.2, 0.22, 0.16, 0.2, 0.24, 0.3],
        ..Default::default()
    }
}

/// A stereo image like a mix: a strong centre and some width.
fn preview_image() -> SpherePluginHost::audio_bridge::StereoImageFrame {
    use SpherePluginHost::audio_bridge::IMAGE_SCOPE_POINTS;
    let mut frame = SpherePluginHost::audio_bridge::StereoImageFrame {
        correlation: 0.62,
        band_correlation: [0.96, 0.71, 0.48, 0.33],
        band_level: [0.2, 0.15, 0.08, 0.03],
        ..Default::default()
    };
    for i in 0..IMAGE_SCOPE_POINTS {
        let a = i as f32 * 0.37;
        let mid = 0.18 * a.sin() + 0.05 * (a * 3.1).sin();
        let side = 0.07 * (a * 1.7).cos();
        frame.scope[2 * i] = mid + side;
        frame.scope[2 * i + 1] = mid - side;
    }
    frame
}

/// Band reductions like a busy mix bus.
const PREVIEW_BAND_REDUCTION: [f32; 4] = [2.5, 4.0, 1.2, 3.1];

/// Host ops whose meters move with [`preview_frame`].
fn kit_host_ops(reduction_db: f32) -> BuiltinEditorHostOps {
    use std::sync::atomic::{AtomicU32, Ordering};
    static TICK: AtomicU32 = AtomicU32::new(300);
    BuiltinEditorHostOps {
        meter_source: Some(Arc::new(move |_| {
            Some(preview_frame(
                TICK.fetch_add(1, Ordering::Relaxed),
                reduction_db,
            ))
        })),
        stereo_image_source: Some(Arc::new(|_| Some((1, preview_image())))),
        band_reduction_source: Some(Arc::new(|_| Some((1, PREVIEW_BAND_REDUCTION)))),
        ..preview_host_ops()
    }
}

/// Seeds `json` for `plugin` and opens `model`'s editor on it, with ten
/// seconds of history already drawn.
#[allow(clippy::too_many_arguments)]
fn open_kit<M: KitModel>(
    options: WindowOptions,
    cx: &mut App,
    model: M,
    plugin: &'static str,
    insert: &'static str,
    json: std::result::Result<String, serde_json::Error>,
    reduction_db: f32,
    then: impl FnOnce(&mut crate::components::plugin_kit::KitEditor<M>) + 'static,
) -> Result<AnyWindowHandle> {
    if let Ok(json) = json {
        seed(plugin, insert, json);
    }
    let handle = cx.open_window(options, move |_, cx| {
        cx.new(|cx| {
            let window = KitWindow::new(
                model,
                key(insert),
                identity(model.title()),
                kit_host_ops(reduction_db),
                no_close(),
                cx,
            );
            {
                let mut live = window.live().borrow_mut();
                for t in 0..300 {
                    live.show(preview_frame(t, reduction_db));
                }
                live.show_image(preview_image());
                live.show_bands(Some(preview_spectrum()), Some(PREVIEW_BAND_REDUCTION));
            }
            window.editor().update(cx, |editor, cx| {
                then(editor);
                cx.notify();
            });
            window
        })
    })?;
    Ok(handle.into())
}

fn dyn_preset(kind: DynKind, name: &str) -> crate::components::dyn_model::DynParams {
    crate::components::dyn_model::presets(kind)
        .iter()
        .find(|preset| preset.name == name)
        .map(|preset| preset.params.clone())
        .unwrap_or_else(|| crate::components::dyn_model::DynParams::defaults(kind))
}

fn dyn_json(
    params: &crate::components::dyn_model::DynParams,
) -> std::result::Result<String, serde_json::Error> {
    use crate::components::dyn_model::DynParams;
    match params.clone() {
        DynParams::Zcomp(p) => zcomp::ipc::ZcompState::new(p).to_json(),
        DynParams::Clipper(p) => clipper67::ipc::Clipper67State::new(p).to_json(),
        DynParams::Transient(p) => transient::ipc::TransientState::new(p).to_json(),
    }
}

/// Ten seconds of meter frames from the real DSP at `params`, run over
/// [`preview_signal`] as the host would meter it.
fn dyn_frames(
    params: &crate::components::dyn_model::DynParams,
) -> Vec<SpherePluginHost::audio_bridge::BuiltinMeterFrame> {
    use crate::components::dyn_model::DynParams;
    use waygate::StereoEffect;
    macro_rules! frames {
        ($dsp:path, $params:expr) => {{
            let dsp = std::cell::RefCell::new(<$dsp>::new(48_000.0));
            dsp.borrow_mut().set_params($params.clone());
            run_preview(
                |l, r| dsp.borrow_mut().process_stereo(l, r),
                || {
                    let f = dsp.borrow().meter_frame();
                    SpherePluginHost::audio_bridge::BuiltinMeterFrame {
                        in_peak: f.in_peak,
                        in_rms: f.in_rms,
                        out_peak: f.out_peak,
                        out_rms: f.out_rms,
                        gain_reduction_db: f.gain_reduction_db,
                        in_clip: f.in_clip,
                        out_clip: f.out_clip,
                        ..Default::default()
                    }
                },
            )
        }};
    }
    match params {
        DynParams::Zcomp(p) => frames!(zcomp::Dsp, p),
        DynParams::Clipper(p) => frames!(clipper67::Dsp, p),
        DynParams::Transient(p) => frames!(transient::Dsp, p),
    }
}

/// Opens `kind`'s plate on factory preset `preset` with `edits` made, its
/// history and meters filled from a run of the real DSP.
fn open_dyn(
    options: WindowOptions,
    cx: &mut App,
    kind: DynKind,
    insert: &'static str,
    preset: &str,
    edits: &[(&str, f32)],
    then: impl FnOnce(&mut crate::components::plugin_kit::KitEditor<DynKind>) + 'static,
) -> Result<AnyWindowHandle> {
    let params = edits
        .iter()
        .fold(dyn_preset(kind, preset), |params, (id, value)| {
            params.with(id, *value)
        });
    let frames = dyn_frames(&params);
    open_plate(
        options,
        cx,
        kind,
        kind.key(),
        insert,
        dyn_json(&params),
        frames,
        [0.0; SpherePluginHost::audio_bridge::BUILTIN_PAD_SLOTS],
        then,
    )
}

fn zcomp_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_dyn(
        options,
        cx,
        DynKind::Zcomp,
        "zcomp",
        "Bass Glue",
        &[],
        |_| {},
    )
}

fn zcomp_bus_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_dyn(
        options,
        cx,
        DynKind::Zcomp,
        "zcomp-bus",
        "Bus Soft",
        &[],
        |editor| editor.ui.vu_output = true,
    )
}

fn clipper67_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_dyn(
        options,
        cx,
        DynKind::Clipper,
        "clipper67",
        "Aggressive",
        &[],
        |_| {},
    )
}

fn clipper67_limit_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_dyn(
        options,
        cx,
        DynKind::Clipper,
        "clipper67-limit",
        "Brick Limit",
        // Pushed, so the limiter has peaks to hold.
        &[("inputDb", 6.0)],
        |_| {},
    )
}

fn transient_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_dyn(
        options,
        cx,
        DynKind::Transient,
        "transient",
        "Punch Up",
        &[],
        |_| {},
    )
}

fn transient_snap_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_dyn(
        options,
        cx,
        DynKind::Transient,
        "transient-snap",
        "Snap Cut",
        &[],
        |_| {},
    )
}

/// Ten seconds of WayGate telemetry, measured by running the real DSP at
/// `params` over a synthetic tom track: a hit every 0.75 s, ringing out,
/// over a bed of kit bleed. One frame per ~33 ms block, as the host
/// publishes them.
fn waygate_frames(
    params: &waygate::Params,
) -> Arc<Vec<SpherePluginHost::audio_bridge::BuiltinMeterFrame>> {
    use waygate::StereoEffect;
    const SR: f32 = 48_000.0;
    const BLOCK: usize = 1_600;
    let mut dsp = waygate::Dsp::new(SR);
    dsp.set_params(params.clone());
    let mut seed = 0x2545_f491u32;
    let mut noise = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed as f32 / u32::MAX as f32 * 2.0 - 1.0
    };
    let mut frames = Vec::with_capacity(300);
    for block in 0..300 {
        for n in 0..BLOCK {
            let i = block * BLOCK + n;
            let since_hit = (i % 36_000) as f32 / SR;
            // Every fourth "hit" is the next tom's bleed: loud enough to sit
            // in the hysteresis band, too quiet to open the gate.
            let velocity = if (i / 36_000) % 4 == 3 { 0.022 } else { 0.7 };
            let tom = velocity
                * (-since_hit / 0.07).exp()
                * (std::f32::consts::TAU * 110.0 * since_hit).sin();
            let bleed = 0.006 * noise();
            let x = tom + bleed;
            dsp.process_stereo(x, x);
        }
        let f = dsp.meter_frame();
        let (slot_in_peak, slot_out_peak) = f.rack_slots();
        frames.push(SpherePluginHost::audio_bridge::BuiltinMeterFrame {
            in_peak: f.in_peak,
            in_rms: f.in_rms,
            out_peak: f.out_peak,
            out_rms: f.out_rms,
            gain_reduction_db: f.gain_reduction_db,
            in_clip: f.in_clip,
            out_clip: f.out_clip,
            slot_in_peak,
            slot_out_peak,
        });
    }
    Arc::new(frames)
}

fn open_waygate(
    options: WindowOptions,
    cx: &mut App,
    insert: &'static str,
    params: waygate::Params,
) -> Result<AnyWindowHandle> {
    use crate::components::waygate_plate::WayGatePlate;
    let frames = waygate_frames(&params).as_ref().clone();
    let json = waygate::ipc::WayGateState::new(params).to_json();
    open_plate(
        options,
        cx,
        WayGatePlate,
        "waygate",
        insert,
        json,
        frames,
        [0.0; SpherePluginHost::audio_bridge::BUILTIN_PAD_SLOTS],
        |_| {},
    )
}

fn waygate_preset(name: &str) -> waygate::Params {
    waygate::factory_presets()
        .into_iter()
        .find(|preset| preset.name == name)
        .map_or_else(waygate::default_params, |preset| preset.params)
}

fn waygate_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_waygate(options, cx, "waygate", waygate_preset("Tom"))
}

fn waygate_duck_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    let params = waygate::Params {
        mode: waygate::Mode::Duck,
        range_db: -12.0,
        ..waygate_preset("Vocal")
    };
    open_waygate(options, cx, "waygate-duck", params)
}

fn band_preset(kind: BandKind, name: &str) -> crate::components::band_model::BandParams {
    crate::components::band_model::presets(kind)
        .iter()
        .find(|preset| preset.name == name)
        .map(|preset| preset.params.clone())
        .unwrap_or_else(|| crate::components::band_model::BandParams::defaults(kind))
}

/// The Compressor over [`preview_signal`], read as the host would: its meter
/// frames, and each band's reduction where the run ends.
fn compressor_telemetry(
    params: &compresser::Params,
) -> (
    Vec<SpherePluginHost::audio_bridge::BuiltinMeterFrame>,
    [f32; SpherePluginHost::audio_bridge::REDUCTION_BANDS],
) {
    use waygate::StereoEffect;
    let dsp = std::cell::RefCell::new(compresser::Dsp::new(48_000.0));
    dsp.borrow_mut().set_params(params.clone());
    let frames = run_preview(
        |l, r| dsp.borrow_mut().process_stereo(l, r),
        || {
            let f = dsp.borrow().meter_frame();
            SpherePluginHost::audio_bridge::BuiltinMeterFrame {
                in_peak: f.in_peak,
                in_rms: f.in_rms,
                out_peak: f.out_peak,
                out_rms: f.out_rms,
                gain_reduction_db: f.gain_reduction_db,
                in_clip: f.in_clip,
                out_clip: f.out_clip,
                ..Default::default()
            }
        },
    );
    let reduction = dsp.borrow().band_reduction_db();
    (frames, reduction)
}

fn open_band(
    options: WindowOptions,
    cx: &mut App,
    kind: BandKind,
    insert: &'static str,
    preset: &str,
) -> Result<AnyWindowHandle> {
    use crate::components::band_model::BandParams;
    let BandParams::Comp(params) = band_preset(kind, preset);
    let (frames, reduction) = compressor_telemetry(&params);
    let json = compresser::ipc::CompresserState::new(params).to_json();
    let handle = open_plate(
        options,
        cx,
        kind,
        "compresser",
        insert,
        json,
        frames,
        [0.0; SpherePluginHost::audio_bridge::BUILTIN_PAD_SLOTS],
        |_| {},
    )?;
    if let Some(window) = handle.downcast::<KitWindow<BandKind>>() {
        let _ = window.update(cx, |window, _, _| {
            window
                .live()
                .borrow_mut()
                .show_bands(Some(preview_spectrum()), Some(reduction))
        });
    }
    Ok(handle)
}

fn compressor_single(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_band(
        options,
        cx,
        BandKind::Comp,
        "compressor-single",
        "Vocal Smooth",
    )
}

fn compressor_multi(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_band(
        options,
        cx,
        BandKind::Comp,
        "compressor-multi",
        "Multiband Master",
    )
}

/// A side spectrum to go with [`preview_spectrum`]: quieter, and thinner
/// toward the lows the way a mix's side is — synthetic, for previews only.
fn preview_side_block() -> PreviewBlock {
    use SpherePluginHost::spectrum::{MAX_HZ, MIN_HZ, SPECTRUM_BINS};
    let mid = preview_spectrum();
    let side: [f32; SPECTRUM_BINS] = std::array::from_fn(|i| {
        let hz = MIN_HZ * (MAX_HZ / MIN_HZ).powf((i as f32 + 0.5) / SPECTRUM_BINS as f32);
        let thin = (300.0 / hz).log2().max(0.0) * 7.0;
        mid[i] - 14.0 - thin
    });
    imager::telemetry::side_block(&side)
}

fn open_imager(
    options: WindowOptions,
    cx: &mut App,
    insert: &'static str,
    params: imager::Params,
    then: impl FnOnce(&mut crate::components::imager_plate::ImagerUi) + 'static,
) -> Result<AnyWindowHandle> {
    use crate::components::imager_plate::ImagerPlate;
    crate::components::plate::ensure_fonts(cx);
    let json = imager::ipc::ImagerState::new(params).to_json();
    let handle = open_kit(
        options,
        cx,
        ImagerPlate,
        "imager",
        insert,
        json,
        0.0,
        move |editor| then(&mut editor.ui),
    )?;
    if let Some(window) = handle.downcast::<KitWindow<ImagerPlate>>() {
        let side = preview_side_block();
        let _ = window.update(cx, |window, _, _| {
            window.live().borrow_mut().show_pads(side)
        });
    }
    Ok(handle)
}

fn imager_preset(name: &str) -> imager::Params {
    imager::factory_presets()
        .into_iter()
        .find(|preset| preset.name == name)
        .map_or_else(imager::default_params, |preset| preset.params)
}

fn imager_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    let params = imager::Params {
        width: [40.0, 100.0, 135.0, 170.0],
        mono_bass: true,
        ..imager::default_params()
    };
    open_imager(options, cx, "imager", params, |ui| ui.selected = 2)
}

fn imager_stereoize(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    let mut params = imager_preset("Natural Space");
    params.global_width = 15.0;
    params.band_bypass[0] = true;
    params.solo_band = 3;
    open_imager(options, cx, "imager-stereoize", params, |ui| {
        ui.page = crate::components::imager_plate::Page::Stereoize;
        ui.selected = 3;
    })
}

fn open_mix(
    options: WindowOptions,
    cx: &mut App,
    insert: &'static str,
    preset: &str,
    select: Option<u8>,
) -> Result<AnyWindowHandle> {
    let params = crate::components::mix_station_model::presets()
        .iter()
        .find(|p| p.name == preset)
        .map(|p| p.params.clone())
        .unwrap_or_else(mixstation::default_params);
    let frames = mixstation_telemetry(&params);
    let json = mixstation::ipc::MixStationState::new(params).to_json();
    open_plate(
        options,
        cx,
        crate::components::mixstation_plate::MixStationPlate,
        "mixstation",
        insert,
        json,
        frames,
        [0.0; SpherePluginHost::audio_bridge::BUILTIN_PAD_SLOTS],
        move |editor| editor.ui.selected = select,
    )
}

/// MixStation over [`preview_signal`], read as the host would: its meter
/// frames, each rack position's levels in the rack blocks.
fn mixstation_telemetry(
    params: &mixstation::Params,
) -> Vec<SpherePluginHost::audio_bridge::BuiltinMeterFrame> {
    use waygate::StereoEffect;
    let dsp = std::cell::RefCell::new(mixstation::Dsp::new(48_000.0));
    dsp.borrow_mut().set_params(params.clone());
    run_preview(
        |l, r| dsp.borrow_mut().process_stereo(l, r),
        || {
            let f = dsp.borrow().meter_frame();
            SpherePluginHost::audio_bridge::BuiltinMeterFrame {
                in_peak: f.in_peak,
                in_rms: f.in_rms,
                out_peak: f.out_peak,
                out_rms: f.out_rms,
                gain_reduction_db: f.gain_reduction_db,
                in_clip: f.in_clip,
                out_clip: f.out_clip,
                slot_in_peak: f.slot_in_peak,
                slot_out_peak: f.slot_out_peak,
            }
        },
    )
}

fn mixstation_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_mix(options, cx, "mixstation", "Mix Bus Polish", Some(2))
}

fn mixstation_empty(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_mix(options, cx, "mixstation-empty", "Empty Rack", None)
}

// ── Inspector MIDI Input menu ───────────────────────────────────────────────

// ── Audio clips ─────────────────────────────────────────────────────────────

/// One track's audio clips at three row heights, as the arrangement draws
/// them: a selected take with fades, a copy raised 3 dB, a short one cut
/// 6 dB, and a breath too narrow for a label. The waveform is a synthetic
/// phrase installed in the peak cache, so no file is read.
struct AudioClipsPreview {
    state: crate::components::timeline::timeline_state::TimelineState,
}

const AUDIO_CLIP_ROWS: [f32; 3] = [96.0, 64.0, 40.0];

impl AudioClipsPreview {
    fn new() -> Self {
        use crate::components::timeline::timeline_state::{
            AudioClipStretchState, AudioImportState, ClipState, ClipType, TimelineState,
        };
        use crate::components::timeline::waveform_cache::{
            install_ready, WaveformLod, WaveformPeak, WaveformPreview, LOD_LEVELS,
        };
        const RATE: u32 = 48_000;
        const SECONDS: f64 = 24.0;
        let frames = (SECONDS * RATE as f64) as u64;

        // A sung phrase: syllables swelling and dying away, with breaths.
        let fine: Vec<WaveformPeak> = (0..(frames as usize).div_ceil(LOD_LEVELS[0]))
            .map(|i| {
                let t = i as f64 * LOD_LEVELS[0] as f64 / RATE as f64;
                let syllable = (t * 1.7).fract();
                let swell = (syllable * std::f64::consts::PI).sin().powf(0.6);
                let phrase = 0.55 + 0.45 * (t * 0.31).sin().abs();
                let texture = 0.75 + 0.25 * ((i as f64 * 12.9898).sin() * 43_758.545).fract();
                let level = (swell * phrase * texture * 0.8) as f32;
                WaveformPeak {
                    min: -level,
                    max: level * 0.92,
                }
            })
            .collect();
        let mut lods = vec![WaveformLod {
            samples_per_peak: LOD_LEVELS[0],
            peaks: fine,
        }];
        for &spp in &LOD_LEVELS[1..] {
            let peaks = lods
                .last()
                .map(|lod| {
                    lod.peaks
                        .chunks(2)
                        .map(|pair| WaveformPeak {
                            min: pair.iter().map(|p| p.min).fold(0.0, f32::min),
                            max: pair.iter().map(|p| p.max).fold(0.0, f32::max),
                        })
                        .collect()
                })
                .unwrap_or_default();
            lods.push(WaveformLod {
                samples_per_peak: spp,
                peaks,
            });
        }
        install_ready(
            "preview-vocal",
            Arc::new(WaveformPreview {
                sample_rate: RATE,
                channels: 1,
                duration_seconds: SECONDS,
                total_frames: frames,
                lods,
            }),
        );

        let mut state = TimelineState::default();
        state.tracks.clear();
        state.viewport.viewport_width = 1_100.0;
        state.viewport.pixels_per_second = 64.0;
        state.sync_pixels_per_beat();
        let clip = |id: &str, name: &str, start: f32, beats: f32, offset: f32, db: f32| {
            let mut clip = ClipState {
                id: id.to_string(),
                name: name.to_string(),
                start_beat: start,
                duration_beats: beats,
                source_duration_seconds: Some(SECONDS),
                offset_beats: offset,
                gain: 10f32.powf(db / 20.0),
                clip_type: ClipType::Audio {
                    file_id: "preview-vocal".to_string(),
                    source_path: Some("preview-vocal".to_string()),
                },
                muted: false,
                audio_import: AudioImportState::Ready,
                stretch: AudioClipStretchState::default(),
            };
            clip.stretch.original_sample_rate = RATE;
            clip.stretch.project_sample_rate = RATE;
            clip.stretch.original_duration_samples = frames;
            let beat_frames = RATE as f32 * 0.5;
            clip.stretch.source_start_samples = (offset * beat_frames) as u64;
            clip.stretch.source_end_samples = ((offset + beats) * beat_frames) as u64;
            clip
        };
        let mut take = clip("clip-1", "Lead Vocal", 0.5, 14.0, 0.0, 0.0);
        take.stretch.fade_in_ms = 400.0;
        take.stretch.fade_out_ms = 900.0;
        let clips = vec![
            take,
            clip("clip-2", "Lead Vocal Copy", 15.0, 10.0, 14.0, 3.0),
            clip("clip-3", "Har b", 26.0, 4.0, 4.0, -6.0),
            clip("clip-4", "Breath", 30.5, 0.75, 9.0, 0.0),
        ];
        for index in 0..AUDIO_CLIP_ROWS.len() {
            use crate::components::timeline::timeline_state::{
                CreateTrackOptions, InputMonitorMode, TrackType,
            };
            let id = state.create_track(CreateTrackOptions {
                track_type: TrackType::Audio,
                name: "Vocal".to_string(),
                color: crate::theme::Colors::track_color_for_index(index * 2 + 1),
                volume: 1.0,
                pan: 0.0,
                armed: false,
                input_monitor: InputMonitorMode::Off,
            });
            let Some(track) = state.tracks.iter_mut().find(|track| track.id == id) else {
                continue;
            };
            track.clips = clips
                .iter()
                .map(|clip| ClipState {
                    id: format!("{}-{index}", clip.id),
                    ..clip.clone()
                })
                .collect();
        }
        state.selection.selected_clip_ids = (0..AUDIO_CLIP_ROWS.len())
            .map(|index| format!("clip-1-{index}"))
            .collect();
        Self { state }
    }
}

impl gpui::Render for AudioClipsPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
        use crate::components::timeline::audio_clip::audio_clip;
        use crate::theme::Colors;
        use gpui::{div, IntoElement, ParentElement, Styled};
        let state = &self.state;
        let lanes = state
            .tracks
            .iter()
            .zip(AUDIO_CLIP_ROWS)
            .map(|(track, height)| {
                let crossfades = state.audio_crossfades(track);
                let clips = track.clips.iter().map(|clip| {
                    audio_clip(
                        clip,
                        &track.id,
                        track.color,
                        state,
                        height,
                        Arc::new(|_, _, _| {}),
                        None,
                        None,
                        None,
                        None,
                        false,
                        state.effective_clip_fades(clip, &crossfades),
                        &state.clip_time_axis(clip),
                        false,
                        None,
                        Arc::new(|_, _, _| {}),
                        Arc::new(|_, _, _| {}),
                    )
                    .into_any_element()
                });
                div()
                    .relative()
                    .w_full()
                    .h(px(height))
                    .border_b(px(1.0))
                    .border_color(Colors::divider())
                    .children(clips)
            });
        div()
            .size_full()
            .font(crate::theme::ui_font())
            .bg(Colors::surface_base())
            .pt(px(12.0))
            .children(lanes)
    }
}

/// The MIDI editor with its tools column open on a chord progression, on
/// the Timing page.
fn midi_tools_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_midi_tools(options, cx, 0, false)
}

/// Time stretch caught mid-drag: the first eight notes selected and pulled
/// to one and a half times their length from the ruler's right handle.
fn midi_stretch_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_midi_stretch(options, cx, 1.5)
}

/// The same selection before the drag: the span and its two handles.
fn midi_stretch_idle_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_midi_stretch(options, cx, 1.0)
}

fn open_midi_stretch(options: WindowOptions, cx: &mut App, factor: f32) -> Result<AnyWindowHandle> {
    use crate::components::piano_roll::PianoRoll;
    let handle = cx.open_window(options, |_, cx| {
        let timeline = preview_midi_timeline(cx);
        cx.new(|cx| {
            let mut roll = PianoRoll::new(timeline, cx);
            roll.set_tools_panel_open(true, cx);
            roll.preview_tools(0, false);
            roll.preview_stretch(8, factor, cx);
            roll
        })
    })?;
    Ok(handle.into())
}

/// The Pitch page, with the Chord Track shown under the ruler.
fn midi_tools_pitch_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_midi_tools(options, cx, 1, true)
}

/// The Perform page.
fn midi_tools_perform_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_midi_tools(options, cx, 2, false)
}

fn open_midi_tools(
    options: WindowOptions,
    cx: &mut App,
    tab: usize,
    chord_strip: bool,
) -> Result<AnyWindowHandle> {
    use crate::components::piano_roll::PianoRoll;
    let handle = cx.open_window(options, |_, cx| {
        let timeline = preview_midi_timeline(cx);
        cx.new(|cx| {
            let mut roll = PianoRoll::new(timeline, cx);
            roll.set_tools_panel_open(true, cx);
            roll.preview_tools(tab, chord_strip);
            roll
        })
    })?;
    Ok(handle.into())
}

fn midi_tool_strum_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_midi_tool(
        options,
        cx,
        crate::components::piano_roll::advanced_tools::AdvancedTool::Strum,
    )
}

fn midi_tool_humanize_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_midi_tool(
        options,
        cx,
        crate::components::piano_roll::advanced_tools::AdvancedTool::Humanize,
    )
}

fn midi_tool_length_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_midi_tool(
        options,
        cx,
        crate::components::piano_roll::advanced_tools::AdvancedTool::Length,
    )
}

fn midi_tool_velocity_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_midi_tool(
        options,
        cx,
        crate::components::piano_roll::advanced_tools::AdvancedTool::Velocity,
    )
}

fn midi_tool_cleanup_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_midi_tool(
        options,
        cx,
        crate::components::piano_roll::advanced_tools::AdvancedTool::CleanUp,
    )
}

/// One tool window, live on the preview progression's notes (the editor
/// itself is not shown; the window only reads it).
fn open_midi_tool(
    options: WindowOptions,
    cx: &mut App,
    tool: crate::components::piano_roll::advanced_tools::AdvancedTool,
) -> Result<AnyWindowHandle> {
    use crate::components::midi_tools_dialog::MidiToolWindow;
    use crate::components::piano_roll::PianoRoll;
    let timeline = preview_midi_timeline(cx);
    let roll = cx.new(|cx| {
        let mut roll = PianoRoll::new(timeline, cx);
        roll.preview_begin_tool(tool, cx);
        roll
    });
    // The window holds the editor weakly; in the app the layout owns it.
    // Nothing owns it here, so keep it for the life of the preview.
    std::mem::forget(roll.clone());
    let handle = cx.open_window(options, move |_, cx| {
        cx.new(|cx| MidiToolWindow::new(roll, tool, cx))
    })?;
    Ok(handle.into())
}

/// A MIDI track holding a four-chord progression (C Am F G, each with a bass
/// note), selected, with the same chords on the Chord Track.
fn preview_midi_timeline(
    cx: &mut App,
) -> gpui::Entity<crate::components::timeline::timeline::Timeline> {
    use crate::components::timeline::timeline::Timeline;
    use crate::components::timeline::timeline_state::ChordPlacement;
    use sphere_midi_service::chords::{Chord, ChordQuality};
    {
        cx.new(|_| {
            let mut timeline = Timeline::new();
            let state = &mut timeline.state;
            let track = state.create_midi_track();
            if let Some(clip) = state.create_midi_clip(&track, 0.0, 16.0) {
                let chords: [(u8, [u8; 3]); 4] = [
                    (0, [60, 64, 67]),
                    (9, [57, 60, 64]),
                    (5, [53, 57, 60]),
                    (7, [55, 59, 62]),
                ];
                for (bar, (_, pitches)) in chords.iter().enumerate() {
                    for (k, pitch) in pitches.iter().enumerate() {
                        let start = bar as f32 * 4.0 + k as f32 * 0.04;
                        state.add_midi_note(&clip, *pitch, start, 3.5, 96 - k as u8 * 8);
                    }
                    state.add_midi_note(&clip, pitches[0] - 12, bar as f32 * 4.0, 1.0, 110);
                }
                state.select_clip(&clip);
            }
            let quality = [
                ChordQuality::Major,
                ChordQuality::Minor,
                ChordQuality::Major,
                ChordQuality::Major,
            ];
            let placements: Vec<ChordPlacement> = chords_roots()
                .iter()
                .zip(quality)
                .map(|(root, quality)| ChordPlacement {
                    chord: Chord::new(*root, quality),
                    flats: false,
                    length_beats: 4.0,
                })
                .collect();
            state.place_chords(0.0, &placements);
            timeline
        })
    }
}

/// The MIDI editor in drum mode, on a two-bar beat; `used` lists only the
/// sounds the clip plays instead of the whole kit.
fn open_midi_drums(options: WindowOptions, cx: &mut App, used: bool) -> Result<AnyWindowHandle> {
    use crate::components::piano_roll::{DrumLanes, PianoRoll};
    use crate::components::timeline::timeline::Timeline;
    let handle = cx.open_window(options, move |_, cx| {
        let timeline = cx.new(|_| {
            let mut timeline = Timeline::new();
            let state = &mut timeline.state;
            let track = state.create_midi_track();
            if let Some(clip) = state.create_midi_clip(&track, 0.0, 8.0) {
                for bar in 0..2 {
                    let at = bar as f32 * 4.0;
                    for kick in [0.0, 1.5, 2.0] {
                        state.add_midi_note(&clip, 36, at + kick, 0.25, 118);
                    }
                    for snare in [1.0, 3.0] {
                        state.add_midi_note(&clip, 38, at + snare, 0.25, 104);
                    }
                    for step in 0..8 {
                        let accent = if step % 2 == 0 { 96 } else { 64 };
                        if bar == 1 && step == 7 {
                            state.add_midi_note(&clip, 46, at + 3.5, 0.25, 92);
                        } else {
                            state.add_midi_note(&clip, 42, at + step as f32 * 0.5, 0.25, accent);
                        }
                    }
                }
                state.add_midi_note(&clip, 49, 0.0, 0.25, 110);
                state.add_midi_note(&clip, 50, 7.25, 0.25, 90);
                state.add_midi_note(&clip, 47, 7.5, 0.25, 96);
                state.add_midi_note(&clip, 45, 7.75, 0.25, 100);
                state.select_clip(&clip);
            }
            timeline
        });
        cx.new(|cx| {
            let mut roll = PianoRoll::new(timeline, cx);
            roll.set_drum_mode(true, cx);
            if used {
                roll.set_drum_lanes(DrumLanes::Used, cx);
            }
            roll
        })
    })?;
    Ok(handle.into())
}

fn midi_drums_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_midi_drums(options, cx, false)
}

fn midi_drums_used_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_midi_drums(options, cx, true)
}

fn chords_roots() -> [u8; 4] {
    [0, 9, 5, 7]
}

fn audio_clips_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    let handle = cx.open_window(options, |_, cx| cx.new(|_| AudioClipsPreview::new()))?;
    Ok(handle.into())
}

// ── Inspector ───────────────────────────────────────────────────────────────

/// The track Inspector on an instrument track: a loaded synth, two effects
/// (the second bypassed), solo and input lit, a volume a little under unity.
struct InspectorPreview {
    tracks: Vec<crate::components::timeline::timeline_state::TrackState>,
    track_id: String,
    name_input: crate::components::text_input::TextInputState,
    clip_name_input: crate::components::text_input::TextInputState,
    color: crate::components::ColorPickerState,
    folded: &'static [&'static str],
}

impl InspectorPreview {
    fn new(folded: &'static [&'static str], cx: &mut Context<Self>) -> Self {
        use crate::components::timeline::timeline_state::{
            volume, CreateTrackOptions, InputMonitorMode, InsertLoadStatus, TimelineState,
            TrackType,
        };
        let mut state = TimelineState::default();
        let color = crate::theme::Colors::track_color_for_index(0);
        let track_id = state.create_track(CreateTrackOptions {
            track_type: TrackType::Instrument,
            name: "Instrument Track 3".to_string(),
            color,
            volume: volume::db_to_norm(-3.0),
            pan: -0.2,
            armed: false,
            input_monitor: InputMonitorMode::Always,
        });
        for name in ["WrapSynth", "EQUZ8", "BurnLimit"] {
            if let Some(slot_id) = state.add_insert(&track_id) {
                if let Some(track) = state.tracks.iter_mut().find(|t| t.id == track_id) {
                    if let Some(slot) = track.inserts.iter_mut().find(|s| s.id == slot_id) {
                        slot.display_name = name.to_string();
                        slot.load_status = InsertLoadStatus::Ready;
                        slot.bypassed = name == "BurnLimit";
                    }
                }
            }
        }
        if let Some(track) = state.tracks.iter_mut().find(|t| t.id == track_id) {
            track.solo = true;
        }
        let name_input =
            crate::components::text_input::TextInputState::new("preview-name", cx.focus_handle());
        let clip_name_input =
            crate::components::text_input::TextInputState::new("preview-clip", cx.focus_handle());
        let color = crate::components::ColorPickerState::new(
            "preview-hex",
            cx.focus_handle(),
            crate::components::ColorPickerValue::custom(color),
            color,
            Vec::new(),
        );
        Self {
            tracks: state.tracks,
            track_id,
            name_input,
            clip_name_input,
            color,
            folded,
        }
    }
}

impl gpui::Render for InspectorPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
        use crate::components::panel::{inspector_panel, InspectorCallbacks, InspectorColorPicker};
        use crate::components::text_input::TextInputCallbacks;
        let callbacks = InspectorCallbacks::preview_noop(self.folded);
        let noop = || Arc::new(|_: &mut Window, _: &mut App| {});
        let picker = InspectorColorPicker {
            state: &self.color,
            hex_focused: false,
            hex_callbacks: TextInputCallbacks::default(),
            callbacks: crate::components::ColorPickerCallbacks {
                on_toggle: noop(),
                on_close: noop(),
                on_pick: Arc::new(|_, _, _| {}),
                on_hue: Arc::new(|_, _, _| {}),
                on_sv: Arc::new(|_, _, _, _| {}),
                on_auto: Arc::new(|_, _, _| {}),
            },
        };
        inspector_panel(
            &self.tracks,
            &crate::audio_connections::AudioConnectionRegistry::default(),
            Some(&self.track_id),
            None,
            None,
            None,
            &self.name_input,
            false,
            TextInputCallbacks::default(),
            &self.clip_name_input,
            false,
            TextInputCallbacks::default(),
            picker,
            true,
            solfege_spatialaudio::SpatialFormat::Stereo,
            &callbacks,
            crate::i18n::I18n::new("en-US"),
        )
    }
}

fn inspector_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    let handle = cx.open_window(options, |_, cx| cx.new(|cx| InspectorPreview::new(&[], cx)))?;
    Ok(handle.into())
}

fn inspector_folded_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    let handle = cx.open_window(options, |_, cx| {
        cx.new(|cx| InspectorPreview::new(&["routing", "program", "mpe", "instrument"], cx))
    })?;
    Ok(handle.into())
}

/// The demo arrangement with its overlay layers, as the studio root stacks
/// them, and levels on the tracks so the header meters show: quiet, healthy,
/// hot and clipping.
struct ArrangementMetersPreview {
    timeline: gpui::Entity<crate::components::timeline::Timeline>,
}

impl gpui::Render for ArrangementMetersPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        use gpui::{div, ParentElement, Styled};
        let overlays: Vec<gpui::AnyView> = self.timeline.update(cx, |timeline, cx| {
            timeline.ensure_overlay_layers(cx);
            timeline.overlay_layers().collect()
        });
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .child(
                gpui::AnyView::from(self.timeline.clone())
                    .cached(crate::components::timeline::Timeline::cached_style()),
            )
            .children(overlays)
    }
}

fn arrangement_meters_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    let handle = cx.open_window(options, |_, cx| {
        let timeline = cx.new(|_| {
            let mut timeline = crate::components::timeline::Timeline::with_demo_content();
            let levels = [
                (0.05, 0.04),
                (0.35, 0.3),
                (0.62, 0.7),
                (0.92, 1.0),
                (0.2, 0.18),
            ];
            for (track, (l, r)) in timeline.state.tracks.iter_mut().zip(levels.iter().cycle()) {
                track.meter_level_l = *l;
                track.meter_level_r = *r;
            }
            timeline
        });
        cx.new(|_| ArrangementMetersPreview { timeline })
    })?;
    Ok(handle.into())
}

/// The System Overloaded dialog as a busy session raises it: four blocks
/// missed while a take records, at 256 samples, Audioguard on Normal.
fn system_overloaded_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    use crate::layout::audioguard_ops::OverloadReport;
    let report = OverloadReport {
        missed_blocks: 4,
        window_seconds: 5,
        buffer_frames: 256,
        sample_rate: 48_000,
        peak_block_ms: 7.1,
        block_ms: 256.0 * 1000.0 / 48_000.0,
        level: DirectAudio::AudioguardLevel::Normal,
        recording: true,
    };
    // Through the app's own dialog path, so the preview has the dialog's real
    // frame (the platform caption on Windows) and its fitted height.
    let _ = options;
    let handle = crate::components::message_box_dialog::open_message_box_for_preview(
        report.message_box(),
        cx,
    )
    .map_err(gpui::private::anyhow::Error::msg)?;
    Ok(handle.into())
}

/// The demo arrangement with its first track on an ARA plug-in, and the notes
/// the plug-in reports for the vocal over the clip: a phrase with one note
/// pulled sharp and one drifting flat, as Melodyne would hand them back.
fn ara_notes_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    use crate::components::timeline::audio_clip::AraClipNote;
    use crate::components::timeline::timeline_state::AraTrackBinding;
    let handle = cx.open_window(options, |_, cx| {
        let timeline = cx.new(|_| {
            let mut timeline = crate::components::timeline::Timeline::with_demo_content();
            let seconds_per_beat = 60.0 / timeline.state.bpm.max(1.0) as f64;
            let track = &mut timeline.state.tracks[0];
            track.ara = Some(AraTrackBinding {
                plugin_id: "vst3:melodyne".to_string(),
                plugin_path: "C:/Melodyne.vst3".to_string(),
                class_id: "ABCD".to_string(),
            });
            let clip = track.clips[0].clone();
            let start = clip.start_beat as f64 * seconds_per_beat;
            let phrase: [(f64, f64, f32); 11] = [
                (0.10, 0.45, 64.0),
                (0.60, 0.40, 67.0),
                (1.05, 0.85, 69.4),
                (2.00, 0.35, 67.0),
                (2.40, 0.40, 64.0),
                (2.85, 0.90, 62.0),
                (4.40, 0.45, 64.0),
                (4.90, 0.40, 66.8),
                (5.35, 0.95, 69.0),
                (6.40, 0.35, 71.0),
                (6.80, 0.60, 68.7),
            ];
            let notes: std::sync::Arc<[AraClipNote]> = phrase
                .iter()
                .map(|&(at, length, pitch)| AraClipNote {
                    start_seconds: start + at,
                    duration_seconds: length,
                    pitch,
                })
                .collect();
            timeline.set_ara_notes([(clip.id, notes)].into_iter().collect());
            timeline
        });
        cx.new(|_| ArrangementMetersPreview { timeline })
    })?;
    Ok(handle.into())
}

/// The demo arrangement with automation lanes shown on its first track:
/// Volume playing a curve, Pan overridden by hand, Mute recording with the
/// Automation Arm on.
fn automation_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    use crate::components::timeline::timeline_state::AutomationTarget;
    let handle = cx.open_window(options, |_, cx| {
        let timeline = cx.new(|_| {
            let mut timeline = crate::components::timeline::Timeline::with_demo_content();
            let state = &mut timeline.state;
            let track_id = state.tracks[0].id.clone();
            let volume = state
                .ensure_automation_lane(&track_id, AutomationTarget::TrackVolume)
                .unwrap();
            for (beat, value) in [(0.0, 0.55), (4.0, 0.8), (8.0, 0.4), (12.0, 0.7)] {
                state.add_automation_point(&track_id, &volume, beat, value);
            }
            let pan = state
                .ensure_automation_lane(&track_id, AutomationTarget::TrackPan)
                .unwrap();
            for (beat, value) in [(0.0, 0.5), (6.0, 0.2), (10.0, 0.8)] {
                state.add_automation_point(&track_id, &pan, beat, value);
            }
            let mute = state
                .ensure_automation_lane(&track_id, AutomationTarget::TrackMute)
                .unwrap();
            let track = state.tracks.iter_mut().find(|t| t.id == track_id).unwrap();
            track.lane_mode =
                crate::components::timeline::timeline_state::TrackLaneMode::Automation;
            for lane in &mut track.automation_lanes {
                lane.visible = true;
                // Volume shows the curve shapes: a sharp S, a fast ease and a
                // soft S.
                if lane.id == volume {
                    use crate::components::timeline::timeline_state::AutomationCurve;
                    for (point, (curve, tension)) in lane.points.iter_mut().zip([
                        (AutomationCurve::Smooth, 0.6),
                        (AutomationCurve::Linear, -0.6),
                        (AutomationCurve::Smooth, -0.5),
                    ]) {
                        point.curve = curve;
                        point.tension = tension;
                    }
                }
                if lane.id == pan {
                    lane.overridden = true;
                }
                if lane.id == mute {
                    lane.recording = true;
                }
            }
            state.automation_arm = true;
            timeline
        });
        cx.new(|_| ArrangementMetersPreview { timeline })
    })?;
    Ok(handle.into())
}

/// A Volume lane walking through the curve styles — S, Sharp S, Soft S, a
/// slow ease, Hold and Jump — with one selected point's value field open.
fn automation_value_edit_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    use crate::components::timeline::timeline_state::{
        AutomationCurveShape, AutomationTarget, TrackLaneMode,
    };
    let handle = cx.open_window(options, |_, cx| {
        let timeline = cx.new(|cx| {
            let mut timeline = crate::components::timeline::Timeline::with_demo_content();
            let state = &mut timeline.state;
            let track_id = state.tracks[0].id.clone();
            let volume = state
                .ensure_automation_lane(&track_id, AutomationTarget::TrackVolume)
                .unwrap();
            // Only this scene's points: the demo track carries some already.
            if let Some(lane) = state.tracks[0]
                .automation_lanes
                .iter_mut()
                .find(|lane| lane.id == volume)
            {
                lane.points.clear();
            }
            let shapes = [
                AutomationCurveShape::SCurve,
                AutomationCurveShape::SharpS,
                AutomationCurveShape::SoftS,
                AutomationCurveShape::Slow,
                AutomationCurveShape::Hold,
                AutomationCurveShape::Jump,
                AutomationCurveShape::Linear,
            ];
            for (i, value) in [0.3, 0.8, 0.25, 0.85, 0.2, 0.75, 0.35]
                .into_iter()
                .enumerate()
            {
                state.add_automation_point(&track_id, &volume, i as f32 * 1.5, value);
            }
            let track = state.tracks.iter_mut().find(|t| t.id == track_id).unwrap();
            track.lane_mode = TrackLaneMode::Automation;
            let mut edited = 0;
            for lane in &mut track.automation_lanes {
                lane.visible = true;
                if lane.id == volume {
                    for (point, shape) in lane.points.iter_mut().zip(shapes) {
                        let (curve, tension) = shape.curve_and_tension();
                        point.curve = curve;
                        point.tension = tension;
                    }
                    edited = lane.points[3].id;
                }
            }
            timeline.open_automation_value_edit(&track_id, &volume, edited, cx);
            timeline
        });
        cx.new(|_| ArrangementMetersPreview { timeline })
    })?;
    Ok(handle.into())
}

/// The master's automation block after the last track: Volume playing a
/// fade, and a second lane switched off.
fn master_automation_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    use crate::components::timeline::timeline_state::{AutomationTarget, MASTER_TRACK_ID};
    let handle = cx.open_window(options, |_, cx| {
        let timeline = cx.new(|_| {
            let mut timeline = crate::components::timeline::Timeline::with_demo_content();
            let state = &mut timeline.state;
            state.toggle_track_lane_mode(MASTER_TRACK_ID);
            let volume = state
                .ensure_automation_lane(MASTER_TRACK_ID, AutomationTarget::TrackVolume)
                .unwrap();
            for (beat, value) in [(0.0, 0.72), (8.0, 0.72), (12.0, 0.5), (16.0, 0.0)] {
                state.add_automation_point(MASTER_TRACK_ID, &volume, beat, value);
            }
            state.master.inserts.push(
                crate::components::timeline::timeline_state::InsertSlotState {
                    display_name: "Limiter".to_string(),
                    ..crate::components::timeline::timeline_state::InsertSlotState::empty(
                        "master-limiter",
                    )
                },
            );
            let bypass = state
                .ensure_automation_lane(
                    MASTER_TRACK_ID,
                    AutomationTarget::InsertBypass {
                        insert_id: "master-limiter".to_string(),
                    },
                )
                .unwrap();
            for (beat, value) in [(0.0, 0.0), (4.0, 1.0), (6.0, 0.0)] {
                state.add_automation_point(MASTER_TRACK_ID, &bypass, beat, value);
            }
            state.set_automation_lane_enabled(MASTER_TRACK_ID, &bypass, false);
            // Scrolled so the last tracks and the master block are in view.
            let layout = state.track_row_layout();
            state.viewport.scroll_y = (layout.master_y - 220.0).max(0.0);
            timeline
        });
        cx.new(|_| ArrangementMetersPreview { timeline })
    })?;
    Ok(handle.into())
}

/// The two tool bar styles: the Classic strip on top, as the shell docks it
/// above the ruler, and the Floating bar where it floats.
struct ToolBarsPreview {
    strip: gpui::Entity<crate::components::timeline::tool_strip::ClassicToolStrip>,
}

impl gpui::Render for ToolBarsPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
        use crate::components::timeline::floating_tools_bar::floating_tools_bar;
        use crate::components::timeline::timeline_state::TimelineTool;
        use crate::theme::Colors;
        use gpui::{div, px, ParentElement, Styled};
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(Colors::surface_base())
            .child(
                gpui::AnyView::from(self.strip.clone()).cached(
                    crate::components::timeline::tool_strip::ClassicToolStrip::cached_style(),
                ),
            )
            .child(
                div().flex_1().relative().child(
                    div()
                        .absolute()
                        .left(px(16.0))
                        .bottom(px(16.0))
                        .child(floating_tools_bar(
                            TimelineTool::Pen,
                            Arc::new(|_, _, _| {}),
                            Arc::new(|_, _, _| {}),
                            1.0,
                            Arc::new(|_, _, _| {}),
                        )),
                ),
            )
    }
}

fn tool_bars_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    let handle = cx.open_window(options, |_, cx| {
        let timeline = cx.new(|_| crate::components::timeline::Timeline::with_demo_content());
        let strip = cx.new(|cx| {
            crate::components::timeline::tool_strip::ClassicToolStrip::new(&timeline, cx)
        });
        cx.new(|_| ToolBarsPreview { strip })
    })?;
    Ok(handle.into())
}

/// Every Futureboard icon at 14, 16, 20 and 40 px, as Studio tints them, with the
/// path it answers to.
struct IconSheetPreview;

impl gpui::Render for IconSheetPreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
        use crate::theme::Colors;
        use gpui::{div, px, svg, ParentElement, Styled};
        let mut grid = div()
            .size_full()
            .p(px(16.0))
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(8.0))
            .bg(Colors::surface_panel());
        for (path, _) in crate::assets::FUTUREBOARD_ICONS {
            let name = path
                .trim_start_matches("icons/")
                .trim_end_matches(".svg")
                .to_string();
            grid = grid.child(
                div()
                    .w(px(150.0))
                    .h(px(84.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap(px(6.0))
                    .rounded(px(6.0))
                    .bg(Colors::surface_input())
                    .child(div().flex().flex_row().items_end().gap(px(8.0)).children(
                        [14.0, 16.0, 20.0, 40.0].map(|edge| {
                            svg()
                                .path(path)
                                .size(px(edge))
                                .text_color(Colors::text_primary())
                        }),
                    ))
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(Colors::text_muted())
                            .child(name),
                    ),
            );
        }
        grid
    }
}

fn icons_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    let handle = cx.open_window(options, |_, cx| cx.new(|_| IconSheetPreview))?;
    Ok(handle.into())
}

/// The jam panel signed in and out of a jam, with nothing typed yet.
fn jam_idle_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    use crate::components::jam_window::JamWindow;
    let state = crate::jam::JamUiState {
        configured: true,
        signed_in: true,
        state_label: "Disconnected".to_string(),
        ..Default::default()
    };
    let handle = cx.open_window(options, |_, cx| cx.new(|cx| JamWindow::preview(state, cx)))?;
    Ok(handle.into())
}

/// The jam panel in a live three-person room: Master going out, a guitar
/// stream routed and playing, a vocal stream not yet routed, an invite link
/// minted.
fn jam_live_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    use crate::components::jam_window::JamWindow;
    use crate::jam::{JamSendView, JamStreamView, JamUiState};
    use sphere_jam_client::protocol::{ParticipantSummary, UserSummary};

    let person =
        |id: &str, username: &str, display: &str, device: &str, state: &str| ParticipantSummary {
            id: format!("par_{id}"),
            user: UserSummary {
                id: format!("usr_{id}"),
                username: username.to_string(),
                display_name: display.to_string(),
                ..Default::default()
            },
            device_id: device.to_string(),
            device_name: device.to_string(),
            role: "performer".into(),
            connection_state: state.into(),
            ..Default::default()
        };
    let stream = |id: &str, user: &str, name: &str, routed: bool, receiving: bool| JamStreamView {
        stream_id: format!("str_{id}"),
        user_id: format!("usr_{user}"),
        device_id: "studio".to_string(),
        handle: format!("@{user}"),
        display_name: String::new(),
        avatar_url: String::new(),
        stream_name: name.to_string(),
        channels: 2,
        channel_labels: vec!["L".to_string(), "R".to_string()],
        sample_rate: 48_000,
        codec: "pcm".to_string(),
        receiving,
        peak: if receiving { 0.62 } else { 0.0 },
        rtt_ms: 38.0,
        routed,
        buffering: false,
        dropouts: if receiving { 2 } else { 0 },
        arrival_latency_ms: receiving.then_some(46.0),
    };
    let state = JamUiState {
        configured: true,
        signed_in: true,
        state_label: "Connected".to_string(),
        connected: true,
        jam_id: "jam_1".to_string(),
        jam_name: "Friday Session".to_string(),
        public_id: "K7Q-2MX".to_string(),
        join_url: "https://jam.futureboard.studio/j/K7Q-2MX".to_string(),
        region_label: "Singapore".to_string(),
        transport_label: "QUIC".to_string(),
        rtt_ms: 38.4,
        clock_offset_ms: 1.2,
        clock_drift_ppm: -3.1,
        clock_locked: true,
        packets_in: 18_204,
        packets_out: 17_990,
        participants: vec![
            person("1", "arizkami", "Ariz", "Studio PC", "connected"),
            person("2", "mint", "Mint", "MacBook", "connected"),
            person("3", "ploy", "", "Studio", "reconnecting"),
        ],
        streams: vec![
            stream("g", "2", "Guitar", true, true),
            stream("v", "3", "Vocal", false, false),
        ],
        publishing: vec!["master".to_string()],
        sending: vec![JamSendView {
            key: "master".to_string(),
            name: "Master".to_string(),
            tap: "Master bus".to_string(),
            level: 0.48,
        }],
        invite_link: Some("https://jam.futureboard.studio/j/K7Q-2MX#s3cr3t".to_string()),
        ..Default::default()
    };
    let handle = cx.open_window(options, |_, cx| cx.new(|cx| JamWindow::preview(state, cx)))?;
    Ok(handle.into())
}

/// The Audio Connections window over an 8-in/8-out interface's default
/// buses, one turned off and one whose device is unplugged, the first row
/// selected.
fn audio_connections_window_preview(
    populated: bool,
    cx: &mut Context<crate::components::audio_connections_window::AudioConnectionsWindow>,
) -> crate::components::audio_connections_window::AudioConnectionsWindow {
    use crate::audio_connections::{
        AudioConnectionDirection, AudioConnectionRegistry, AvailablePorts, ChannelLayout,
    };
    use crate::components::audio_connections_window::{
        AudioConnectionsSnapshot, AudioConnectionsWindow,
    };
    let ports = AvailablePorts::for_device("dev-1", "Studio 24c USB ASIO", 8, 8);
    let registry = if populated {
        let mut registry = AudioConnectionRegistry::default_template(&ports, "dev-1");
        let inputs = registry.by_direction(AudioConnectionDirection::Input);
        if let Some(last) = inputs.last().map(|connection| connection.id.clone()) {
            registry.set_enabled(&last, false);
        }
        let (missing, _) = registry.add_connection(
            AudioConnectionDirection::Input,
            ChannelLayout::Stereo,
            &ports,
        );
        registry.update_name(&missing, "Field Recorder");
        registry.update_device(&missing, Some("dev-1"), &ports);
        if let Some(connection) = registry.get_mut(&missing) {
            connection.device_id = Some("zoom-f3".to_string());
        }
        registry.revalidate(&ports);
        registry
    } else {
        AudioConnectionRegistry::new()
    };
    let snapshot = AudioConnectionsSnapshot {
        registry,
        ports,
        input_device: "Studio 24c USB ASIO".to_string(),
        output_device: "Studio 24c USB ASIO".to_string(),
        has_project: true,
        references: Default::default(),
    };
    let mut window = AudioConnectionsWindow::new(
        snapshot,
        Arc::new(|_, _, _| {}),
        Arc::new(|_, _| {}),
        cx.focus_handle(),
    );
    window.preview_select_first();
    window
}

fn audio_connections_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    let handle = cx.open_window(options, |_, cx| {
        cx.new(|cx| audio_connections_window_preview(true, cx))
    })?;
    Ok(handle.into())
}

fn audio_connections_empty_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    let handle = cx.open_window(options, |_, cx| {
        cx.new(|cx| audio_connections_window_preview(false, cx))
    })?;
    Ok(handle.into())
}

/// The Inspector's MIDI Input menu, open, with the ports from a real rig and
/// two tracks whose plug-ins it can take MIDI from; the second is chosen.
struct MidiInputTreePreview;

impl gpui::Render for MidiInputTreePreview {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl gpui::IntoElement {
        use crate::components::timeline::timeline_state::TrackMidiInputRouting;
        use gpui::{div, ParentElement, Styled};
        let current = TrackMidiInputRouting::PluginOutput {
            track_id: "track-3".to_string(),
        };
        let devices: Vec<String> = [
            "Studio 24c MIDI In",
            "Springbeats vMIDI1",
            "Springbeats vMIDI2",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        let sources = [
            ("track-2".to_string(), "Lead - Serum 2".to_string()),
            ("track-3".to_string(), "Drums - EZdrummer 3".to_string()),
        ];
        let nodes = crate::components::panel::midi_input_tree(&current, &devices, &sources);
        div()
            .size_full()
            .bg(crate::theme::Colors::surface_panel())
            .child(crate::components::combo_box::combo_box_tree_menu(
                "preview-midi-input-menu",
                crate::overlay::OverlayPosition {
                    x: px(16.0),
                    y: px(16.0),
                    width: Some(px(240.0)),
                    max_height: Some(px(380.0)),
                },
                "vsti:track-3",
                nodes,
                Arc::new(|_, _, _| {}),
            ))
    }
}

fn midi_input_tree(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    let handle = cx.open_window(options, |_, cx| cx.new(|_| MidiInputTreePreview))?;
    Ok(handle.into())
}

// ── VerbSpace and EchoSpace scenes ─────────────────────────────────────────

/// A steady meter reading for the time-effect plates' In / Out rows.
fn time_plate_frames(
    in_peak: f32,
    out_peak: f32,
) -> Vec<SpherePluginHost::audio_bridge::BuiltinMeterFrame> {
    vec![SpherePluginHost::audio_bridge::BuiltinMeterFrame {
        in_peak,
        out_peak,
        ..Default::default()
    }]
}

fn verbspace_with(
    options: WindowOptions,
    cx: &mut App,
    insert: &'static str,
    params: verbspace::Params,
) -> Result<AnyWindowHandle> {
    use crate::components::verbspace_plate::VerbSpacePlate;
    let json = verbspace::ipc::VerbspaceState::new(params).to_json();
    open_plate(
        options,
        cx,
        VerbSpacePlate,
        "verbspace",
        insert,
        json,
        time_plate_frames(0.42, 0.5),
        [0.0; SpherePluginHost::audio_bridge::BUILTIN_PAD_SLOTS],
        |_| {},
    )
}

fn verbspace_preset(name: &str) -> verbspace::Params {
    verbspace::factory_presets()
        .into_iter()
        .find(|preset| preset.name == name)
        .map_or_else(verbspace::default_params, |preset| preset.params)
}

fn verbspace_hall(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    verbspace_with(options, cx, "verbspace-hall", verbspace::default_params())
}

fn verbspace_plate(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    verbspace_with(
        options,
        cx,
        "verbspace-plate",
        verbspace_preset("Bright Plate"),
    )
}

fn echospace_with(
    options: WindowOptions,
    cx: &mut App,
    insert: &'static str,
    params: echospace::Params,
) -> Result<AnyWindowHandle> {
    use crate::components::echospace_plate::EchoSpacePlate;
    let json = echospace::ipc::EchospaceState::new(params).to_json();
    open_plate(
        options,
        cx,
        EchoSpacePlate,
        "echospace",
        insert,
        json,
        time_plate_frames(0.38, 0.44),
        [0.0; SpherePluginHost::audio_bridge::BUILTIN_PAD_SLOTS],
        |_| {},
    )
}

fn echospace_preset(name: &str) -> echospace::Params {
    echospace::factory_presets()
        .into_iter()
        .find(|preset| preset.name == name)
        .map_or_else(echospace::default_params, |preset| preset.params)
}

fn echospace_default(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    echospace_with(
        options,
        cx,
        "echospace-default",
        echospace::default_params(),
    )
}

fn echospace_synced(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    echospace_with(
        options,
        cx,
        "echospace-synced",
        echospace_preset("Dotted Eighth"),
    )
}

fn echospace_ambient(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    echospace_with(
        options,
        cx,
        "echospace-ambient",
        echospace_preset("Ambient Wash"),
    )
}

// ── Solfege Voxtune ────────────────────────────────────────────────────────

/// A synthetic sung phrase — six notes with scoops and vibrato, gaps between
/// them — analysed by the real engine, with a few edits made through the
/// document (a raised note, a shake, an accent, a muted note) so every layer
/// of the canvas shows.
fn voxtune_editor(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    voxtune_scene(options, cx, "panel-note")
}

fn voxtune_scene(
    options: WindowOptions,
    cx: &mut App,
    panel: &'static str,
) -> Result<AnyWindowHandle> {
    use crate::components::voxtune::{EngineTransport, VoxtuneEditor};
    use sphere_voxtune::engine::{CantoEngine, ORN_SHAKE};
    use sphere_voxtune::session::VoxDocument;
    use sphere_voxtune::shared::Shared;

    let sr = 48_000u32;
    let melody: [(f32, f32, f32); 6] = [
        (57.0, 0.20, 0.55),
        (60.0, 0.85, 0.50),
        (64.3, 1.45, 0.80),
        (62.0, 2.40, 0.45),
        (60.0, 2.95, 0.55),
        (57.0, 3.65, 1.10),
    ];
    let total = (5.0 * sr as f32) as usize;
    let mut samples = vec![0.0f32; total];
    let mut phase = 0.0f32;
    for (i, s) in samples.iter_mut().enumerate() {
        let t = i as f32 / sr as f32;
        let Some(&(midi, start, dur)) = melody.iter().find(|(_, a, d)| t >= *a && t < a + d) else {
            continue;
        };
        let u = (t - start) / dur;
        let scoop = -0.6 * (1.0 - (u * 8.0).min(1.0));
        let vib = 0.25 * (u * 2.0).min(1.0) * (std::f32::consts::TAU * 5.5 * t).sin();
        let hz = 440.0 * 2f32.powf((midi + scoop + vib - 69.0) / 12.0);
        phase += hz / sr as f32;
        let env = (u * 12.0).min(1.0) * ((1.0 - u) * 8.0).min(1.0);
        *s = env
            * 0.35
            * (1..6)
                .map(|k| (std::f32::consts::TAU * phase * k as f32).sin() / k as f32)
                .sum::<f32>();
    }
    let shared = Arc::new(Shared::new(CantoEngine::new(sr as f32)));
    let analysis = shared
        .lock()
        .load_mono_samples(samples, sr)
        .map_err(gpui::private::anyhow::Error::msg)?;
    let mut doc = VoxDocument::from_analysis(&analysis);
    let pitched: Vec<usize> = (0..doc.notes.len())
        .filter(|&i| !doc.notes[i].unpitched)
        .collect();
    if let Some(&i) = pitched.get(2) {
        let n = &mut doc.notes[i];
        n.pitch = n.detected.round();
        n.pinned = true;
    }
    if let Some(&i) = pitched.get(5) {
        let n = &mut doc.notes[i];
        n.orn_style = ORN_SHAKE;
        n.orn_depth = 1.0;
        n.orn_speed = 0.5;
    }
    if let Some(&i) = pitched.get(1) {
        doc.notes[i].accent = 0.7;
    }
    // One note sung flat, so the off-key colour and tag show.
    if let Some(&i) = pitched.get(4) {
        let n = &mut doc.notes[i];
        n.detected -= 0.34;
        n.pitch = n.detected;
    }
    if let Some(&i) = pitched.get(3) {
        doc.notes[i].muted = true;
    }
    let select = pitched.get(2).copied();
    Ok(cx
        .open_window(options, move |_window, cx| {
            cx.new(|cx| {
                let host = EngineTransport {
                    shared: shared.clone(),
                    on_change: None,
                };
                let mut editor = VoxtuneEditor::new(shared.clone(), Box::new(host), cx);
                editor.adopt(doc, "vocal-take.wav", cx);
                editor.select_note(select, cx);
                use crate::components::voxtune::VoxTool;
                match panel {
                    "tool:pitch" => editor.preview_gesture(VoxTool::Pitch, 1.0, cx),
                    "tool:time" => editor.preview_gesture(VoxTool::Time, 0.0, cx),
                    "tool:cut" => editor.preview_gesture(VoxTool::Cut, 0.0, cx),
                    panel => {
                        editor.run_command(panel, cx);
                    }
                }
                editor
            })
        })?
        .into())
}

// ── Plate editors: BurnLimit, EQUZ8, EQUZX and Zapp ───────────────────────

/// Samples per telemetry frame at 48 kHz: the host publishes about every
/// 33 ms.
const PREVIEW_BLOCK: usize = 1_600;
/// Ten seconds of frames, as the history keeps.
const PREVIEW_BLOCKS: usize = 300;

/// A drum-bus-like stereo signal at sample `n`: a kick every half second, a
/// snare on the off-beat and a bed of noise — synthetic, for previews only.
fn preview_signal(n: usize, noise: &mut u32) -> (f32, f32) {
    const SR: f32 = 48_000.0;
    let t = n as f32 / SR;
    let beat = t * 2.0;
    let phase = beat.fract();
    let kick = (std::f32::consts::TAU * 55.0 * phase / 2.0).sin() * (-phase * 9.0).exp();
    *noise ^= *noise << 13;
    *noise ^= *noise >> 17;
    *noise ^= *noise << 5;
    let white = (*noise as f32 / u32::MAX as f32) * 2.0 - 1.0;
    let off = (beat + 0.5).fract();
    let snare = white * (-off * 14.0).exp() * if (beat as u32) % 2 == 1 { 0.5 } else { 0.0 };
    let swell = 0.6 + 0.4 * (t * 0.7).sin();
    let bed = white * 0.04 * swell;
    let left = 0.5 * kick + 0.35 * snare + bed;
    let right = 0.5 * kick + 0.3 * snare - bed * 0.6;
    (left, right)
}

/// Runs `process` over ten seconds of [`preview_signal`], reading `meter`
/// after every block as the host would.
fn run_preview(
    mut process: impl FnMut(f32, f32) -> (f32, f32),
    mut meter: impl FnMut() -> SpherePluginHost::audio_bridge::BuiltinMeterFrame,
) -> Vec<SpherePluginHost::audio_bridge::BuiltinMeterFrame> {
    let mut noise = 0x2545_f491u32;
    let mut frames = Vec::with_capacity(PREVIEW_BLOCKS);
    for block in 0..PREVIEW_BLOCKS {
        for i in 0..PREVIEW_BLOCK {
            let (l, r) = preview_signal(block * PREVIEW_BLOCK + i, &mut noise);
            let _ = process(l, r);
        }
        frames.push(meter());
    }
    frames
}

type PreviewBlock = [f32; SpherePluginHost::audio_bridge::BUILTIN_PAD_SLOTS];

fn burnlimit_telemetry(
    params: &burnlimit::Params,
) -> (
    Vec<SpherePluginHost::audio_bridge::BuiltinMeterFrame>,
    PreviewBlock,
) {
    use waygate::StereoEffect;
    let dsp = std::cell::RefCell::new(burnlimit::Dsp::new(48_000.0));
    dsp.borrow_mut().set_params(params.clone());
    let frames = run_preview(
        |l, r| dsp.borrow_mut().process_stereo(l, r),
        || {
            let f = dsp.borrow().meter_frame();
            SpherePluginHost::audio_bridge::BuiltinMeterFrame {
                in_peak: f.in_peak,
                in_rms: f.in_rms,
                out_peak: f.out_peak,
                out_rms: f.out_rms,
                gain_reduction_db: f.gain_reduction_db,
                in_clip: f.in_clip,
                out_clip: f.out_clip,
                ..Default::default()
            }
        },
    );
    let block = dsp.borrow().telemetry();
    (frames, block)
}

fn zapp_telemetry(
    params: &zapp::Params,
) -> (
    Vec<SpherePluginHost::audio_bridge::BuiltinMeterFrame>,
    PreviewBlock,
) {
    use waygate::StereoEffect;
    let dsp = std::cell::RefCell::new(zapp::Dsp::new(48_000.0));
    dsp.borrow_mut().set_params(params.clone());
    let frames = run_preview(
        |l, r| dsp.borrow_mut().process_stereo(l, r),
        || {
            let f = dsp.borrow().meter_frame();
            SpherePluginHost::audio_bridge::BuiltinMeterFrame {
                in_peak: f.in_peak,
                in_rms: f.in_rms,
                out_peak: f.out_peak,
                out_rms: f.out_rms,
                in_clip: f.in_clip,
                out_clip: f.out_clip,
                ..Default::default()
            }
        },
    );
    let block = dsp.borrow().telemetry();
    (frames, block)
}

fn equz8_telemetry(params: &equz8::Params) -> PreviewBlock {
    use waygate::StereoEffect;
    let dsp = std::cell::RefCell::new(equz8::Dsp::new(48_000.0));
    dsp.borrow_mut().set_params(params.clone());
    run_preview(
        |l, r| dsp.borrow_mut().process_stereo(l, r),
        Default::default,
    );
    let block = dsp.borrow().telemetry();
    block
}

fn equzx_telemetry(params: &equzx::Params) -> PreviewBlock {
    use waygate::StereoEffect;
    let dsp = std::cell::RefCell::new(equzx::Dsp::new(48_000.0));
    dsp.borrow_mut().set_params(params.clone());
    run_preview(
        |l, r| dsp.borrow_mut().process_stereo(l, r),
        Default::default,
    );
    let block = dsp.borrow().telemetry();
    block
}

/// Seeds `json` for `plugin` and opens plate editor `model` on it, its
/// history and telemetry block filled from a run of the real DSP.
#[allow(clippy::too_many_arguments)]
fn open_plate<M: KitModel>(
    options: WindowOptions,
    cx: &mut App,
    model: M,
    plugin: &'static str,
    insert: &'static str,
    json: std::result::Result<String, serde_json::Error>,
    frames: Vec<SpherePluginHost::audio_bridge::BuiltinMeterFrame>,
    block: PreviewBlock,
    then: impl FnOnce(&mut crate::components::plugin_kit::KitEditor<M>) + 'static,
) -> Result<AnyWindowHandle> {
    if let Ok(json) = json {
        seed(plugin, insert, json);
    }
    plate::ensure_fonts(cx);
    let last = frames.last().copied();
    let host_ops = BuiltinEditorHostOps {
        meter_source: last.map(|frame| {
            Arc::new(move |_: &PluginInstanceKey| Some(frame))
                as crate::components::builtin_plugin_editor_window::BuiltinMeterSource
        }),
        pad_level_source: Some(Arc::new(move |_| Some((1, block)))),
        ..preview_host_ops()
    };
    let handle = cx.open_window(options, move |_, cx| {
        cx.new(|cx| {
            let window = KitWindow::new(
                model,
                key(insert),
                identity(model.title()),
                host_ops,
                no_close(),
                cx,
            );
            {
                let mut live = window.live().borrow_mut();
                for frame in frames {
                    live.show(frame);
                }
                live.show_pads(block);
                live.show_bands(Some(preview_spectrum()), None);
            }
            window.editor().update(cx, |editor, cx| {
                then(editor);
                cx.notify();
            });
            window
        })
    })?;
    Ok(handle.into())
}

/// BurnLimit driven hard enough to limit: its Loud Master preset.
fn burnlimit_preview_params() -> burnlimit::Params {
    burnlimit::factory_presets()
        .into_iter()
        .find(|preset| preset.name == "Loud Master")
        .map_or_else(burnlimit::default_params, |preset| preset.params)
}

fn burnlimit_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    use crate::components::burn_limit_plate::BurnLimitPlate;
    let params = burnlimit_preview_params();
    let (frames, block) = burnlimit_telemetry(&params);
    let json = burnlimit::ipc::BurnLimitState::new(params).to_json();
    open_plate(
        options,
        cx,
        BurnLimitPlate,
        "burnlimit",
        "burnlimit-plate",
        json,
        frames,
        block,
        |_| {},
    )
}

fn zapp_with(
    options: WindowOptions,
    cx: &mut App,
    insert: &'static str,
    params: zapp::Params,
) -> Result<AnyWindowHandle> {
    use crate::components::zapp_plate::ZappPlate;
    let (frames, block) = zapp_telemetry(&params);
    let json = zapp::ipc::ZappState::new(params).to_json();
    open_plate(
        options,
        cx,
        ZappPlate,
        "zapp",
        insert,
        json,
        frames,
        block,
        |_| {},
    )
}

fn zapp_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    zapp_with(options, cx, "zapp", zapp::default_params())
}

fn zapp_fold_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    let params = zapp::factory_presets()
        .into_iter()
        .find(|preset| preset.name == "Wavefolder")
        .map_or_else(zapp::default_params, |preset| preset.params);
    zapp_with(options, cx, "zapp-fold", params)
}

/// Duckstep over [`preview_signal`] with a 120 BPM transport rolling, the
/// way the host drives it: the transport each block, the meter frame (its
/// detector in slot 0's input) after it.
fn duckstep_telemetry(
    params: &duckstep::Params,
) -> (
    Vec<SpherePluginHost::audio_bridge::BuiltinMeterFrame>,
    PreviewBlock,
) {
    use waygate::StereoEffect;
    const TEMPO: f64 = 120.0;
    let mut dsp = duckstep::Dsp::new(48_000.0);
    dsp.set_params(params.clone());
    let mut noise = 0x2545_f491u32;
    let mut frames = Vec::with_capacity(PREVIEW_BLOCKS);
    for block in 0..PREVIEW_BLOCKS {
        let start = block * PREVIEW_BLOCK;
        dsp.set_transport(TEMPO as f32, start as f64 / 48_000.0 * TEMPO / 60.0, true);
        for i in 0..PREVIEW_BLOCK {
            let (l, r) = preview_signal(start + i, &mut noise);
            let _ = dsp.process_stereo(l, r);
        }
        let f = dsp.meter_frame();
        let mut slot_in_peak = [0.0; SpherePluginHost::audio_bridge::BUILTIN_RACK_SLOTS];
        slot_in_peak[0] = f.detector_peak;
        frames.push(SpherePluginHost::audio_bridge::BuiltinMeterFrame {
            in_peak: f.in_peak,
            in_rms: f.in_rms,
            out_peak: f.out_peak,
            out_rms: f.out_rms,
            gain_reduction_db: f.gain_reduction_db,
            in_clip: f.in_clip,
            out_clip: f.out_clip,
            slot_in_peak,
            ..Default::default()
        });
    }
    (frames, dsp.telemetry())
}

fn duckstep_with(
    options: WindowOptions,
    cx: &mut App,
    insert: &'static str,
    params: duckstep::Params,
) -> Result<AnyWindowHandle> {
    use crate::components::duckstep_plate::DuckstepPlate;
    let (frames, block) = duckstep_telemetry(&params);
    let json = duckstep::ipc::DuckstepState::new(params).to_json();
    open_plate(
        options,
        cx,
        DuckstepPlate,
        "duckstep",
        insert,
        json,
        frames,
        block,
        |_| {},
    )
}

fn duckstep_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    duckstep_with(options, cx, "duckstep", duckstep::default_params())
}

fn duckstep_shaper_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    let mut params = duckstep::factory_presets()
        .into_iter()
        .find(|preset| preset.name == "Dotted Bounce")
        .map_or_else(duckstep::default_params, |preset| preset.params);
    // Less than full depth, so the curve before depth shows too.
    params.depth = 80.0;
    params.offset = 10.0;
    duckstep_with(options, cx, "duckstep-shaper", params)
}

// ── AirBoost ───────────────────────────────────────────────────────────────

fn airboost_telemetry(
    params: &airboost::Params,
) -> (
    Vec<SpherePluginHost::audio_bridge::BuiltinMeterFrame>,
    PreviewBlock,
) {
    use waygate::StereoEffect;
    let dsp = std::cell::RefCell::new(airboost::Dsp::new(48_000.0));
    dsp.borrow_mut().set_params(params.clone());
    let frames = run_preview(
        |l, r| dsp.borrow_mut().process_stereo(l, r),
        || {
            let f = dsp.borrow().meter_frame();
            SpherePluginHost::audio_bridge::BuiltinMeterFrame {
                in_peak: f.in_peak,
                in_rms: f.in_rms,
                out_peak: f.out_peak,
                out_rms: f.out_rms,
                gain_reduction_db: f.gain_reduction_db,
                in_clip: f.in_clip,
                out_clip: f.out_clip,
                ..Default::default()
            }
        },
    );
    let block = dsp.borrow().telemetry();
    (frames, block)
}

fn airboost_with(
    options: WindowOptions,
    cx: &mut App,
    insert: &'static str,
    params: airboost::Params,
) -> Result<AnyWindowHandle> {
    use crate::components::airboost_plate::AirBoostPlate;
    let (frames, block) = airboost_telemetry(&params);
    let json = airboost::ipc::AirBoostState::new(params).to_json();
    open_plate(
        options,
        cx,
        AirBoostPlate,
        "airboost",
        insert,
        json,
        frames,
        block,
        |_| {},
    )
}

fn airboost_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    airboost_with(options, cx, "airboost", airboost::default_params())
}

/// A bell at 8 kHz with presence, from the Bright Guitar preset.
fn airboost_bell_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    let params = airboost::factory_presets()
        .into_iter()
        .find(|preset| preset.name == "Bright Guitar")
        .map_or_else(airboost::default_params, |preset| preset.params);
    airboost_with(options, cx, "airboost-bell", params)
}

// ── De-Esser ───────────────────────────────────────────────────────────────

/// A voice-like signal at sample `n`: a 180 Hz buzz with its harmonics and,
/// twice a second, a burst of hiss — sibilance — for previews only.
fn deesser_preview_signal(n: usize, noise: &mut u32) -> (f32, f32) {
    const SR: f32 = 48_000.0;
    let t = n as f32 / SR;
    let phase = std::f32::consts::TAU * 180.0 * t;
    let voice = (1..=8)
        .map(|h| (phase * h as f32).sin() / h as f32)
        .sum::<f32>()
        * 0.12
        * (0.7 + 0.3 * (t * 1.3).sin());
    *noise ^= *noise << 13;
    *noise ^= *noise >> 17;
    *noise ^= *noise << 5;
    let white = (*noise as f32 / u32::MAX as f32) * 2.0 - 1.0;
    let burst = ((t * 2.0).fract() * std::f32::consts::PI)
        .sin()
        .max(0.0)
        .powi(6);
    let hiss = white * 0.45 * burst;
    (voice + hiss, voice + hiss * 0.9)
}

fn deesser_telemetry(
    params: &deesser::Params,
) -> (
    Vec<SpherePluginHost::audio_bridge::BuiltinMeterFrame>,
    PreviewBlock,
) {
    use waygate::StereoEffect;
    let mut dsp = deesser::Dsp::new(48_000.0);
    dsp.set_params(params.clone());
    let mut noise = 0x2545_f491u32;
    let mut frames = Vec::with_capacity(PREVIEW_BLOCKS);
    for block in 0..PREVIEW_BLOCKS {
        for i in 0..PREVIEW_BLOCK {
            let (l, r) = deesser_preview_signal(block * PREVIEW_BLOCK + i, &mut noise);
            let _ = dsp.process_stereo(l, r);
        }
        let f = dsp.meter_frame();
        let mut frame = SpherePluginHost::audio_bridge::BuiltinMeterFrame {
            in_peak: f.in_peak,
            in_rms: f.in_rms,
            out_peak: f.out_peak,
            out_rms: f.out_rms,
            gain_reduction_db: f.gain_reduction_db,
            in_clip: f.in_clip,
            out_clip: f.out_clip,
            ..Default::default()
        };
        // As the host does: the detector rides slot 0's input peak.
        frame.slot_in_peak[0] = f.detector_peak;
        frames.push(frame);
    }
    // On to the middle of the next burst, so the snapshot catches a cut.
    let end = PREVIEW_BLOCKS * PREVIEW_BLOCK;
    for n in end..end + 12_000 {
        let (l, r) = deesser_preview_signal(n, &mut noise);
        let _ = dsp.process_stereo(l, r);
    }
    let block = dsp.telemetry();
    (frames, block)
}

fn deesser_with(
    options: WindowOptions,
    cx: &mut App,
    insert: &'static str,
    params: deesser::Params,
) -> Result<AnyWindowHandle> {
    use crate::components::deesser_plate::DeEsserPlate;
    let (frames, block) = deesser_telemetry(&params);
    let json = deesser::ipc::DeEsserState::new(params).to_json();
    open_plate(
        options,
        cx,
        DeEsserPlate,
        "deesser",
        insert,
        json,
        frames,
        block,
        |_| {},
    )
}

fn deesser_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    deesser_with(options, cx, "deesser", deesser::default_params())
}

fn deesser_wide_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    let params = deesser::factory_presets()
        .into_iter()
        .find(|preset| preset.name == "Wideband Bright")
        .map_or_else(deesser::default_params, |preset| preset.params);
    deesser_with(options, cx, "deesser-wide", params)
}

/// EQUZ8's vocal preset with band 5 riding its detector.
fn equz8_vocal_params() -> equz8::Params {
    let mut params = equz8::factory_presets()
        .iter()
        .find(|preset| preset.name.contains("Vocal"))
        .map_or_else(equz8::default_params, |preset| preset.params.clone());
    let band = &mut params.bands[4];
    band.active = true;
    band.band_type = equz8::BandType::Bell;
    band.dynamic = true;
    band.threshold_db = -40.0;
    band.range_db = -6.0;
    band.ratio = 3.0;
    params
}

fn open_equz8(
    options: WindowOptions,
    cx: &mut App,
    insert: &'static str,
    params: equz8::Params,
    selected: usize,
) -> Result<AnyWindowHandle> {
    use crate::components::equz8_plate::Equz8Plate;
    let block = equz8_telemetry(&params);
    let json = equz8::ipc::Equz8State::new(params).to_json();
    open_plate(
        options,
        cx,
        Equz8Plate,
        "equz8",
        insert,
        json,
        Vec::new(),
        block,
        move |editor| editor.ui.select(selected),
    )
}

fn equz8_empty(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_equz8(options, cx, "equz8-empty", equz8::default_params(), 2)
}

fn equz8_vocal(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_equz8(options, cx, "equz8-vocal", equz8_vocal_params(), 4)
}

/// A mix-bus EQUZX: a low cut, a mid scoop, presence, a dynamic de-esser,
/// side air and a high cut.
fn equzx_mix_params() -> equzx::Params {
    use equzx::{BandChannel, BandType};
    let mut params = equzx::default_params();
    let mut band = |slot: usize, kind: BandType, freq: f32, gain: f32, q: f32| {
        let b = &mut params.bands[slot];
        b.active = true;
        b.band_type = kind;
        b.freq = freq;
        b.gain_db = gain;
        b.q = q;
        b.ratio = 2.5;
        b.threshold_db = -30.0;
        b.range_db = -6.0;
    };
    band(0, BandType::LowCut, 30.0, 0.0, 0.71);
    band(1, BandType::Bell, 180.0, -1.8, 2.0);
    band(2, BandType::Bell, 380.0, -3.0, 4.0);
    band(3, BandType::Bell, 2_400.0, 2.0, 1.0);
    band(4, BandType::Bell, 6_800.0, -2.5, 6.0);
    band(5, BandType::HighShelf, 11_000.0, 2.5, 0.71);
    band(6, BandType::HighCut, 19_000.0, 0.0, 0.71);
    params.bands[0].slope = 48.0;
    params.bands[2].channel = BandChannel::Mid;
    params.bands[4].dynamic = true;
    params.bands[4].threshold_db = -45.0;
    params.bands[5].channel = BandChannel::Side;
    params
}

fn open_equzx(
    options: WindowOptions,
    cx: &mut App,
    insert: &'static str,
    params: equzx::Params,
    selected: Option<usize>,
) -> Result<AnyWindowHandle> {
    use crate::components::equzx_plate::EquzxPlate;
    let block = equzx_telemetry(&params);
    let json = equzx::ipc::EquzxState::new(params).to_json();
    open_plate(
        options,
        cx,
        EquzxPlate,
        "equzx",
        insert,
        json,
        Vec::new(),
        block,
        move |editor| editor.ui.select(selected),
    )
}

fn equzx_empty(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_equzx(options, cx, "equzx-empty", equzx::default_params(), None)
}

fn equzx_mix(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_equzx(options, cx, "equzx-mix", equzx_mix_params(), Some(4))
}
// ── The other native plug-in editors, at their defaults ────────────────────

macro_rules! default_editor {
    ($name:ident, $window:ty, $plugin:literal, $title:literal) => {
        fn $name(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
            let handle = cx.open_window(options, |_, cx| {
                cx.new(|cx| {
                    <$window>::new(
                        key($plugin),
                        identity($title),
                        preview_host_ops(),
                        no_close(),
                        cx,
                    )
                })
            })?;
            Ok(handle.into())
        }
    };
}

/// Rodhareist on `page`, its meters filled from a run of the real DSP over
/// the preview drum bus.
fn rodhareist_on(
    page: crate::components::rodhareist_panel::Page,
    edit: fn(&mut rodharerist::Params),
    options: WindowOptions,
    cx: &mut App,
) -> Result<AnyWindowHandle> {
    // On the Pedalboard, the busiest block: the amp, six knobs.
    rodhareist_focused(page, edit, rodharerist::StageKind::Amp, options, cx)
}

/// [`rodhareist_on`], the Pedalboard showing `focus`.
fn rodhareist_focused(
    page: crate::components::rodhareist_panel::Page,
    edit: fn(&mut rodharerist::Params),
    focus: rodharerist::StageKind,
    options: WindowOptions,
    cx: &mut App,
) -> Result<AnyWindowHandle> {
    rodhareist_with(page, edit, focus, |_, _| {}, options, cx)
}

/// What a Rodhareist scene sets on the editor once it is built.
type RodView = fn(
    &mut crate::components::rodhareist_window::RodhareistEditorWindow,
    &mut gpui::Context<crate::components::rodhareist_window::RodhareistEditorWindow>,
);

/// [`rodhareist_focused`], with the editor's view state set by `view`.
fn rodhareist_with(
    page: crate::components::rodhareist_panel::Page,
    edit: fn(&mut rodharerist::Params),
    focus: rodharerist::StageKind,
    view: RodView,
    options: WindowOptions,
    cx: &mut App,
) -> Result<AnyWindowHandle> {
    use waygate::StereoEffect;
    let dsp = std::cell::RefCell::new(rodharerist::Dsp::new(48_000.0));
    let _ = run_preview(
        |l, r| dsp.borrow_mut().process_stereo(l, r),
        Default::default,
    );
    let block = dsp.borrow().telemetry();
    plate::ensure_fonts(cx);
    let handle = cx.open_window(options, move |_, cx| {
        cx.new(|cx| {
            let mut window = crate::components::rodhareist_window::RodhareistEditorWindow::new(
                key("rodharerist"),
                identity("Rodhareist"),
                preview_host_ops(),
                no_close(),
                cx,
            );
            window.show_pads(block);
            window.show_page(page);
            let mut params = window.params_snapshot();
            edit(&mut params);
            window.set_params(params, cx);
            if page == crate::components::rodhareist_panel::Page::Pedalboard {
                window.chain_focus = Some(crate::components::rodhareist_panel::ChainFocus::Block(
                    focus,
                ));
            }
            view(&mut window, cx);
            window
        })
    })?;
    Ok(handle.into())
}

fn rodhareist(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    rodhareist_on(
        crate::components::rodhareist_panel::Page::Home,
        |p| {
            p.input_trim_db = 4.5;
            p.output_trim_db = -3.0;
        },
        options,
        cx,
    )
}

/// Home on the Users tab: the save row and the saved `.pst` list.
fn rodhareist_users(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    rodhareist_with(
        crate::components::rodhareist_panel::Page::Home,
        |_| {},
        rodharerist::StageKind::Amp,
        |window, _| window.ui.preset_tab = crate::components::rodhareist_panel::PresetTab::Users,
        options,
        cx,
    )
}

/// Delay and Reverb moved down to lane 2, which opens Parallel; the
/// Pedalboard shows the Delay.
fn rodhareist_lanes(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    use rodharerist::StageKind;
    rodhareist_focused(
        crate::components::rodhareist_panel::Page::Pedalboard,
        |p| {
            p.stage_row[StageKind::Delay as usize] = 1;
            p.stage_row[StageKind::Reverb as usize] = 1;
            p.row_send[0] = rodharerist::RowSend::Parallel;
        },
        StageKind::Delay,
        options,
        cx,
    )
}

/// The tuner page reading `hz`, with the needle eased for a second of frames
/// so the still shows where it settles.
fn rodhareist_tuning(
    view: RodView,
    options: WindowOptions,
    cx: &mut App,
) -> Result<AnyWindowHandle> {
    rodhareist_with(
        crate::components::rodhareist_panel::Page::Home,
        |p| p.tuner_on = true,
        rodharerist::StageKind::Amp,
        view,
        options,
        cx,
    )
}

fn tune_preview(
    window: &mut crate::components::rodhareist_window::RodhareistEditorWindow,
    hz: f32,
) {
    use crate::components::rodhareist_panel::{pitch_of, tuner_frame};
    use rodharerist::telemetry as tm;
    let mut block = window
        .pads()
        .copied()
        .unwrap_or([0.0; SpherePluginHost::audio_bridge::BUILTIN_PAD_SLOTS]);
    block[tm::TUNER_HZ] = hz;
    block[tm::TUNER_CLARITY] = 0.92;
    window.show_pads(block);
    let start = std::time::Instant::now() - std::time::Duration::from_secs(1);
    for frame in 0..60 {
        let at = start + std::time::Duration::from_millis(16 * frame);
        tuner_frame(&mut window.ui.tuner, pitch_of(hz), at);
    }
}

/// A2 played 19 cents sharp: the needle right of centre, the strobe drifting.
fn rodhareist_tuner(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    rodhareist_tuning(|window, _| tune_preview(window, 111.22), options, cx)
}

/// A2 in tune: the window green, the strobe still.
fn rodhareist_tuner_in_tune(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    rodhareist_tuning(|window, _| tune_preview(window, 110.05), options, cx)
}

/// The Delay in Sync: the time knob reads a division, the toggle says Sync.
fn rodhareist_delay_sync(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    rodhareist_focused(
        crate::components::rodhareist_panel::Page::Pedalboard,
        |p| p.delay_sync = rodharerist::DelaySync::DottedEighth,
        rodharerist::StageKind::Delay,
        options,
        cx,
    )
}

/// Settings › Footswitches on a full chain: sixteen slots to pick from.
fn rodhareist_footswitches(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    rodhareist_with(
        crate::components::rodhareist_panel::Page::Settings,
        |p| {
            p.stage_order =
                std::array::from_fn(|slot| rodharerist::StageKind::ALL.get(slot).copied());
        },
        rodharerist::StageKind::Amp,
        |window, _| {
            window.ui.settings = crate::components::rodhareist_panel::SettingsSection::Footswitches;
        },
        options,
        cx,
    )
}

/// The amp's DRIVE value typed in on the Pedalboard: the field opens under
/// the knob that was double-clicked.
fn rodhareist_knob_entry(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    rodhareist_with(
        crate::components::rodhareist_panel::Page::Pedalboard,
        |_| {},
        rodharerist::StageKind::Amp,
        |window, cx| {
            window.show_knob_entry(
                "amp_gain",
                "DRIVE",
                (0.0, 10.0),
                gpui::point(gpui::px(370.0), gpui::px(628.0)),
                cx,
            );
        },
        options,
        cx,
    )
}

/// Explore on Amp, the Crunch subtab (#407).
fn rodhareist_explore_amp(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    rodhareist_with(
        crate::components::rodhareist_panel::Page::Explore,
        |_| {},
        rodharerist::StageKind::Amp,
        |window, _| {
            use crate::components::rodhareist_panel::Family;
            window.ui.family = Some(Family::Amp);
            window.ui.explore_amp = Some(Family::Crunch);
        },
        options,
        cx,
    )
}

fn rodhareist_explore(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    rodhareist_on(
        crate::components::rodhareist_panel::Page::Explore,
        |_| {},
        options,
        cx,
    )
}

fn rodhareist_pedalboard(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    rodhareist_on(
        crate::components::rodhareist_panel::Page::Pedalboard,
        |_| {},
        options,
        cx,
    )
}

/// The amp running a NAM capture, on a chain long enough to narrow the
/// blocks.
fn rodhareist_nam(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    rodhareist_on(
        crate::components::rodhareist_panel::Page::Pedalboard,
        |p| {
            p.tone_engine = rodharerist::ToneEngineKind::NamCapture;
            let used: Vec<_> = p.stage_order.iter().flatten().copied().collect();
            let spare = rodharerist::StageKind::ALL
                .iter()
                .copied()
                .filter(|kind| !used.contains(kind));
            let free: Vec<usize> = (0..rodharerist::PATH_SLOTS)
                .filter(|&slot| p.stage_order[slot].is_none())
                .take(3)
                .collect();
            for (slot, kind) in free.into_iter().zip(spare) {
                p.stage_order[slot] = Some(kind);
            }
        },
        options,
        cx,
    )
}

/// The Stereo delay focused, with Mod on the left side only and Reverb on
/// the right.
fn rodhareist_stereo(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    use rodharerist::{DelayModel, StageChannel, StageKind};
    rodhareist_focused(
        crate::components::rodhareist_panel::Page::Pedalboard,
        |p| {
            p.delay_model = DelayModel::Stereo;
            p.delay_time_r_ms = 280.0;
            p.stage_channel[StageKind::Mod as usize] = StageChannel::Left;
            p.stage_channel[StageKind::Reverb as usize] = StageChannel::Right;
        },
        StageKind::Delay,
        options,
        cx,
    )
}

/// Settings → Experimental at its defaults: every feature off. The model
/// points at a scratch path, so nothing here can write the real settings file.
fn settings_experimental(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    use crate::components::settings_dialog::SettingsTab;
    settings_on(
        SettingsTab::Experimental,
        crate::settings::SettingsSchema::default(),
        options,
        cx,
    )
}

/// The Audio page with WASAPI Shared selected and Shared Headroom on, so the
/// backend-specific switch is drawn.
fn settings_audio_shared(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    use crate::components::settings_dialog::SettingsTab;
    let mut schema = crate::settings::SettingsSchema::default();
    schema.hardware.audio.driver_type = "WASAPI Shared".to_string();
    schema.hardware.audio.shared_headroom = true;
    settings_on(SettingsTab::Audio, schema, options, cx)
}

/// The Preferences window on `tab`, showing `schema`.
fn settings_on(
    tab: crate::components::settings_dialog::SettingsTab,
    schema: crate::settings::SettingsSchema,
    options: WindowOptions,
    cx: &mut App,
) -> Result<AnyWindowHandle> {
    use crate::components::settings_dialog::SettingsWindow;
    use crate::settings::{SettingsAudioLatencySnapshot, SettingsModel};
    let settings = cx.new(|_| SettingsModel {
        current: schema,
        path: std::env::temp_dir().join("futureboard-preview-settings.json"),
    });
    let handle = cx.open_window(options, move |_, cx| {
        cx.new(|cx| {
            let mut window = SettingsWindow::new(
                settings,
                Vec::new(),
                Vec::new(),
                vec!["WASAPI Shared".to_string()],
                Vec::new(),
                Vec::new(),
                None,
                Arc::new(SettingsAudioLatencySnapshot::default),
                None,
                None,
                None,
                Arc::new(|_, _| {}),
                None,
                None,
                cx,
            );
            window.show_tab(tab);
            window
        })
    })?;
    Ok(handle.into())
}

fn rodhareist_settings(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    rodhareist_on(
        crate::components::rodhareist_panel::Page::Settings,
        |_| {},
        options,
        cx,
    )
}
default_editor!(
    wrap_synth,
    crate::components::wrap_synth_window::WrapSynthEditorWindow,
    "wrapsynth",
    "WrapSynth"
);
/// WrapSynth on one of its pages, with a patch that shows the page off.
fn wrap_synth_on(
    page: crate::components::wrap_synth_plate::SynthPage,
    preset: usize,
    options: WindowOptions,
    cx: &mut App,
) -> Result<AnyWindowHandle> {
    let handle = cx.open_window(options, |_, cx| {
        cx.new(|cx| {
            let mut window = crate::components::wrap_synth_window::WrapSynthEditorWindow::new(
                key("wrapsynth"),
                identity("WrapSynth"),
                preview_host_ops(),
                no_close(),
                cx,
            );
            window.show_for_preview(page, wrapsynth::factory_presets()[preset].params);
            window
        })
    })?;
    Ok(handle.into())
}

fn wrap_synth_modulation(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    // Wobble Bass: a synced LFO on the cutoff and the wavetable.
    wrap_synth_on(
        crate::components::wrap_synth_plate::SynthPage::Modulation,
        6,
        options,
        cx,
    )
}

fn wrap_synth_voice_fx(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    // Supersaw Lead: legato with glide, delay and reverb.
    wrap_synth_on(
        crate::components::wrap_synth_plate::SynthPage::VoiceFx,
        1,
        options,
        cx,
    )
}

default_editor!(
    drum_sampler,
    crate::components::drum_sampler_window::DrumSamplerEditorWindow,
    "drumsampler",
    "Drum Sampler"
);

/// A one-shot's overview as the host sends it: 256 peaks of a hit that
/// rises in `attack` points and falls away over `decay`, roughened by a
/// fixed noise `grain` (0 for a clean tone).
fn drum_preview_peaks(attack: usize, decay: f32, grain: f32, seed: u32) -> Vec<u8> {
    let mut state = seed.max(1);
    (0..drumsampler::WAVEFORM_POINTS)
        .map(|i| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let noise = (state % 1_000) as f32 / 1_000.0;
            let rise = ((i + 1) as f32 / attack.max(1) as f32).min(1.0);
            let fall = (-(i.saturating_sub(attack) as f32) / decay).exp();
            let level = rise * fall * (1.0 - grain + grain * noise);
            (level.clamp(0.0, 1.0) * 255.0) as u8
        })
        .collect()
}

/// Bank A of a kit — kick, snare, hats, toms, a clap and a crash — with
/// the selected snare trimmed, enveloped and filtered, a hi-hat choke
/// group and two pads on outputs of their own.
fn drum_preview_kit() -> (
    drumsampler::Params,
    Vec<Option<crate::components::builtin_plugin_editor::DrumPadWaveform>>,
) {
    use crate::components::builtin_plugin_editor::DrumPadWaveform;
    let mut params = drumsampler::default_params();
    let mut waveforms = vec![None; drumsampler::PADS];
    let kit: [(usize, &str, usize, f32, f32, f32); 10] = [
        (0, "Kick Deep 01.wav", 2, 60.0, 0.05, 0.42),
        (1, "Snare Crack.wav", 2, 34.0, 0.55, 0.38),
        (2, "Hat Closed.wav", 1, 9.0, 0.8, 0.09),
        (3, "Hat Open.wav", 2, 50.0, 0.8, 0.55),
        (4, "Clap Wide.wav", 3, 26.0, 0.7, 0.33),
        (5, "Tom Low.wav", 2, 70.0, 0.1, 0.62),
        (6, "Tom Mid.wav", 2, 55.0, 0.1, 0.5),
        (7, "Rim Shot.wav", 1, 8.0, 0.4, 0.1),
        (8, "Crash Bright 18in.wav", 3, 160.0, 0.75, 2.4),
        (12, "Perc Shaker Loop Long Name.wav", 4, 30.0, 0.85, 0.7),
    ];
    for (seed, (pad, name, attack, decay, grain, seconds)) in kit.into_iter().enumerate() {
        params.pads[pad].sample_name = Some(name.to_string());
        waveforms[pad] = Some(DrumPadWaveform {
            name: name.to_string(),
            frames: (seconds * 48_000.0) as u64,
            channels: if pad == 8 { 2 } else { 1 },
            sample_rate: 48_000,
            peaks: drum_preview_peaks(attack, decay, grain, 7 + seed as u32 * 31),
        });
    }
    let snare = &mut params.pads[1];
    snare.start = 0.04;
    snare.end = 0.78;
    snare.attack_ms = 2.0;
    snare.hold_ms = 30.0;
    snare.decay_ms = 260.0;
    snare.filter_mode = drumsampler::FilterMode::HighPass;
    snare.cutoff_hz = 180.0;
    snare.resonance = 35.0;
    snare.tune_semitones = 2.0;
    snare.gain_db = -3.0;
    snare.pan = -0.15;
    snare.output = 2;
    params.pads[0].output = 1;
    params.pads[2].choke_group = 1;
    params.pads[3].choke_group = 1;
    params.pads[5].muted = true;
    params.pads[7].solo = true;
    params.pads[8].gain_db = -9.5;
    params.master_gain_db = -1.5;
    (params, waveforms)
}

fn drum_sampler_on(
    view: crate::components::drum_sampler_panel::DrumView,
    options: WindowOptions,
    cx: &mut App,
) -> Result<AnyWindowHandle> {
    let handle = cx.open_window(options, move |_, cx| {
        cx.new(|cx| {
            let mut window = crate::components::drum_sampler_window::DrumSamplerEditorWindow::new(
                key("drumsampler"),
                identity("Drum Sampler"),
                preview_host_ops(),
                no_close(),
                cx,
            );
            let (params, waveforms) = drum_preview_kit();
            window.show_for_preview(params, waveforms, 1, view);
            window
        })
    })?;
    Ok(handle.into())
}

fn drum_sampler_kit(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    drum_sampler_on(
        crate::components::drum_sampler_panel::DrumView::Pads,
        options,
        cx,
    )
}

fn drum_sampler_mixer(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    drum_sampler_on(
        crate::components::drum_sampler_panel::DrumView::Mixer,
        options,
        cx,
    )
}
default_editor!(
    quick_sampler,
    crate::components::quick_sampler_window::QuickSamplerEditorWindow,
    "quicksampler",
    "Quick Sampler"
);
default_editor!(
    slicer,
    crate::components::slicer_window::SlicerEditorWindow,
    "slicer",
    "Slicer"
);

/// A synthetic sample's overview, for previews only: `(min, max)` per
/// column from `level`, a 0–1 envelope over the sample's length, with a
/// little grain so it reads as audio.
fn preview_sample(
    name: &str,
    seconds: f64,
    level: impl Fn(f32) -> f32,
) -> crate::components::quick_sampler_panel::SampleSummary {
    const BUCKETS: usize = 1_600;
    let peaks = (0..BUCKETS)
        .map(|i| {
            let t = i as f32 / BUCKETS as f32;
            let grain = 0.8 + 0.2 * (i as f32 * 1.91).sin().abs();
            let high = (level(t) * grain).clamp(0.0, 1.0);
            let low = -high * (0.88 + 0.12 * (i as f32 * 0.73).cos());
            (low, high)
        })
        .collect();
    crate::components::quick_sampler_panel::SampleSummary {
        file_name: name.to_string(),
        sample_rate: 44_100,
        channels: 2,
        seconds,
        peaks: Arc::new(peaks),
    }
}

/// Quick Sampler with a held note loaded — a swell, a sustain with some
/// vibrato, a tail — looping forward over its steady part, through a
/// resonant low-pass and a slow envelope.
fn quick_sampler_loop(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    let sample = preview_sample("Choir Ah C4.wav", 3.69, |t| {
        let swell = (t / 0.08).min(1.0);
        let body = 0.62 + 0.08 * (t * 70.0).sin() + 0.25 * (-(t - 0.08).max(0.0) * 9.0).exp();
        let tail = ((1.0 - t) / 0.18).clamp(0.0, 1.0);
        swell * body * tail
    });
    let params = quicksampler::QuickSamplerParams {
        start: 0.03,
        end: 0.93,
        loop_mode: quicksampler::LoopMode::Forward,
        loop_start: 0.42,
        loop_end: 0.76,
        filter: quicksampler::FilterMode::LowPass,
        cutoff_hz: 2_400.0,
        resonance: 0.45,
        attack_ms: 12.0,
        decay_ms: 380.0,
        sustain: 0.6,
        release_ms: 900.0,
        ..Default::default()
    };
    let handle = cx.open_window(options, move |_, cx| {
        cx.new(|cx| {
            let mut window = crate::components::quick_sampler_window::QuickSamplerEditorWindow::new(
                key("quicksampler"),
                identity("Quick Sampler"),
                preview_host_ops(),
                no_close(),
                cx,
            );
            window.show_for_preview(sample, params);
            window
        })
    })?;
    Ok(handle.into())
}

/// The Slicer with a drum loop cut at its eight hits, the third slice
/// picked; `mode` is how it says it was cut (Beat shows the strip at its
/// widest).
fn slicer_loop_by(
    options: WindowOptions,
    cx: &mut App,
    mode: slicer::SliceMode,
) -> Result<AnyWindowHandle> {
    const HITS: [(f32, f32); 8] = [
        (0.0, 1.0),
        (0.124, 0.55),
        (0.25, 0.9),
        (0.376, 0.5),
        (0.5, 1.0),
        (0.61, 0.45),
        (0.75, 0.85),
        (0.874, 0.6),
    ];
    let sample = preview_sample("Break 136.wav", 1.76, |t| {
        let mut level = 0.03;
        for (at, gain) in HITS {
            if t >= at {
                level += gain * (-(t - at) * 1.76 * 14.0).exp();
            }
        }
        level
    });
    let points: Vec<f32> = HITS.iter().map(|(at, _)| (at - 0.002).max(0.0)).collect();
    let cut = slicer::SlicerParams {
        slice_mode: mode,
        sample_bpm: 136.0,
        ..Default::default()
    };
    let params = slicer::slicing::with_points(cut, &points, 0.0);
    let handle = cx.open_window(options, move |_, cx| {
        cx.new(|cx| {
            let mut window = crate::components::slicer_window::SlicerEditorWindow::new(
                key("slicer"),
                identity("Slicer"),
                preview_host_ops(),
                no_close(),
                cx,
            );
            window.show_for_preview(sample, params, Some(2));
            window
        })
    })?;
    Ok(handle.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_names_are_unique_file_stems() {
        let names: Vec<_> = scenes().iter().map(|scene| scene.name).collect();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len());
        assert!(names
            .iter()
            .all(|name| name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')));
    }

    #[test]
    fn the_preview_spectrum_stays_on_the_analyser_scale() {
        use SpherePluginHost::spectrum::{CEIL_DB, FLOOR_DB};
        let frame = preview_spectrum();
        assert!(frame.iter().all(|db| (FLOOR_DB..=CEIL_DB).contains(db)));
        assert!(frame[10] > frame[120], "it tilts down toward the top");
    }

    #[test]
    fn the_plate_previews_run_the_real_dsps() {
        let (frames, block) = burnlimit_telemetry(&burnlimit_preview_params());
        assert!(frames.iter().any(|frame| frame.gain_reduction_db > 0.0));
        assert!(block[burnlimit::telemetry::SHORT_LUFS] > -60.0);
        let (frames, _) = zapp_telemetry(&zapp::default_params());
        assert!(frames.iter().any(|frame| frame.out_peak > 0.0));
    }
}

/// The Process Explorer with a session's worth of plug-ins.
/// The Background Tasks popup over the status bar's corner, with a job in
/// each state.
fn background_tasks_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    use crate::components::background_tasks::{
        now_ms, BackgroundTaskActions, BackgroundTaskKind, BackgroundTaskProgress,
        BackgroundTaskStatus, BackgroundTaskStore, BackgroundTaskUpdate,
    };
    use crate::theme::Colors;
    use gpui::{div, IntoElement, ParentElement, Render, Styled};
    struct BackgroundTasksPreview {
        store: BackgroundTaskStore,
    }
    impl Render for BackgroundTasksPreview {
        fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let noop: crate::components::BackgroundTaskToggleCb = Arc::new(|_, _, _| {});
            let noop_id: crate::components::BackgroundTaskCancelCb = Arc::new(|_, _, _| {});
            div()
                .relative()
                .size_full()
                .font(crate::theme::ui_font())
                .bg(Colors::surface_window())
                .child(crate::components::background_task_panel(
                    &self.store,
                    BackgroundTaskActions {
                        on_close: noop.clone(),
                        on_cancel: noop_id.clone(),
                        on_dismiss: noop_id,
                        on_clear_finished: noop,
                    },
                ))
        }
    }
    let mut store = BackgroundTaskStore::default();
    let mut add = |id: &str,
                   kind,
                   title: &str,
                   detail: Option<&str>,
                   status,
                   progress: Option<(u32, u32)>,
                   error: Option<&str>,
                   cancellable: bool,
                   ran_for_ms: u64| {
        store.add_or_update(
            id,
            BackgroundTaskUpdate {
                kind,
                title: title.to_string(),
                detail: detail.map(str::to_string),
                status: BackgroundTaskStatus::Running,
                progress: progress
                    .map(|(current, total)| BackgroundTaskProgress { current, total }),
                error: None,
                cancellable,
                parent_id: None,
            },
        );
        if status != BackgroundTaskStatus::Running {
            store.add_or_update(
                id,
                BackgroundTaskUpdate {
                    kind,
                    title: title.to_string(),
                    detail: detail.map(str::to_string),
                    status,
                    progress: None,
                    error: error.map(str::to_string),
                    cancellable: false,
                    parent_id: None,
                },
            );
        }
        if let Some(task) = store.tasks.get_mut(id) {
            let now = now_ms();
            task.started_at = Some(now.saturating_sub(ran_for_ms + 9_000));
            if task.finished_at.is_some() {
                task.finished_at = Some(now.saturating_sub(9_000));
            } else {
                task.started_at = Some(now.saturating_sub(ran_for_ms));
            }
        }
    };
    add(
        "render",
        BackgroundTaskKind::Render,
        "Export Mixdown",
        Some("Lead Vox, Bass, Drums"),
        BackgroundTaskStatus::Running,
        Some((620, 1000)),
        None,
        true,
        74_000,
    );
    add(
        "ara",
        BackgroundTaskKind::Analysis,
        "ARA analysis",
        Some("2 audio sources being analysed"),
        BackgroundTaskStatus::Running,
        None,
        None,
        false,
        12_000,
    );
    add(
        "import",
        BackgroundTaskKind::Import,
        "Import Gtr DI take 3.wav",
        None,
        BackgroundTaskStatus::Failed,
        None,
        Some("Could not read the file: unsupported sample format"),
        false,
        1_500,
    );
    add(
        "scan",
        BackgroundTaskKind::PluginLoad,
        "Scan plug-ins",
        Some("214 plug-ins, 2 failed"),
        BackgroundTaskStatus::Complete,
        None,
        None,
        false,
        41_000,
    );
    add(
        "stems",
        BackgroundTaskKind::AudioProcess,
        "Extract stems: Full Mix",
        None,
        BackgroundTaskStatus::Cancelled,
        None,
        None,
        false,
        6_000,
    );
    store.panel_open = true;
    let handle = cx.open_window(options, |_, cx| {
        cx.new(|_| BackgroundTasksPreview { store })
    })?;
    Ok(handle.into())
}

/// The modeless Virtual Keyboard window, with a chord held.
fn virtual_keyboard_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    use crate::components::virtual_keyboard_window::VirtualKeyboardWindow;
    use crate::components::VirtualKeyboardPanel;
    let keyboard = cx.new(VirtualKeyboardPanel::new);
    keyboard.update(cx, |keyboard, cx| {
        keyboard.set_target_status(Some("Keys — Vital".to_string()), None);
        for note in [60, 64, 67] {
            keyboard.mouse_note_on(note, cx);
        }
    });
    let handle = cx.open_window(options, move |_, cx| {
        cx.new(|cx| {
            VirtualKeyboardWindow::new(keyboard, Arc::new(|_, _| false), Arc::new(|_, _| {}), cx)
        })
    })?;
    Ok(handle.into())
}

fn process_explorer_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    use crate::components::process_explorer_window::{
        PluginKind, PluginProcessRow, ProcessExplorerReading, ProcessExplorerWindow,
    };
    let row =
        |name: &str, vendor: Option<&str>, track: &str, color: u32, kind, pid, dsp, latency| {
            PluginProcessRow {
                insert_id: name.to_string(),
                plugin_name: name.to_string(),
                vendor: vendor.map(str::to_string),
                track_name: track.to_string(),
                track_color: gpui::rgb(color),
                kind,
                host_pid: pid,
                dsp_load: dsp,
                latency_samples: latency,
                bypassed: false,
                problem: None,
            }
        };
    let mut rows = vec![
        row(
            "Melodyne",
            Some("Celemony"),
            "Lead Vox",
            0x4f8fdc,
            PluginKind::Vst3,
            Some(14212),
            Some(0.142),
            0,
        ),
        row(
            "Pro-Q 3",
            Some("FabFilter"),
            "Lead Vox",
            0x4f8fdc,
            PluginKind::Vst3,
            Some(14212),
            Some(0.031),
            0,
        ),
        row(
            "AirBoost",
            None,
            "Lead Vox",
            0x4f8fdc,
            PluginKind::BuiltIn,
            None,
            Some(0.004),
            0,
        ),
        row(
            "Vital",
            Some("Matt Tytel"),
            "Pad",
            0x8f6fdc,
            PluginKind::Vst3,
            Some(15880),
            Some(0.268),
            0,
        ),
        row(
            "Surge XT",
            Some("Surge Synth Team"),
            "Bass",
            0x4fbf7f,
            PluginKind::Clap,
            Some(16004),
            Some(0.087),
            0,
        ),
        row(
            "EchoSpace",
            None,
            "FX Return",
            0x6f8f9f,
            PluginKind::BuiltIn,
            None,
            Some(0.012),
            0,
        ),
        row(
            "BurnLimit",
            None,
            "Master",
            0x9aa0a6,
            PluginKind::BuiltIn,
            None,
            Some(0.009),
            512,
        ),
        row(
            "TDR Nova",
            Some("Tokyo Dawn Records"),
            "Drums",
            0xd08f4f,
            PluginKind::Vst2,
            Some(16112),
            None,
            64,
        ),
    ];
    rows[7].problem = Some("Loading".to_string());
    rows[5].bypassed = true;
    // A session's worth, so the list has to scroll.
    for copy in 0..3 {
        let mut more = rows[..8].to_vec();
        for row in &mut more {
            row.insert_id = format!("{}-{copy}", row.insert_id);
        }
        rows.extend(more);
    }
    let reading = ProcessExplorerReading {
        rows,
        sample_rate: 48_000,
    };
    let handle = cx.open_window(options, |_, cx| {
        cx.new(|cx| ProcessExplorerWindow::preview(reading, cx))
    })?;
    Ok(handle.into())
}

/// The start screen, with this machine's own recent list and settings.
fn welcome_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    use crate::welcome::{WelcomeCallbacks, WelcomeWindow};
    let callbacks = WelcomeCallbacks {
        on_action: std::sync::Arc::new(|_, _, _| {}),
        footer_action: None,
    };
    let handle = cx.open_window(options, |_, cx| {
        cx.new(|cx| WelcomeWindow::new(callbacks, cx.focus_handle()))
    })?;
    Ok(handle.into())
}
