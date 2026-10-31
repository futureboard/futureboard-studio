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
use crate::components::fx_model::{presets, FxKind, FxParams};
use crate::components::fx_window::{fx_window_size, FxEditorWindow};
use crate::components::gate_panel::WayGateModel;
use crate::components::mix_station_panel::MixStationModel;
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
            name: "deesser-wide",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: deesser_wide_scene,
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
            name: "whitesharp-vibrato",
            width: WHITESHARP_WINDOW_SIZE.0,
            height: WHITESHARP_WINDOW_SIZE.1 + WHITESHARP_DOCK_H,
            open: whitesharp_vibrato_scene,
        },
        Scene {
            name: "fa2a",
            width: DynKind::Fa2a.window_size().0,
            height: DynKind::Fa2a.window_size().1,
            open: fa2a_scene,
        },
        Scene {
            name: "fa76",
            width: DynKind::Fa76.window_size().0,
            height: DynKind::Fa76.window_size().1,
            open: fa76_scene,
        },
        Scene {
            name: "zcomp",
            width: DynKind::Zcomp.window_size().0,
            height: DynKind::Zcomp.window_size().1,
            open: zcomp_scene,
        },
        Scene {
            name: "burnlimit",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: burnlimit_scene,
        },
        Scene {
            name: "clipper67",
            width: DynKind::Clipper.window_size().0,
            height: DynKind::Clipper.window_size().1,
            open: clipper67_scene,
        },
        Scene {
            name: "transient",
            width: DynKind::Transient.window_size().0,
            height: DynKind::Transient.window_size().1,
            open: transient_scene,
        },
        Scene {
            name: "waygate",
            width: WayGateModel.window_size().0,
            height: WayGateModel.window_size().1,
            open: waygate_scene,
        },
        Scene {
            name: "waygate-duck",
            width: WayGateModel.window_size().0,
            height: WayGateModel.window_size().1,
            open: waygate_duck_scene,
        },
        Scene {
            name: "compressor-single",
            width: BandKind::Comp.window_size().0,
            height: BandKind::Comp.window_size().1,
            open: compressor_single,
        },
        Scene {
            name: "compressor-multi",
            width: BandKind::Comp.window_size().0,
            height: BandKind::Comp.window_size().1,
            open: compressor_multi,
        },
        Scene {
            name: "imager",
            width: BandKind::Imager.window_size().0,
            height: BandKind::Imager.window_size().1,
            open: imager_scene,
        },
        Scene {
            name: "imager-stereoize",
            width: BandKind::Imager.window_size().0,
            height: BandKind::Imager.window_size().1,
            open: imager_stereoize,
        },
        Scene {
            name: "mixstation",
            width: MixStationModel.window_size().0,
            height: MixStationModel.window_size().1,
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
            width: MixStationModel.window_size().0,
            height: MixStationModel.window_size().1,
            open: mixstation_empty,
        },
        Scene {
            name: "rodhareist",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: rodhareist,
        },
        Scene {
            name: "rodhareist-explore",
            width: plate::WINDOW_SIZE.0,
            height: plate::WINDOW_SIZE.1,
            open: rodhareist_explore,
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
            name: "wrapsynth",
            width: 1_280.0,
            height: 900.0,
            open: wrap_synth,
        },
        Scene {
            name: "wrapsynth-modulation",
            width: 1_280.0,
            height: 1_040.0,
            open: wrap_synth_modulation,
        },
        Scene {
            name: "wrapsynth-voice-fx",
            width: 1_280.0,
            height: 900.0,
            open: wrap_synth_voice_fx,
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
        Scene {
            name: "mixer",
            width: 1_680.0,
            height: 760.0,
            open: mixer::mixer,
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
        DynParams::Fa2a(p) => fa2a::ipc::Fa2aState::new(p).to_json(),
        DynParams::Fa76(p) => fa76::ipc::Fa76State::new(p).to_json(),
        DynParams::Zcomp(p) => zcomp::ipc::ZcompState::new(p).to_json(),
        DynParams::Clipper(p) => clipper67::ipc::Clipper67State::new(p).to_json(),
        DynParams::Transient(p) => transient::ipc::TransientState::new(p).to_json(),
    }
}

fn open_dyn(
    options: WindowOptions,
    cx: &mut App,
    kind: DynKind,
    preset: &str,
    reduction_db: f32,
) -> Result<AnyWindowHandle> {
    let params = dyn_preset(kind, preset);
    open_kit(
        options,
        cx,
        kind,
        kind.key(),
        kind.key(),
        dyn_json(&params),
        reduction_db,
        |_| {},
    )
}

