//! Insert editors.
//!
//! A built-in opens Studio's own native editor window, wired to this engine:
//! the editor reads its starting values from the shared parameter mirror
//! (seeded here from the session) and sends every edit back as a wire
//! parameter, exactly as it would inside Studio.
//!
//! A third-party plug-in's editor is the plug-in's own native view, run by
//! the plug-in host and attached into a plain window of ours, sized to what
//! the plug-in asks for.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{AnyWindowHandle, App, Bounds, Context, Pixels, Window};
use livestage_engine::{Command, Id, InsertPlugin, LiveEngine, StripRef};
use sphere_ui_components::components::builtin_plugin_editor::{
    builtin_state_apply, builtin_state_remove,
};
use sphere_ui_components::components::builtin_plugin_editor_window::{
    BuiltinEditorHostOps, BuiltinParamForwarder, PluginInstanceKey,
};
use sphere_ui_components::components::native_plugin_shell::ShellIdentity;

use crate::app::{LiveStageApp, send};

#[derive(Default)]
pub struct Editors {
    windows: HashMap<Id, AnyWindowHandle>,
    #[cfg(feature = "external-plugins")]
    external: HashMap<Id, AnyWindowHandle>,
}

impl Editors {
    /// The window of `insert`'s built-in editor, if one is open.
    #[cfg_attr(not(feature = "ui-preview"), allow(dead_code))]
    pub fn window(&self, insert: Id) -> Option<AnyWindowHandle> {
        self.windows.get(&insert).copied()
    }

    /// Close `insert`'s editor, if one is open.
    pub fn close(&mut self, engine: &mut LiveEngine, insert: Id, cx: &mut App) {
        if let Some(handle) = self.windows.remove(&insert) {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        }
        #[cfg(feature = "external-plugins")]
        if let Some(handle) = self.external.remove(&insert) {
            // The plug-in's view leaves its parent before the parent goes.
            engine.close_external_editor(insert);
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        }
        #[cfg(not(feature = "external-plugins"))]
        let _ = engine;
        builtin_state_remove(&mirror_key(insert));
    }

    pub fn close_all(&mut self, engine: &mut LiveEngine, cx: &mut App) {
        let mut ids: Vec<Id> = self.windows.keys().copied().collect();
        #[cfg(feature = "external-plugins")]
        ids.extend(self.external.keys().copied());
        for id in ids {
            self.close(engine, id, cx);
        }
    }
}

/// The key the built-in editors and their parameter mirror know an insert by.
fn mirror_key(insert: Id) -> String {
    format!("livestage-{insert}")
}

/// Open (or bring forward) the editor of `insert` on `strip`.
pub fn open_editor(
    app: &mut LiveStageApp,
    strip: StripRef,
    insert: Id,
    owner_bounds: Option<Bounds<Pixels>>,
    cx: &mut Context<LiveStageApp>,
) {
    if let Some(handle) = app.editors.windows.get(&insert).copied() {
        if handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
        {
            return;
        }
        app.editors.windows.remove(&insert);
    }
    let session = app.engine.session();
    let Some(core) = session.strip(strip) else {
        return;
    };
    let Some((position, slot)) = core
        .inserts
        .iter()
        .enumerate()
        .find(|(_, slot)| slot.id == insert)
    else {
        return;
    };
    let strip_name = match strip {
        StripRef::Channel(id) => session.channel(id).map(|c| c.name.clone()),
        StripRef::Bus(id) => session.bus(id).map(|b| b.name.clone()),
        StripRef::Matrix(id) => session.matrix(id).map(|m| m.name.clone()),
        StripRef::Master => Some("Master".to_string()),
    }
    .unwrap_or_default();
    let identity = ShellIdentity {
        plugin_name: slot.plugin.display_name(),
        track_name: strip_name,
        insert_number: position + 1,
    };
    match slot.plugin.clone() {
        InsertPlugin::Builtin { stem, params } => {
            open_builtin(app, insert, &stem, &params, identity, owner_bounds, cx);
        }
        #[cfg(feature = "external-plugins")]
        InsertPlugin::External { .. } => {
            open_external(app, insert, identity, cx);
        }
        #[cfg(not(feature = "external-plugins"))]
        InsertPlugin::External { .. } => {
            app.say("This build runs built-in effects only");
        }
    }
}

type Opener<E> = fn(
    Option<Bounds<Pixels>>,
    PluginInstanceKey,
    ShellIdentity,
    BuiltinEditorHostOps,
    Arc<dyn Fn(&mut Window, &mut App)>,
    &mut App,
) -> Result<gpui::WindowHandle<E>, String>;

