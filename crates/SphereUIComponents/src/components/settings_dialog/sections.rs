//! Split out of `settings_dialog.rs` (god-file decomposition). `use super::*`.

use super::*;

pub(crate) fn icon(path: &'static str, size: f32, color: gpui::Rgba) -> impl IntoElement {
    svg().path(path).w(px(size)).h(px(size)).text_color(color)
}

pub(crate) fn hardware_select(
    combo: HardwareCombo,
    trigger_id: &'static str,
    selected: &str,
    open_combo: Option<HardwareCombo>,
    on_toggle: Arc<dyn Fn(HardwareCombo, Option<OverlayAnchor>, &mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    let open = open_combo == Some(combo);
    let toggle = on_toggle.clone();
    div().w_full().child(combo_box_trigger(
        trigger_id,
        selected.to_string(),
        open,
        move |event, window, cx| {
            let layout = settings_form_column(window);
            let bounds = form_combo_trigger_bounds(layout, event, COMBO_TRIGGER_HEIGHT);
            let anchor = if open {
                None
            } else {
                Some(OverlayAnchor { bounds })
            };
            toggle(combo, anchor, window, cx);
        },
    ))
}

pub(crate) fn locale_label(i18n: I18n, locale: Locale) -> String {
    i18n.tr(locale.language_key())
}

pub(crate) fn selected_locale_label(i18n: I18n, language_code: &str) -> String {
    locale_label(i18n, Locale::from_code(language_code))
}

/// The page list. While searching, `hits` names the pages with matches; the
/// rest dim but stay clickable, and picking one leaves the search.
pub(crate) fn build_settings_sidebar_items(
    state: &SettingsDialogState,
    callbacks: &SettingsDialogCallbacks,
    i18n: I18n,
    hits: Option<&[SettingsTab]>,
) -> Vec<gpui::AnyElement> {
    let mut items = Vec::new();
    let mut nav_index = 0usize;
    for (group_index, tabs) in SettingsTab::nav_groups().iter().enumerate() {
        if group_index > 0 {
            items.push(
                div()
                    .h(px(1.0))
                    .mx(px(crate::theme::space::LOOSE))
                    .my(px(crate::theme::space::SNUG))
                    .bg(Colors::divider())
                    .into_any_element(),
            );
        }
        for tab in tabs.iter().copied() {
            let active = hits.is_none() && state.active_tab == tab;
            let dimmed = hits.is_some_and(|hits| !hits.contains(&tab));
            let select = callbacks.on_select_tab.clone();
            items.push(pref_nav_item(
                ("settings-tab", nav_index),
                i18n.tr(tab.label_key()),
                tab.icon(),
                active,
                dimmed,
                move |window, cx| select(&tab, window, cx),
            ));
            nav_index += 1;
        }
    }
    items
}

/// One page in the sidebar. Selected is said twice: the selected fill and an
/// accent marker on the leading edge.
pub(crate) fn pref_nav_item(
    id: impl Into<gpui::ElementId>,
    label: String,
    icon_path: &'static str,
    active: bool,
    dimmed: bool,
    on_select: impl Fn(&mut Window, &mut App) + 'static,
) -> gpui::AnyElement {
    use crate::theme::{radius, size, space, typography};
    let base = Colors::surface_panel_alt();
    let rest = if active {
        Colors::composite(base, Colors::state_selected())
    } else {
        Colors::with_alpha(base, 0.0)
    };
    let hover = Colors::composite(if active { rest } else { base }, Colors::state_hover());
    div()
        .id(id)
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(space::BASE))
        .h(px(size::COMFORTABLE))
        .px(px(space::BASE))
        .mx(px(space::BASE))
        .rounded(px(radius::CONTROL))
        .bg(rest)
        .text_size(px(typography::UI_SM))
        .font_weight(if active {
            gpui::FontWeight::SEMIBOLD
        } else {
            gpui::FontWeight::NORMAL
        })
        .text_color(if active {
            Colors::text_primary()
        } else if dimmed {
            Colors::text_disabled()
        } else {
            Colors::text_secondary()
        })
        .cursor(gpui::CursorStyle::PointingHand)
        .hover(move |s| s.bg(hover))
        .on_click(move |_, window, cx| on_select(window, cx))
        .when(active, |item| {
            item.child(
                div()
                    .absolute()
                    .left_0()
                    .top(px(space::SNUG))
                    .bottom(px(space::SNUG))
                    .w(px(3.0))
                    .rounded(px(radius::PILL))
                    .bg(Colors::accent_primary()),
            )
        })
        .child(
            svg()
                .path(icon_path)
                .size(px(14.0))
                .flex_shrink_0()
                .text_color(if active {
                    Colors::accent_primary()
                } else if dimmed {
                    Colors::text_disabled()
                } else {
                    Colors::text_muted()
                }),
        )
        .child(div().truncate().child(label))
        .into_any_element()
}

