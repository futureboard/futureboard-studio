//! The built-in Soundfont Player's panel.
//!
//! One page for both of the player's modes:
//!
//! * a **preset browser** down the left — every preset in the loaded font,
//!   grouped the way a General MIDI bank is laid out (the sixteen instrument
//!   families of bank 0, then variation banks, then drum kits) and filtered by
//!   the window's search field;
//! * the **instrument** on the right — in single mode the one preset the
//!   whole player plays; in sixteen-channel mode a rack of sixteen parts, one
//!   per MIDI channel, each with its own preset, level, pan, mute and solo,
//!   the way a General MIDI module is played from several MIDI tracks;
//! * the shaping that applies to the whole player (amp envelope, output);
//! * a keyboard pinned along the bottom that plays through the track.
//!
//! In sixteen-channel mode the browser assigns to the selected part, and the
//! keyboard plays on its channel.
//!
//! This file only renders: the state is the track's (see
//! [`SoundfontPlayerSettingsState`]) plus the font's metadata, both held by
//! [`crate::components::soundfont_player_window::SoundfontPlayerWindow`].

use std::sync::Arc;

use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, InteractiveElement, IntoElement, ParentElement, StatefulInteractiveElement,
    Styled, UniformListScrollHandle, Window, div, px, svg, uniform_list,
};

use crate::assets;
use crate::components::controls::{
    FbButtonKind, FbLatch, FbSegment, fb_button, fb_checkbox, fb_segment, fb_segmented_track,
    fb_stepper_button, fb_toggle,
};
use crate::components::knob::{format_pan_label, knob, knob_bipolar};
use crate::components::mdi::{
    MdiDocumentKind, MdiWorkspaceCallbacks, MdiWorkspaceState, mdi_workspace,
};
use crate::components::slider::slider;
use crate::components::timeline::timeline_state::{
    SoundfontChannel, SoundfontPlayerMode, SoundfontPlayerSettingsState,
};
use crate::soundfont_player::{
    DRUM_BANK, SoundfontEnvelope, SoundfontPresetInfo, SoundfontRenderQuality,
};
use crate::theme::{Colors, radius, size, space, typography};

pub const SOUNDFONT_PLAYER_MDI_TITLE: &str = "Soundfont Player";
/// The window's title in sixteen-channel mode.
pub const SOUNDFONT_MULTI_TITLE: &str = "Soundfont Multi";

/// MIDI channels the sixteen-channel rack shows.
pub const CHANNELS: usize = 16;

/// What the panel draws: the track's settings, the loaded font's metadata,
/// and the window's transient state.
#[derive(Clone)]
pub struct SoundfontPlayerPanelState {
    pub file_name: Option<String>,
    pub bank_name: Option<String>,
    pub presets: Vec<SoundfontPresetInfo>,
    /// The track's player settings — the authority the engine plays from.
    pub settings: SoundfontPlayerSettingsState,
    pub loading: bool,
    pub status: Option<String>,
    /// Sixteen-channel mode: the part the browser assigns to and the keyboard
    /// plays, 0-based.
    pub selected_channel: u8,
    /// Per channel, the tracks whose notes arrive on it.
    pub channel_sources: [Vec<String>; CHANNELS],
    /// Tracks routed here whose notes each keep their own channel.
    pub per_note_sources: Vec<String>,
    /// MIDI note of the leftmost key on the panel keyboard.
    pub keyboard_root: u8,
    /// Notes the panel is currently holding on the engine, so keys and the
    /// Test button show what is actually sounding.
    pub active_notes: Vec<u8>,
    /// `true` while the Test button's audition is holding notes.
    pub testing: bool,
}

/// Three octaves starting at C3: a bass line and a lead both fit, and the
/// keyboard still reads as keys at the window's minimum width.
pub const KEYBOARD_DEFAULT_ROOT: u8 = 48;
const KEYBOARD_WHITE_KEYS: usize = 21;
const KEYBOARD_LOWEST_ROOT: u8 = 0;
const KEYBOARD_HIGHEST_ROOT: u8 = 108;

impl Default for SoundfontPlayerPanelState {
    fn default() -> Self {
        Self {
            file_name: None,
            bank_name: None,
            presets: Vec::new(),
            settings: SoundfontPlayerSettingsState::default(),
            loading: false,
            status: None,
            selected_channel: 0,
            channel_sources: Default::default(),
            per_note_sources: Vec::new(),
            keyboard_root: KEYBOARD_DEFAULT_ROOT,
            active_notes: Vec::new(),
            testing: false,
        }
    }
}

impl SoundfontPlayerPanelState {
    /// Whether the panel has a loaded font to play. Gestures are disabled
    /// rather than sending MIDI that could not make a sound.
    pub fn is_playable(&self) -> bool {
        self.file_name.is_some() && !self.loading
    }

    pub fn is_multi(&self) -> bool {
        self.settings.mode == SoundfontPlayerMode::Multi
    }

    /// The preset the browser marks and assigns to: the player's own in single
    /// mode, the selected part's in sixteen-channel mode.
    pub fn browser_target(&self) -> Option<(i32, i32)> {
        if self.is_multi() {
            self.settings.channels[self.selected_channel as usize % CHANNELS].preset
        } else {
            self.settings.preset
        }
    }

    /// The MIDI channel the keyboard and Test play on.
    pub fn preview_channel(&self) -> u8 {
        if self.is_multi() {
            self.selected_channel.min(15)
        } else {
            0
        }
    }

    pub fn preset_name(&self, preset: Option<(i32, i32)>) -> Option<&str> {
        let (bank, patch) = preset?;
        self.presets
            .iter()
            .find(|p| p.bank == bank && p.patch == patch)
            .map(|p| p.name.as_str())
    }

    pub fn shift_keyboard_octave(&mut self, delta: i32) {
        let root = self.keyboard_root as i32 + delta * 12;
        self.keyboard_root =
            root.clamp(KEYBOARD_LOWEST_ROOT as i32, KEYBOARD_HIGHEST_ROOT as i32) as u8;
    }
}

type VoidCb = Arc<dyn Fn(&mut Window, &mut App) + 'static>;
type PresetCb = Arc<dyn Fn(&(i32, i32), &mut Window, &mut App) + 'static>;
type F32Cb = Arc<dyn Fn(&f32, &mut Window, &mut App) + 'static>;
type UsizeCb = Arc<dyn Fn(&usize, &mut Window, &mut App) + 'static>;
type NoteCb = Arc<dyn Fn(&u8, &mut Window, &mut App) + 'static>;
type I32Cb = Arc<dyn Fn(&i32, &mut Window, &mut App) + 'static>;
type EnvelopeCb = Arc<dyn Fn(&SoundfontEnvelope, &mut Window, &mut App) + 'static>;
type QualityCb = Arc<dyn Fn(&SoundfontRenderQuality, &mut Window, &mut App) + 'static>;
type ModeCb = Arc<dyn Fn(&SoundfontPlayerMode, &mut Window, &mut App) + 'static>;
type ChannelCb = Arc<dyn Fn(&(u8, SoundfontChannel), &mut Window, &mut App) + 'static>;