fn open_builtin(
    app: &mut LiveStageApp,
    insert: Id,
    stem: &str,
    params: &std::collections::BTreeMap<u32, f32>,
    identity: ShellIdentity,
    owner_bounds: Option<Bounds<Pixels>>,
    cx: &mut Context<LiveStageApp>,
) {
    use sphere_ui_components::components::{
        band_panel, dyn_panel, eq_window, fx_window, gate_panel, mix_station_panel,
        rodhareist_window, white_sharp_window,
    };

    let key = PluginInstanceKey {
        track_id: "livestage".to_string(),
        insert_id: mirror_key(insert),
    };
    // The editor starts from what the session holds.
    builtin_state_remove(&key.insert_id);
    for (index, value) in params {
        builtin_state_apply(stem, &key.insert_id, *index, *value);
    }

    let this = cx.entity().downgrade();
    let mirror_stem = stem.to_string();
    let forward: BuiltinParamForwarder = Arc::new(move |key, index, value, cx| {
        builtin_state_apply(&mirror_stem, &key.insert_id, index, value);
        let this = this.clone();
        // This runs inside the editor window's update; the mixer is updated
        // once that is over.
        cx.defer(move |cx| {
            send(
                &this,
                cx,
                Command::SetInsertParam {
                    insert,
                    index,
                    value,
                },
            );
        });
    });
    let host_ops = BuiltinEditorHostOps {
        forward_param: Some(forward),
        ..BuiltinEditorHostOps::default()
    };
    let closed = cx.entity().downgrade();
    let on_close: Arc<dyn Fn(&mut Window, &mut App)> = Arc::new(move |_, cx| {
        let closed = closed.clone();
        cx.defer(move |cx| {
            let _ = closed.update(cx, |app, _| {
                app.editors.windows.remove(&insert);
            });
        });
    });

    fn open<E: 'static>(
        opener: Opener<E>,
        bounds: Option<Bounds<Pixels>>,
        key: PluginInstanceKey,
        identity: ShellIdentity,
        host_ops: BuiltinEditorHostOps,
        on_close: Arc<dyn Fn(&mut Window, &mut App)>,
        cx: &mut App,
    ) -> Result<AnyWindowHandle, String> {
        opener(bounds, key, identity, host_ops, on_close, cx).map(Into::into)
    }

    let args = (owner_bounds, key, identity, host_ops, on_close);
    let (b, k, i, h, c) = args;
    let opened = match stem {
        "equz8" => open(eq_window::open_equz8_editor, b, k, i, h, c, cx),
        "equzx" => open(eq_window::open_equzx_editor, b, k, i, h, c, cx),
        "verbspace" => open(fx_window::open_verbspace_editor, b, k, i, h, c, cx),
        "echospace" => open(fx_window::open_echospace_editor, b, k, i, h, c, cx),
        "fa2a" => open(dyn_panel::open_fa2a_editor, b, k, i, h, c, cx),
        "fa76" => open(dyn_panel::open_fa76_editor, b, k, i, h, c, cx),
        "zcomp" => open(dyn_panel::open_zcomp_editor, b, k, i, h, c, cx),
        "burnlimit" => open(dyn_panel::open_burnlimit_editor, b, k, i, h, c, cx),
        "clipper67" => open(dyn_panel::open_clipper67_editor, b, k, i, h, c, cx),
        "transient" => open(dyn_panel::open_transient_editor, b, k, i, h, c, cx),
        "waygate" => open(gate_panel::open_waygate_editor, b, k, i, h, c, cx),
        "compresser" => open(band_panel::open_compresser_editor, b, k, i, h, c, cx),
        "imager" => open(band_panel::open_imager_editor, b, k, i, h, c, cx),
        "mixstation" => open(mix_station_panel::open_mixstation_editor, b, k, i, h, c, cx),
        "whitesharp" => open(
            white_sharp_window::open_whitesharp_editor,
            b,
            k,
            i,
            h,
            c,
            cx,
        ),
        "rodharerist" => open(rodhareist_window::open_rodhareist_editor, b, k, i, h, c, cx),
        other => Err(format!("{other} has no editor")),
    };
    match opened {
        Ok(handle) => {
            app.editors.windows.insert(insert, handle);
        }
        Err(error) => app.say(error),
    }
}

#[cfg(feature = "external-plugins")]
mod external {
    use gpui::{
        AppContext, Context, IntoElement, ParentElement, Render, Styled, TitlebarOptions, Window,
        WindowBounds, WindowKind, WindowOptions, div, px, size,
    };
    use livestage_engine::Id;
    use livestage_engine::external::ExternalEvent;
    use sphere_ui_components::components::native_plugin_shell::ShellIdentity;
    use sphere_ui_components::theme::Colors;

    use crate::app::LiveStageApp;

