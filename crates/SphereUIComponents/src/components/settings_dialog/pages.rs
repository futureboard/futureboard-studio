//! What each Preferences page holds, as [`PrefGroup`]s.
//!
//! Every row here drives real behaviour or reports real state; values that
//! were only stored and never read, and read-outs that stated fixed text as if
//! it were a setting, are not offered.

use super::*;

/// Everything a page builder reads.
pub(crate) struct PageCtx<'a> {
    pub i18n: I18n,
    pub schema: &'a SettingsSchema,
    pub state: &'a SettingsDialogState,
    pub callbacks: &'a SettingsDialogCallbacks,
    pub latency: &'a SettingsAudioLatencySnapshot,
    pub input_test: &'a InputTestMeterState,
    pub inputs: &'a [String],
    pub outputs: &'a [String],
    pub backends: &'a [String],
    pub input_channels: &'a [(String, u32)],
    pub output_channels: &'a [(String, u32)],
}

type OnUpdate = Arc<dyn Fn(UpdateSettingFn, &mut Window, &mut App) + 'static>;

/// A click that applies `change` to the settings.
fn apply(
    up: &OnUpdate,
    change: impl Fn(&mut SettingsSchema) + Send + Sync + 'static,
) -> impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static {
    let up = up.clone();
    let change: UpdateSettingFn = Arc::new(change);
    move |_, w, cx| up(change.clone(), w, cx)
}

/// A segmented choice whose pick applies `change(value)`.
fn choice<T: Copy + PartialEq + Send + Sync + 'static>(
    id: &'static str,
    options: Vec<(T, String)>,
    selected: T,
    up: &OnUpdate,
    change: impl Fn(&mut SettingsSchema, T) + Send + Sync + 'static,
) -> gpui::AnyElement {
    let up = up.clone();
    let change = Arc::new(change);
    let last = options.len().saturating_sub(1);
    // The one control width, as the dropdowns have: segments share it, and
    // every control on a page ends on the same edge.
    let mut track = fb_segmented_track().w(px(PREFS_CONTROL_W));
    for (index, (value, label)) in options.into_iter().enumerate() {
        let position = match (index, last) {
            (_, 0) => FbSegment::Only,
            (0, _) => FbSegment::First,
            (i, l) if i == l => FbSegment::Last,
            _ => FbSegment::Middle,
        };
        let up = up.clone();
        let change = change.clone();
        track = track.child(fb_segment(
            (id, index),
            label,
            value == selected,
            position,
            move |_, w, cx| {
                let change = change.clone();
                up(Arc::new(move |s| change(s, value)), w, cx)
            },
        ));
    }
    track.into_any_element()
}

impl<'a> PageCtx<'a> {
    fn up(&self) -> OnUpdate {
        self.callbacks.on_update_setting.clone()
    }

    fn select(&self, combo: HardwareCombo, id: &'static str, label: &str) -> gpui::AnyElement {
        pref_select(
            combo,
            id,
            label,
            self.callbacks.open_hardware_combo,
            self.callbacks.on_toggle_hardware_combo.clone(),
        )
    }

    fn tr(&self, key: &str) -> String {
        self.i18n.tr(key)
    }
}

pub(crate) fn page_groups(tab: SettingsTab, cx: &PageCtx) -> Vec<PrefGroup> {
    match tab {
        SettingsTab::General => general(cx),
        SettingsTab::Audio => audio(cx),
        SettingsTab::Midi => midi(cx),
        SettingsTab::Recording => recording(cx),
        SettingsTab::Playback => playback(cx),
        SettingsTab::Editing => editing(cx),
        SettingsTab::Appearance => appearance(cx),
        SettingsTab::Performance => performance(cx),
        SettingsTab::About => about(cx),
    }
}

// ── General ─────────────────────────────────────────────────────────────────