#[derive(Clone)]
pub struct SoundfontPlayerCallbacks {
    pub on_browse: VoidCb,
    pub on_set_mode: ModeCb,
    /// A browser pick: the player's preset in single mode, the selected part's
    /// in sixteen-channel mode.
    pub on_select_preset: PresetCb,
    pub on_select_channel: NoteCb,
    /// One complete part, so a knob drag cannot land a partial edit.
    pub on_set_channel: ChannelCb,
    pub on_set_volume: F32Cb,
    pub on_toggle_reverb_chorus: VoidCb,
    pub on_set_polyphony: UsizeCb,
    /// One complete envelope — the panel sends the whole struct so a knob drag
    /// cannot land a partial edit.
    pub on_set_envelope: EnvelopeCb,
    pub on_set_quality: QualityCb,
    /// Press and release of one panel key — routed to the engine so the note
    /// sounds through the track it belongs to.
    pub on_note_on: NoteCb,
    pub on_note_off: NoteCb,
    /// Auditions the current preset without needing the transport or a clip.
    pub on_test: VoidCb,
    /// Releases everything the panel is holding.
    pub on_all_notes_off: VoidCb,
    pub on_shift_octave: I32Cb,
}

const POLYPHONY_MIN: usize = 1;
const POLYPHONY_MAX: usize = 256;
const POLYPHONY_STEP: usize = 8;

/// Width of the preset browser column.
pub const BROWSER_W: f32 = 264.0;
/// One browser row, header or preset: uniform so the list can virtualize.
const BROWSER_ROW_H: f32 = size::ROW;
/// One part in the sixteen-channel rack.
const CHANNEL_ROW_H: f32 = 34.0;

pub fn ensure_soundfont_player_document(state: &mut MdiWorkspaceState) -> String {
    if let Some(existing) = state
        .documents
        .iter()
        .find(|doc| doc.kind == MdiDocumentKind::SoundfontPlayer)
        .map(|doc| doc.id.clone())
    {
        state.restore_document(&existing);
        return existing;
    }
    state.open_document(MdiDocumentKind::SoundfontPlayer, SOUNDFONT_PLAYER_MDI_TITLE)
}

pub fn soundfont_player_mdi_workspace(
    state: &MdiWorkspaceState,
    callbacks: MdiWorkspaceCallbacks,
    panel: &SoundfontPlayerPanelState,
    panel_callbacks: SoundfontPlayerCallbacks,
) -> AnyElement {
    mdi_workspace(state, callbacks, |doc| match doc.kind {
        MdiDocumentKind::SoundfontPlayer => {
            let rows = browser_rows(&panel.presets, "");
            soundfont_player_panel(
                panel,
                panel_callbacks.clone(),
                div().into_any_element(),
                rows,
                UniformListScrollHandle::new(),
            )
        }
        MdiDocumentKind::Generic => empty_document(),
    })
}

// ── Browser model ──────────────────────────────────────────────────────────

/// The sixteen instrument families of a General MIDI bank, eight programs
/// each, in program order.
const GM_FAMILIES: [&str; 16] = [
    "Piano",
    "Chromatic Percussion",
    "Organ",
    "Guitar",
    "Bass",
    "Strings",
    "Ensemble",
    "Brass",
    "Reed",
    "Pipe",
    "Synth Lead",
    "Synth Pad",
    "Synth Effects",
    "Ethnic",
    "Percussive",
    "Sound Effects",
];

/// One line of the preset browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserRow {
    Header(String),
    Preset { bank: i32, patch: i32, name: String },
}

/// Whether `presets` is laid out as a General MIDI bank: most of bank 0's
/// programs filled. Only then do the GM family names say what a program is —
/// a single-instrument font keeps its one sound at program 0 whatever it is.
fn is_general_midi(presets: &[SoundfontPresetInfo]) -> bool {
    presets.iter().filter(|p| p.bank == 0).count() >= 64
}

/// The group a preset is listed under: its GM family in a General MIDI
/// bank, its bank otherwise.
fn preset_group(bank: i32, patch: i32, general_midi: bool) -> String {
    if bank >= DRUM_BANK {
        "Drum Kits".to_string()
    } else if bank == 0 && general_midi {
        GM_FAMILIES[(patch.clamp(0, 127) / 8) as usize].to_string()
    } else {
        format!("Bank {bank}")
    }
}

/// The browser's rows for `presets` (sorted by bank and patch, as
/// [`crate::soundfont_player::SoundfontPlayer::list_presets`] returns them)
/// matching `query`: a preset matches when its name or its group contains the
/// query, ignoring case. A header is only listed above presets that match.
pub fn browser_rows(presets: &[SoundfontPresetInfo], query: &str) -> Vec<BrowserRow> {
    let query = query.trim().to_lowercase();
    let mut rows = Vec::new();
    let mut current_group: Option<String> = None;
    let general_midi = is_general_midi(presets);
    for preset in presets {
        let group = preset_group(preset.bank, preset.patch, general_midi);
        let matches = query.is_empty()
            || preset.name.to_lowercase().contains(&query)
            || group.to_lowercase().contains(&query);
        if !matches {
            continue;
        }
        if current_group.as_deref() != Some(group.as_str()) {
            rows.push(BrowserRow::Header(group.clone()));
            current_group = Some(group);
        }
        rows.push(BrowserRow::Preset {
            bank: preset.bank,
            patch: preset.patch,
            name: preset.name.clone(),
        });
    }
    rows
}

/// The parts a sixteen-channel player starts with on `presets`: the font's
/// first melodic preset (a General MIDI piano) on every channel and its first
/// drum kit on channel 10, the way a GM module powers up. A part that already
/// holds a preset the font has is kept.
pub fn default_parts(
    presets: &[SoundfontPresetInfo],
    mut channels: [SoundfontChannel; CHANNELS],
) -> [SoundfontChannel; CHANNELS] {
    let has = |preset: (i32, i32)| presets.iter().any(|p| (p.bank, p.patch) == preset);
    let melodic = presets
        .iter()
        .find(|p| p.bank < DRUM_BANK)
        .or_else(|| presets.first())
        .map(|p| (p.bank, p.patch));
    let drums = presets
        .iter()
        .find(|p| p.bank >= DRUM_BANK)
        .map(|p| (p.bank, p.patch))
        .or(melodic);
    for (index, channel) in channels.iter_mut().enumerate() {
        if channel.preset.is_some_and(has) {
            continue;
        }
        channel.preset = if index == crate::soundfont_player::PERCUSSION_CHANNEL as usize {
            drums
        } else {
            melodic
        };
    }
    channels
}

// ── Panel ──────────────────────────────────────────────────────────────────

/// The whole panel. `search` is the window's search field, `rows` the
/// browser rows for its query, and `browser_scroll` the list's scroll owner.
pub fn soundfont_player_panel(
    panel: &SoundfontPlayerPanelState,
    cb: SoundfontPlayerCallbacks,
    search: AnyElement,
    rows: Vec<BrowserRow>,
    browser_scroll: UniformListScrollHandle,
) -> AnyElement {
    let main = if panel.file_name.is_none() && !panel.loading {
        empty_state(panel, &cb)
    } else {
        instrument(panel, &cb)
    };
    div()
        .flex()
        .flex_col()
        .size_full()
        .bg(Colors::surface_window())
        .child(header(panel, &cb))
        .when_some(panel.status.clone(), |root, status| {
            root.child(status_banner(status))
        })
        .child(
            div()
                .flex()
                .flex_row()
                .flex_1()
                .min_h(px(0.0))
                .child(browser(panel, &cb, search, rows, browser_scroll))
                .child(main),
        )
        .child(keyboard_footer(panel, &cb))
        .into_any_element()
}

