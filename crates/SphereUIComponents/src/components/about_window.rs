//! About Futureboard Studio window.
//!
//! The app's identity at the top, then a card describing this copy: its
//! edition, version and platform, and on a Professional build its license and
//! audio engine. Below that are the people and the technology behind the app,
//! from `packages/shared/CREDIT.txt`, on two tabs. Copy Info puts the
//! description of this copy on the clipboard for a support request. Opened
//! from Help → About Futureboard Studio (`app:about`).

use std::sync::OnceLock;
use std::time::Duration;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, img, px, size, App, AppContext, Bounds, ClipboardItem, Context, FocusHandle,
    InteractiveElement, IntoElement, KeyDownEvent, ObjectFit, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, StyledImage, Window, WindowBackgroundAppearance,
    WindowBounds, WindowHandle, WindowKind,
};

use crate::components::controls::{
    fb_button, fb_section_label, fb_segment, fb_segmented_track, FbButtonKind, FbSegment,
};
use crate::components::title_bar::external_window_titlebar;
use crate::edition::{EditionInfo, LicenseDisplayState};
use crate::embedded_assets::APP_LOGO_PATH;
use crate::theme::{self, radius, space, typography, Colors};
use crate::window_position::{apply_owner_display, centered_window_bounds};

pub const ABOUT_WINDOW_WIDTH: f32 = 560.0;
pub const ABOUT_WINDOW_HEIGHT: f32 = 680.0;

/// Full project credits, embedded at compile time so the window has no runtime
/// dependency on the source tree / install layout.
const CREDITS_TEXT: &str = include_str!("../../../../packages/shared/CREDIT.txt");
/// The line in `CREDIT.txt` that separates the people from the technology.
const TECHNOLOGY_MARKER: &str = "Technology Used";

/// Side padding of every section. The window is not resizable, so the content
/// width is fixed and the people grid can use exact column widths.
const PAD: f32 = space::SECTION + space::TIGHT;
const CONTENT_W: f32 = ABOUT_WINDOW_WIDTH - PAD * 2.0;
const PERSON_CARD_W: f32 = (CONTENT_W - space::BASE) / 2.0;
const LOGO: f32 = 72.0;
/// The app name is the one display-size string in the window, so it sits
/// above the UI type ladder.
const TITLE_SIZE: f32 = 22.0;
const INITIALS_DISC: f32 = 32.0;
const FACT_LABEL_W: f32 = 96.0;
const TECH_NAME_W: f32 = 148.0;
const TAB_TRACK_W: f32 = 232.0;
/// How long Copy Info reads "Copied" before it goes back.
const COPIED_FOR: Duration = Duration::from_millis(1_600);

#[derive(Clone, Copy, PartialEq, Eq)]
enum AboutTab {
    People,
    BuiltWith,
}

pub struct AboutWindow {
    focus_handle: FocusHandle,
    tab: AboutTab,
    copied: bool,
    /// Preview only: describe this edition instead of the installed one.
    pinned_edition: Option<EditionInfo>,
}

impl AboutWindow {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            tab: AboutTab::People,
            copied: false,
            pinned_edition: None,
        }
    }

    fn copy_info(&mut self, text: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.copied = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(COPIED_FOR).await;
            let _ = this.update(cx, |this, cx| {
                this.copied = false;
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for AboutWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let version = crate::edition::app_version();
        let info = self
            .pinned_edition
            .clone()
            .or_else(crate::edition::current_edition_info);
        let edition = info
            .as_ref()
            .map(|info| info.edition)
            .unwrap_or("Community");
        let summary = copy_summary(&version, info.as_ref());

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(Colors::surface_base())
            .text_color(Colors::text_primary())
            .font(theme::ui_font())
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|_this, event: &KeyDownEvent, window, _cx| {
                if event.keystroke.key.as_str() == "escape" {
                    window.remove_window();
                }
            }))
            .child(external_window_titlebar(
                "About Futureboard Studio",
                "about-window-close",
                move |window, _cx| {
                    window.remove_window();
                },
            ))
            .child(hero(edition, info.is_some()))
            .child(this_copy_card(&version, info.as_ref()))
            .child(self.tab_bar(cx))
            .child(
                div()
                    .id("about-credits")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px(px(PAD))
                    .pb(px(space::SECTION))
                    .flex()
                    .flex_col()
                    .gap(px(space::SECTION))
                    .children(match self.tab {
                        AboutTab::People => credits()
                            .people
                            .iter()
                            .map(|section| people_section(section).into_any_element())
                            .collect::<Vec<_>>(),
                        AboutTab::BuiltWith => credits()
                            .technology
                            .iter()
                            .map(|section| technology_section(section).into_any_element())
                            .collect(),
                    }),
            )
            .child(self.footer(summary, cx))
    }
}