fn general(cx: &PageCtx) -> Vec<PrefGroup> {
    let s = cx.schema;
    let up = cx.up();
    let mut groups = vec![
        PrefGroup::new(cx.tr("settings.section.application"))
            .row(PrefRow::field(
                cx.tr("settings.field.language"),
                &["language", "locale"],
                cx.select(
                    HardwareCombo::Language,
                    "settings-general-language",
                    &selected_locale_label(cx.i18n, &s.general.language),
                ),
            ))
            .row(PrefRow::field(
                cx.tr("settings.show-start-screen"),
                &["start", "screen", "wizard"],
                pref_switch(
                    "show-start-screen",
                    s.general.show_start_screen,
                    apply(&up, |s| {
                        s.general.show_start_screen = !s.general.show_start_screen
                    }),
                ),
            ))
            .row(PrefRow::field(
                cx.tr("settings.check-updates"),
                &["update", "updates"],
                pref_switch(
                    "check-updates",
                    s.general.check_updates,
                    apply(&up, |s| s.general.check_updates = !s.general.check_updates),
                ),
            ))
            .row(PrefRow::field(
                cx.tr("settings.field.update-channel"),
                &["update", "channel", "beta"],
                cx.select(
                    HardwareCombo::UpdateChannel,
                    "settings-general-update-channel",
                    s.general.update_channel.label(),
                ),
            )),
        PrefGroup::new(cx.tr("settings.section.autosave-backup"))
            .row(PrefRow::field(
                cx.tr("settings.autosave.enabled"),
                &["autosave", "save"],
                pref_switch(
                    "autosave-enabled",
                    s.general.autosave.enabled,
                    apply(&up, |s| {
                        s.general.autosave.enabled = !s.general.autosave.enabled
                    }),
                ),
            ))
            .row(PrefRow::field(
                cx.tr("settings.field.interval"),
                &["autosave", "interval", "minutes"],
                cx.select(
                    HardwareCombo::AutosaveInterval,
                    "settings-general-autosave-interval",
                    &cx.i18n.tr_vars(
                        "settings.interval.minutes",
                        &[("n", s.general.autosave.interval_minutes.to_string())],
                    ),
                ),
            )),
    ];

    let open_shortcuts = cx.callbacks.on_open_keyboard_shortcuts.clone();
    let open_plugins = cx.callbacks.on_open_plugin_manager.clone();
    groups.push(
        PrefGroup::new("Tools & Integrations")
            .row(PrefRow::described(
                "Keyboard Shortcuts",
                "Search, rebind and reset commands.",
                &["shortcuts", "keymap", "hotkeys", "keybindings", "key"],
                pref_button(
                    "settings-open-keyboard-shortcuts",
                    "Edit Shortcuts…",
                    open_shortcuts.is_some(),
                    move |_, window, cx| {
                        if let Some(open) = open_shortcuts.as_ref() {
                            open(window, cx);
                        }
                    },
                ),
            ))
            .row(PrefRow::described(
                "Plug-ins",
                "Scan, rescan and review VST3 and CLAP plug-ins.",
                &["plugin", "plugins", "vst3", "clap", "scan", "rescan"],
                pref_button(
                    "settings-open-plugin-manager",
                    "Plug-in Manager…",
                    open_plugins.is_some(),
                    move |_, window, cx| {
                        if let Some(open) = open_plugins.as_ref() {
                            open(window, cx);
                        }
                    },
                ),
            ))
            .row(PrefRow::described(
                "Discord Rich Presence",
                "Show Futureboard Studio in your Discord status.",
                &["discord", "rpc", "presence"],
                pref_switch(
                    "settings-discord-rpc-toggle",
                    s.general.discord_rpc_enabled,
                    apply(&up, |s| {
                        s.general.discord_rpc_enabled = !s.general.discord_rpc_enabled
                    }),
                ),
            )),
    );
    groups
}

// ── Audio ───────────────────────────────────────────────────────────────────