/// Nameplate and the two choices that define the instrument: which font, and
/// whether it is one instrument or sixteen parts.
fn header(panel: &SoundfontPlayerPanelState, cb: &SoundfontPlayerCallbacks) -> AnyElement {
    let title = panel
        .bank_name
        .clone()
        .filter(|name| !name.trim().is_empty())
        .or_else(|| panel.file_name.clone())
        .unwrap_or_else(|| "No SoundFont".to_string());
    let subtitle = if panel.loading {
        "Loading…".to_string()
    } else {
        match &panel.file_name {
            Some(file) => format!("{file} · {} presets", panel.presets.len()),
            None => "Load a .sf2 to play".to_string(),
        }
    };
    let browse = cb.on_browse.clone();
    let mut modes = fb_segmented_track();
    for (index, mode) in SoundfontPlayerMode::ALL.into_iter().enumerate() {
        let on_mode = cb.on_set_mode.clone();
        modes = modes.child(fb_segment(
            ("soundfont-mode", index),
            mode.label(),
            panel.settings.mode == mode,
            if index == 0 {
                FbSegment::First
            } else {
                FbSegment::Last
            },
            move |_, w, cx| on_mode(&mode, w, cx),
        ));
    }
    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .gap(px(space::LOOSE))
        .px(px(space::SECTION))
        .py(px(space::BASE))
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_base())
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .flex_shrink_0()
                .size(px(size::PROMINENT))
                .rounded(px(radius::CONTROL))
                .bg(Colors::surface_card())
                .child(
                    svg()
                        .path(if panel.is_multi() {
                            assets::ICON_LIST_MUSIC_PATH
                        } else {
                            assets::ICON_MUSIC_PATH
                        })
                        .size(px(16.0))
                        .text_color(if panel.is_playable() {
                            Colors::accent_primary()
                        } else {
                            Colors::text_muted()
                        }),
                ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .gap(px(space::HAIR))
                .child(
                    div()
                        .truncate()
                        .text_size(px(typography::UI_MD))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_primary())
                        .child(title),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(typography::UI_XS))
                        .text_color(Colors::text_muted())
                        .child(subtitle),
                ),
        )
        .child(modes.flex_shrink_0().w(px(212.0)))
        .child(fb_button(
            "soundfont-browse",
            "Load .sf2…",
            FbButtonKind::Default,
            !panel.loading,
            move |_, w, cx| browse(w, cx),
        ))
        .into_any_element()
}

fn status_banner(message: String) -> AnyElement {
    div()
        .flex_shrink_0()
        .mx(px(space::SECTION))
        .mt(px(space::BASE))
        .px(px(space::BASE))
        .py(px(space::SNUG))
        .rounded(px(radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::status_error())
        .bg(Colors::with_alpha(Colors::status_error(), 0.12))
        .text_size(px(typography::DENSE_LABEL))
        .text_color(Colors::status_error())
        .child(message)
        .into_any_element()
}

// ── Browser ────────────────────────────────────────────────────────────────

fn browser(
    panel: &SoundfontPlayerPanelState,
    cb: &SoundfontPlayerCallbacks,
    search: AnyElement,
    rows: Vec<BrowserRow>,
    scroll: UniformListScrollHandle,
) -> AnyElement {
    let target = panel.browser_target();
    let caption = if panel.is_multi() {
        format!("Presets → Ch {}", panel.selected_channel + 1)
    } else {
        "Presets".to_string()
    };
    let body = if panel.presets.is_empty() {
        browser_message(if panel.loading {
            "Reading presets…"
        } else {
            "No SoundFont loaded."
        })
    } else if rows.is_empty() {
        browser_message("No preset matches the search.")
    } else {
        let count = rows.len();
        let rows = Arc::new(rows);
        let on_select = cb.on_select_preset.clone();
        let playable = panel.is_playable();
        div()
            .flex_1()
            .min_h(px(0.0))
            .child(
                uniform_list("soundfont-browser", count, move |range, _window, _cx| {
                    range
                        .filter_map(|index| {
                            let row = rows.get(index)?;
                            Some(browser_row(index, row, target, playable, &on_select))
                        })
                        .collect::<Vec<_>>()
                })
                .track_scroll(&scroll)
                .size_full(),
            )
            .into_any_element()
    };
    div()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .w(px(BROWSER_W))
        .h_full()
        .border_r(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_panel())
        .child(
            div()
                .flex()
                .flex_col()
                .flex_shrink_0()
                .gap(px(space::SNUG))
                .p(px(space::BASE))
                .child(
                    div()
                        .text_size(px(typography::DENSE_CAPTION))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_faint())
                        .child(caption.to_uppercase()),
                )
                .child(search),
        )
        .child(body)
        .into_any_element()
}

fn browser_message(text: &'static str) -> AnyElement {
    div()
        .flex_1()
        .p(px(space::LOOSE))
        .text_size(px(typography::UI_XS))
        .text_color(Colors::text_muted())
        .child(text)
        .into_any_element()
}

fn browser_row(
    index: usize,
    row: &BrowserRow,
    target: Option<(i32, i32)>,
    playable: bool,
    on_select: &PresetCb,
) -> AnyElement {
    match row {
        BrowserRow::Header(group) => div()
            .id(("soundfont-browser-row", index))
            .flex()
            .items_end()
            .h(px(BROWSER_ROW_H))
            .px(px(space::LOOSE))
            .pb(px(space::HAIR))
            .text_size(px(typography::DENSE_CAPTION))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(Colors::text_faint())
            .child(group.to_uppercase())
            .into_any_element(),
        BrowserRow::Preset { bank, patch, name } => {
            let key = (*bank, *patch);
            let selected = target == Some(key);
            let on_select = on_select.clone();
            let rest = if selected {
                Colors::state_selected()
            } else {
                Colors::with_alpha(Colors::state_selected(), 0.0)
            };
            let hover = Colors::composite(Colors::surface_panel(), Colors::state_hover());
            div()
                .id(("soundfont-browser-row", index))
                .relative()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::BASE))
                .h(px(BROWSER_ROW_H))
                .px(px(space::LOOSE))
                .bg(rest)
                .when(!selected, |row| row.hover(move |s| s.bg(hover)))
                .when(playable, |row| {
                    row.cursor(gpui::CursorStyle::PointingHand)
                        .on_click(move |_, w, cx| on_select(&key, w, cx))
                })
                .when(selected, |row| row.child(leading_marker()))
                .child(
                    div()
                        .w(px(26.0))
                        .flex_shrink_0()
                        .text_size(px(typography::DENSE_LABEL))
                        .text_color(Colors::text_faint())
                        .child(format!("{patch:03}")),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .truncate()
                        .text_size(px(typography::UI_XS))
                        .text_color(if selected {
                            Colors::text_primary()
                        } else {
                            Colors::text_secondary()
                        })
                        .child(name.clone()),
                )
                .when(*bank != 0 && *bank < DRUM_BANK, |row| {
                    row.child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(typography::DENSE_CAPTION))
                            .text_color(Colors::text_faint())
                            .child(format!("B{bank}")),
                    )
                })
                .into_any_element()
        }
    }
}