impl AboutWindow {
    fn tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tabs = [
            (AboutTab::People, "People"),
            (AboutTab::BuiltWith, "Built With"),
        ];
        div()
            .flex_none()
            .px(px(PAD))
            .pt(px(space::SECTION + space::TIGHT))
            .pb(px(space::LOOSE))
            .child(fb_segmented_track().w(px(TAB_TRACK_W)).children(
                tabs.into_iter().enumerate().map(|(index, (tab, label))| {
                    fb_segment(
                        ("about-tab", index),
                        label,
                        self.tab == tab,
                        if index == 0 {
                            FbSegment::First
                        } else {
                            FbSegment::Last
                        },
                        cx.listener(move |this, _, _, cx| {
                            this.tab = tab;
                            cx.notify();
                        }),
                    )
                }),
            ))
    }

    fn footer(&self, summary: String, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::BASE))
            .flex_none()
            .h(px(56.0))
            .px(px(PAD))
            .border_t(px(1.0))
            .border_color(Colors::border_subtle())
            .bg(Colors::surface_panel())
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_size(px(typography::DENSE_LABEL))
                    .text_color(Colors::text_muted())
                    .child("© Nawaphol Bunchuea (Arizkami)"),
            )
            .child(fb_button(
                "about-copy-info",
                if self.copied { "Copied" } else { "Copy Info" },
                FbButtonKind::Default,
                true,
                cx.listener(move |this, _, _, cx| this.copy_info(summary.clone(), cx)),
            ))
            .child(fb_button(
                "about-close",
                "Close",
                FbButtonKind::Primary,
                true,
                move |_, window, _cx| {
                    window.remove_window();
                },
            ))
    }
}

/// Logo, name, and the edition chip.
fn hero(edition: &str, professional: bool) -> impl IntoElement {
    div()
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::SECTION + space::TIGHT))
        .px(px(PAD))
        .pt(px(space::BLOCK))
        .pb(px(space::SECTION + space::TIGHT))
        .child(
            img(SharedString::from(APP_LOGO_PATH))
                .flex_none()
                .w(px(LOGO))
                .h(px(LOGO))
                .rounded(px(radius::DIALOG))
                .object_fit(ObjectFit::Contain),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .min_w(px(0.0))
                .gap(px(space::TIGHT))
                .child(
                    div()
                        .text_size(px(TITLE_SIZE))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(Colors::text_primary())
                        .child("Futureboard Studio"),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(space::SNUG))
                        .mt(px(space::TIGHT))
                        .child(chip(
                            format!("{edition} Edition"),
                            if professional {
                                ChipTone::Accent
                            } else {
                                ChipTone::Neutral
                            },
                        )),
                ),
        )
}

#[derive(Clone, Copy)]
enum ChipTone {
    Neutral,
    Accent,
    Good,
    Warning,
}

/// A small rounded pill. Tone is carried by fill, border and text together,
/// and the words always say the state too.
fn chip(label: impl Into<SharedString>, tone: ChipTone) -> impl IntoElement {
    let (bg, fg, border) = match tone {
        ChipTone::Neutral => (
            Colors::surface_panel(),
            Colors::text_secondary(),
            Colors::border_subtle(),
        ),
        ChipTone::Accent => (
            Colors::accent_muted(),
            Colors::accent_primary(),
            Colors::with_alpha(Colors::accent_primary(), 0.55),
        ),
        ChipTone::Good => (
            Colors::with_alpha(Colors::status_success(), 0.14),
            Colors::status_success(),
            Colors::with_alpha(Colors::status_success(), 0.45),
        ),
        ChipTone::Warning => (
            Colors::with_alpha(Colors::status_warning(), 0.14),
            Colors::status_warning(),
            Colors::with_alpha(Colors::status_warning(), 0.45),
        ),
    };
    div()
        .flex_none()
        .px(px(space::BASE + space::HAIR))
        .py(px(space::HAIR + 1.0))
        .rounded(px(radius::PILL))
        .bg(bg)
        .border(px(1.0))
        .border_color(border)
        .text_size(px(typography::DENSE_LABEL))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(fg)
        .child(label.into())
}

