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

use crate::components::builtin_plugin_editor::builtin_state_seed;
use crate::components::builtin_plugin_editor_window::{BuiltinEditorHostOps, PluginInstanceKey};
use crate::components::eq_model::{EqKind, EqParams, Placement, Shape};
use crate::components::eq_window::EqEditorWindow;
use crate::components::fx_model::{presets, FxKind, FxParams};
use crate::components::fx_window::{fx_window_size, FxEditorWindow};
use crate::components::native_plugin_shell::ShellIdentity;

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

/// Moves `window` off every desktop and shows it there without activating
/// it: it lays out and draws like any window — at the scale of the display
/// it opened on — while no one sees it and focus stays where it was.
#[cfg(target_os = "windows")]
pub fn tuck_away(window: &Window) -> std::result::Result<(), String> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, ShowWindow, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_SHOWNOACTIVATE,
    };
    const OFF_DESKTOP: i32 = -24_000;
    let handle = HasWindowHandle::window_handle(window)
        .map_err(|error| format!("no window handle: {error}"))?;
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return Err("not a Win32 window".to_string());
    };
    let hwnd = HWND(win32.hwnd.get() as *mut _);
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
            name: "equz8-empty",
            width: 980.0,
            height: 720.0,
            open: equz8_empty,
        },
        Scene {
            name: "equz8-vocal",
            width: 980.0,
            height: 720.0,
            open: equz8_vocal,
        },
        Scene {
            name: "equz8-bypassed",
            width: 980.0,
            height: 720.0,
            open: equz8_bypassed,
        },
        Scene {
            name: "equzx-empty",
            width: 1_080.0,
            height: 740.0,
            open: equzx_empty,
        },
        Scene {
            name: "equzx-mix",
            width: 1_080.0,
            height: 740.0,
            open: equzx_mix,
        },
        Scene {
            name: "equzx-side-view",
            width: 1_080.0,
            height: 740.0,
            open: equzx_side_view,
        },
        Scene {
            name: "verbspace-hall",
            width: fx_window_size(FxKind::Verb).0,
            height: fx_window_size(FxKind::Verb).1,
            open: verbspace_hall,
        },
        Scene {
            name: "verbspace-plate",
            width: fx_window_size(FxKind::Verb).0,
            height: fx_window_size(FxKind::Verb).1,
            open: verbspace_plate,
        },
        Scene {
            name: "echospace-default",
            width: fx_window_size(FxKind::Echo).0,
            height: fx_window_size(FxKind::Echo).1,
            open: echospace_default,
        },
        Scene {
            name: "echospace-synced",
            width: fx_window_size(FxKind::Echo).0,
            height: fx_window_size(FxKind::Echo).1,
            open: echospace_synced,
        },
        Scene {
            name: "echospace-ambient",
            width: fx_window_size(FxKind::Echo).0,
            height: fx_window_size(FxKind::Echo).1,
            open: echospace_ambient,
        },
        Scene {
            name: "rodhareist",
            width: 1_280.0,
            height: 880.0,
            open: rodhareist,
        },
        Scene {
            name: "wrapsynth",
            width: 1_280.0,
            height: 900.0,
            open: wrap_synth,
        },
        Scene {
            name: "drum-sampler",
            width: 1_200.0,
            height: 820.0,
            open: drum_sampler,
        },
        Scene {
            name: "quick-sampler",
            width: 1_100.0,
            height: 760.0,
            open: quick_sampler,
        },
        Scene {
            name: "slicer",
            width: 1_100.0,
            height: 760.0,
            open: slicer,
        },
    ]
}

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

fn seed_eq(insert: &str, params: &EqParams) {
    let json = match params {
        EqParams::Z8(p) => equz8::ipc::Equz8State::new(p.clone()).to_json(),
        EqParams::Zx(p) => equzx::ipc::EquzxState::new(p.clone()).to_json(),
    };
    let plugin = match params.kind() {
        EqKind::Z8 => "equz8",
        EqKind::Zx => "equzx",
    };
    if let Ok(json) = json {
        seed(plugin, insert, json);
    }
}

fn open_eq(
    options: WindowOptions,
    cx: &mut App,
    kind: EqKind,
    insert: &'static str,
    then: impl FnOnce(&mut EqEditorWindow, &mut Context<EqEditorWindow>) + 'static,
) -> Result<AnyWindowHandle> {
    let title = kind.title();
    let handle = cx.open_window(options, move |_, cx| {
        cx.new(|cx| {
            let mut editor = EqEditorWindow::new(
                kind,
                key(insert),
                identity(title),
                preview_host_ops(),
                no_close(),
                cx,
            );
            then(&mut editor, cx);
            editor
        })
    })?;
    Ok(handle.into())
}

// ── VerbSpace and EchoSpace scenes ─────────────────────────────────────────

/// Seeds params and opens the editor on them.
fn open_fx(
    options: WindowOptions,
    cx: &mut App,
    insert: &'static str,
    params: FxParams,
) -> Result<AnyWindowHandle> {
    let kind = params.kind();
    let (plugin, json) = match &params {
        FxParams::Verb(p) => (
            "verbspace",
            verbspace::ipc::VerbspaceState::new(p.clone()).to_json(),
        ),
        FxParams::Echo(p) => (
            "echospace",
            echospace::ipc::EchospaceState::new(p.clone()).to_json(),
        ),
    };
    if let Ok(json) = json {
        seed(plugin, insert, json);
    }
    let handle = cx.open_window(options, move |_, cx| {
        cx.new(|cx| {
            FxEditorWindow::new(
                kind,
                key(insert),
                identity(kind.title()),
                preview_host_ops(),
                no_close(),
                cx,
            )
        })
    })?;
    Ok(handle.into())
}