/// The selected row's leading-edge accent marker, drawn over the row so the
/// selection never reflows it.
fn leading_marker() -> impl IntoElement {
    div()
        .absolute()
        .left_0()
        .top(px(space::TIGHT))
        .bottom(px(space::TIGHT))
        .w(px(2.0))
        .bg(Colors::accent_primary())
}

// ── Instrument ─────────────────────────────────────────────────────────────

fn empty_state(panel: &SoundfontPlayerPanelState, cb: &SoundfontPlayerCallbacks) -> AnyElement {
    let browse = cb.on_browse.clone();
    div()
        .flex()
        .flex_col()
        .flex_1()
        .items_center()
        .justify_center()
        .gap(px(space::LOOSE))
        .p(px(space::PAGE))
        .child(
            div()
                .text_size(px(typography::UI_MD))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_primary())
                .child(if panel.is_multi() {
                    "One SoundFont, sixteen parts"
                } else {
                    "Play any preset of a SoundFont"
                }),
        )
        .child(
            div()
                .max_w(px(340.0))
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_muted())
                .child(if panel.is_multi() {
                    "Each MIDI channel plays its own preset. Route MIDI tracks to this \
                     track and give each one a channel in its MIDI output."
                } else {
                    "Load a General MIDI or any other .sf2 bank, then pick a preset from \
                     the browser."
                }),
        )
        .child(fb_button(
            "soundfont-empty-browse",
            "Load .sf2…",
            FbButtonKind::Primary,
            true,
            move |_, w, cx| browse(w, cx),
        ))
        .into_any_element()
}

fn instrument(panel: &SoundfontPlayerPanelState, cb: &SoundfontPlayerCallbacks) -> AnyElement {
    // One scroll owner for the instrument column; the header, browser and
    // keyboard stay put when the window is short.
    let mut body = div()
        .id("soundfont-instrument")
        .flex()
        .flex_col()
        .flex_1()
        .min_w(px(0.0))
        .min_h(px(0.0))
        .overflow_y_scroll()
        .p(px(space::LOOSE))
        .gap(px(space::LOOSE));
    body = if panel.is_multi() {
        body.child(channel_rack(panel, cb))
    } else {
        body.child(preset_card(panel, cb))
    };
    body.child(
        card("Amp Envelope")
            .child(envelope_row(panel, cb))
            .child(envelope_hint(panel)),
    )
    .child(
        card("Output")
            .child(volume_row(panel, cb))
            .child(engine_row(panel, cb))
            .child(quality_row(panel, cb))
            .child(quality_hint(panel)),
    )
    .into_any_element()
}

/// A titled group of controls: the card plane, one step above the window.
fn card(title: &'static str) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        // The column is the scroll owner; a card keeps its own height so a
        // short window scrolls rather than crushing the knobs.
        .flex_shrink_0()
        .gap(px(space::SNUG))
        .p(px(space::LOOSE))
        .rounded(px(radius::SURFACE))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_card())
        .child(card_title(title))
}

fn card_title(title: &'static str) -> impl IntoElement {
    div()
        .text_size(px(typography::DENSE_CAPTION))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_faint())
        .child(title.to_uppercase())
}

/// A small note under a control group. Explains a real behavior; never
/// decoration.
fn hint(text: String, accented: bool) -> AnyElement {
    div()
        .text_size(px(typography::DENSE_CAPTION))
        .text_color(if accented {
            Colors::accent_primary()
        } else {
            Colors::text_faint()
        })
        .child(text)
        .into_any_element()
}

/// Single mode: the one preset the player plays.
fn preset_card(panel: &SoundfontPlayerPanelState, cb: &SoundfontPlayerCallbacks) -> AnyElement {
    let preset = panel.settings.preset;
    let name = panel
        .preset_name(preset)
        .map(str::to_string)
        .unwrap_or_else(|| "No preset selected".to_string());
    let detail = match preset {
        Some((bank, patch)) => format!(
            "{} · Bank {bank} · Program {}",
            preset_group(bank, patch, is_general_midi(&panel.presets)),
            patch + 1
        ),
        None => "Pick a preset in the browser.".to_string(),
    };
    let mut card = card("Preset").child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::LOOSE))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.0))
                    .gap(px(space::HAIR))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(16.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Colors::text_primary())
                            .child(name),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(typography::UI_XS))
                            .text_color(Colors::text_muted())
                            .child(detail),
                    ),
            )
            .child(test_button(panel, cb)),
    );
    if let Some((bank, _)) = preset.filter(|(bank, _)| *bank >= DRUM_BANK) {
        card = card.child(hint(
            format!(
                "Drum bank {bank}: this track's notes play on MIDI channel {}.",
                crate::soundfont_player::PERCUSSION_CHANNEL + 1
            ),
            true,
        ));
    }
    card.into_any_element()
}

fn test_button(panel: &SoundfontPlayerPanelState, cb: &SoundfontPlayerCallbacks) -> AnyElement {
    let playing = panel.testing || !panel.active_notes.is_empty();
    if playing {
        let panic = cb.on_all_notes_off.clone();
        fb_button(
            "soundfont-test-stop",
            "Stop",
            FbButtonKind::Default,
            true,
            move |_, w, cx| panic(w, cx),
        )
        .into_any_element()
    } else {
        let test = cb.on_test.clone();
        fb_button(
            "soundfont-test",
            "Test",
            FbButtonKind::Default,
            panel.is_playable() && panel.browser_target().is_some(),
            move |_, w, cx| test(w, cx),
        )
        .into_any_element()
    }
}

/// Sixteen-channel mode: one row per MIDI channel.
fn channel_rack(panel: &SoundfontPlayerPanelState, cb: &SoundfontPlayerCallbacks) -> AnyElement {
    let mut rack = div()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .rounded(px(radius::SURFACE))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_card())
        .overflow_hidden()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::BASE))
                .h(px(size::DEFAULT))
                .px(px(space::LOOSE))
                .border_b(px(1.0))
                .border_color(Colors::border_subtle())
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_faint())
                .child(div().w(px(CHANNEL_NUMBER_W)).child("CH"))
                .child(div().flex_1().min_w(px(0.0)).child("PART"))
                .child(div().w(px(CHANNEL_LEVEL_W)).child("LEVEL"))
                .child(div().w(px(CHANNEL_PAN_W)).child("PAN"))
                .child(div().w(px(CHANNEL_LATCH_W)).child(""))
                .child(test_button(panel, cb)),
        );
    for channel in 0..CHANNELS as u8 {
        rack = rack.child(channel_row(panel, cb, channel));
    }
    let mut column = div()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .gap(px(space::SNUG))
        .child(rack);
    if !panel.per_note_sources.is_empty() {
        column = column.child(hint(
            format!(
                "Per-note channels: {} — each note plays the part of its own channel.",
                panel.per_note_sources.join(", ")
            ),
            false,
        ));
    }
    column
        .child(hint(
            "Route MIDI tracks to this track and set each one's MIDI output channel; the \
             browser assigns a preset to the selected part."
                .to_string(),
            false,
        ))
        .into_any_element()
}

