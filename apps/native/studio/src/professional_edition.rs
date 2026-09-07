//! Bridge to the ignored Professional Edition implementation.
//!
//! This tracked module deliberately uses `include!` instead of a `#[path]`
//! module. Rustfmt resolves `#[path]` modules even when their feature is
//! disabled, which made Community Edition checks require the private source
//! tree. The include macros are expanded only when this feature-gated module is
//! compiled by an Professional Edition build.
//!
//! Compiling with `--features professional` grants nothing on its own. Providers
//! below install only after a signed license token verifies for this machine,
//! so an Professional build on an unlicensed machine behaves like Community.

mod license {
    include!(concat!(
        env!("OUT_DIR"),
        "/futureboard-professional/license.rs"
    ));
}

mod license_activation_dialog {
    include!(concat!(
        env!("OUT_DIR"),
        "/futureboard-professional/license_activation_dialog.rs"
    ));
}

mod eula {
    include!(concat!(
        env!("OUT_DIR"),
        "/futureboard-professional/eula.rs"
    ));
}

mod eula_dialog {
    include!(concat!(
        env!("OUT_DIR"),
        "/futureboard-professional/eula_dialog.rs"
    ));
}

mod updates {
    include!(concat!(
        env!("OUT_DIR"),
        "/futureboard-professional/updates.rs"
    ));
}

#[cfg(target_os = "windows")]
mod asio {
    include!(concat!(
        env!("OUT_DIR"),
        "/futureboard-professional/asio.rs"
    ));
}

pub use license_activation_dialog::open_license_activation_window;

/// Open the License window over the given owner window.
///
/// One entry point for every surface that offers it — the Welcome footer and the
/// About panel. The window explains this machine's license and hands off to the
/// Futureboard Launcher; Studio itself no longer activates anything.
pub fn open_license_activation(
    owner_bounds: Option<gpui::Bounds<gpui::Pixels>>,
    cx: &mut gpui::App,
) {
    if let Err(error) = open_license_activation_window(owner_bounds, cx) {
        eprintln!("[LicenseActivation] failed to open the license window: {error}");
    }
}

#[cfg(target_os = "windows")]
use std::sync::atomic::{AtomicBool, Ordering};

/// Cached result of the existing signed-token verification path. The engine's
/// provider callback is only an atomic load; it never verifies a token or does
/// filesystem/network work on an audio-related path.
#[cfg(target_os = "windows")]
static ASIO_ENTITLEMENT: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "windows")]
fn current_asio_entitlement() -> bool {
    ASIO_ENTITLEMENT.load(Ordering::Acquire)
}

/// Show the first-run EULA dialog when the current agreement version has not yet
/// been accepted. Called once the first app surface is up, so it appears as a
/// modal on top. Declining (or closing) the dialog quits the app.
pub fn show_eula_if_needed(cx: &mut gpui::App) {
    if eula::needs_acceptance() {
        if let Err(error) = eula_dialog::open_eula_window(cx) {
            eprintln!("[EULA] failed to open dialog: {error}");
        }
    }
}

/// Install the Professional Edition runtime providers that a verified license
/// grants. Safe to call more than once: provider registration is process-wide,
/// while the cached live entitlement is refreshed on every call so activation,
/// renewal, expiry, or entitlement changes take effect without a restart.
///
/// An unlicensed machine is not an error. Its cached capability is false, so
/// the audio backend list and all future ASIO host acquisitions stay disabled.
pub fn install_licensed_providers() -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        let asio_entitled = license::active_license()
            .is_some_and(|license| license.grants(license::ENTITLEMENT_ASIO));
        ASIO_ENTITLEMENT.store(asio_entitled, Ordering::Release);

        if !asio_entitled || DirectAudio::asio_support_enabled() {
            return Ok(());
        }

        DirectAudio::backend::register_asio_host_provider(
            DirectAudio::backend::AsioHostProvider::new(asio::host, current_asio_entitlement),
        )?;
    }

    Ok(())
}

/// Install Professional Edition runtime providers before the application starts.
///
/// Providers install from the sealed license file with no network involved, so
/// this cannot delay the DAW opening however the licensing service is doing.
///
/// A lapsed license is not re-pulled here: renewal belongs to the Futureboard
/// Launcher, which is the only program that talks to the activation service.
/// The License window says so, and offers to open it.
pub fn install() -> Result<(), String> {
    sphere_ui_components::edition::set_edition_provider(std::sync::Arc::new(edition_info));
    // Lets the About panel open the License window without the shared crate
    // knowing what licensing is. A machine already inside a project can then
    // check its license, and reach the Launcher, without closing it.
    sphere_ui_components::edition::set_license_action_handler(std::sync::Arc::new(
        |window: &mut gpui::Window, cx: &mut gpui::App| {
            open_license_activation(Some(window.bounds()), cx);
        },
    ));

    // Account sign-in is not an entitlement and is installed for every edition
    // by `sphere_ui_components::account::install_default_account_provider`. A
    // Professional build only adds licensing on top of that identity.

    // Purely local: the sealed license file is read and verified against this
    // machine, with no network at any point. Renewal and activation live in the
    // Futureboard Launcher now, so there is nothing to spawn here and nothing
    // that can delay the DAW opening.
    install_licensed_providers()?;
    Ok(())
}