/// What this copy is: version and platform, and on a Professional
/// build the license and the audio engine. Every row is read from the running
/// app; nothing here is a claim the build cannot back.
fn this_copy_card(version: &str, info: Option<&EditionInfo>) -> impl IntoElement {
    let license_row = info.map(|info| {
        let (label, _) = crate::edition::license_status_label(info.license.as_ref());
        let tone = match info.license.as_ref().map(|license| license.state) {
            Some(LicenseDisplayState::Active) => ChipTone::Good,
            Some(LicenseDisplayState::Expired) => ChipTone::Warning,
            None => ChipTone::Neutral,
        };
        let licensee = info
            .license
            .as_ref()
            .and_then(|license| license.licensee.as_deref())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(|name| format!("Licensed to {name}"));
        fact_row(
            "License",
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::BASE))
                .min_w(px(0.0))
                .child(chip(label, tone))
                .children(licensee.map(|text| fact_text(text, Colors::text_secondary()))),
        )
    });
    let expiry_row = info
        .and_then(|info| info.license.as_ref())
        .map(|license| fact_row("Expires", fact_value(license.expiry_text())));
    let engine = info.and_then(|info| info.audio_engine.clone());
    let action = info
        .filter(|_| crate::edition::license_action_available())
        .map(|info| {
            fb_button(
                "about-license-action",
                if info.license.is_some() {
                    "Manage License…"
                } else {
                    "Activate License…"
                },
                FbButtonKind::Default,
                true,
                |_, window, cx| crate::edition::dispatch_license_action(window, cx),
            )
        });

    div()
        .flex_none()
        .mx(px(PAD))
        .flex()
        .flex_col()
        .gap(px(space::HAIR))
        .px(px(space::LOOSE + space::TIGHT))
        .py(px(space::LOOSE))
        .rounded(px(radius::SURFACE))
        .bg(Colors::surface_panel())
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .child(
            div()
                .pb(px(space::TIGHT))
                .child(fb_section_label("THIS COPY")),
        )
        .child(fact_row("Version", fact_value(version.to_string())))
        .child(fact_row("Platform", fact_value(platform_label())))
        .children(license_row)
        .children(expiry_row)
        .children(engine.map(|badge| {
            fact_row(
                "Audio Engine",
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(space::BASE))
                    .min_w(px(0.0))
                    .children(badge.logo.clone().map(|logo| {
                        img(gpui::ImageSource::Image(logo))
                            .flex_none()
                            .h(px(20.0))
                            .max_w(px(72.0))
                            .object_fit(ObjectFit::Contain)
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w(px(0.0))
                            .child(fact_text(badge.name.to_string(), Colors::text_primary()))
                            .child(
                                div()
                                    .text_size(px(typography::DENSE_CAPTION))
                                    .text_color(Colors::text_muted())
                                    .child(badge.notice),
                            ),
                    ),
            )
        }))
        .children(action.map(|button| div().flex().justify_end().pt(px(space::SNUG)).child(button)))
}

fn fact_row(label: &'static str, value: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .min_h(px(theme::size::DEFAULT + space::HAIR))
        .child(
            div()
                .flex_none()
                .w(px(FACT_LABEL_W))
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_muted())
                .child(label),
        )
        .child(div().flex_1().min_w(px(0.0)).child(value))
}

fn fact_value(text: String) -> impl IntoElement {
    fact_text(text, Colors::text_primary())
}

fn fact_text(text: String, color: gpui::Rgba) -> impl IntoElement {
    div()
        .min_w(px(0.0))
        .truncate()
        .text_size(px(typography::UI_XS))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(color)
        .child(text)
}

/// "Windows · x64" and the like, from the running binary.
fn platform_label() -> String {
    let os = match std::env::consts::OS {
        "windows" => "Windows",
        "macos" => "macOS",
        "linux" => "Linux",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "ARM64",
        other => other,
    };
    format!("{os} · {arch}")
}

/// The plain-text description Copy Info puts on the clipboard.
fn copy_summary(version: &str, info: Option<&EditionInfo>) -> String {
    let edition = info.map(|info| info.edition).unwrap_or("Community");
    let mut lines = vec![
        format!("Futureboard Studio {version}"),
        format!("Edition: {edition}"),
        format!("Platform: {}", platform_label()),
    ];
    if let Some(info) = info {
        let (label, _) = crate::edition::license_status_label(info.license.as_ref());
        lines.push(format!("License: {label}"));
        if let Some(license) = info.license.as_ref() {
            lines.push(format!("Expires: {}", license.expiry_text()));
        }
        if let Some(engine) = info.audio_engine.as_ref() {
            lines.push(format!("Audio engine: {}", engine.name));
        }
    }
    lines.join("\n")
}