const CHANNEL_NUMBER_W: f32 = 22.0;
const CHANNEL_LEVEL_W: f32 = 116.0;
const CHANNEL_PAN_W: f32 = 58.0;
const CHANNEL_LATCH_W: f32 = 44.0;

fn channel_row(
    panel: &SoundfontPlayerPanelState,
    cb: &SoundfontPlayerCallbacks,
    channel: u8,
) -> AnyElement {
    let index = channel as usize;
    let part = panel.settings.channels[index];
    let selected = panel.selected_channel == channel;
    let audible =
        crate::soundfont_player::audible_channels(&panel.settings.channels) & (1 << index) != 0;
    let name = panel
        .preset_name(part.preset)
        .map(str::to_string)
        .or_else(|| {
            part.preset
                .map(|(bank, patch)| format!("Missing {bank}:{patch}"))
        })
        .unwrap_or_else(|| "Font default".to_string());
    let sources = &panel.channel_sources[index];
    let select = cb.on_select_channel.clone();
    let hover = Colors::composite(Colors::surface_card(), Colors::state_hover());

    let set = cb.on_set_channel.clone();
    let level = {
        let set = set.clone();
        move |value: &f32, w: &mut Window, cx: &mut App| {
            let volume = (value.clamp(0.0, 1.0) * 127.0).round() as u8;
            set(&(channel, SoundfontChannel { volume, ..part }), w, cx);
        }
    };
    let pan = {
        let set = set.clone();
        move |value: &f32, w: &mut Window, cx: &mut App| {
            let pan = (64.0 + value.clamp(-1.0, 1.0) * 63.5)
                .round()
                .clamp(0.0, 127.0) as u8;
            set(&(channel, SoundfontChannel { pan, ..part }), w, cx);
        }
    };
    let mute = {
        let set = set.clone();
        move |_: &gpui::ClickEvent, w: &mut Window, cx: &mut App| {
            set(
                &(
                    channel,
                    SoundfontChannel {
                        mute: !part.mute,
                        ..part
                    },
                ),
                w,
                cx,
            );
        }
    };
    let solo = move |_: &gpui::ClickEvent, w: &mut Window, cx: &mut App| {
        set(
            &(
                channel,
                SoundfontChannel {
                    solo: !part.solo,
                    ..part
                },
            ),
            w,
            cx,
        );
    };
    let pan_value = (part.pan as f32 - 64.0) / 63.5;

    div()
        .id(("soundfont-channel", index))
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .h(px(CHANNEL_ROW_H))
        .px(px(space::LOOSE))
        .when(index + 1 < CHANNELS, |row| {
            row.border_b(px(1.0)).border_color(Colors::border_subtle())
        })
        .bg(if selected {
            Colors::state_selected()
        } else {
            Colors::with_alpha(Colors::state_selected(), 0.0)
        })
        .when(!selected, |row| row.hover(move |s| s.bg(hover)))
        .cursor(gpui::CursorStyle::PointingHand)
        .on_click(move |_, w, cx| select(&channel, w, cx))
        .when(selected, |row| row.child(leading_marker()))
        .child(
            div()
                .w(px(CHANNEL_NUMBER_W))
                .flex_shrink_0()
                .text_size(px(typography::UI_XS))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(if selected {
                    Colors::accent_primary()
                } else {
                    Colors::text_muted()
                })
                .child(format!("{}", channel + 1)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .opacity(if audible { 1.0 } else { 0.45 })
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(space::SNUG))
                        .min_w(px(0.0))
                        .child(
                            div()
                                .truncate()
                                .min_w(px(0.0))
                                .text_size(px(typography::UI_XS))
                                .text_color(Colors::text_primary())
                                .child(name),
                        )
                        .when(part.is_drum_kit(), |row| {
                            row.child(
                                div()
                                    .flex_shrink_0()
                                    .px(px(space::TIGHT))
                                    .rounded(px(radius::MICRO))
                                    .bg(Colors::surface_badge())
                                    .text_size(px(typography::DENSE_CAPTION))
                                    .text_color(Colors::text_muted())
                                    .child("KIT"),
                            )
                        }),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(typography::DENSE_CAPTION))
                        .text_color(if sources.is_empty() {
                            Colors::text_disabled()
                        } else {
                            Colors::text_faint()
                        })
                        .child(if sources.is_empty() {
                            "No track".to_string()
                        } else {
                            format!("← {}", sources.join(", "))
                        }),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::SNUG))
                .w(px(CHANNEL_LEVEL_W))
                .flex_shrink_0()
                .child(div().flex_1().min_w(px(0.0)).child(slider(
                    format!("soundfont-channel-level-{index}"),
                    part.volume as f32 / 127.0,
                    Colors::accent_primary(),
                    level,
                )))
                .child(
                    div()
                        .w(px(22.0))
                        .flex_shrink_0()
                        .text_size(px(typography::DENSE_LABEL))
                        .text_color(Colors::text_secondary())
                        .child(part.volume.to_string()),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::TIGHT))
                .w(px(CHANNEL_PAN_W))
                .flex_shrink_0()
                .child(knob_bipolar(
                    format!("soundfont-channel-pan-{index}"),
                    pan_value,
                    -1.0,
                    1.0,
                    20.0,
                    Colors::accent_primary(),
                    None,
                    0.0,
                    pan,
                ))
                .child(
                    div()
                        .text_size(px(typography::DENSE_LABEL))
                        .text_color(Colors::text_secondary())
                        .child(format_pan_label(pan_value)),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(space::TIGHT))
                .w(px(CHANNEL_LATCH_W))
                .flex_shrink_0()
                .child(fb_toggle(
                    ("soundfont-channel-mute", index),
                    "M",
                    FbLatch::Mute,
                    part.mute,
                    size::MICRO,
                    mute,
                ))
                .child(fb_toggle(
                    ("soundfont-channel-solo", index),
                    "S",
                    FbLatch::Solo,
                    part.solo,
                    size::MICRO,
                    solo,
                )),
        )
        .into_any_element()
}

fn volume_row(panel: &SoundfontPlayerPanelState, cb: &SoundfontPlayerCallbacks) -> AnyElement {
    let on_change = cb.on_set_volume.clone();
    let volume = panel.settings.volume;
    labelled_row(
        "Volume",
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(space::BASE))
            .flex_1()
            .child(div().flex_1().min_w(px(0.0)).child(slider(
                "soundfont-volume",
                volume,
                Colors::accent_primary(),
                move |value, w, cx| on_change(value, w, cx),
            )))
            .child(
                div()
                    .w(px(36.0))
                    .flex_shrink_0()
                    .text_size(px(typography::DENSE_LABEL))
                    .text_color(Colors::text_secondary())
                    .child(format!("{:.0}%", volume * 100.0)),
            ),
    )
}

fn labelled_row(label: &'static str, content: impl IntoElement) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .min_h(px(size::COMFORTABLE))
        .gap(px(space::BASE))
        .child(
            div()
                .w(px(72.0))
                .flex_shrink_0()
                .text_size(px(typography::DENSE_LABEL))
                .text_color(Colors::text_muted())
                .child(label),
        )
        .child(content)
        .into_any_element()
}