/// Factory preset `name` of `kind`.
fn fx_preset(kind: FxKind, name: &str) -> FxParams {
    presets(kind)
        .iter()
        .find(|preset| preset.name == name)
        .map(|preset| preset.params.clone())
        .unwrap_or_else(|| FxParams::defaults(kind))
}

fn verbspace_hall(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_fx(
        options,
        cx,
        "verbspace-hall",
        FxParams::defaults(FxKind::Verb),
    )
}

fn verbspace_plate(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_fx(
        options,
        cx,
        "verbspace-plate",
        fx_preset(FxKind::Verb, "Bright Plate"),
    )
}

fn echospace_default(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_fx(
        options,
        cx,
        "echospace-default",
        FxParams::defaults(FxKind::Echo),
    )
}

fn echospace_synced(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_fx(
        options,
        cx,
        "echospace-synced",
        fx_preset(FxKind::Echo, "Dotted Eighth"),
    )
}

fn echospace_ambient(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_fx(
        options,
        cx,
        "echospace-ambient",
        fx_preset(FxKind::Echo, "Ambient Wash"),
    )
}

// ── EQ scenes ──────────────────────────────────────────────────────────────

fn equz8_empty(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    seed_eq("equz8-empty", &EqParams::defaults(EqKind::Z8));
    open_eq(options, cx, EqKind::Z8, "equz8-empty", |_, _| {})
}

/// The "Vocal Clarity" preset with its 4.8 kHz band made a dynamic
/// de-esser and selected, so the dynamics and their reach show.
fn equz8_vocal_params() -> EqParams {
    let preset = equz8::factory_presets()
        .iter()
        .find(|preset| preset.name == "Vocal Clarity")
        .map(|preset| preset.params.clone())
        .unwrap_or_else(equz8::default_params);
    let mut params = EqParams::Z8(preset);
    let mut band = params.band(5);
    band.dynamic = true;
    band.threshold_db = -30.0;
    band.range_db = -6.0;
    band.attack_ms = 2.0;
    band.release_ms = 80.0;
    params.set_band(5, band);
    params
}

fn equz8_vocal(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    seed_eq("equz8-vocal", &equz8_vocal_params());
    open_eq(options, cx, EqKind::Z8, "equz8-vocal", |editor, cx| {
        editor.select(5, cx)
    })
}

fn equz8_bypassed(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    let mut params = equz8_vocal_params();
    params.set_power(false);
    seed_eq("equz8-bypassed", &params);
    open_eq(options, cx, EqKind::Z8, "equz8-bypassed", |_, _| {})
}

fn equzx_empty(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    seed_eq("equzx-empty", &EqParams::defaults(EqKind::Zx));
    open_eq(options, cx, EqKind::Zx, "equzx-empty", |_, _| {})
}

/// A mix-bus layout: a steep low cut, a mid scoop on the mid only, a
/// presence lift, a side-only air shelf and a dynamic de-esser.
fn equzx_mix_params() -> EqParams {
    use crate::components::eq_model::new_band;
    let mut params = EqParams::defaults(EqKind::Zx);
    let mut cut = new_band(EqKind::Zx, 32.0, 0.0, Placement::Stereo);
    cut.shape = Shape::LowCut;
    cut.slope = 48.0;
    params.set_band(0, cut);
    let mut scoop = new_band(EqKind::Zx, 380.0, -3.5, Placement::Mid);
    scoop.q = 1.4;
    params.set_band(1, scoop);
    params.set_band(2, new_band(EqKind::Zx, 2_400.0, 2.0, Placement::Stereo));
    let mut air = new_band(EqKind::Zx, 9_000.0, 4.0, Placement::Side);
    air.shape = Shape::HighShelf;
    air.q = 0.7;
    params.set_band(3, air);
    let mut ess = new_band(EqKind::Zx, 6_500.0, 0.0, Placement::Stereo);
    ess.q = 3.0;
    ess.dynamic = true;
    ess.threshold_db = -28.0;
    ess.range_db = -7.0;
    params.set_band(4, ess);
    let mut top = new_band(EqKind::Zx, 18_000.0, 0.0, Placement::Stereo);
    top.shape = Shape::HighCut;
    top.slope = 24.0;
    params.set_band(5, top);
    params
}

fn equzx_mix(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    seed_eq("equzx-mix", &equzx_mix_params());
    open_eq(options, cx, EqKind::Zx, "equzx-mix", |editor, cx| {
        editor.select(4, cx)
    })
}

fn equzx_side_view(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    seed_eq("equzx-side-view", &equzx_mix_params());
    open_eq(options, cx, EqKind::Zx, "equzx-side-view", |editor, cx| {
        editor.set_view(Placement::Side, cx);
        editor.select(3, cx);
    })
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

default_editor!(
    rodhareist,
    crate::components::rodhareist_window::RodhareistEditorWindow,
    "rodharerist",
    "Rodhareist"
);
default_editor!(
    wrap_synth,
    crate::components::wrap_synth_window::WrapSynthEditorWindow,
    "wrapsynth",
    "WrapSynth"
);
default_editor!(
    drum_sampler,
    crate::components::drum_sampler_window::DrumSamplerEditorWindow,
    "drumsampler",
    "Drum Sampler"
);
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
    fn the_eq_scenes_hold_valid_params() {
        for params in [equz8_vocal_params(), equzx_mix_params()] {
            assert!(params
                .listed()
                .iter()
                .any(|i| params.band(*i).dynamics_live()));
        }
        assert!(equzx_mix_params().uses_mid_side());
    }
}