/// Point the shared Software Update dialog at the licensed R2 transport instead
/// of the public GitHub release list.
///
/// Returns whether it took over. `false` means this build has no licensing
/// endpoint baked in, and the caller keeps the Community transport — which is
/// the only sane fallback: without an endpoint there is nothing to ask.
///
/// This must run *after* `app::setup` would otherwise register the GitHub
/// provider, so the application calls it from there rather than from
/// [`install`], which runs before GPUI starts.
pub fn register_update_provider() -> bool {
    let Some(provider) =
        updates::configured_update_provider(env!("CARGO_PKG_VERSION"), install_staged_update)
    else {
        return false;
    };
    sphere_ui_components::update_service::set_update_provider(provider);
    true
}

/// Hand a staged Professional download to the platform installer.
///
/// The hand-off (Inno `/SILENT` on Windows, bundle swap on macOS, AppImage
/// replace on Linux) is edition-independent, so the licensed transport reuses
/// the application's implementation rather than carrying a second copy.
fn install_staged_update(
    staged: &std::path::Path,
) -> Result<sphere_ui_components::update_service::InstallOutcome, String> {
    crate::updater::install_update(staged, &crate::updater::cache_root())
}

/// Build the edition/license snapshot the shared About panel renders. Re-reads
/// and re-verifies the stored token on each call, so it always reflects current
/// state (post-activation, post-renewal) with no explicit refresh wiring.
fn edition_info() -> sphere_ui_components::edition::EditionInfo {
    use sphere_ui_components::edition::EditionInfo;

    let license = license::stored_license_status()
        .as_ref()
        .map(license::LicenseStatus::to_display);

    EditionInfo {
        edition: "Professional",
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        license,
        audio_engine: asio_engine_badge(),
    }
}

/// Steinberg ASIO logo, staged by `build.rs` beside the private sources and
/// embedded so the About window needs no install-layout file.
#[cfg(target_os = "windows")]
static ASIO_LOGO_PNG: &[u8] = include_bytes!(concat!(
    env!("OUT_DIR"),
    "/futureboard-professional/asio-logo.png"
));

/// The ASIO Audio Engine badge for the About surfaces.
///
/// Present only while the licensed ASIO host is actually registered — the same
/// condition under which the audio backend list offers ASIO — so About never
/// advertises an engine this machine cannot select. GPUI decodes the PNG on
/// first paint; the `Image` handle is built once and shared across renders.
fn asio_engine_badge() -> Option<sphere_ui_components::edition::AudioEngineBadge> {
    #[cfg(target_os = "windows")]
    {
        use sphere_ui_components::edition::AudioEngineBadge;
        use std::sync::{Arc, OnceLock};

        if !DirectAudio::asio_support_enabled() {
            return None;
        }
        static LOGO: OnceLock<Arc<gpui::Image>> = OnceLock::new();
        let logo = LOGO
            .get_or_init(|| {
                Arc::new(gpui::Image::from_bytes(
                    gpui::ImageFormat::Png,
                    ASIO_LOGO_PNG.to_vec(),
                ))
            })
            .clone();
        return Some(AudioEngineBadge {
            name: "ASIO Audio Engine",
            notice: "ASIO is a trademark and software of Steinberg Media Technologies GmbH.",
            logo: Some(logo),
        });
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::license;

    /// ASIO registration must track the verified license exactly — never the
    /// `professional` feature, which only decides whether the code is compiled in.
    ///
    /// Asserting both directions against the same machine keeps this honest on
    /// a developer box either way: unlicensed, ASIO must stay off; licensed, it
    /// must come on.
    #[test]
    fn asio_registration_tracks_the_verified_license() {
        let licensed = license::active_license()
            .is_some_and(|license| license.grants(license::ENTITLEMENT_ASIO));
        eprintln!("[license-e2e] machine holds an ASIO entitlement: {licensed}");

        super::install_licensed_providers().expect("installing must not fail");

        assert_eq!(
            DirectAudio::asio_support_enabled(),
            licensed,
            "an ASIO host must be registered if and only if a verified license grants it"
        );
        assert_eq!(
            DirectAudio::backend::BackendKind::allowed_for_current_platform()
                .contains(&DirectAudio::backend::BackendKind::Asio),
            licensed,
            "the backend list must offer ASIO if and only if a verified license grants it"
        );
    }
}
