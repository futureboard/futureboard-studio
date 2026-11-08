//! The preset menu in a built-in editor's identity strip: the plug-in's own
//! `.pst` presets, Save As…, Import… and Show Folder — the same list and the
//! same files the external plug-ins' editors use.
//!
//! The menu owns no preset logic. It lists the plug-in's folder
//! (`SpherePluginHost::preset::list_user_presets`), asks for paths through the
//! OS dialogs parented to its own window, and queues what was chosen; the
//! studio drains the queue ([`take_requests`]) and does the loading and saving
//! against the insert, the mirror, the host and the undo history.

use std::path::PathBuf;
use std::sync::Mutex;

use gpui::prelude::FluentBuilder;
use gpui::{
    anchored, deferred, div, point, px, svg, Anchor, AnyElement, App, Context, FocusHandle,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render,
    StatefulInteractiveElement, Styled, Window,
};

use crate::assets;
use crate::components::builtin_plugin_editor_window::PluginInstanceKey;
use crate::components::native_plugin_shell::ShellIdentity;
use crate::components::plugin_editor_chrome::{
    filtered_presets, preset_menu_size, render_preset_popover, PluginEditorAction,
    PluginEditorChrome, PresetPopoverView,
};
use crate::theme::{self, Colors};

/// What a built-in editor asked of its presets.
#[derive(Debug, Clone, PartialEq)]
pub enum BuiltinPresetRequest {
    /// Load this `.pst` into the insert.
    Load(PathBuf),
    /// Save the insert's current sound to this file.
    SaveTo(PathBuf),
    /// Load a `.pst` from elsewhere and keep a copy in the plug-in's folder.
    Import(PathBuf),
    /// Show the plug-in's preset folder in the file manager.
    RevealFolder,
}

static REQUESTS: Mutex<Vec<(PluginInstanceKey, BuiltinPresetRequest)>> = Mutex::new(Vec::new());

fn push(key: &PluginInstanceKey, request: BuiltinPresetRequest) {
    if let Ok(mut queue) = REQUESTS.lock() {
        queue.push((key.clone(), request));
    }
}

/// Everything the built-in editors asked for since the last call, oldest
/// first. Drained by the studio's editor poll.
pub fn take_requests() -> Vec<(PluginInstanceKey, BuiltinPresetRequest)> {
    REQUESTS
        .lock()
        .map(|mut queue| std::mem::take(&mut *queue))
        .unwrap_or_default()
}

/// One built-in editor's preset menu: its trigger, and the list while open.
pub struct BuiltinPresetMenu {
    key: PluginInstanceKey,
    /// The plug-in id a preset must carry, and the name its folder has.
    /// Empty where nothing drains the requests (LiveStage): no menu then.
    plugin_id: String,
    plugin_name: String,
    open: bool,
    /// The folder's presets as of opening: `(name, file)`.
    presets: Vec<(String, PathBuf)>,
    view: PresetPopoverView,
    focus: FocusHandle,
}

impl BuiltinPresetMenu {
    pub fn new(key: PluginInstanceKey, identity: &ShellIdentity, cx: &mut Context<Self>) -> Self {
        Self {
            key,
            plugin_id: identity.plugin_id.clone(),
            plugin_name: identity.preset_plugin_name.clone(),
            open: false,
            presets: Vec::new(),
            view: PresetPopoverView::default(),
            focus: cx.focus_handle(),
        }
    }

    /// The editor took another insert.
    pub fn rebind(
        &mut self,
        key: PluginInstanceKey,
        identity: &ShellIdentity,
        cx: &mut Context<Self>,
    ) {
        self.key = key;
        self.plugin_id = identity.plugin_id.clone();
        self.plugin_name = identity.preset_plugin_name.clone();
        self.open = false;
        cx.notify();
    }

    fn available(&self) -> bool {
        !self.plugin_id.is_empty() && !self.plugin_name.trim().is_empty()
    }

    fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open {
            self.close(cx);
            return;
        }
        self.presets =
            SpherePluginHost::preset::list_user_presets(&self.plugin_id, &self.plugin_name);
        self.view = PresetPopoverView::default();
        self.open = true;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        if self.open {
            self.open = false;
            cx.notify();
        }
    }

    fn names(&self) -> Vec<String> {
        self.presets.iter().map(|(name, _)| name.clone()).collect()
    }

    fn dir(&self) -> Option<PathBuf> {
        SpherePluginHost::preset::user_preset_dir(&self.plugin_name)
    }

    fn act(&mut self, action: PluginEditorAction, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            PluginEditorAction::SelectPreset(index) => {
                if let Some((_, path)) = self.presets.get(index) {
                    push(&self.key, BuiltinPresetRequest::Load(path.clone()));
                }
            }
            PluginEditorAction::SavePreset => self.save_through_dialog(window, cx),
            PluginEditorAction::ImportPreset => self.import_through_dialog(window, cx),
            PluginEditorAction::RevealPresetFolder => {
                push(&self.key, BuiltinPresetRequest::RevealFolder);
            }
            _ => {}
        }
        self.close(cx);
    }

    /// Asks where to save, starting in the plug-in's folder with the next
    /// free `Preset n`, and queues the save. The dialog belongs to this
    /// editor, so it opens above it.
    #[cfg(feature = "native-dialogs")]
    fn save_through_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.dir() else {
            return;
        };
        let _ = std::fs::create_dir_all(&dir);
        let names = self.names();
        let suggested = (names.len() + 1..)
            .map(|n| format!("Preset {n}"))
            .find(|name| !names.contains(name))
            .unwrap_or_else(|| "Preset".to_string());
        let dialog = rfd::AsyncFileDialog::new()
            .set_title(format!("Save {} Preset", self.plugin_name))
            .set_directory(&dir)
            .set_file_name(format!("{suggested}.pst"))
            .add_filter("Futureboard Preset", &["pst"])
            .set_parent(window);
        let key = self.key.clone();
        cx.spawn(async move |_, _| {
            let Some(handle) = dialog.save_file().await else {
                return;
            };
            push(
                &key,
                BuiltinPresetRequest::SaveTo(with_pst(handle.path().to_path_buf())),
            );
        })
        .detach();
    }

    #[cfg(not(feature = "native-dialogs"))]
    fn save_through_dialog(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}

    /// Asks for a `.pst` to import and queues it.
    #[cfg(feature = "native-dialogs")]
    fn import_through_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut dialog = rfd::AsyncFileDialog::new()
            .set_title("Import Preset")
            .add_filter("Futureboard Preset", &["pst"])
            .set_parent(window);
        if let Some(dir) = self.dir() {
            let _ = std::fs::create_dir_all(&dir);
            dialog = dialog.set_directory(dir);
        }
        let key = self.key.clone();
        cx.spawn(async move |_, _| {
            let Some(handle) = dialog.pick_file().await else {
                return;
            };
            push(
                &key,
                BuiltinPresetRequest::Import(handle.path().to_path_buf()),
            );
        })
        .detach();
    }

    #[cfg(not(feature = "native-dialogs"))]
    fn import_through_dialog(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}

    fn move_highlight(&mut self, delta: i32, cx: &mut Context<Self>) {
        let visible = filtered_presets(&self.names(), &self.view.filter);
        if visible.is_empty() {
            return;
        }
        let count = visible.len() as i32;
        let position = self
            .view
            .highlighted
            .and_then(|index| visible.iter().position(|&candidate| candidate == index));
        let next = match position {
            None if delta > 0 => 0,
            None => visible.len() - 1,
            Some(current) => (((current as i32 + delta) % count + count) % count) as usize,
        };
        self.view.highlighted = Some(visible[next]);
        cx.notify();
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        let visible = filtered_presets(&self.names(), &self.view.filter);
        if !self
            .view
            .highlighted
            .is_some_and(|index| visible.contains(&index))
        {
            self.view.highlighted = visible.first().copied();
        }
        cx.notify();
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.open {
            return;
        }
        let modifiers = event.keystroke.modifiers;
        match event.keystroke.key.as_str() {
            "escape" => {
                cx.stop_propagation();
                if self.view.filter.is_empty() {
                    self.close(cx);
                } else {
                    self.view.filter.clear();
                    self.refilter(cx);
                }
            }
            "down" => {
                cx.stop_propagation();
                self.move_highlight(1, cx);
            }
            "up" => {
                cx.stop_propagation();
                self.move_highlight(-1, cx);
            }
            "enter" => {
                cx.stop_propagation();
                let visible = filtered_presets(&self.names(), &self.view.filter);
                let pick = self
                    .view
                    .highlighted
                    .filter(|index| visible.contains(index))
                    .or_else(|| visible.first().copied());
                if let Some(index) = pick {
                    self.act(PluginEditorAction::SelectPreset(index), window, cx);
                }
            }
            "backspace" => {
                cx.stop_propagation();
                if self.view.filter.pop().is_some() {
                    self.refilter(cx);
                }
            }
            _ if !modifiers.control && !modifiers.alt && !modifiers.platform => {
                let Some(typed) = event.keystroke.key_char.as_deref() else {
                    return;
                };
                if !typed.is_empty() && typed.chars().all(|c| !c.is_control()) {
                    cx.stop_propagation();
                    self.view.filter.push_str(typed);
                    self.refilter(cx);
                }
            }
            _ => {}
        }
    }

    fn popover(&self, cx: &mut Context<Self>) -> AnyElement {
        let chrome = PluginEditorChrome {
            plugin_name: self.plugin_name.clone(),
            presets: self.names(),
            ..PluginEditorChrome::default()
        };
        let entity = cx.entity().downgrade();
        let emit = move |action: PluginEditorAction, window: &mut Window, cx: &mut App| {
            let _ = entity.update(cx, |menu, cx| menu.act(action, window, cx));
        };
        let size = preset_menu_size(self.presets.len());
        let close = cx.entity().downgrade();
        deferred(
            anchored()
                .anchor(Anchor::TopRight)
                .offset(point(px(0.0), px(theme::space::TIGHT)))
                .snap_to_window_with_margin(px(theme::space::TIGHT))
                .child(
                    div()
                        .id("builtin-preset-popover")
                        .w(size.width)
                        .h(size.height)
                        .shadow_lg()
                        .on_mouse_down_out(move |_, _, cx| {
                            let _ = close.update(cx, |menu, cx| menu.close(cx));
                        })
                        .child(render_preset_popover(&chrome, &self.view, emit)),
                ),
        )
        // Over a plate's live displays and its own menus.
        .with_priority(3)
        .into_any_element()
    }
}

