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
use crate::components::eq_model::{EqKind, EqParams, Placement, Shape};
use crate::components::eq_window::EqEditorWindow;
use crate::components::fx_model::{presets, FxKind, FxParams};
use crate::components::fx_window::{fx_window_size, FxEditorWindow};
use crate::components::gate_panel::WayGateModel;
use crate::components::mix_station_panel::MixStationModel;
use crate::components::native_plugin_shell::ShellIdentity;
use crate::components::plugin_kit::{KitModel, KitWindow};
use crate::components::white_sharp_window::{WhiteSharpWindow, WHITESHARP_WINDOW_SIZE};

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
            name: "whitesharp",
            width: WHITESHARP_WINDOW_SIZE.0,
            height: WHITESHARP_WINDOW_SIZE.1,
            open: whitesharp_scene,
        },
        Scene {
            name: "whitesharp-live",
            width: WHITESHARP_WINDOW_SIZE.0,
            height: WHITESHARP_WINDOW_SIZE.1,
            open: whitesharp_live_scene,
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
            width: DynKind::BurnLimit.window_size().0,
            height: DynKind::BurnLimit.window_size().1,
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

// ── WhiteSharp ─────────────────────────────────────────────────────────────

fn whitesharp_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    whitesharp_window(options, cx, whitesharp::LatencyMode::Quality)
}

/// The same insert on the live path: no added delay, formant controls
/// unavailable.
fn whitesharp_live_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    whitesharp_window(options, cx, whitesharp::LatencyMode::Live)
}

fn whitesharp_window(
    options: WindowOptions,
    cx: &mut App,
    latency: whitesharp::LatencyMode,
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
        seed("whitesharp", "whitesharp", json);
    }
    let handle = cx.open_window(options, move |_, cx| {
        cx.new(|cx| {
            let window = WhiteSharpWindow::new(
                key("whitesharp"),
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
        DynParams::BurnLimit(p) => burnlimit::ipc::BurnLimitState::new(p).to_json(),
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

fn burnlimit_scene(options: WindowOptions, cx: &mut App) -> Result<AnyWindowHandle> {
    open_dyn(options, cx, DynKind::BurnLimit, "Loud Punch", 4.0)
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