pub type AudioLatencySnapshotProvider = Arc<dyn Fn() -> SettingsAudioLatencySnapshot + Send + Sync>;

pub(crate) fn latency_ms_label(i18n: &I18n, ms: f64) -> String {
    if ms > 0.0 {
        i18n.tr_vars("settings.latency.ms-value", &[("ms", format!("{ms:.2}"))])
    } else {
        i18n.tr("settings.latency.unavailable")
    }
}

pub(crate) fn midi_direction_label(i18n: &I18n, direction: MidiDeviceDirection) -> String {
    match direction {
        MidiDeviceDirection::Input => i18n.tr("settings.midi.type.input"),
        MidiDeviceDirection::Output => i18n.tr("settings.midi.type.output"),
        MidiDeviceDirection::InputOutput => i18n.tr("settings.midi.type.input-output"),
    }
}

/// A device's state, in words. The table carries it in monochrome: the word,
/// plus a dot that is filled, hollow or crossed, never a hue alone.
pub(crate) fn midi_device_status_label(i18n: &I18n, device: &MidiDeviceSetting) -> String {
    if !device.connected {
        i18n.tr("settings.midi.status.missing")
    } else if !device.enabled {
        i18n.tr("settings.midi.status.disabled")
    } else {
        i18n.tr("settings.midi.status.connected")
    }
}

pub(crate) fn midi_device_icon(direction: MidiDeviceDirection) -> &'static str {
    match direction {
        MidiDeviceDirection::Input => assets::ICON_MIC_PATH,
        MidiDeviceDirection::Output => assets::ICON_VOLUME_2_PATH,
        MidiDeviceDirection::InputOutput => assets::ICON_ROUTE_PATH,
    }
}

/// Column widths of the device table. The name takes what is left.
const MIDI_TYPE_COL_W: f32 = 104.0;
const MIDI_STATUS_COL_W: f32 = 104.0;
const MIDI_ENABLED_COL_W: f32 = 64.0;
const MIDI_ROW_H: f32 = 32.0;
const MIDI_HEADER_H: f32 = 26.0;
const MIDI_STATUS_DOT: f32 = 7.0;

/// The status column's dot: filled while the device is in use, hollow while
/// it is switched off, and a hollow one under a strike while it is unplugged.
fn midi_status_dot(device: &MidiDeviceSetting) -> impl IntoElement {
    let ink = Colors::text_secondary();
    let dot = div()
        .relative()
        .flex_none()
        .size(px(MIDI_STATUS_DOT))
        .rounded(px(theme::radius::PILL));
    if !device.connected {
        dot.border(px(1.0))
            .border_color(Colors::text_faint())
            .child(
                div()
                    .absolute()
                    .left(px(-1.0))
                    .top(px(MIDI_STATUS_DOT / 2.0 - 0.5))
                    .w(px(MIDI_STATUS_DOT + 2.0))
                    .h(px(1.0))
                    .bg(Colors::text_faint()),
            )
    } else if !device.enabled {
        dot.border(px(1.0)).border_color(Colors::text_muted())
    } else {
        dot.bg(ink)
    }
}

