pub mod about_window;
pub mod add_track_dialog;
pub(crate) mod app_chrome;
mod ara_editor_host;
pub mod audio_connections_panel;
pub mod audio_connections_window;
mod audio_editor_adapter;
mod audio_editor_host;
pub mod background_tasks;
mod bottom_panel;
mod bottom_panel_shell;
pub mod box_list_view;
pub mod builtin_plugin_editor;
pub(crate) mod builtin_plugin_editor_surface;
pub mod builtin_plugin_editor_window;
pub mod builtin_plugin_files;
pub mod clock_window;
pub mod color_picker;
pub mod combo_box;
pub mod command_palette;
pub mod context_menu;
pub mod controls;
pub mod edit;
mod editor_panel;
mod effect_editor_tab_view;
pub mod extensions_window;
pub mod fader;
pub mod file_browser;
pub mod form;
mod icon;
mod icon_button;
pub mod inspector;
pub mod jam_window;
pub mod key_recorder;
pub mod keymap_window;
pub mod knob;
pub mod master_transport_meter;
pub mod mdi;
pub mod menu_bar;
pub mod menu_dropdown;
pub mod message_box_dialog;
pub mod midi_editor_window;
pub mod midi_export_dialog;
pub mod midi_import_dialog;
pub mod mixer_master_strip_view;
pub mod mixer_panel;
pub mod mixer_panel_view;
pub mod mixer_render;
pub mod mixer_surface;
pub mod mixer_tree_cache;
pub mod mixer_tree_model;
pub mod mixer_tree_sidebar;
pub mod mixer_tree_sidebar_view;
pub use mixer_tree_sidebar_view::MixerTreeSidebar;
mod mixer_window;
pub(crate) use mixer_window::{external_mixer_debug, external_mixer_debug_enabled};
pub mod gpu_editor_diagnostics;
pub mod inspector_kit;
pub mod native_editor_shell;
pub mod numeric_edit;
pub mod panel;
mod performance_overlay;
pub mod performance_window;
pub mod piano_roll;
pub mod plugin_content_host;
pub mod plugin_editor_backend;
pub mod plugin_editor_chrome;
pub mod plugin_editor_window;
pub mod plugin_format_badge;
pub mod plugin_manager;
pub mod plugin_picker;
pub mod plugin_shell_text;
pub mod progress_dialog;
pub mod project_settings_window;
pub mod project_switcher;
pub mod reorder;
pub mod routing_matrix_window;
pub mod scroll_thumb;
pub mod settings_components;
pub mod settings_dialog;
pub mod settings_layout;
mod sidebar;
pub mod slider;
pub mod solfege_editor;
pub mod song_text_panel;
pub mod soundfont_player_mdi;
pub mod soundfont_player_window;
pub(crate) mod spin_drag;
mod status_bar;
mod status_bar_view;
pub mod stem_extractor_dialog;
pub mod text_input;
pub mod timeline;
pub mod title_bar;
pub mod transport_key;
pub mod transport_perf_meter;
pub mod update_dialog;
pub mod video_player_window;
pub mod virtual_keyboard;

