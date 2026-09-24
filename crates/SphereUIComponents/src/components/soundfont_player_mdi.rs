//! SoundFont Player MDI document UI.
//!
//! Feature set (preset browser, volume, reverb/chorus, polyphony, keyboard
//! range) is modeled after General MIDI soundfont players like Fruity
//! Soundfont Player / LiveSynth Pro. The visual language is Futureboard's own
//! flat/dark/token-driven chrome — no skeuomorphic reproduction of another
//! plugin's skin (see `DESIGN.md` / `tasks/SKILL.md` "no copied plugin
//! branding"). All preset/bank data comes from
//! [`crate::soundfont_player::SoundfontPresetInfo`] (`SphereSoundfontPlayer`,
//! no gpui dependency) — this file only renders it.

use std::sync::Arc;

use gpui::{
    div, px, svg, AnyElement, App, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, Window,
};

use crate::assets;
use crate::components::controls::{
    fb_button, fb_checkbox, fb_segmented_button, fb_stepper_button, FbButtonKind,
};
use crate::components::form::{
    select_dismiss_backdrop, select_with_placement, SelectMenuPlacement, SelectOption,
};
use crate::components::knob::knob;
use crate::components::mdi::{
    mdi_workspace, MdiDocumentKind, MdiWorkspaceCallbacks, MdiWorkspaceState,
};
use crate::components::slider::slider;
use crate::soundfont_player::{
    SoundfontEnvelope, SoundfontPresetInfo, SoundfontRenderQuality, DRUM_BANK, PERCUSSION_CHANNEL,
};
use crate::theme::Colors;

pub const SOUNDFONT_PLAYER_MDI_TITLE: &str = "Soundfont Player";

/// Preset browser + engine control state for one Soundfont Player document.
/// Transient UI state — the real preset/bank data and player instance live in
/// [`crate::components::soundfont_player_window::SoundfontPlayerWindow`].
#[derive(Clone)]
pub struct SoundfontPlayerPanelState {
    pub file_name: Option<String>,
    pub bank_name: Option<String>,
    pub presets: Vec<SoundfontPresetInfo>,
    pub selected_preset: Option<(i32, i32)>,
    pub master_volume: f32,
    pub reverb_chorus: bool,
    pub polyphony: usize,
    /// Amp envelope over the player's output. Default is bypassed — the panel
    /// says so rather than implying the SoundFont is being reshaped.
    pub envelope: SoundfontEnvelope,
    /// Internal synthesis oversampling.
    pub quality: SoundfontRenderQuality,
    pub preset_list_open: bool,
    pub loading: bool,
    pub status: Option<String>,
    /// MIDI note of the leftmost key on the panel keyboard.
    pub keyboard_root: u8,
    /// Notes the panel is currently holding on the engine, so keys and the
    /// Test button show what is actually sounding.
    pub active_notes: Vec<u8>,
    /// `true` while the Test button's audition is holding notes.
    pub testing: bool,
}

/// Two octaves starting at C3 — enough to reach a recognisable range without
/// crowding the window, and centred where a General MIDI preset sounds best.
pub const KEYBOARD_DEFAULT_ROOT: u8 = 48;
/// How many white keys the panel keyboard shows.
const KEYBOARD_WHITE_KEYS: usize = 14;
const KEYBOARD_LOWEST_ROOT: u8 = 0;
const KEYBOARD_HIGHEST_ROOT: u8 = 108;