fn audio(cx: &PageCtx) -> Vec<PrefGroup> {
    let s = cx.schema;
    let latency = cx.latency;
    let driver_label = sanitized_backend_label(&s.hardware.audio.driver_type, cx.backends);
    // The effective (edition-sanitized) driver: a latent Exclusive "ASIO" pin
    // must not keep the ASIO-only device UI alive on Community.
    let is_asio = driver_label == "ASIO";
    let device_label = |name: &str, available: &[String]| {
        if name.trim().is_empty() || !available.iter().any(|d| d == name) {
            "Default".to_string()
        } else {
            name.to_string()
        }
    };
    let input_label = device_label(&s.hardware.audio.device_in, cx.inputs);
    let output_label = device_label(&s.hardware.audio.device_out, cx.outputs);

    let mut device = PrefGroup::new(cx.tr("settings.section.audio-engine")).row(PrefRow::field(
        cx.tr("settings.field.backend"),
        &[
            "driver",
            "backend",
            "wasapi",
            "wdm",
            "ks",
            "asio",
            "coreaudio",
            "alsa",
        ],
        cx.select(
            HardwareCombo::AudioDriver,
            "settings-audio-driver",
            &driver_label,
        ),
    ));
    if !is_asio {
        device = device.row(PrefRow::field(
            cx.tr("settings.field.input-device"),
            &["input", "microphone", "device"],
            cx.select(
                HardwareCombo::InputDevice,
                "settings-audio-input",
                &input_label,
            ),
        ));
    }
    device = device
        .row(PrefRow::field(
            if is_asio {
                "ASIO Device".to_string()
            } else {
                cx.tr("settings.field.output-device")
            },
            &["output", "speakers", "headphones", "device"],
            cx.select(
                HardwareCombo::OutputDevice,
                "settings-audio-output",
                &output_label,
            ),
        ))
        .rows(driver_status_rows(
            &cx.i18n,
            latency,
            cx.state,
            cx.callbacks,
        ));

    // Channels the chosen devices report. With no device chosen ("Default"),
    // the first scanned device stands in, so a real scan is not reported as
    // "no channels".
    let channel_count = |name: &str, list: &[(String, u32)]| {
        list.iter()
            .find(|(n, _)| n == name)
            .or_else(|| name.trim().is_empty().then(|| list.first()).flatten())
            .map(|(_, count)| *count)
            .unwrap_or(0)
    };
    let describe = |options: &[crate::audio_routing::AudioRouteOption]| {
        if options.is_empty() {
            "None reported".to_string()
        } else {
            options
                .iter()
                .map(|o| o.label.clone())
                .collect::<Vec<_>>()
                .join(" · ")
        }
    };
    let in_options = crate::audio_routing::build_input_channel_options(channel_count(
        &s.hardware.audio.device_in,
        cx.input_channels,
    ));
    let out_options = crate::audio_routing::build_output_channel_options(channel_count(
        &s.hardware.audio.device_out,
        cx.output_channels,
    ));
    if !is_asio {
        device = device.row(PrefRow::field(
            "Input Channels",
            &["channels", "input"],
            pref_value(describe(&in_options)),
        ));
    }
    device = device.row(PrefRow::field(
        "Output Channels",
        &["channels", "output"],
        pref_value(describe(&out_options)),
    ));

    let buffer_ms = latency.buffer_ms.max(
        s.general.project_defaults.buffer_size as f64
            / s.general.project_defaults.sample_rate.max(1) as f64
            * 1000.0,
    );
    let format = PrefGroup::new(cx.tr("settings.section.sample-rate-buffer"))
        .row(PrefRow::field(
            cx.tr("settings.field.sample-rate"),
            &["sample", "rate", "hz", "khz"],
            cx.select(
                HardwareCombo::SampleRate,
                "settings-audio-sample-rate",
                &cx.i18n.tr_vars(
                    "settings.sample-rate.hz",
                    &[("rate", s.general.project_defaults.sample_rate.to_string())],
                ),
            ),
        ))
        .row(PrefRow::described(
            cx.tr("settings.field.buffer-size"),
            latency_ms_label(&cx.i18n, if latency.engine_open { buffer_ms } else { 0.0 }),
            &["buffer", "latency", "samples"],
            cx.select(
                HardwareCombo::BufferSize,
                "settings-audio-buffer-size",
                &format!("{} samples", s.general.project_defaults.buffer_size),
            ),
        ))
        .note(cx.tr("settings.buffer.hint"));

    vec![device, format, latency_group(cx)]
}