// ── Credits ─────────────────────────────────────────────────────────────────

/// One block of `CREDIT.txt`: its first line, then its entries.
#[derive(Debug, PartialEq)]
struct CreditSection {
    title: &'static str,
    entries: Vec<CreditEntry>,
}

/// `name - detail`, or a bare name.
#[derive(Debug, PartialEq)]
struct CreditEntry {
    name: &'static str,
    detail: Option<&'static str>,
}

impl CreditEntry {
    /// A person's name without the trailing `(handle)`, and the handle.
    fn person(&self) -> (&'static str, Option<&'static str>) {
        match self.name.rsplit_once(" (") {
            Some((name, handle)) if handle.ends_with(')') => {
                (name.trim(), Some(handle.trim_end_matches(')').trim()))
            }
            _ => (self.name, None),
        }
    }

    /// Up to two initials for the person's disc.
    fn initials(&self) -> String {
        self.person()
            .0
            .split_whitespace()
            .filter_map(|word| word.chars().next())
            .take(2)
            .flat_map(char::to_uppercase)
            .collect()
    }
}

#[derive(Debug, Default, PartialEq)]
struct Credits {
    people: Vec<CreditSection>,
    technology: Vec<CreditSection>,
}

/// Blocks are separated by blank lines and open with their title. The block
/// that is only [`TECHNOLOGY_MARKER`] splits the people from the technology.
fn parse_credits(text: &'static str) -> Credits {
    let mut credits = Credits::default();
    let mut in_technology = false;
    let mut current: Option<CreditSection> = None;
    let flush = |section: Option<CreditSection>, technology: bool, credits: &mut Credits| {
        if let Some(section) = section {
            if technology {
                credits.technology.push(section);
            } else {
                credits.people.push(section);
            }
        }
    };
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            flush(current.take(), in_technology, &mut credits);
            continue;
        }
        match current.as_mut() {
            None if line == TECHNOLOGY_MARKER => in_technology = true,
            None => {
                current = Some(CreditSection {
                    title: line,
                    entries: Vec::new(),
                })
            }
            Some(section) => section.entries.push(match line.split_once(" - ") {
                Some((name, detail)) => CreditEntry {
                    name: name.trim(),
                    detail: Some(detail.trim()),
                },
                None => CreditEntry {
                    name: line,
                    detail: None,
                },
            }),
        }
    }
    flush(current.take(), in_technology, &mut credits);
    credits
}

fn credits() -> &'static Credits {
    static CREDITS: OnceLock<Credits> = OnceLock::new();
    CREDITS.get_or_init(|| parse_credits(CREDITS_TEXT))
}

fn section_title(title: &'static str) -> impl IntoElement {
    div()
        .text_size(px(typography::UI_SM))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_primary())
        .child(title)
}

/// A credits block as a two-column grid of person cards.
fn people_section(section: &'static CreditSection) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(space::BASE))
        .child(section_title(section.title))
        .child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(space::BASE))
                .children(section.entries.iter().map(person_card)),
        )
}

fn person_card(entry: &'static CreditEntry) -> impl IntoElement {
    let (name, handle) = entry.person();
    let meta = match (entry.detail, handle) {
        (Some(role), Some(handle)) => Some(format!("{role} · {handle}")),
        (Some(role), None) => Some(role.to_string()),
        (None, Some(handle)) => Some(handle.to_string()),
        (None, None) => None,
    };
    div()
        .w(px(PERSON_CARD_W))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE + space::HAIR))
        .px(px(space::BASE + space::HAIR))
        .py(px(space::BASE))
        .rounded(px(radius::CONTROL))
        .bg(Colors::surface_panel())
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .child(
            div()
                .size(px(INITIALS_DISC))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(radius::PILL))
                .bg(Colors::with_alpha(Colors::accent_primary(), 0.14))
                .text_size(px(typography::UI_XS))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::accent_primary())
                .child(entry.initials()),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .gap(px(1.0))
                .child(
                    div()
                        .truncate()
                        .text_size(px(typography::UI_SM))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(Colors::text_primary())
                        .child(name),
                )
                .children(meta.map(|meta| {
                    div()
                        .truncate()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_muted())
                        .child(meta)
                })),
        )
}