/// One cell of the header row.
fn midi_header_cell(label: String, width: Option<f32>) -> impl IntoElement {
    let cell = div()
        .flex()
        .items_center()
        .text_size(px(theme::typography::DENSE_CAPTION))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Colors::text_faint())
        .child(label.to_uppercase());
    match width {
        Some(width) => cell.flex_none().w(px(width)),
        None => cell.flex_1().min_w_0(),
    }
}

/// One device as a table row: name with its direction glyph, type, status,
/// and the switch. All text in the neutral ramp; a missing device dims.
pub(crate) fn midi_device_table_row(
    row_index: usize,
    device: &MidiDeviceSetting,
    i18n: &I18n,
    on_update: &Arc<dyn Fn(UpdateSettingFn, &mut Window, &mut App) + 'static>,
) -> impl IntoElement {
    let snapshot = device.clone();
    let enabled = device.enabled;
    let up = on_update.clone();
    let status = midi_device_status_label(i18n, device);
    let type_label = midi_direction_label(i18n, device.direction);
    let missing = !device.connected;
    let (name_ink, cell_ink) = if missing {
        (Colors::text_muted(), Colors::text_faint())
    } else {
        (Colors::text_primary(), Colors::text_secondary())
    };
    let hover = Colors::composite(Colors::surface_input(), Colors::state_hover());

    div()
        .id(("midi-device-row", row_index))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::LOOSE))
        .h(px(MIDI_ROW_H))
        .px(px(theme::space::LOOSE))
        .when(row_index > 0, |row| {
            row.border_t(px(1.0)).border_color(Colors::divider())
        })
        .hover(move |style| style.bg(hover))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(theme::space::BASE))
                .flex_1()
                .min_w_0()
                .child(icon(
                    midi_device_icon(device.direction),
                    13.0,
                    Colors::text_muted(),
                ))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(theme::typography::UI_SM))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(name_ink)
                        .child(device.name.clone()),
                ),
        )
        .child(
            div()
                .flex_none()
                .w(px(MIDI_TYPE_COL_W))
                .truncate()
                .text_size(px(theme::typography::UI_XS))
                .text_color(cell_ink)
                .child(type_label),
        )
        .child(
            div()
                .flex_none()
                .w(px(MIDI_STATUS_COL_W))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(theme::space::SNUG))
                .child(midi_status_dot(device))
                .child(
                    div()
                        .truncate()
                        .text_size(px(theme::typography::UI_XS))
                        .text_color(cell_ink)
                        .child(status),
                ),
        )
        .child(
            div()
                .flex_none()
                .w(px(MIDI_ENABLED_COL_W))
                .flex()
                .justify_end()
                .child(box_list_switch(
                    ("midi-device-toggle", row_index),
                    enabled,
                    move |_, w, cx| {
                        let next = !enabled;
                        if midi_settings_debug_enabled() {
                            eprintln!("[MIDI settings] toggle {} enabled={next}", snapshot.name);
                        }
                        let saved_for_update = snapshot.clone();
                        up(
                            Arc::new(move |s| {
                                let mut updated = saved_for_update.clone();
                                updated.enabled = next;
                                upsert_midi_device(&mut s.hardware.midi, updated);
                            }),
                            w,
                            cx,
                        );
                    },
                )),
        )
}