pub use about_window::{open_about_window, AboutWindow};
pub use add_track_dialog::{
    open_add_track_window, AddTrackDialogCallbacks, AddTrackDialogState, AddTrackKind,
    AddTrackWindow,
};
pub use app_chrome::{
    account_chip, account_menu_overlay, app_chrome, app_chrome_drawn_height, bpm_debug_enabled,
    bpm_drag_sensitivity, BpmChangeCb, BpmDragCb, BpmDragSample, BpmMenuCb, ChromeActionCb,
    PanelChromeState, ProjectChromeState, TransportChromeState, BPM_DRAG_DEADZONE_PX, BPM_MAX,
    BPM_MIN,
};
pub use ara_editor_host::AraEditorHost;
pub use audio_editor_host::AudioEditorHost;
pub use background_tasks::{
    background_task_button, background_task_panel, BackgroundTaskCancelCb, BackgroundTaskKind,
    BackgroundTaskProgress, BackgroundTaskStatus, BackgroundTaskStore, BackgroundTaskToggleCb,
    BackgroundTaskUpdate,
};
pub use bottom_panel::{bottom_panel, BottomPanelResizeDrag, BottomPanelState, BottomTab};
pub(crate) use bottom_panel_shell::BottomPanelShell;
pub use box_list_view::{
    box_list_empty_state, box_list_group_label, box_list_icon_button, box_list_item,
    box_list_item_badge, box_list_item_content, box_list_item_leading_icon, box_list_item_subtitle,
    box_list_item_title, box_list_item_trailing, box_list_toggle, box_list_view, BoxListBadgeTone,
};
pub use color_picker::{
    color_picker_field, color_picker_trigger, default_presets, ColorPickerCallbacks,
    ColorPickerPlacement, ColorPickerState, ColorPickerValue,
};
pub use combo_box::{combo_box_menu, combo_box_trigger, dedupe_preserve_order, ComboBoxOption};
pub use command_palette::{
    command_palette_entries, command_palette_overlay, CommandPaletteEntry, CommandPaletteState,
};
pub use controls::{
    fb_button, fb_checkbox, fb_color_swatch, fb_field_label, fb_form_row, fb_section_header,
    fb_section_label, fb_segmented_button, fb_stepper_button, fb_tooltip, FbButtonKind,
};
pub use editor_panel::ClipEditorPanel;
pub(crate) use effect_editor_tab_view::EffectEditorTabView;
pub use extensions_window::{open_extensions_window, ExtensionsWindow};
pub use fader::fader;
pub use icon::icon;
pub use icon_button::icon_button;
pub use inspector::{
    inspector_checkbox, inspector_hint_text, inspector_mini_button, inspector_numeric_stepper,
    inspector_numeric_stepper_with_drag_callbacks, inspector_row, inspector_section,
    inspector_select, inspector_value, InspectorNumericChangeCb, InspectorNumericGestureCb,
    InspectorSelectOption,
};
pub use jam_window::{open_jam_window, CreateTrackFromStream, JamWindow, JamWindowHandlers};
pub use key_recorder::{key_recorder_field, KeyRecorderState};
pub use keymap_window::{open_keymap_window, KeymapChangedCb, KeymapWindow};
pub use knob::knob;
pub use master_transport_meter::{MasterTransportMeter, MasterTransportMeterCallbacks};
pub use mdi::{
    mdi_workspace, MdiDocumentKind, MdiDocumentState, MdiWorkspaceCallbacks, MdiWorkspaceState,
};
pub use menu_bar::{menu_bar, menu_label_button};
pub use message_box_dialog::{
    open_message_box_window, unsaved_changes_options, MessageBoxKind, MessageBoxOptions,
    MessageBoxResult, MessageBoxWindow, MESSAGE_BOX_WIDTH,
};
pub use midi_editor_window::{open_midi_editor_window, MidiEditorTarget, MidiEditorWindow};
pub use midi_export_dialog::{
    open_midi_export_dialog, MidiExportDialog, MidiExportDialogSetup, MidiExportTrackChoice,
};
pub use midi_import_dialog::{open_midi_import_dialog, MidiImportDialog, MidiImportDialogSetup};
pub use mixer_master_strip_view::MixerMasterStripView;
pub use mixer_panel::mixer_panel;
pub use mixer_panel_view::{docked_mixer_shell, MixerPanelView};
pub use mixer_window::{open_mixer_window, MixerContextMenu, MixerSnapshot, MixerWindow};
pub use numeric_edit::{
    is_intermediate_draft, parse_draft, CommitPolicy, CommitTrigger, NumberFormat,
    NumericEditSession,
};
pub use panel::{inspector_debug, inspector_debug_enabled, right_panel};
pub use performance_overlay::{performance_overlay, PerformanceOverlaySnapshot};
pub use piano_roll::PianoRoll;
pub use plugin_manager::{
    open_plugin_manager_window, open_plugin_scan_progress_dialog, FilterCounts,
    PluginManagerDialogState, PluginManagerWindow, SidebarFilter, SortDir, SortKey,
};
pub use progress_dialog::{
    open_copying_file_dialog_window, open_loading_session_dialog_window,
    open_progress_dialog_window, progress_bar, CopyingFileDialogOptions,
    LoadingSessionDialogOptions, ProgressBarValue, ProgressDialogCancelCb, ProgressDialogOptions,
    ProgressDialogWindow, PROGRESS_DIALOG_WIDTH,
};
pub use routing_matrix_window::{
    open_routing_matrix_window, RoutingMatrixSnapshot, RoutingMatrixWindow,
};
pub use settings_components::{
    settings_box_list, settings_box_list_group, settings_combo_trigger, settings_control_slot,
    settings_label, settings_label_width, settings_readout, settings_restart_footer,
    settings_restart_label, settings_row, settings_row_restart, settings_row_shell,
    settings_row_with_description, settings_section, settings_section_hint_text, settings_status,
    settings_toggle, RESTART_FOOTER_TEXT,
};
pub use settings_dialog::{
    open_settings_window, settings_dialog, HardwareCombo, OnSettingUpdate, SettingsDialogCallbacks,
    SettingsDialogState, SettingsTab, SettingsWindow,
};
pub use sidebar::{sidebar, BrowserCallbacks, SIDEBAR_WIDTH};
pub use slider::slider;
pub use solfege_editor::{SolfegeEditorPanel, SolfegeEditorTab};
pub use song_text_panel::{
    open_song_text_window, SongTextPanelKind, SongTextPanelView, SongTextWindow,
};
pub use soundfont_player_mdi::{
    ensure_soundfont_player_document, soundfont_player_mdi_workspace, SOUNDFONT_PLAYER_MDI_TITLE,
};
pub use soundfont_player_window::{open_soundfont_player_window, SoundfontPlayerWindow};
pub use status_bar::{
    status_bar, status_bar_with_background_tasks, PerfMetricsToggleCb, StatusBarContent,
    StatusBarPerfMetrics,
};
pub(crate) use status_bar_view::status_content_signature;
pub(crate) use status_bar_view::StatusBarView;
pub use stem_extractor_dialog::{
    open_stem_extractor_window, StemExtractApplyRequest, StemExtractJobState,
    StemExtractJobSummary, StemExtractResultStem, StemExtractorDialogDefaults, StemExtractorWindow,
    StemSourceClip, STEM_EXTRACTOR_WINDOW_WIDTH,
};
pub use text_input::{text_field, TextInputAction, TextInputState};
pub use transport_perf_meter::{TransportPerfMeter, TransportPerfSnapshot};
pub use update_dialog::{open_update_dialog, open_update_dialog_for, UpdateDialogWindow};
pub use video_player_window::{open_video_player_window, VideoPlayerSnapshot, VideoPlayerWindow};
pub use virtual_keyboard::{
    VirtualKeyboardEventSink, VirtualKeyboardPanel, VirtualKeyboardPanelState,
    VirtualKeyboardService,
};