/// What the engine reports about latency right now.
fn latency_group(cx: &PageCtx) -> PrefGroup {
    let i18n = &cx.i18n;
    let latency = cx.latency;
    let group = PrefGroup::new(cx.tr("settings.section.latency-report"));
    if !latency.engine_open {
        return group.row(PrefRow::field(
            cx.tr("settings.field.device-state"),
            &["latency", "engine"],
            pref_value(cx.tr("settings.latency.engine-closed")),
        ));
    }
    let ms = |value: f64| pref_value(latency_ms_label(i18n, value));
    let mut rows = vec![PrefRow::field(
        cx.tr("settings.field.active-sample-rate"),
        &["sample", "rate", "active"],
        pref_value(i18n.tr_vars(
            "settings.latency.sample-rate-value",
            &[("hz", latency.active_sample_rate.max(1).to_string())],
        )),
    )];
    if latency.restart_pending {
        rows.push(PrefRow::described(
            cx.tr("settings.field.preferred-sample-rate"),
            cx.tr("settings.latency.sample-rate-restart-pending"),
            &["sample", "rate", "restart"],
            pref_value(i18n.tr_vars(
                "settings.latency.sample-rate-value",
                &[("hz", latency.deferred_sample_rate.max(1).to_string())],
            )),
        ));
    } else if latency.requested_sample_rate > 0
        && latency.active_sample_rate > 0
        && latency.requested_sample_rate != latency.active_sample_rate
    {
        rows.push(PrefRow::described(
            cx.tr("settings.field.requested-sample-rate"),
            i18n.tr_vars(
                "settings.latency.sample-rate-mismatch",
                &[
                    ("active", latency.active_sample_rate.to_string()),
                    ("requested", latency.requested_sample_rate.to_string()),
                ],
            ),
            &["sample", "rate", "requested"],
            pref_value(i18n.tr_vars(
                "settings.latency.sample-rate-value",
                &[("hz", latency.requested_sample_rate.to_string())],
            )),
        ));
    }
    // Input/output halves only where the backend reports them (ASIO).
    if latency.round_trip_reported {
        rows.push(PrefRow::field(
            cx.tr("settings.field.input-latency"),
            &["latency", "input"],
            ms(latency.input_ms),
        ));
        rows.push(PrefRow::field(
            cx.tr("settings.field.output-latency"),
            &["latency", "output"],
            ms(latency.output_ms),
        ));
    }
    rows.push(PrefRow::field(
        if latency.round_trip_reported {
            cx.tr("settings.field.round-trip-latency")
        } else {
            cx.tr("settings.field.round-trip-latency-estimated")
        },
        &["latency", "round trip"],
        ms(latency.round_trip_ms),
    ));
    rows.push(PrefRow::field(
        cx.tr("settings.field.plugin-path-latency"),
        &["latency", "plugin", "pdc"],
        ms(latency.max_path_ms),
    ));
    group.rows(rows)
}

// ── MIDI ────────────────────────────────────────────────────────────────────

fn midi(cx: &PageCtx) -> Vec<PrefGroup> {
    vec![
        PrefGroup::new(cx.tr("settings.section.midi-devices")).row(PrefRow::block(
            cx.tr("settings.section.midi-devices"),
            &[
                "midi",
                "inputs",
                "outputs",
                "port",
                "keyboard",
                "controller",
            ],
            midi_devices_body(
                cx.schema,
                &cx.i18n,
                cx.callbacks.on_update_setting.clone(),
                cx.callbacks.on_refresh_midi.clone(),
            ),
        )),
    ]
}

// ── Recording ───────────────────────────────────────────────────────────────