/// The MIDI devices as one table: a header row, then the inputs (a two-way
/// port once, with them) and the outputs, with a rescan under it.
pub(crate) fn midi_devices_body(
    schema: &SettingsSchema,
    i18n: &I18n,
    on_update: Arc<dyn Fn(UpdateSettingFn, &mut Window, &mut App) + 'static>,
    on_refresh_midi: Option<Arc<dyn Fn(&mut Window, &mut App) + 'static>>,
) -> impl IntoElement {
    let detected = cached_midi_devices();
    let resolved = resolve_midi_devices(&schema.hardware.midi.devices, &detected);

    // Inputs first, then outputs. A two-way port is listed once, with the
    // inputs; its type says it is both. Listing it twice gave two switches
    // for the one device.
    let ordered: Vec<_> = resolved
        .iter()
        .filter(|d| d.direction != MidiDeviceDirection::Output)
        .chain(
            resolved
                .iter()
                .filter(|d| d.direction == MidiDeviceDirection::Output),
        )
        .cloned()
        .collect();

    let mut body = div().flex().flex_col().gap(px(theme::space::BASE));

    if ordered.is_empty() {
        let refresh = on_refresh_midi.clone();
        return body.child(box_list_empty_state(
            i18n.tr("settings.midi.empty"),
            i18n.tr("settings.midi.refresh"),
            move |_, w, cx| {
                if let Some(cb) = refresh.as_ref() {
                    cb(w, cx);
                }
            },
        ));
    }

    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(theme::space::LOOSE))
        .h(px(MIDI_HEADER_H))
        .px(px(theme::space::LOOSE))
        .bg(Colors::surface_panel_alt())
        .border_b(px(1.0))
        .border_color(Colors::border_subtle())
        .child(midi_header_cell(
            i18n.tr_or("settings.midi.column.device", "Device"),
            None,
        ))
        .child(midi_header_cell(
            i18n.tr_or("settings.midi.column.type", "Type"),
            Some(MIDI_TYPE_COL_W),
        ))
        .child(midi_header_cell(
            i18n.tr_or("settings.midi.column.status", "Status"),
            Some(MIDI_STATUS_COL_W),
        ))
        .child(
            div()
                .flex_none()
                .w(px(MIDI_ENABLED_COL_W))
                .flex()
                .justify_end()
                .child(midi_header_cell(
                    i18n.tr_or("settings.midi.column.enabled", "Enabled"),
                    None,
                )),
        );

    body = body.child(
        div()
            .flex()
            .flex_col()
            .rounded(px(theme::radius::CONTROL))
            .border(px(1.0))
            .border_color(Colors::border_subtle())
            .bg(Colors::surface_input())
            .overflow_hidden()
            .child(header)
            .children(ordered.iter().enumerate().map(|(index, device)| {
                midi_device_table_row(index, device, i18n, &on_update).into_any_element()
            })),
    );

    if let Some(refresh) = on_refresh_midi {
        body = body.child(
            div()
                .flex()
                .flex_row()
                .justify_end()
                .child(box_list_icon_button(
                    "midi-devices-refresh",
                    assets::ICON_REPEAT_PATH,
                    "Refresh MIDI devices",
                    move |_, w, cx| refresh(w, cx),
                )),
        );
    }

    body
}

pub(crate) fn input_test_meter_row(
    state: &InputTestMeterState,
    callbacks: &SettingsDialogCallbacks,
) -> impl IntoElement {
    let level = state.level.clamp(0.0, 1.0);
    let level_percent = (level * 100.0).round() as u32;
    let toggle = callbacks.on_toggle_input_test.clone();
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(fb_button(
                    "settings-input-test-toggle",
                    if state.active {
                        "Stop Test"
                    } else {
                        "Test Input"
                    },
                    if state.active {
                        FbButtonKind::Primary
                    } else {
                        FbButtonKind::Default
                    },
                    true,
                    move |_, window, cx| toggle(&(), window, cx),
                ))
                .child(
                    div()
                        .flex_1()
                        .h(px(10.0))
                        .rounded(px(crate::theme::radius::CONTROL))
                        .border(px(1.0))
                        .border_color(Colors::border_subtle())
                        .bg(Colors::meter_rail())
                        .child(
                            div()
                                .h_full()
                                .w(gpui::relative(level))
                                .rounded(px(crate::theme::radius::CONTROL))
                                .bg(if level >= 0.9 {
                                    Colors::meter_high()
                                } else if level >= 0.65 {
                                    Colors::meter_mid()
                                } else {
                                    Colors::meter_low()
                                }),
                        ),
                )
                .child(
                    div()
                        .w(px(38.0))
                        .text_size(px(10.0))
                        .text_color(Colors::text_muted())
                        .child(format!("{level_percent}%")),
                ),
        )
        .when_some(state.error.clone(), |el, error| {
            el.child(
                div()
                    .text_size(px(10.0))
                    .text_color(Colors::status_error())
                    .child(error),
            )
        })
}