impl Default for SoundfontPlayerPanelState {
    fn default() -> Self {
        Self {
            file_name: None,
            bank_name: None,
            presets: Vec::new(),
            selected_preset: None,
            master_volume: 1.0,
            reverb_chorus: true,
            polyphony: 64,
            envelope: SoundfontEnvelope::default(),
            quality: SoundfontRenderQuality::default(),
            preset_list_open: false,
            loading: false,
            status: None,
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

    pub fn shift_keyboard_octave(&mut self, delta: i32) {
        let root = self.keyboard_root as i32 + delta * 12;
        self.keyboard_root =
            root.clamp(KEYBOARD_LOWEST_ROOT as i32, KEYBOARD_HIGHEST_ROOT as i32) as u8;
    }
}

type SoundfontVoidCb = Arc<dyn Fn(&mut Window, &mut App) + 'static>;
type SoundfontPresetCb = Arc<dyn Fn(&(i32, i32), &mut Window, &mut App) + 'static>;
type SoundfontF32Cb = Arc<dyn Fn(&f32, &mut Window, &mut App) + 'static>;
type SoundfontUsizeCb = Arc<dyn Fn(&usize, &mut Window, &mut App) + 'static>;
type SoundfontNoteCb = Arc<dyn Fn(&u8, &mut Window, &mut App) + 'static>;
type SoundfontI32Cb = Arc<dyn Fn(&i32, &mut Window, &mut App) + 'static>;
type SoundfontEnvelopeCb = Arc<dyn Fn(&SoundfontEnvelope, &mut Window, &mut App) + 'static>;
type SoundfontQualityCb = Arc<dyn Fn(&SoundfontRenderQuality, &mut Window, &mut App) + 'static>;

#[derive(Clone)]
pub struct SoundfontPlayerCallbacks {
    pub on_browse: SoundfontVoidCb,
    pub on_toggle_preset_list: SoundfontVoidCb,
    pub on_select_preset: SoundfontPresetCb,
    pub on_set_volume: SoundfontF32Cb,
    pub on_toggle_reverb_chorus: SoundfontVoidCb,
    pub on_set_polyphony: SoundfontUsizeCb,
    /// One complete envelope — the panel sends the whole struct so a knob drag
    /// cannot land a partial edit.
    pub on_set_envelope: SoundfontEnvelopeCb,
    pub on_set_quality: SoundfontQualityCb,
    /// Press and release of one panel key — routed to the engine so the note
    /// sounds through the track it belongs to.
    pub on_note_on: SoundfontNoteCb,
    pub on_note_off: SoundfontNoteCb,
    /// Auditions the selected preset without needing the transport or a clip.
    pub on_test: SoundfontVoidCb,
    /// Releases everything the panel is holding.
    pub on_all_notes_off: SoundfontVoidCb,
    pub on_shift_octave: SoundfontI32Cb,
}

const POLYPHONY_MIN: usize = 1;
const POLYPHONY_MAX: usize = 256;
const POLYPHONY_STEP: usize = 8;

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
        MdiDocumentKind::SoundfontPlayer => soundfont_player_panel(panel, panel_callbacks.clone()),
        MdiDocumentKind::Generic => empty_document(),
    })
}

pub fn soundfont_player_panel(
    panel: &SoundfontPlayerPanelState,
    cb: SoundfontPlayerCallbacks,
) -> AnyElement {
    let title = panel
        .bank_name
        .clone()
        .unwrap_or_else(|| "Soundfont Player".to_string());
    let subtitle = if panel.loading {
        "Loading…".to_string()
    } else {
        panel
            .file_name
            .clone()
            .unwrap_or_else(|| "No SoundFont loaded".to_string())
    };

    // One scroll owner for the whole instrument body. The header and the
    // keyboard footer are pinned outside it, so the two things a player needs
    // at all times — which patch is loaded, and something to press — never
    // scroll away when the window is short.
    let mut body = div()
        .id("soundfont-player-body")
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.0))
        .overflow_y_scroll()
        .p(px(SECTION_PAD))
        .gap(px(10.0));

    let mut source = section("Source")
        .child(soundfont_row(panel, &cb))
        .child(preset_row(panel, &cb));
    if let Some(hint) = preset_routing_hint(panel) {
        source = source.child(hint);
    }
    body = body
        .child(source)
        .child(
            section("Amp Envelope")
                .child(envelope_row(panel, &cb))
                .child(envelope_hint(panel)),
        )
        .child(
            section("Output")
                .child(volume_row(panel, &cb))
                .child(engine_row(panel, &cb))
                .child(quality_row(panel, &cb))
                .child(quality_hint(panel)),
        );

    if let Some(status) = panel.status.clone() {
        body = body.child(status_banner(status));
    }

    let dismiss = cb.on_toggle_preset_list.clone();
    let mut root = div()
        // Layout owner for the panel and the anchor plane for the preset
        // dropdown's click-outside backdrop.
        .relative()
        .flex()
        .flex_col()
        .size_full()
        .bg(Colors::surface_window())
        .child(header_row(&title, &subtitle, panel))
        .child(body)
        .child(
            div()
                .flex()
                .flex_col()
                .flex_shrink_0()
                .gap(px(5.0))
                .px(px(SECTION_PAD))
                .pb(px(SECTION_PAD))
                .pt(px(8.0))
                .border_t(px(1.0))
                .border_color(Colors::border_subtle())
                .bg(Colors::surface_base())
                .child(keyboard_header(panel, &cb))
                .child(keyboard(panel, &cb)),
        );
    if panel.preset_list_open {
        // The deferred menu paints above this and occludes its own clicks, so
        // only a genuine outside click reaches the backdrop and closes it.
        root = root.child(select_dismiss_backdrop(Arc::new(
            move |_: &(), window, app| dismiss(window, app),
        )));
    }
    root.into_any_element()
}