fn fa2a_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_dyn(options, cx, DynKind::Fa2a, "Vocal Level", 6.0)
}

fn fa76_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_dyn(options, cx, DynKind::Fa76, "Vocal Catch", 9.0)
}

fn zcomp_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_dyn(options, cx, DynKind::Zcomp, "Vocal Catch", 5.0)
}

fn clipper67_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_dyn(options, cx, DynKind::Clipper, "Hybrid Glue", 3.0)
}

fn transient_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_dyn(options, cx, DynKind::Transient, "Punch Up", 4.0)
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
    if let Ok(json) = waygate::ipc::WayGateState::new(params.clone()).to_json() {
        seed("waygate", insert, json);
    }
    let frames = waygate_frames(&params);
    let source = frames.clone();
    let tick = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let host_ops = BuiltinEditorHostOps {
        meter_source: Some(Arc::new(move |_| {
            let at = tick.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            source.get(at % source.len()).copied()
        })),
        ..preview_host_ops()
    };
    let handle = cx.open_window(options, move |_, cx| {
        cx.new(|cx| {
            let window = KitWindow::new(
                WayGateModel,
                key(insert),
                identity("WayGate"),
                host_ops,
                no_close(),
                cx,
            );
            {
                let mut live = window.live().borrow_mut();
                for frame in frames.iter() {
                    live.show(*frame);
                }
            }
            window
        })
    })?;
    Ok(handle.into())
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

fn open_band(
    options: WindowOptions,
    cx: &mut App,
    kind: BandKind,
    insert: &'static str,
    preset: &str,
) -> Result<AnyWindowHandle> {
    use crate::components::band_model::BandParams;
    let (plugin, json) = match band_preset(kind, preset) {
        BandParams::Comp(p) => (
            "compresser",
            compresser::ipc::CompresserState::new(p).to_json(),
        ),
        BandParams::Imager(p) => ("imager", imager::ipc::ImagerState::new(p).to_json()),
    };
    open_kit(options, cx, kind, plugin, insert, json, 5.0, |_| {})
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

fn open_imager(
    options: WindowOptions,
    cx: &mut App,
    insert: &'static str,
    preset: &str,
    scope: crate::components::plugin_live::ScopeMode,
) -> Result<AnyWindowHandle> {
    let crate::components::band_model::BandParams::Imager(p) =
        band_preset(BandKind::Imager, preset)
    else {
        unreachable!("an Imager preset is Imager params");
    };
    let json = imager::ipc::ImagerState::new(p).to_json();
    open_kit(
        options,
        cx,
        BandKind::Imager,
        "imager",
        insert,
        json,
        5.0,
        move |editor| {
            editor.ui.scope = scope;
            editor.ui.link_bands = true;
        },
    )
}

fn imager_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    use crate::components::plugin_live::ScopeMode;
    open_imager(
        options,
        cx,
        "imager",
        "Master Polish",
        ScopeMode::PolarSample,
    )
}

fn imager_stereoize(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    use crate::components::plugin_live::ScopeMode;
    open_imager(
        options,
        cx,
        "imager-stereoize",
        "Natural Space",
        ScopeMode::PolarLevel,
    )
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
    let json = mixstation::ipc::MixStationState::new(params).to_json();
    open_kit(
        options,
        cx,
        MixStationModel,
        "mixstation",
        insert,
        json,
        3.0,
        move |editor| editor.ui.selected = select,
    )
}

fn mixstation_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_mix(options, cx, "mixstation", "Mix Bus Polish", Some(2))
}

fn mixstation_empty(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_mix(options, cx, "mixstation-empty", "Empty Rack", None)
}

// ── Inspector MIDI Input menu ───────────────────────────────────────────────

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
    let burst = ((t * 2.0).fract() * std::f32::consts::PI).sin().max(0.0).powi(6);
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
    page: crate::components::wrap_synth_panel::SynthPage,
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
        crate::components::wrap_synth_panel::SynthPage::Modulation,
        6,
        options,
        cx,
    )
}

fn wrap_synth_voice_fx(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    // Supersaw Lead: legato with glide, delay and reverb.
    wrap_synth_on(
        crate::components::wrap_synth_panel::SynthPage::VoiceFx,
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
    fn the_plate_previews_run_the_real_dsps() {
        let (frames, block) = burnlimit_telemetry(&burnlimit_preview_params());
        assert!(frames.iter().any(|frame| frame.gain_reduction_db > 0.0));
        assert!(block[burnlimit::telemetry::SHORT_LUFS] > -60.0);
        let (frames, _) = zapp_telemetry(&zapp::default_params());
        assert!(frames.iter().any(|frame| frame.out_peak > 0.0));
    }
}