fn engine_row(panel: &SoundfontPlayerPanelState, cb: &SoundfontPlayerCallbacks) -> AnyElement {
    let toggle_reverb = cb.on_toggle_reverb_chorus.clone();
    let set_polyphony_dec = cb.on_set_polyphony.clone();
    let set_polyphony_inc = cb.on_set_polyphony.clone();
    let polyphony = panel.settings.polyphony;
    let dec_value = polyphony.saturating_sub(POLYPHONY_STEP).max(POLYPHONY_MIN);
    let inc_value = (polyphony + POLYPHONY_STEP).min(POLYPHONY_MAX);
    labelled_row(
        "Voices",
        div()
            .flex()
            .flex_row()
            .items_center()
            .flex_1()
            .gap(px(space::SNUG))
            .child(fb_stepper_button(
                "soundfont-polyphony-dec",
                "−",
                move |_, w, cx| set_polyphony_dec(&dec_value, w, cx),
            ))
            .child(
                div()
                    .w(px(34.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(typography::UI_XS))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(Colors::text_primary())
                    .child(polyphony.to_string()),
            )
            .child(fb_stepper_button(
                "soundfont-polyphony-inc",
                "+",
                move |_, w, cx| set_polyphony_inc(&inc_value, w, cx),
            ))
            .child(div().flex_1())
            .child(fb_checkbox(
                "soundfont-reverb-chorus",
                "Reverb & Chorus",
                panel.settings.reverb_chorus,
                !panel.loading,
                move |_, w, cx| toggle_reverb(w, cx),
            )),
    )
}

/// The A/D/S/R knob row. Four knobs on one baseline with their values read out
/// underneath, which is the layout a sampler player's envelope is scanned in.
fn envelope_row(panel: &SoundfontPlayerPanelState, cb: &SoundfontPlayerCallbacks) -> AnyElement {
    let envelope = panel.settings.envelope.sanitized();
    div()
        .flex()
        .flex_row()
        .items_start()
        .gap(px(space::SECTION))
        .child(envelope_knob(
            "soundfont-env-attack",
            "Attack",
            envelope,
            envelope.attack_ms,
            0.0,
            ENVELOPE_KNOB_MAX_MS,
            format_ms(envelope.attack_ms),
            cb.on_set_envelope.clone(),
            |envelope, value| envelope.attack_ms = value,
        ))
        .child(envelope_knob(
            "soundfont-env-decay",
            "Decay",
            envelope,
            envelope.decay_ms,
            0.0,
            ENVELOPE_KNOB_MAX_MS,
            format_ms(envelope.decay_ms),
            cb.on_set_envelope.clone(),
            |envelope, value| envelope.decay_ms = value,
        ))
        .child(envelope_knob(
            "soundfont-env-sustain",
            "Sustain",
            envelope,
            envelope.sustain,
            0.0,
            1.0,
            format!("{:.0}%", envelope.sustain * 100.0),
            cb.on_set_envelope.clone(),
            |envelope, value| envelope.sustain = value,
        ))
        .child(envelope_knob(
            "soundfont-env-release",
            "Release",
            envelope,
            envelope.release_ms,
            0.0,
            ENVELOPE_KNOB_MAX_MS,
            format_ms(envelope.release_ms),
            cb.on_set_envelope.clone(),
            |envelope, value| envelope.release_ms = value,
        ))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .justify_end()
                .child(envelope_reset(panel, cb)),
        )
        .into_any_element()
}

/// Widest time the knobs sweep to. The engine accepts up to
/// [`crate::soundfont_player::ENVELOPE_MAX_TIME_MS`], but a 4-second sweep
/// keeps the useful part of the range under the pointer instead of
/// compressing it into the first few degrees.
const ENVELOPE_KNOB_MAX_MS: f32 = 4_000.0;

fn format_ms(ms: f32) -> String {
    if ms <= 0.0 {
        "Off".to_string()
    } else if ms < 1_000.0 {
        format!("{ms:.0} ms")
    } else {
        format!("{:.2} s", ms / 1_000.0)
    }
}

/// One envelope knob plus its label and readout. The knob reports an absolute
/// value for its own parameter only, so `base` — the envelope as it stands —
/// travels with it and `apply` writes the dragged value into a copy. The
/// callback therefore always carries a complete, consistent struct.
#[allow(clippy::too_many_arguments)]
fn envelope_knob(
    id: &'static str,
    label: &'static str,
    base: SoundfontEnvelope,
    value: f32,
    min: f32,
    max: f32,
    readout: String,
    on_change: EnvelopeCb,
    apply: impl Fn(&mut SoundfontEnvelope, f32) + 'static,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .w(px(52.0))
        .gap(px(space::HAIR))
        .child(knob(
            id,
            value,
            min,
            max,
            Colors::accent_primary(),
            None,
            move |new_value, w, cx| {
                let mut envelope = base;
                apply(&mut envelope, *new_value);
                on_change(&envelope, w, cx);
            },
        ))
        .child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .text_color(Colors::text_muted())
                .child(label),
        )
        .child(
            div()
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_secondary())
                .child(readout),
        )
        .into_any_element()
}

fn envelope_reset(panel: &SoundfontPlayerPanelState, cb: &SoundfontPlayerCallbacks) -> AnyElement {
    let on_change = cb.on_set_envelope.clone();
    let bypassed = panel.settings.envelope.is_bypassed();
    fb_button(
        "soundfont-env-reset",
        "Reset",
        FbButtonKind::Default,
        !bypassed,
        move |_, w, cx| on_change(&SoundfontEnvelope::default(), w, cx),
    )
    .into_any_element()
}

/// Says what the envelope is actually doing. `DESIGN.md`: a control that looks
/// active must connect to real behavior, and an inactive one must say so.
fn envelope_hint(panel: &SoundfontPlayerPanelState) -> AnyElement {
    if panel.settings.envelope.is_bypassed() {
        return hint(
            "Bypassed — the SoundFont's own envelopes play unchanged.".to_string(),
            false,
        );
    }
    let mut text = String::from(
        "Shapes the whole player's output: attack and decay run from silence, release when the last note ends.",
    );
    if panel.settings.envelope.sanitized().release_ms <= 0.0 {
        text.push_str(" Release Off keeps the SoundFont tail.");
    }
    hint(text, true)
}

fn quality_row(panel: &SoundfontPlayerPanelState, cb: &SoundfontPlayerCallbacks) -> AnyElement {
    let mut track = fb_segmented_track();
    let last = SoundfontRenderQuality::ALL.len() - 1;
    for (index, quality) in SoundfontRenderQuality::ALL.into_iter().enumerate() {
        let on_change = cb.on_set_quality.clone();
        track = track.child(fb_segment(
            ("soundfont-quality", quality.oversample()),
            quality.label(),
            panel.settings.quality == quality,
            match index {
                0 => FbSegment::First,
                i if i == last => FbSegment::Last,
                _ => FbSegment::Middle,
            },
            move |_, w, cx| on_change(&quality, w, cx),
        ));
    }
    labelled_row("Quality", track.flex_1())
}

fn quality_hint(panel: &SoundfontPlayerPanelState) -> AnyElement {
    let factor = panel.settings.quality.oversample();
    let text = if factor == 1 {
        "Renders at the project rate. Raise this if transposed samples sound harsh.".to_string()
    } else {
        format!(
            "Renders at {factor}x internally and filters back down — less sampler aliasing, about {factor}x the CPU."
        )
    };
    hint(text, factor > 1)
}

