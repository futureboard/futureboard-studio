//! Sign-in dialog and account-menu popup.
//!
//! Mirrors the license-activation dialog's window language. Sign-in hands off to
//! the system browser for the account service's identity providers (Google,
//! Discord, GitHub); the account menu is the titlebar dropdown surface
//! (identity + Sign out).

use gpui::{
    div, px, size, App, AppContext, Bounds, Context, InteractiveElement, IntoElement, KeyDownEvent,
    ParentElement, Pixels, Render, Styled, Window, WindowBackgroundAppearance, WindowBounds,
    WindowHandle, WindowKind,
};

use crate::auth::{self, OAuthProvider};

use crate::components::controls::{fb_button, FbButtonKind};
use crate::components::title_bar::external_window_titlebar_compact;
use crate::theme::{self, Colors};
use crate::window_position::{apply_owner_display, centered_window_bounds};

const LOGIN_WIDTH: f32 = 420.0;
const LOGIN_HEIGHT: f32 = 336.0;

// ── Sign-in window ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum LoginState {
    Idle,
    Unavailable,
    /// The browser is open and the loopback listener is waiting for it.
    Waiting,
    Success(String),
    Error(String),
}

/// Sign-in window: a provider choice and nothing else.
///
/// There is deliberately no email/password form. The Futureboard account
/// service authenticates people through their identity provider, so a password
/// field here would be a control that cannot work — and a credential this app
/// has no business handling.
pub struct LoginWindow {
    state: LoginState,
}

impl LoginWindow {
    fn new() -> Self {
        Self {
            state: if auth::auth_configured() {
                LoginState::Idle
            } else {
                LoginState::Unavailable
            },
        }
    }

    fn is_busy(&self) -> bool {
        matches!(self.state, LoginState::Waiting)
    }

    fn begin_oauth(&mut self, provider: OAuthProvider, cx: &mut Context<Self>) {
        if self.is_busy() {
            return;
        }
        if !auth::auth_configured() {
            self.state = LoginState::Unavailable;
            cx.notify();
            return;
        }
        self.state = LoginState::Waiting;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { auth::oauth_sign_in(provider) })
                .await;
            let _ = this.update(cx, |this, cx| this.finish(result, cx));
        })
        .detach();
    }

    fn finish(&mut self, result: Result<auth::UserProfile, String>, cx: &mut Context<Self>) {
        match result {
            Ok(profile) => {
                let who = profile
                    .username
                    .or(profile.email)
                    .unwrap_or_else(|| "your account".to_string());
                self.state = LoginState::Success(format!("Signed in as {who}."));
                // Reflect the new session in the titlebar chip immediately.
                cx.refresh_windows();
            }
            Err(error) => self.state = LoginState::Error(error),
        }
        cx.notify();
    }

    fn handle_key(&mut self, event: &KeyDownEvent, window: &mut Window, _cx: &mut Context<Self>) {
        if event.keystroke.key.as_str() == "escape" {
            window.remove_window();
        }
    }

    fn status(&self) -> (String, gpui::Rgba) {
        match &self.state {
            LoginState::Idle => (
                "Sign-in opens your browser. Come back here when it says you can close the tab."
                    .to_string(),
                Colors::text_muted(),
            ),
            LoginState::Unavailable => (
                "Sign-in is not configured for this build.".to_string(),
                Colors::status_warning(),
            ),
            LoginState::Waiting => (
                "Waiting for the browser to finish signing in…".to_string(),
                Colors::text_muted(),
            ),
            LoginState::Success(message) => (message.clone(), Colors::status_success()),
            LoginState::Error(message) => (message.clone(), Colors::status_error()),
        }
    }
}

/// Providers the account service offers, in the order they are shown.
const LOGIN_PROVIDERS: [(&str, OAuthProvider); 3] = [
    ("login-google", OAuthProvider::Google),
    ("login-discord", OAuthProvider::Discord),
    ("login-github", OAuthProvider::GitHub),
];

impl Render for LoginWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let target = cx.entity().clone();
        let configured = auth::auth_configured();
        let busy = self.is_busy();
        let done = matches!(self.state, LoginState::Success(_));
        let can_sign_in = configured && !busy && !done;
        let (status, status_color) = self.status();

        div()
            .key_context("LoginWindow")
            .capture_key_down(move |event, window, cx| {
                target.update(cx, |this, cx| this.handle_key(event, window, cx));
            })
            .flex()
            .flex_col()
            .size_full()
            .font(theme::ui_font())
            .bg(Colors::surface_window())
            .border(px(1.0))
            .border_color(Colors::border_subtle())
            .child(external_window_titlebar_compact(
                "Sign In",
                "login-close",
                |window, _cx| window.remove_window(),
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .p(px(16.0))
                    .gap(px(10.0))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Colors::text_primary())
                            .child("Sign in to Futureboard Studio"),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(Colors::text_muted())
                            .child(
                            "A signed-in account activates the license you bought without a key.",
                        ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.0))
                            .children(LOGIN_PROVIDERS.map(|(id, provider)| {
                                let provider_target = cx.entity().clone();
                                fb_button(
                                    id,
                                    provider.label(),
                                    FbButtonKind::Default,
                                    can_sign_in,
                                    move |_, _window, cx| {
                                        provider_target
                                            .update(cx, |this, cx| this.begin_oauth(provider, cx));
                                    },
                                )
                            })),
                    )
                    .child(
                        div()
                            .min_h(px(30.0))
                            .p(px(8.0))
                            .rounded_md()
                            .border(px(1.0))
                            .border_color(Colors::with_alpha(status_color, 0.35))
                            .bg(Colors::surface_input())
                            .text_size(px(10.5))
                            .text_color(status_color)
                            .child(status),
                    )
                    .child(div().mt_auto().flex().justify_end().child(fb_button(
                        "login-cancel",
                        if done { "Close" } else { "Cancel" },
                        FbButtonKind::Default,
                        true,
                        |_, window, _cx| window.remove_window(),
                    ))),
            )
    }
}

pub fn open_login_window(
    owner_bounds: Option<Bounds<Pixels>>,
    cx: &mut App,
) -> Result<WindowHandle<LoginWindow>, String> {
    let bounds = centered_window_bounds(owner_bounds, size(px(LOGIN_WIDTH), px(LOGIN_HEIGHT)), cx);
    let mut options = crate::platform_chrome::external_dialog_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(bounds));
    options.kind = WindowKind::Dialog;
    options.is_resizable = false;
    options.is_minimizable = false;
    options.window_background = WindowBackgroundAppearance::Transparent;
    apply_owner_display(&mut options, owner_bounds, cx);

    crate::dialog_frame::open_dialog_window(cx, options, |_window, cx| {
        cx.new(|_cx| LoginWindow::new())
    })
    .map_err(|error| error.to_string())
}
