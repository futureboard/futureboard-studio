#![allow(dead_code)]
#![allow(
    clippy::arc_with_non_send_sync,
    clippy::clone_on_copy,
    clippy::collapsible_match,
    clippy::derivable_impls,
    clippy::doc_lazy_continuation,
    clippy::doc_overindented_list_items,
    clippy::double_ended_iterator_last,
    clippy::enum_variant_names,
    clippy::excessive_precision,
    clippy::field_reassign_with_default,
    clippy::let_unit_value,
    clippy::manual_clamp,
    clippy::manual_is_multiple_of,
    clippy::manual_map,
    clippy::manual_pattern_char_comparison,
    clippy::manual_range_contains,
    clippy::module_inception,
    clippy::needless_borrow,
    clippy::needless_borrows_for_generic_args,
    clippy::needless_lifetimes,
    clippy::needless_return,
    clippy::new_without_default,
    clippy::ptr_arg,
    clippy::single_match,
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::unnecessary_min_or_max,
    clippy::unnecessary_sort_by,
    clippy::useless_conversion,
    clippy::useless_format
)]

pub mod account;
pub mod app_state;
pub mod assets;
pub mod audio_connections;
pub mod audio_routing;
pub mod audio_routing_compile;
pub mod auth;
pub mod auth_dialog;
pub mod boot;
pub mod color;
pub mod components;
pub mod deeplink;
pub mod device_registry;
pub mod edition;
pub mod embedded_assets;
pub mod export;
pub mod extensions_registry;
pub mod feeds;
pub mod fonts;
pub mod forensic_trace;
pub mod frame_scheduler;
pub mod i18n;
pub mod input_routing;
/// Audio Jam: the client, the bridge into the audio engine, and the routing
/// glue that makes a remote performer selectable as a track input.
pub mod jam;
pub mod keymap;
pub mod layout;
pub mod loading_session;
pub mod loopback_ports;
pub mod menu;
pub mod native_macos_menu;
pub mod output_routing;
pub mod overlay;
pub mod paths;
pub mod perf;
pub mod platform_chrome;
pub mod pre_studio_install;
pub mod project;
pub mod session_shutdown;
pub mod settings;
pub mod shell_metrics;
pub mod shutdown;
pub mod solfege;
pub mod soundfont_player;
pub mod system_load;
pub mod tap_tempo;
pub mod update_service;
pub mod window_lifecycle;
pub mod window_position;
pub mod workspace_layout;
pub use shutdown::ShutdownState;
/// Re-export of the separated plugin-host bridge client so the native app can
/// log bridge env / drive the bridge without a direct `sphere-plugin-host` dep.
pub use SpherePluginHost::plugin_host_client;
pub use SpherePluginHost::plugin_host_lifecycle;
pub use SpherePluginHost::plugin_host_main_window;
pub use SpherePluginHost::process_manager::PluginHostProcessManager;
pub mod splash;
pub mod startup;
pub mod theme;
pub mod user_manual;
pub mod welcome;

pub fn ui_debug_enabled() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| {
        std::env::var("FUTUREBOARD_UI_DEBUG")
            .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    })
}