// ── Keyboard ───────────────────────────────────────────────────────────────

/// Semitone offsets of the white keys within one octave, and of the black keys
/// with the white key each sits after.
const WHITE_SEMITONES: [u8; 7] = [0, 2, 4, 5, 7, 9, 11];
const BLACK_SEMITONES: [(usize, u8); 5] = [(0, 1), (1, 3), (3, 6), (4, 8), (5, 10)];
const WHITE_KEY_W: f32 = 26.0;
const BLACK_KEY_W: f32 = 16.0;
const KEY_H: f32 = 58.0;
const BLACK_KEY_H: f32 = 36.0;

const NOTE_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

/// `60` → `"C4"`, matching the pitch labels used elsewhere in Studio.
pub fn note_label(pitch: u8) -> String {
    let octave = pitch as i32 / 12 - 1;
    format!("{}{octave}", NOTE_NAMES[pitch as usize % 12])
}

fn keyboard_footer(panel: &SoundfontPlayerPanelState, cb: &SoundfontPlayerCallbacks) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .gap(px(space::SNUG))
        .px(px(space::SECTION))
        .pt(px(space::BASE))
        .pb(px(space::LOOSE))
        .border_t(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_base())
        .child(keyboard_header(panel, cb))
        .child(div().flex().justify_center().child(keyboard(panel, cb)))
        .into_any_element()
}

fn keyboard_header(panel: &SoundfontPlayerPanelState, cb: &SoundfontPlayerCallbacks) -> AnyElement {
    let down = cb.on_shift_octave.clone();
    let up = cb.on_shift_octave.clone();
    let highest = panel
        .keyboard_root
        .saturating_add((KEYBOARD_WHITE_KEYS.div_ceil(7) * 12) as u8)
        .min(127);
    let target = if panel.is_multi() {
        let part = panel.settings.channels[panel.selected_channel as usize % CHANNELS];
        format!(
            "Ch {} · {}",
            panel.selected_channel + 1,
            panel.preset_name(part.preset).unwrap_or("Font default")
        )
    } else {
        panel
            .preset_name(panel.settings.preset)
            .unwrap_or("No preset")
            .to_string()
    };
    let status = if panel.active_notes.is_empty() {
        format!(
            "{} – {}",
            note_label(panel.keyboard_root),
            note_label(highest)
        )
    } else {
        let names: Vec<String> = panel.active_notes.iter().copied().map(note_label).collect();
        format!("Playing {}", names.join(" "))
    };

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(typography::DENSE_CAPTION))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_faint())
                .child("KEYBOARD"),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(typography::UI_XS))
                .text_color(Colors::text_secondary())
                .child(target),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_size(px(typography::DENSE_LABEL))
                .text_color(if panel.active_notes.is_empty() {
                    Colors::text_muted()
                } else {
                    Colors::accent_primary()
                })
                .child(status),
        )
        .child(fb_stepper_button(
            "soundfont-octave-down",
            "−",
            move |_, w, cx| down(&-1, w, cx),
        ))
        .child(fb_stepper_button(
            "soundfont-octave-up",
            "+",
            move |_, w, cx| up(&1, w, cx),
        ))
        .into_any_element()
}

/// The panel keyboard. Press-and-hold plays through the engine on the owning
/// track: the same MIDI preview path the piano roll uses, not a separate
/// preview synth.
fn keyboard(panel: &SoundfontPlayerPanelState, cb: &SoundfontPlayerCallbacks) -> AnyElement {
    let playable = panel.is_playable();
    let mut white_row = div().flex().flex_row().h(px(KEY_H));
    for index in 0..KEYBOARD_WHITE_KEYS {
        let Some(pitch) = white_pitch(panel.keyboard_root, index) else {
            continue;
        };
        white_row = white_row.child(key(
            ("soundfont-white-key", index),
            pitch,
            false,
            panel.active_notes.contains(&pitch),
            playable,
            cb,
        ));
    }

    let mut board = div()
        .relative()
        .w(px(WHITE_KEY_W * KEYBOARD_WHITE_KEYS as f32))
        .h(px(KEY_H))
        .rounded(px(radius::CONTROL))
        .overflow_hidden()
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_muted())
        .child(white_row);

    for index in 0..KEYBOARD_WHITE_KEYS {
        let octave = index / 7;
        let degree = index % 7;
        let Some((_, semitone)) = BLACK_SEMITONES.iter().find(|(white, _)| *white == degree) else {
            continue;
        };
        let pitch = panel.keyboard_root as u16 + (octave * 12) as u16 + *semitone as u16;
        if pitch > 127 {
            continue;
        }
        let pitch = pitch as u8;
        let left = WHITE_KEY_W * (index as f32 + 1.0) - BLACK_KEY_W / 2.0;
        board = board.child(
            key(
                ("soundfont-black-key", index),
                pitch,
                true,
                panel.active_notes.contains(&pitch),
                playable,
                cb,
            )
            .absolute()
            .left(px(left))
            .top(px(0.0)),
        );
    }

    board.into_any_element()
}

fn white_pitch(root: u8, index: usize) -> Option<u8> {
    let pitch = root as u16 + ((index / 7) * 12) as u16 + WHITE_SEMITONES[index % 7] as u16;
    (pitch <= 127).then_some(pitch as u8)
}

fn key(
    id: impl Into<gpui::ElementId>,
    pitch: u8,
    black: bool,
    active: bool,
    playable: bool,
    cb: &SoundfontPlayerCallbacks,
) -> gpui::Stateful<gpui::Div> {
    let note_on = cb.on_note_on.clone();
    let note_off = cb.on_note_off.clone();
    let rest = if active {
        Colors::accent_muted()
    } else if black {
        Colors::piano_black_key()
    } else {
        Colors::piano_white_key()
    };
    let mut key = div()
        .id(id)
        .w(px(if black { BLACK_KEY_W } else { WHITE_KEY_W }))
        .h(px(if black { BLACK_KEY_H } else { KEY_H }))
        .flex()
        .items_end()
        .justify_center()
        .pb(px(space::TIGHT))
        .border_r(px(1.0))
        .border_color(Colors::piano_key_seam())
        .bg(rest)
        .text_size(px(8.0))
        .text_color(if active {
            Colors::accent_primary()
        } else {
            Colors::piano_key_label()
        })
        .child(if black || pitch % 12 != 0 {
            String::new()
        } else {
            note_label(pitch)
        });

    if black {
        key = key.rounded_b(px(radius::CONTROL_SM)).border(px(1.0));
    }

    if playable {
        let hover = Colors::composite(rest, Colors::state_hover());
        key = key
            .cursor(gpui::CursorStyle::PointingHand)
            .when(!active, |key| key.hover(move |s| s.bg(hover)))
            .on_mouse_down(gpui::MouseButton::Left, move |_, w, cx| {
                note_on(&pitch, w, cx)
            })
            .on_mouse_up(gpui::MouseButton::Left, move |_, w, cx| {
                note_off(&pitch, w, cx)
            });
    }
    key
}