fn recording(cx: &PageCtx) -> Vec<PrefGroup> {
    let s = cx.schema;
    let up = cx.up();
    vec![
        PrefGroup::new("While Recording")
            .row(PrefRow::described(
                "Default Monitoring",
                "What newly armed tracks monitor.",
                &["monitor", "monitoring", "input"],
                choice(
                    "rec-monitor",
                    vec![
                        (DefaultMonitorMode::Off, "Off".to_string()),
                        (DefaultMonitorMode::Auto, "Auto".to_string()),
                        (DefaultMonitorMode::Input, "Input".to_string()),
                    ],
                    s.recording.default_monitor_mode,
                    &up,
                    |s, v| s.recording.default_monitor_mode = v,
                ),
            ))
            .row(PrefRow::field(
                "Save the project before recording starts",
                &["save", "record"],
                pref_switch(
                    "rec-save-before-recording",
                    s.recording.audio.save_before_recording,
                    apply(&up, |s| {
                        s.recording.audio.save_before_recording =
                            !s.recording.audio.save_before_recording
                    }),
                ),
            ))
            .row(PrefRow::field(
                "Build waveforms when recording stops",
                &["waveform", "record"],
                pref_switch(
                    "rec-generate-waveform",
                    s.recording.audio.generate_waveform_after_record,
                    apply(&up, |s| {
                        s.recording.audio.generate_waveform_after_record =
                            !s.recording.audio.generate_waveform_after_record
                    }),
                ),
            ))
            .row(PrefRow::described(
                "Recording Offset",
                "Shifts where recorded clips start, to cancel measured latency.",
                &["offset", "latency", "compensation", "record"],
                pref_stepper(
                    "rec-offset",
                    format!("{} ms", s.recording.audio.recording_offset_ms),
                    apply(&up, |s| {
                        s.recording.audio.recording_offset_ms =
                            (s.recording.audio.recording_offset_ms - 1).clamp(-2000, 2000)
                    }),
                    apply(&up, |s| {
                        s.recording.audio.recording_offset_ms =
                            (s.recording.audio.recording_offset_ms + 1).clamp(-2000, 2000)
                    }),
                ),
            )),
        PrefGroup::new("Input Test").row(PrefRow::block(
            "Input Test",
            &["input", "test", "meter", "level"],
            input_test_meter_row(cx.input_test, cx.callbacks),
        )),
    ]
}

// ── Playback (transport, metronome, engine) ─────────────────────────────────