/// A technology block: each library's name beside what it does here.
fn technology_section(section: &'static CreditSection) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(space::SNUG))
        .child(section_title(section.title))
        .child(
            div()
                .flex()
                .flex_col()
                .px(px(space::LOOSE))
                .py(px(space::SNUG))
                .rounded(px(radius::CONTROL))
                .bg(Colors::surface_panel())
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .children(section.entries.iter().enumerate().map(|(index, entry)| {
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(space::LOOSE))
                        .py(px(space::SNUG))
                        .when(index > 0, |row| {
                            row.border_t(px(1.0)).border_color(Colors::border_subtle())
                        })
                        .child(
                            div()
                                .flex_none()
                                .w(px(TECH_NAME_W))
                                .text_size(px(typography::UI_XS))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(Colors::text_primary())
                                .child(entry.name),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .text_size(px(typography::UI_XS))
                                .text_color(Colors::text_muted())
                                .children(entry.detail),
                        )
                })),
        )
}

/// Opens the About window for the UI preview (`ui-preview` only): on the
/// Built With tab when `built_with`, describing `edition` when given.
#[cfg(feature = "ui-preview")]
pub(crate) fn open_preview_about_window(
    options: gpui::WindowOptions,
    edition: Option<EditionInfo>,
    built_with: bool,
    cx: &mut App,
) -> gpui::Result<WindowHandle<AboutWindow>> {
    cx.open_window(options, move |_, cx| {
        cx.new(|cx| {
            let mut window = AboutWindow::new(cx);
            window.pinned_edition = edition;
            if built_with {
                window.tab = AboutTab::BuiltWith;
            }
            window
        })
    })
}

pub fn open_about_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    cx: &mut App,
) -> Result<WindowHandle<AboutWindow>, String> {
    let window_bounds = centered_window_bounds(
        owner_bounds,
        size(px(ABOUT_WINDOW_WIDTH), px(ABOUT_WINDOW_HEIGHT)),
        cx,
    );
    let mut options = crate::platform_chrome::external_dialog_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.kind = WindowKind::Dialog;
    options.is_resizable = false;
    options.is_minimizable = false;
    options.window_background = WindowBackgroundAppearance::Transparent;
    apply_owner_display(&mut options, owner_bounds, cx);

    crate::dialog_frame::open_dialog_window(cx, options, move |_window, cx| {
        cx.new(AboutWindow::new)
    })
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "Created by\nAda Lovelace (ada)\n\nCore Team\nAlan Turing (alan) - Developer\nGrace Hopper - QA\n\nTechnology Used\n\nCore\nRust - The language\nGPUI (From Zed) - UI\n";

    #[test]
    fn credits_split_into_people_and_technology() {
        let credits = parse_credits(SAMPLE);
        let people: Vec<_> = credits.people.iter().map(|s| s.title).collect();
        let technology: Vec<_> = credits.technology.iter().map(|s| s.title).collect();
        assert_eq!(people, ["Created by", "Core Team"]);
        assert_eq!(technology, ["Core"]);
        assert_eq!(
            credits.people[1].entries[0],
            CreditEntry {
                name: "Alan Turing (alan)",
                detail: Some("Developer"),
            }
        );
        assert_eq!(credits.technology[0].entries[1].name, "GPUI (From Zed)");
    }

    #[test]
    fn a_person_splits_off_their_handle_and_initials() {
        let credits = parse_credits(SAMPLE);
        let ada = &credits.people[0].entries[0];
        assert_eq!(ada.person(), ("Ada Lovelace", Some("ada")));
        assert_eq!(ada.initials(), "AL");
        let grace = &credits.people[1].entries[1];
        assert_eq!(grace.person(), ("Grace Hopper", None));
    }

    #[test]
    fn the_shipped_credits_have_both_halves() {
        let credits = credits();
        assert!(!credits.people.is_empty());
        assert!(!credits.technology.is_empty());
        assert!(credits
            .people
            .iter()
            .chain(&credits.technology)
            .all(|section| !section.entries.is_empty()));
    }

    #[test]
    fn copy_info_describes_a_community_copy() {
        let text = copy_summary("1.2.3", None);
        assert!(text.starts_with("Futureboard Studio 1.2.3\nEdition: Community\nPlatform: "));
        assert!(!text.contains("License"));
    }
}