fn empty_document() -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(typography::UI_XS))
        .text_color(Colors::text_muted())
        .child("Empty document")
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset(bank: i32, patch: i32, name: &str) -> SoundfontPresetInfo {
        SoundfontPresetInfo {
            bank,
            patch,
            name: name.to_string(),
        }
    }

    /// A General MIDI bank: all 128 programs of bank 0 (named for the few the
    /// tests look at), a variation and two kits.
    fn gm_presets() -> Vec<SoundfontPresetInfo> {
        let mut presets: Vec<SoundfontPresetInfo> = (0..128)
            .map(|patch| {
                let name = match patch {
                    0 => "Grand Piano".to_string(),
                    1 => "Bright Piano".to_string(),
                    33 => "Finger Bass".to_string(),
                    other => format!("Program {other}"),
                };
                preset(0, patch, &name)
            })
            .collect();
        presets.push(preset(8, 4, "Detuned EP"));
        presets.push(preset(128, 0, "Standard Kit"));
        presets.push(preset(128, 25, "TR-808"));
        presets
    }

    #[test]
    fn ensure_soundfont_player_reuses_existing_document() {
        let mut state = MdiWorkspaceState::default();
        let first = ensure_soundfont_player_document(&mut state);
        let second = ensure_soundfont_player_document(&mut state);
        assert_eq!(first, second);
        assert_eq!(state.document_count(), 1);
    }

    #[test]
    fn the_browser_groups_a_general_midi_bank_by_family_then_banks_then_kits() {
        let rows = browser_rows(&gm_presets(), "");
        let headers: Vec<&str> = rows
            .iter()
            .filter_map(|row| match row {
                BrowserRow::Header(group) => Some(group.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            headers.len(),
            16 + 2,
            "the sixteen families, a bank, the kits"
        );
        assert_eq!(headers[..2], ["Piano", "Chromatic Percussion"]);
        assert_eq!(headers[16..], ["Bank 8", "Drum Kits"]);
        assert_eq!(rows.len(), 18 + 131);
    }

    #[test]
    fn a_font_that_is_not_a_general_midi_bank_is_listed_by_bank() {
        // One instrument at program 0 is not a piano.
        let rows = browser_rows(&[preset(0, 0, "Alto Sax")], "");
        assert_eq!(
            rows,
            [
                BrowserRow::Header("Bank 0".to_string()),
                BrowserRow::Preset {
                    bank: 0,
                    patch: 0,
                    name: "Alto Sax".to_string()
                },
            ]
        );
    }

    #[test]
    fn a_search_keeps_matching_presets_under_their_group() {
        let rows = browser_rows(&gm_presets(), "  FINGER ");
        assert_eq!(
            rows,
            [
                BrowserRow::Header("Bass".to_string()),
                BrowserRow::Preset {
                    bank: 0,
                    patch: 33,
                    name: "Finger Bass".to_string()
                },
            ]
        );
        // A group name matches every preset in it.
        let kits = browser_rows(&gm_presets(), "drum");
        assert_eq!(kits.len(), 3);
        assert!(browser_rows(&gm_presets(), "zzz").is_empty());
    }

    #[test]
    fn default_parts_are_a_piano_everywhere_and_a_kit_on_channel_ten() {
        let parts = default_parts(&gm_presets(), sphere_soundfont_player::default_channels());
        assert_eq!(parts[0].preset, Some((0, 0)));
        assert_eq!(parts[15].preset, Some((0, 0)));
        assert_eq!(parts[9].preset, Some((128, 0)));
    }

    #[test]
    fn default_parts_keep_a_preset_the_font_has_and_replace_one_it_lacks() {
        let mut channels = sphere_soundfont_player::default_channels();
        channels[2].preset = Some((0, 33));
        channels[3].preset = Some((5, 99));
        let parts = default_parts(&gm_presets(), channels);
        assert_eq!(parts[2].preset, Some((0, 33)));
        assert_eq!(parts[3].preset, Some((0, 0)));
    }

    #[test]
    fn the_keyboard_and_browser_follow_the_selected_part_in_multi_mode() {
        let mut panel = SoundfontPlayerPanelState::default();
        panel.settings.preset = Some((0, 1));
        panel.settings.channels[4].preset = Some((0, 33));
        panel.selected_channel = 4;
        assert_eq!(panel.browser_target(), Some((0, 1)));
        assert_eq!(panel.preview_channel(), 0);
        panel.settings.mode = SoundfontPlayerMode::Multi;
        assert_eq!(panel.browser_target(), Some((0, 33)));
        assert_eq!(panel.preview_channel(), 4);
    }

    #[test]
    fn note_labels_match_middle_c_convention() {
        assert_eq!(note_label(60), "C4");
        assert_eq!(note_label(61), "C#4");
        assert_eq!(note_label(48), "C3");
        assert_eq!(note_label(0), "C-1");
    }

    #[test]
    fn white_keys_walk_the_major_scale_from_the_root() {
        let root = KEYBOARD_DEFAULT_ROOT;
        let pitches: Vec<u8> = (0..8).filter_map(|i| white_pitch(root, i)).collect();
        assert_eq!(pitches, vec![48, 50, 52, 53, 55, 57, 59, 60]);
    }

    #[test]
    fn white_keys_stop_at_the_top_of_the_midi_range() {
        assert_eq!(white_pitch(120, 0), Some(120));
        assert_eq!(white_pitch(120, 6), None);
    }

    #[test]
    fn octave_shift_clamps_to_the_playable_range() {
        let mut panel = SoundfontPlayerPanelState::default();
        panel.shift_keyboard_octave(-1);
        assert_eq!(panel.keyboard_root, KEYBOARD_DEFAULT_ROOT - 12);
        for _ in 0..12 {
            panel.shift_keyboard_octave(-1);
        }
        assert_eq!(panel.keyboard_root, 0);
        for _ in 0..12 {
            panel.shift_keyboard_octave(1);
        }
        assert_eq!(panel.keyboard_root, 108);
    }

    #[test]
    fn a_fresh_panel_reports_an_unshaped_single_instrument() {
        // The defaults must be the pass-through state, so opening the window on
        // an existing track cannot change how it already sounds.
        let panel = SoundfontPlayerPanelState::default();
        assert!(panel.settings.envelope.is_bypassed());
        assert_eq!(panel.settings.quality, SoundfontRenderQuality::Standard);
        assert_eq!(panel.settings.volume, 1.0);
        assert!(!panel.is_multi());
    }

    #[test]
    fn envelope_times_read_out_with_their_unit_and_name_zero_as_off() {
        assert_eq!(format_ms(0.0), "Off");
        assert_eq!(format_ms(250.0), "250 ms");
        assert_eq!(format_ms(1_500.0), "1.50 s");
    }

    #[test]
    fn the_knob_sweep_stays_inside_the_range_the_engine_accepts() {
        const _: () =
            assert!(ENVELOPE_KNOB_MAX_MS <= crate::soundfont_player::ENVELOPE_MAX_TIME_MS);
    }

    #[test]
    fn panel_is_only_playable_once_a_font_is_loaded() {
        let mut panel = SoundfontPlayerPanelState::default();
        assert!(!panel.is_playable(), "no font loaded yet");
        panel.file_name = Some("GeneralUser-GS.sf2".to_string());
        assert!(panel.is_playable());
        panel.loading = true;
        assert!(!panel.is_playable(), "a load in flight blocks gestures");
    }
}