fn playback(cx: &PageCtx) -> Vec<PrefGroup> {
    use crate::settings::{DropoutProtectionMode as Dp, SpacebarAction};
    let s = cx.schema;
    let up = cx.up();
    let met = &s.recording.metronome;
    let sound = if met.sound_type == "Beep" {
        "Beep"
    } else {
        "Woodblock"
    };
    vec![
        PrefGroup::new(cx.tr("settings.section.playback-transport"))
            .row(PrefRow::field(
                cx.tr("settings.field.spacebar-action"),
                &["spacebar", "space", "play", "stop", "pause"],
                choice(
                    "space-action",
                    vec![
                        (
                            SpacebarAction::PlayPause,
                            cx.tr("settings.spacebar.play-pause"),
                        ),
                        (
                            SpacebarAction::PlayStop,
                            cx.tr("settings.spacebar.play-stop-soon"),
                        ),
                    ],
                    s.playback.spacebar_action,
                    &up,
                    |s, v| s.playback.spacebar_action = v,
                ),
            ))
            .row(PrefRow::field(
                cx.tr("settings.return-on-stop"),
                &["return", "stop", "playhead", "start"],
                pref_switch(
                    "return-on-stop",
                    s.playback.return_playhead_on_stop,
                    apply(&up, |s| {
                        s.playback.return_playhead_on_stop = !s.playback.return_playhead_on_stop
                    }),
                ),
            )),
        PrefGroup::new(cx.tr("settings.section.metronome"))
            .row(PrefRow::field(
                cx.tr("settings.metronome.enable"),
                &["metronome", "click"],
                pref_switch(
                    "rec-metronome-enabled",
                    met.enabled,
                    apply(&up, |s| {
                        s.recording.metronome.enabled = !s.recording.metronome.enabled
                    }),
                ),
            ))
            .row(PrefRow::field(
                cx.tr("settings.field.click-volume"),
                &["metronome", "click", "volume", "level"],
                pref_slider(
                    "metronome-volume-slider",
                    met.volume,
                    format!("{:.0}%", met.volume * 100.0),
                    {
                        let up = up.clone();
                        move |value, w, cx| {
                            let volume = *value;
                            up(
                                Arc::new(move |s| s.recording.metronome.volume = volume),
                                w,
                                cx,
                            );
                        }
                    },
                ),
            ))
            .row(PrefRow::field(
                cx.tr("settings.field.click-sound"),
                &["metronome", "click", "sound", "beep", "woodblock"],
                choice(
                    "met-sound",
                    vec![
                        ("Woodblock", cx.tr("settings.metronome.woodblock")),
                        ("Beep", cx.tr("settings.metronome.beep")),
                    ],
                    sound,
                    &up,
                    |s, v| s.recording.metronome.sound_type = v.to_string(),
                ),
            ))
            .row(PrefRow::field(
                cx.tr("settings.metronome.count-in"),
                &["count-in", "countin", "pre-roll", "record"],
                pref_switch(
                    "met-count-in-enabled",
                    met.count_in_enabled,
                    apply(&up, |s| {
                        s.recording.metronome.count_in_enabled =
                            !s.recording.metronome.count_in_enabled
                    }),
                ),
            ))
            .row(PrefRow::field(
                cx.tr("settings.field.count-in-bars"),
                &["count-in", "bars"],
                choice(
                    "met-count-in-bars",
                    (1u32..=4).map(|bars| (bars, bars.to_string())).collect(),
                    met.count_in_bars.clamp(1, 4),
                    &up,
                    |s, v| s.recording.metronome.count_in_bars = v,
                ),
            )),
        PrefGroup::new(cx.tr("settings.section.playback-latency"))
            .row(PrefRow::described(
                cx.tr("settings.field.latency-compensation"),
                cx.tr("settings.latency.pdc-toggle-hint"),
                &["latency", "pdc", "delay", "compensation", "plugin"],
                pref_switch(
                    "playback-pdc-enabled",
                    s.playback.latency_compensation,
                    apply(&up, |s| {
                        s.playback.latency_compensation = !s.playback.latency_compensation
                    }),
                ),
            ))
            .row(PrefRow::described(
                "Dropout Protection",
                "Headroom against UI and plug-in jitter. Medium is recommended; Off is lowest latency.",
                &["dropout", "protection", "glitch", "xrun", "buffer"],
                choice(
                    "dropout",
                    vec![
                        (Dp::Off, "Off".to_string()),
                        (Dp::Light, "Light".to_string()),
                        (Dp::Medium, "Medium".to_string()),
                        (Dp::High, "High".to_string()),
                    ],
                    s.playback.dropout_protection,
                    &up,
                    |s, v| s.playback.dropout_protection = v,
                ),
            )),
    ]
}

// ── Editing ─────────────────────────────────────────────────────────────────