    /// The window a third-party editor attaches into. It draws nothing of
    /// its own: the plug-in's view covers the whole client area.
    pub struct ExternalEditorWindow;

    impl Render for ExternalEditorWindow {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .bg(Colors::surface_canvas())
                .text_color(Colors::text_muted())
                .child("Loading the plug-in's editor…")
        }
    }

    /// The native handle of `window`: an HWND on Windows, an X11 window id
    /// on Linux.
    fn native_handle(window: &Window) -> Option<u64> {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        match HasWindowHandle::window_handle(window).ok()?.as_raw() {
            RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as u64),
            RawWindowHandle::Xlib(handle) => Some(handle.window as u64),
            RawWindowHandle::Xcb(handle) => Some(u64::from(handle.window.get())),
            _ => None,
        }
    }

    pub fn open_external(
        app: &mut LiveStageApp,
        insert: Id,
        identity: ShellIdentity,
        cx: &mut Context<LiveStageApp>,
    ) {
        if let Some(handle) = app.editors.external.get(&insert).copied() {
            if handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
            {
                return;
            }
            app.editors.external.remove(&insert);
        }
        if app.engine.insert_state(insert) != livestage_engine::InsertState::Ready {
            app.say(format!("{} is not loaded", identity.plugin_name));
            return;
        }
        let title = format!("{} — {}", identity.plugin_name, identity.track_name);
        let options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some(title.into()),
                appears_transparent: false,
                traffic_light_position: None,
            }),
            window_bounds: Some(WindowBounds::Windowed(gpui::Bounds::centered(
                None,
                size(px(800.0), px(560.0)),
                cx,
            ))),
            kind: WindowKind::Normal,
            is_resizable: true,
            focus: true,
            show: true,
            ..WindowOptions::default()
        };
        let opened = cx.open_window(options, |_, cx| cx.new(|_| ExternalEditorWindow));
        let handle = match opened {
            Ok(handle) => handle,
            Err(error) => {
                app.say(error.to_string());
                return;
            }
        };
        let parent = handle
            .update(cx, |_, window, _| {
                let scale = window.scale_factor();
                let size = window.viewport_size();
                (
                    native_handle(window),
                    (f32::from(size.width) * scale) as u32,
                    (f32::from(size.height) * scale) as u32,
                    (96.0 * scale) as u32,
                )
            })
            .ok();
        let Some((Some(parent), width, height, dpi)) = parent else {
            app.say("This window system cannot host a plug-in editor");
            let _ = handle.update(cx, |_, window, _| window.remove_window());
            return;
        };
        if let Err(error) = app
            .engine
            .open_external_editor(insert, parent, width, height, dpi)
        {
            app.say(error);
            let _ = handle.update(cx, |_, window, _| window.remove_window());
            return;
        }
        app.editors.external.insert(insert, handle.into());
        let this = cx.entity().downgrade();
        let _ = handle.update(cx, |_, window, cx| {
            window.on_window_should_close(cx, move |_, cx| {
                let _ = this.update(cx, |app, _| {
                    app.engine.close_external_editor(insert);
                    app.editors.external.remove(&insert);
                });
                true
            });
        });
    }

    /// Size editor windows to what their plug-ins ask for, and forget the
    /// ones the host closed.
    pub fn handle_external_events(
        app: &mut LiveStageApp,
        events: Vec<ExternalEvent>,
        cx: &mut Context<LiveStageApp>,
    ) {
        for event in events {
            match event {
                ExternalEvent::EditorAttached {
                    insert,
                    width,
                    height,
                }
                | ExternalEvent::EditorResize {
                    insert,
                    width,
                    height,
                } => {
                    if width == 0 || height == 0 {
                        continue;
                    }
                    if let Some(handle) = app.editors.external.get(&insert).copied() {
                        let _ = handle.update(cx, |_, window, _| {
                            let scale = window.scale_factor().max(0.5);
                            window
                                .resize(size(px(width as f32 / scale), px(height as f32 / scale)));
                        });
                        app.engine.resize_external_editor(insert, width, height, 96);
                    }
                }
                ExternalEvent::EditorClosed { insert } => {
                    if let Some(handle) = app.editors.external.remove(&insert) {
                        let _ = handle.update(cx, |_, window, _| window.remove_window());
                    }
                }
                ExternalEvent::LoadFailed { error, .. } => app.say(error),
                ExternalEvent::HostLost => {
                    app.say("The plug-in host stopped: third-party effects are passing through");
                    for (_, handle) in app.editors.external.drain() {
                        let _ = handle.update(cx, |_, window, _| window.remove_window());
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(feature = "external-plugins")]
pub use external::handle_external_events;
#[cfg(feature = "external-plugins")]
use external::open_external;