/// `path` with a `.pst` extension, whatever the dialog handed back. Appended
/// rather than `set_extension`, which would eat the "2" of "Lead v1.2".
#[cfg(feature = "native-dialogs")]
fn with_pst(path: PathBuf) -> PathBuf {
    if path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("pst"))
    {
        return path;
    }
    let mut raw = path.into_os_string();
    raw.push(".pst");
    PathBuf::from(raw)
}

impl Render for BuiltinPresetMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.available() {
            return div().into_any_element();
        }
        let open = self.open;
        let rest = Colors::surface_titlebar();
        let hover = Colors::composite(rest, Colors::state_hover());
        let selected = Colors::composite(rest, Colors::state_selected());
        let trigger = div()
            .id("builtin-preset-trigger")
            .flex()
            .flex_row()
            .items_center()
            .gap(px(theme::space::TIGHT))
            .h(px(theme::size::DENSE))
            .px(px(theme::space::SNUG))
            .rounded(px(theme::radius::CONTROL_SM))
            .when(open, |trigger| trigger.bg(selected))
            .text_size(px(theme::typography::UI_XS))
            .text_color(Colors::text_secondary())
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(move |style| style.bg(hover).text_color(Colors::text_primary()))
            .on_click(cx.listener(|menu, _, window, cx| menu.toggle(window, cx)))
            .child("Presets")
            .child(
                svg()
                    .path(assets::ICON_CHEVRON_DOWN_PATH)
                    .w(px(theme::menu::ICON_SIZE))
                    .h(px(theme::menu::ICON_SIZE))
                    .text_color(Colors::text_muted()),
            );
        div()
            .relative()
            .flex_shrink_0()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .child(trigger)
            .children(open.then(|| {
                div()
                    .absolute()
                    .right_0()
                    .bottom_0()
                    .child(self.popover(cx))
            }))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_drained_once_in_order() {
        let key = PluginInstanceKey {
            track_id: "t".into(),
            insert_id: "i".into(),
        };
        let _ = take_requests();
        push(&key, BuiltinPresetRequest::RevealFolder);
        push(&key, BuiltinPresetRequest::Load(PathBuf::from("a.pst")));
        let drained = take_requests();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].1, BuiltinPresetRequest::RevealFolder);
        assert!(take_requests().is_empty());
    }
}