fn editing(cx: &PageCtx) -> Vec<PrefGroup> {
    let s = cx.schema;
    let up = cx.up();
    let snap = s.editing.snap.default_snap_value.as_str();
    let snap = ["1/4", "1/8", "1/16", "1/32"]
        .into_iter()
        .find(|v| *v == snap)
        .unwrap_or("1/16");
    vec![
        PrefGroup::new(cx.tr("settings.section.editing-mouse")).row(PrefRow::field(
            cx.tr("settings.natural-scroll"),
            &["natural", "scroll", "trackpad", "wheel", "mouse"],
            pref_switch(
                "editing-natural-scroll",
                s.editing.mouse.natural_scroll,
                apply(&up, |s| {
                    s.editing.mouse.natural_scroll = !s.editing.mouse.natural_scroll
                }),
            ),
        )),
        // Seeds for a new project; an open project keeps its own snap, set
        // from the timeline.
        PrefGroup::new("New Projects")
            .row(PrefRow::field(
                cx.tr("settings.snap-to-grid"),
                &["snap", "grid"],
                pref_switch(
                    "editing-snap-grid",
                    s.editing.snap.snap_to_grid,
                    apply(&up, |s| {
                        s.editing.snap.snap_to_grid = !s.editing.snap.snap_to_grid
                    }),
                ),
            ))
            .row(PrefRow::field(
                cx.tr("settings.field.default-snap"),
                &["snap", "grid", "default", "resolution"],
                choice(
                    "snap-default",
                    ["1/4", "1/8", "1/16", "1/32"]
                        .into_iter()
                        .map(|v| (v, v.to_string()))
                        .collect(),
                    snap,
                    &up,
                    |s, v| s.editing.snap.default_snap_value = v.to_string(),
                ),
            ))
            .note("An open project keeps its own snap, set from the timeline."),
        PrefGroup::new(cx.tr("settings.section.editing-history")).row(PrefRow::field(
            cx.tr("settings.field.max-undo-steps"),
            &["undo", "redo", "history", "steps"],
            pref_stepper(
                "undo-steps",
                s.editing.history.max_undo_steps.to_string(),
                apply(&up, |s| {
                    s.editing.history.max_undo_steps =
                        s.editing.history.max_undo_steps.saturating_sub(5).max(10)
                }),
                apply(&up, |s| {
                    s.editing.history.max_undo_steps =
                        (s.editing.history.max_undo_steps + 5).min(500)
                }),
            ),
        )),
    ]
}

// ── Appearance ──────────────────────────────────────────────────────────────

fn appearance(cx: &PageCtx) -> Vec<PrefGroup> {
    let s = cx.schema;
    let up = cx.up();
    let theme_name = theme::available_theme_summaries()
        .into_iter()
        .find(|(id, _)| id == &s.appearance.theme)
        .map(|(_, name)| name)
        .unwrap_or_else(|| s.appearance.theme.clone());
    let mut interface =
        PrefGroup::new(cx.tr("settings.section.theme-interface")).row(PrefRow::field(
            cx.tr("settings.field.theme-preset"),
            &["theme", "dark", "light", "color"],
            cx.select(HardwareCombo::Theme, "settings-theme", &theme_name),
        ));
    if cfg!(target_os = "windows") {
        interface = interface
            .row(PrefRow::described(
                settings_restart_label(cx.tr("settings.field.text-rendering"), true),
                cx.tr("settings.hint.text-rendering"),
                &["text", "font", "render", "directwrite", "gdi", "blurry"],
                choice(
                    "settings-text-rendering",
                    vec![
                        (TextRenderingBackend::DirectWrite, "DirectWrite".to_string()),
                        (TextRenderingBackend::Gdi, "GDI+".to_string()),
                    ],
                    s.appearance.text_rendering,
                    &up,
                    |s, v| s.appearance.text_rendering = v,
                ),
            ))
            .note(RESTART_NOTE);
    }
    vec![interface]
}

/// Said once under a group whose rows carry the `*` restart marker.
const RESTART_NOTE: &str = "* Takes effect the next time Futureboard Studio starts.";

// ── Performance ─────────────────────────────────────────────────────────────

