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
    /// Keeps the press that closed the list from opening it again.
    reopen: ReopenGuard,
}

/// How long after a click-outside close a press on the trigger counts as that
/// same press.
const REOPEN_GUARD: std::time::Duration = std::time::Duration::from_millis(250);

/// A press that closes the list on the trigger must not reopen it on release
/// (#228). GPUI closes the list in the press's capture phase
/// (`on_mouse_down_out`), the trigger sees the same press in its bubble
/// phase, and the click fires on release, however long the button is held
/// (#264). So the decision is made at the press, not at the release.
#[derive(Debug, Default)]
struct ReopenGuard {
    /// When a press outside the list last closed it.
    dismissed_at: Option<std::time::Instant>,
    /// The trigger's current press is the one that closed the list.
    press_dismissed: bool,
}

impl ReopenGuard {
    /// A press outside the list closed it.
    fn dismissed(&mut self, now: std::time::Instant) {
        self.dismissed_at = Some(now);
    }

    /// The left button went down on the trigger.
    fn press(&mut self, now: std::time::Instant) {
        self.press_dismissed = self
            .dismissed_at
            .take()
            .is_some_and(|at| now.saturating_duration_since(at) < REOPEN_GUARD);
    }

    /// The trigger was clicked while the list was closed: whether to open it.
    fn release_opens(&mut self) -> bool {
        !std::mem::take(&mut self.press_dismissed)
    }
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
            reopen: ReopenGuard::default(),
        }
    }

    /// Whether the open list has the keyboard: a window that claims keys
    /// before they reach their target (Space for the transport) leaves them
    /// to the filter then (#227).
    pub fn owns_keyboard(&self, window: &Window) -> bool {
        self.open && self.focus.is_focused(window)
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
        self.reopen = ReopenGuard::default();
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
        if !self.reopen.release_opens() {
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
                            let _ = close.update(cx, |menu, cx| {
                                menu.reopen.dismissed(std::time::Instant::now());
                                menu.close(cx);
                            });
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
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|menu, _, _, _| menu.reopen.press(std::time::Instant::now())),
            )
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
    use std::time::{Duration, Instant};

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

    /// Event order of one press on the trigger while the list is open:
    /// capture-phase `on_mouse_down_out` closes it, the trigger's bubble-phase
    /// mouse-down follows within the same dispatch, the click on release.
    /// The release reads no clock, so the hold time cannot matter (#264).
    #[test]
    fn press_that_closed_the_list_never_reopens_it() {
        let mut guard = ReopenGuard::default();
        let t0 = Instant::now();
        guard.dismissed(t0);
        guard.press(t0);
        assert!(!guard.release_opens());
    }

    /// Releasing long after `REOPEN_GUARD` (e.g. 500 ms or 1 second) must
    /// still not reopen the list (#264).
    #[test]
    fn holding_trigger_past_guard_window_does_not_reopen_on_release() {
        let mut guard = ReopenGuard::default();
        let t0 = Instant::now();
        guard.dismissed(t0);
        guard.press(t0);
        // The hold time does not affect release_opens since the decision was
        // made at press time.
        assert!(!guard.release_opens());
    }

    #[test]
    fn plain_click_on_the_closed_trigger_opens_the_list() {
        let mut guard = ReopenGuard::default();
        guard.press(Instant::now());
        assert!(guard.release_opens());
    }

    #[test]
    fn next_click_after_a_swallowed_one_opens_the_list() {
        let mut guard = ReopenGuard::default();
        let t0 = Instant::now();
        guard.dismissed(t0);
        guard.press(t0);
        assert!(!guard.release_opens());
        guard.press(t0 + Duration::from_millis(100));
        assert!(guard.release_opens());
    }

    #[test]
    fn closing_elsewhere_does_not_block_a_later_trigger_click() {
        let mut guard = ReopenGuard::default();
        let t0 = Instant::now();
        guard.dismissed(t0);
        guard.press(t0 + REOPEN_GUARD + Duration::from_millis(10));
        assert!(guard.release_opens());
    }

    #[test]
    fn press_released_off_the_trigger_leaves_no_stale_state() {
        let mut guard = ReopenGuard::default();
        let t0 = Instant::now();
        guard.dismissed(t0);
        // Dismissing press on the trigger, released elsewhere: no click.
        guard.press(t0);
        // The next press on the trigger decides afresh.
        guard.press(t0 + Duration::from_secs(1));
        assert!(guard.release_opens());
    }
}