/// Padding shared by the header, the scrolling body, and the keyboard footer so
/// their content stays on one vertical alignment.
const SECTION_PAD: f32 = 16.0;

/// A titled group inside the instrument body — the raised control-group plane
/// from `DESIGN.md`'s surface order, one step above the window surface.
fn section(title: &'static str) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        // The body is the scroll owner; a section must keep its own height so a
        // short window scrolls rather than crushing the knob row.
        .flex_shrink_0()
        .gap(px(6.0))
        .p(px(10.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_card())
        .child(
            div()
                .text_size(px(9.5))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_faint())
                .child(title),
        )
}

/// Small trailing note under a control group. Explains a real behavior; never
/// used for decoration.
fn section_hint(text: String, accented: bool) -> AnyElement {
    div()
        .text_size(px(9.5))
        .text_color(if accented {
            Colors::accent_primary()
        } else {
            Colors::text_faint()
        })
        .child(text)
        .into_any_element()
}

/// The instrument nameplate: bank name, source file, and a badge for the state
/// a player checks at a glance — how many voices, and whether the panel is
/// sounding right now.
fn header_row(title: &str, subtitle: &str, panel: &SoundfontPlayerPanelState) -> AnyElement {
    let playing = panel.testing || !panel.active_notes.is_empty();
    div()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .gap(px(10.0))
        .px(px(SECTION_PAD))
        .py(px(12.0))
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .bg(Colors::surface_base())
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .flex_shrink_0()
                .w(px(30.0))
                .h(px(30.0))
                .rounded(px(crate::theme::radius::CONTROL))
                .border(px(1.0))
                .border_color(if playing {
                    Colors::border_accent()
                } else {
                    Colors::border_subtle()
                })
                .bg(Colors::surface_card())
                .child(
                    svg()
                        .path(assets::ICON_MUSIC_PATH)
                        .w(px(15.0))
                        .h(px(15.0))
                        .text_color(if playing {
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
                .gap(px(2.0))
                .child(
                    div()
                        .truncate()
                        .text_size(px(13.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Colors::text_primary())
                        .child(title.to_string()),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(10.0))
                        .text_color(Colors::text_muted())
                        .child(subtitle.to_string()),
                ),
        )
        .child(header_badge(panel))
        .into_any_element()
}

fn header_badge(panel: &SoundfontPlayerPanelState) -> AnyElement {
    let (label, accented) = if panel.loading {
        ("Loading".to_string(), false)
    } else if !panel.is_playable() {
        ("No instrument".to_string(), false)
    } else if panel.testing || !panel.active_notes.is_empty() {
        (format!("{} voices", panel.active_notes.len()), true)
    } else {
        (format!("{} voice limit", panel.polyphony), false)
    };
    div()
        .flex_shrink_0()
        .px(px(7.0))
        .py(px(3.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .border(px(1.0))
        .border_color(if accented {
            Colors::border_accent()
        } else {
            Colors::border_subtle()
        })
        .bg(if accented {
            Colors::accent_muted()
        } else {
            Colors::surface_card()
        })
        .text_size(px(9.5))
        .text_color(if accented {
            Colors::accent_primary()
        } else {
            Colors::text_muted()
        })
        .child(label)
        .into_any_element()
}

fn field_row(
    label: &'static str,
    value: impl IntoElement,
    action: Option<AnyElement>,
) -> AnyElement {
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .min_h(px(26.0))
        .gap(px(8.0))
        .child(
            div()
                .w(px(64.0))
                .flex_shrink_0()
                .text_size(px(10.5))
                .text_color(Colors::text_muted())
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .h(px(24.0))
                .flex()
                .items_center()
                .truncate()
                .px(px(8.0))
                .rounded(px(crate::theme::radius::CONTROL))
                .border(px(1.0))
                .border_color(Colors::border_subtle())
                .bg(Colors::surface_input())
                .text_size(px(10.5))
                .text_color(Colors::text_secondary())
                .child(value),
        );
    if let Some(action) = action {
        row = row.child(action);
    }
    row.into_any_element()
}

fn soundfont_row(panel: &SoundfontPlayerPanelState, cb: &SoundfontPlayerCallbacks) -> AnyElement {
    let browse = cb.on_browse.clone();
    let value = panel
        .file_name
        .clone()
        .unwrap_or_else(|| "No .sf2 loaded".to_string());
    field_row(
        "SoundFont",
        value,
        Some(
            fb_button(
                "soundfont-browse",
                "Browse…",
                FbButtonKind::Default,
                !panel.loading,
                move |_, w, cx| browse(w, cx),
            )
            .into_any_element(),
        ),
    )
}

/// Stable option id for one preset. `select` keys on strings, so bank/patch —
/// the identity the player actually selects on — round-trips through this pair.
fn preset_option_id(bank: i32, patch: i32) -> String {
    format!("{bank}:{patch}")
}

fn parse_preset_option_id(id: &str) -> Option<(i32, i32)> {
    let (bank, patch) = id.split_once(':')?;
    Some((bank.parse().ok()?, patch.parse().ok()?))
}

/// Preset/bank chooser plus the audition buttons.
///
/// The chooser is the shared overlay [`select`] rather than a list that expands
/// inside the panel: a General MIDI bank is 128+ presets, and pushing the rest
/// of the instrument down the page to browse them made the control unusable at
/// the window's minimum height. The overlay menu floats above the panel and is
/// dismissed by [`select_dismiss_backdrop`] at the panel root.
fn preset_row(panel: &SoundfontPlayerPanelState, cb: &SoundfontPlayerCallbacks) -> AnyElement {
    let has_presets = !panel.presets.is_empty();
    let selected_id = panel
        .selected_preset
        .map(|(bank, patch)| preset_option_id(bank, patch));
    let options = panel
        .presets
        .iter()
        .map(|preset| {
            SelectOption::new(
                preset_option_id(preset.bank, preset.patch),
                preset.name.clone(),
            )
            .description(format!("Bank {} · Patch {}", preset.bank, preset.patch))
        })
        .collect::<Vec<_>>();

    let toggle = cb.on_toggle_preset_list.clone();
    let on_select = cb.on_select_preset.clone();
    let test = cb.on_test.clone();
    let panic = cb.on_all_notes_off.clone();
    let playing = panel.testing || !panel.active_notes.is_empty();

    div()
        .flex()
        .flex_row()
        .items_center()
        .min_h(px(26.0))
        .gap(px(8.0))
        .child(
            div()
                .w(px(64.0))
                .flex_shrink_0()
                .text_size(px(10.5))
                .text_color(Colors::text_muted())
                .child("Preset"),
        )
        .child(div().flex_1().min_w(px(0.0)).child(select_with_placement(
            "soundfont-preset-select",
            selected_id.as_deref(),
            if has_presets {
                "Select a preset"
            } else {
                "No SoundFont loaded"
            },
            options,
            panel.preset_list_open,
            !has_presets,
            SelectMenuPlacement::Below,
            Arc::new(move |_: &(), window, app| toggle(window, app)),
            Arc::new(move |id: &String, window, app| {
                if let Some(key) = parse_preset_option_id(id) {
                    on_select(&key, window, app);
                }
            }),
        )))
        .child(if playing {
            fb_button(
                "soundfont-test-stop",
                "Stop",
                FbButtonKind::Default,
                true,
                move |_, w, cx| panic(w, cx),
            )
            .into_any_element()
        } else {
            fb_button(
                "soundfont-test",
                "Test",
                FbButtonKind::Primary,
                panel.is_playable(),
                move |_, w, cx| test(w, cx),
            )
            .into_any_element()
        })
        .into_any_element()
}

fn volume_row(panel: &SoundfontPlayerPanelState, cb: &SoundfontPlayerCallbacks) -> AnyElement {
    let on_change = cb.on_set_volume.clone();
    div()
        .flex()
        .flex_row()
        .items_center()
        .min_h(px(26.0))
        .gap(px(8.0))
        .child(
            div()
                .w(px(64.0))
                .flex_shrink_0()
                .text_size(px(10.5))
                .text_color(Colors::text_muted())
                .child("Volume"),
        )
        .child(slider(
            "soundfont-volume",
            panel.master_volume,
            Colors::accent_primary(),
            move |value, w, cx| on_change(value, w, cx),
        ))
        .child(
            div()
                .w(px(36.0))
                .flex_shrink_0()
                .text_size(px(10.0))
                .text_color(Colors::text_faint())
                .child(format!("{:.0}%", panel.master_volume * 100.0)),
        )
        .into_any_element()
}

fn engine_row(panel: &SoundfontPlayerPanelState, cb: &SoundfontPlayerCallbacks) -> AnyElement {
    let toggle_reverb = cb.on_toggle_reverb_chorus.clone();
    let set_polyphony_dec = cb.on_set_polyphony.clone();
    let set_polyphony_inc = cb.on_set_polyphony.clone();
    let polyphony = panel.polyphony;
    let dec_value = polyphony.saturating_sub(POLYPHONY_STEP).max(POLYPHONY_MIN);
    let inc_value = (polyphony + POLYPHONY_STEP).min(POLYPHONY_MAX);

    div()
        .flex()
        .flex_row()
        .items_center()
        .min_h(px(28.0))
        .gap(px(14.0))
        .child(fb_checkbox(
            "soundfont-reverb-chorus",
            "Reverb & Chorus",
            panel.reverb_chorus,
            !panel.loading,
            move |_, w, cx| toggle_reverb(w, cx),
        ))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .text_size(px(10.5))
                        .text_color(Colors::text_muted())
                        .child("Polyphony"),
                )
                .child(fb_stepper_button(
                    "soundfont-polyphony-dec",
                    "–",
                    move |_, w, cx| set_polyphony_dec(&dec_value, w, cx),
                ))
                .child(
                    div()
                        .w(px(32.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(11.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(Colors::text_primary())
                        .child(polyphony.to_string()),
                )
                .child(fb_stepper_button(
                    "soundfont-polyphony-inc",
                    "+",
                    move |_, w, cx| set_polyphony_inc(&inc_value, w, cx),
                )),
        )
        .into_any_element()
}

/// Says where a drum-bank preset's notes actually go.
///
/// Bank select on MIDI channel 10 is offset into a SoundFont's drum banks, so a
/// kit exists on that channel and nowhere else. The player therefore routes this
/// track's notes there whatever channel the piano roll wrote — real behavior the
/// panel has to state rather than leave the user to discover.
fn preset_routing_hint(panel: &SoundfontPlayerPanelState) -> Option<AnyElement> {
    let (bank, _) = panel.selected_preset?;
    (bank >= DRUM_BANK).then(|| {
        section_hint(
            format!(
                "Drum bank {bank} — this track's notes play on MIDI channel {}.",
                PERCUSSION_CHANNEL + 1
            ),
            true,
        )
    })
}

/// The A/D/S/R knob row. Four knobs on one baseline with their values read out
/// underneath, which is the layout a sampler player's envelope is scanned in.
fn envelope_row(panel: &SoundfontPlayerPanelState, cb: &SoundfontPlayerCallbacks) -> AnyElement {
    let envelope = panel.envelope.sanitized();
    div()
        .flex()
        .flex_row()
        .items_start()
        .gap(px(14.0))
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
/// [`ENVELOPE_MAX_TIME_MS`], but a 4-second sweep keeps the useful part of the
/// range under the pointer instead of compressing it into the first few degrees.
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
    on_change: SoundfontEnvelopeCb,
    apply: impl Fn(&mut SoundfontEnvelope, f32) + 'static,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .items_center()
        .w(px(52.0))
        .gap(px(3.0))
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
                .text_size(px(9.5))
                .text_color(Colors::text_muted())
                .child(label),
        )
        .child(
            div()
                .text_size(px(9.5))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(Colors::text_secondary())
                .child(readout),
        )
        .into_any_element()
}

fn envelope_reset(panel: &SoundfontPlayerPanelState, cb: &SoundfontPlayerCallbacks) -> AnyElement {
    let on_change = cb.on_set_envelope.clone();
    let bypassed = panel.envelope.is_bypassed();
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
    if panel.envelope.is_bypassed() {
        return section_hint(
            "Bypassed — the SoundFont's own envelopes play unchanged.".to_string(),
            false,
        );
    }
    let mut hint = String::from(
        "Shapes the instrument's output: attack and decay run from silence, release when the last note ends.",
    );
    if panel.envelope.sanitized().release_ms <= 0.0 {
        hint.push_str(" Release Off keeps the SoundFont tail.");
    }
    section_hint(hint, true)
}

fn quality_row(panel: &SoundfontPlayerPanelState, cb: &SoundfontPlayerCallbacks) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .min_h(px(28.0))
        .gap(px(8.0))
        .child(
            div()
                .w(px(64.0))
                .flex_shrink_0()
                .text_size(px(10.5))
                .text_color(Colors::text_muted())
                .child("Quality"),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .flex_1()
                .min_w(px(0.0))
                .gap(px(4.0))
                .children(SoundfontRenderQuality::ALL.into_iter().map(|quality| {
                    let on_change = cb.on_set_quality.clone();
                    fb_segmented_button(
                        ("soundfont-quality", quality.oversample()),
                        quality.label(),
                        panel.quality == quality,
                        move |_, w, cx| on_change(&quality, w, cx),
                    )
                })),
        )
        .into_any_element()
}

fn quality_hint(panel: &SoundfontPlayerPanelState) -> AnyElement {
    let factor = panel.quality.oversample();
    let hint = if factor == 1 {
        "Renders at the project rate. Raise this if transposed samples sound harsh.".to_string()
    } else {
        format!(
            "Renders at {factor}x internally and filters back down — less sampler aliasing, about {factor}x the CPU."
        )
    };
    section_hint(hint, factor > 1)
}

fn status_banner(message: String) -> AnyElement {
    div()
        .flex_shrink_0()
        .px(px(8.0))
        .py(px(5.0))
        .rounded(px(crate::theme::radius::CONTROL))
        .border(px(1.0))
        .border_color(Colors::status_error())
        .bg(Colors::with_alpha(Colors::status_error(), 0.12))
        .text_size(px(10.5))
        .text_color(Colors::status_error())
        .child(message)
        .into_any_element()
}

/// Semitone offsets of the white keys within one octave, and of the black keys
/// with the white key each sits between.
const WHITE_SEMITONES: [u8; 7] = [0, 2, 4, 5, 7, 9, 11];
const BLACK_SEMITONES: [(usize, u8); 5] = [(0, 1), (1, 3), (3, 6), (4, 8), (5, 10)];
const WHITE_KEY_W: f32 = 30.0;
const BLACK_KEY_W: f32 = 18.0;
const KEY_H: f32 = 62.0;
const BLACK_KEY_H: f32 = 38.0;

const NOTE_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

/// `60` → `"C4"`, matching the pitch labels used elsewhere in Studio.
pub fn note_label(pitch: u8) -> String {
    let octave = pitch as i32 / 12 - 1;
    format!("{}{octave}", NOTE_NAMES[pitch as usize % 12])
}

fn keyboard_header(panel: &SoundfontPlayerPanelState, cb: &SoundfontPlayerCallbacks) -> AnyElement {
    let down = cb.on_shift_octave.clone();
    let up = cb.on_shift_octave.clone();
    let highest = panel
        .keyboard_root
        .saturating_add((KEYBOARD_WHITE_KEYS.div_ceil(7) * 12) as u8)
        .min(127);
    let range = format!(
        "{} – {}",
        note_label(panel.keyboard_root),
        note_label(highest)
    );
    let holding = if panel.active_notes.is_empty() {
        range
    } else {
        let names: Vec<String> = panel.active_notes.iter().copied().map(note_label).collect();
        format!("Playing {}", names.join(" "))
    };

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .child(
            div()
                .text_size(px(10.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(Colors::text_faint())
                .child("Keyboard"),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .truncate()
                .text_size(px(10.0))
                .text_color(if panel.active_notes.is_empty() {
                    Colors::text_muted()
                } else {
                    Colors::accent_primary()
                })
                .child(holding),
        )
        .child(fb_stepper_button(
            "soundfont-octave-down",
            "–",
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
/// track: this is the same MIDI preview path the piano roll uses, not a
/// separate preview synth.
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
        .rounded(px(crate::theme::radius::CONTROL))
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
    let mut key = div()
        .id(id)
        .w(px(if black { BLACK_KEY_W } else { WHITE_KEY_W }))
        .h(px(if black { BLACK_KEY_H } else { KEY_H }))
        .flex()
        .items_end()
        .justify_center()
        .pb(px(4.0))
        .border_r(px(1.0))
        .border_color(if active {
            Colors::border_accent()
        } else {
            Colors::border_subtle()
        })
        .bg(if active {
            Colors::accent_muted()
        } else if black {
            Colors::surface_base()
        } else {
            Colors::surface_input()
        })
        .text_size(px(8.0))
        .text_color(if active {
            Colors::accent_primary()
        } else {
            Colors::text_faint()
        })
        .child(if black || pitch % 12 != 0 {
            String::new()
        } else {
            note_label(pitch)
        });

    if black {
        key = key
            .rounded_b(px(crate::theme::radius::CONTROL))
            .border(px(1.0));
    }

    if playable {
        key = key
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(|s| s.bg(Colors::surface_hover()))
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
        .text_size(px(11.0))
        .text_color(Colors::text_muted())
        .child("Empty document")
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_soundfont_player_reuses_existing_document() {
        let mut state = MdiWorkspaceState::default();
        let first = ensure_soundfont_player_document(&mut state);
        let second = ensure_soundfont_player_document(&mut state);
        assert_eq!(first, second);
        assert_eq!(state.document_count(), 1);
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
    fn a_freshly_loaded_panel_reports_full_volume() {
        // The panel is the authority for volume (the engine reads it from the
        // track), so it must not start at RustySynth's own internal default.
        let panel = SoundfontPlayerPanelState::default();
        assert_eq!(panel.master_volume, 1.0);
    }

    #[test]
    fn a_fresh_panel_reports_an_unshaped_instrument() {
        // The defaults must be the pass-through state, so opening the window on
        // an existing track cannot change how it already sounds.
        let panel = SoundfontPlayerPanelState::default();
        assert!(panel.envelope.is_bypassed());
        assert_eq!(panel.quality, SoundfontRenderQuality::Standard);
        assert_eq!(panel.quality.oversample(), 1);
    }

    #[test]
    fn envelope_times_read_out_with_their_unit_and_name_zero_as_off() {
        assert_eq!(format_ms(0.0), "Off");
        assert_eq!(format_ms(250.0), "250 ms");
        assert_eq!(format_ms(1_500.0), "1.50 s");
    }

    #[test]
    fn the_knob_sweep_stays_inside_the_range_the_engine_accepts() {
        // Both sides are constants, so this is settled at compile time rather
        // than waiting for the test to run.
        const _: () =
            assert!(ENVELOPE_KNOB_MAX_MS <= crate::soundfont_player::ENVELOPE_MAX_TIME_MS);
        let clamped = SoundfontEnvelope {
            attack_ms: ENVELOPE_KNOB_MAX_MS,
            decay_ms: ENVELOPE_KNOB_MAX_MS,
            sustain: 1.0,
            release_ms: ENVELOPE_KNOB_MAX_MS,
        }
        .sanitized();
        assert_eq!(clamped.attack_ms, ENVELOPE_KNOB_MAX_MS);
        assert_eq!(clamped.release_ms, ENVELOPE_KNOB_MAX_MS);
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