fn performance(cx: &PageCtx) -> Vec<PrefGroup> {
    let s = cx.schema;
    // The process-wide adapter list; never enumerated on the render path.
    // `None` = still detecting.
    let cached = cached_gpu_devices();
    let detecting = cached.is_none();
    let detected: Vec<_> = cached.map(|list| list.as_ref().clone()).unwrap_or_default();
    let gpu_label = match &s.performance.gpu_device {
        GpuDevicePreference::Auto => "Auto".to_string(),
        GpuDevicePreference::DeviceId(_) if detecting => "Detecting…".to_string(),
        GpuDevicePreference::DeviceId(id) => detected
            .iter()
            .find(|d| &d.id == id)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| "Auto".to_string()),
    };
    let status = match (s.performance.render_mode, detected.len()) {
        (RenderMode::Auto, _) => "Accelerated GPUI paint.".to_string(),
        (RenderMode::CpuRender, _) => "CPU paint fallback.".to_string(),
        (RenderMode::GpuAcceleration, _) if detecting => "Detecting GPU adapters…".to_string(),
        (RenderMode::GpuAcceleration, 0) => "No GPU adapter found — CPU paint is used.".to_string(),
        (RenderMode::GpuAcceleration, n) => format!("GPU layers on · {n} adapter(s)."),
    };
    vec![
        PrefGroup::new("Rendering")
            .row(PrefRow::described(
                settings_restart_label("Renderer", true),
                status,
                &["renderer", "gpu", "cpu", "wgpu", "render"],
                cx.select(
                    HardwareCombo::Renderer,
                    "settings-performance-renderer-trigger",
                    s.performance.render_mode.label(),
                ),
            ))
            .row(PrefRow::field(
                settings_restart_label("GPU Device", true),
                &["gpu", "device", "adapter", "graphics"],
                cx.select(
                    HardwareCombo::GpuDevice,
                    "settings-performance-gpu-device-trigger",
                    &gpu_label,
                ),
            ))
            .row(PrefRow::described(
                "Frame Rate",
                "Display Sync follows the monitor; idle frames are drawn only on demand.",
                &[
                    "frame",
                    "fps",
                    "refresh",
                    "vsync",
                    "display sync",
                    "battery",
                ],
                cx.select(
                    HardwareCombo::FrameRate,
                    "settings-performance-frame-rate-trigger",
                    s.performance.frame_rate.label(),
                ),
            ))
            .note(RESTART_NOTE),
    ]
}

// ── About ───────────────────────────────────────────────────────────────────

fn about(_cx: &PageCtx) -> Vec<PrefGroup> {
    let edition = crate::edition::current_edition_info();
    let version = crate::edition::app_version();
    let mut group = PrefGroup::new("Futureboard Studio")
        .row(PrefRow::field(
            "Version",
            &["version", "about", "build"],
            pref_value(version.to_string()),
        ))
        .row(PrefRow::field(
            "Edition",
            &["edition", "about"],
            pref_value(
                edition
                    .as_ref()
                    .map(|info| info.edition)
                    .unwrap_or("Community"),
            ),
        ));

    if let Some(info) = edition.as_ref() {
        // Named only while the licensed engine is registered, so this row
        // never claims an unavailable backend.
        if let Some(engine) = info.audio_engine.as_ref() {
            group = group.row(PrefRow::field(
                "Audio Engine",
                &["engine", "audio"],
                pref_value(engine.name),
            ));
        }
        let (label, ok) = crate::edition::license_status_label(info.license.as_ref());
        group = group.row(PrefRow::field(
            "License",
            &["license", "activation"],
            settings_status_badge(label, ok),
        ));
        if let Some(license) = info.license.as_ref() {
            if let Some(licensee) = license
                .licensee
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
            {
                group = group.row(PrefRow::field(
                    "Licensed To",
                    &["license", "licensee"],
                    pref_value(licensee.to_string()),
                ));
            }
            group = group.row(PrefRow::field(
                "Expires",
                &["license", "expiry", "expires"],
                pref_value(license.expiry_text()),
            ));
            if !license.entitlements.is_empty() {
                group = group.row(PrefRow::field(
                    "Entitlements",
                    &["license", "entitlements"],
                    pref_value(license.entitlements.join(", ")),
                ));
            }
        }
        // Only offered where activation can actually open. The label follows
        // the state: there is nothing to "manage" without a license.
        if crate::edition::license_action_available() {
            group = group.row(PrefRow::field(
                "Activation",
                &["license", "activate", "activation"],
                pref_button(
                    "settings-license-activate",
                    if info.license.is_some() {
                        "Manage License…"
                    } else {
                        "Activate License…"
                    },
                    true,
                    |_, window, cx| crate::edition::dispatch_license_action(window, cx),
                ),
            ));
        }
    }
    vec![group.note("© 2026 Futureboard Studio team")]
}
