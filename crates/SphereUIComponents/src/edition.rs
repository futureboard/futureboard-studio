//! Process-global edition / license status surfaced to shared UI (the About
//! page in Settings).
//!
//! This module holds **no** license logic and **no** signing keys. It is a
//! dependency-free hand-off point: the shared crate must never depend on the
//! private Professional Edition crate, so the Professional build's app layer installs
//! a provider here and the shared About panel reads from it. A Community build
//! installs nothing and the About panel falls back to its plain edition info.
//!
//! The provider is a closure so it stays fresh: the About panel re-reads license
//! state each time it is shown, which is exactly what lets an activation — or a
//! background renewal on a later launch — appear without special refresh wiring.
//! It is called only from the (rare, user-driven) Settings render, never a hot
//! path.

use std::sync::{Arc, OnceLock, RwLock};

use gpui::{App, Window};

/// Whether a bound license is currently usable or has lapsed. Mirrors the
/// Professional Edition's own state enum without depending on that crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LicenseDisplayState {
    Active,
    Expired,
}

/// The parts of a verified license worth showing the owner. Never carries the
/// token itself or anything secret — only what the owner already knows.
#[derive(Debug, Clone)]
pub struct LicenseDisplay {
    pub state: LicenseDisplayState,
    /// Display name from the verified token, if the service supplied one.
    pub licensee: Option<String>,
    /// Entitlement ids the license grants, e.g. `["asio"]`.
    pub entitlements: Vec<String>,
    /// Unix seconds the license lapses, or `None` for a perpetual license.
    pub expires_at: Option<u64>,
}

impl LicenseDisplay {
    /// Human-readable expiry line. Lives here rather than in one panel so the
    /// About page and the activation dialog cannot drift into describing the
    /// same license differently.
    pub fn expiry_text(&self) -> String {
        match (self.state, self.expires_at) {
            (_, None) => "Perpetual".to_string(),
            (LicenseDisplayState::Expired, Some(_)) => {
                "Expired — reactivate to continue".to_string()
            }
            (LicenseDisplayState::Active, Some(expires_at)) => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_secs())
                    .unwrap_or(0);
                match expires_at.saturating_sub(now) / 86_400 {
                    0 => "Under a day remaining".to_string(),
                    1 => "1 day remaining".to_string(),
                    days => format!("{days} days remaining"),
                }
            }
        }
    }
}

/// Badge text for a license, and whether it reads as a good state. `None` is a
/// machine with no license at all, which is not an error — a Community-behaving
/// Professional build is a supported state.
pub fn license_status_label(license: Option<&LicenseDisplay>) -> (&'static str, bool) {
    match license.map(|license| license.state) {
        Some(LicenseDisplayState::Active) => ("Active", true),
        Some(LicenseDisplayState::Expired) => ("Expired", false),
        None => ("Not activated", false),
    }
}

/// What the About panel shows about this build.
#[derive(Debug, Clone)]
pub struct EditionInfo {
    /// Human-readable edition name, e.g. `"Professional"`.
    pub edition: &'static str,
    /// The application version string the app layer reports.
    pub app_version: String,
    /// The current license, or `None` when this build/machine is not licensed.
    pub license: Option<LicenseDisplay>,
    /// A licensed audio-engine integration the About surfaces should name and
    /// credit, or `None` when the build runs on the platform's stock backends.
    pub audio_engine: Option<AudioEngineBadge>,
}

/// A licensed audio-engine integration — the Professional Edition's ASIO host —
/// as the About window and the Settings About panel present it. Carries the
/// partner logo the engine's license terms ask to be shown with its name.
#[derive(Debug, Clone)]
pub struct AudioEngineBadge {
    /// Display name, e.g. `"ASIO Audio Engine"`.
    pub name: &'static str,
    /// Trademark / attribution line shown beside the logo.
    pub notice: &'static str,
    /// Logo bitmap for `gpui::img`, decoded by GPUI on first paint; `None`
    /// renders a text-only badge.
    pub logo: Option<Arc<gpui::Image>>,
}

type EditionProvider = Arc<dyn Fn() -> EditionInfo + Send + Sync + 'static>;

fn slot() -> &'static RwLock<Option<EditionProvider>> {
    static SLOT: OnceLock<RwLock<Option<EditionProvider>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

/// Install the edition/license provider. Called once by the app layer of an
/// Professional build during startup. Idempotent: a later call replaces it.
pub fn set_edition_provider(provider: EditionProvider) {
    if let Ok(mut guard) = slot().write() {
        *guard = Some(provider);
    }
}

/// Current edition/license info, or `None` on a build with no provider
/// (Community Edition). The provider is invoked with no lock held so it can
/// safely touch other shared state.
pub fn current_edition_info() -> Option<EditionInfo> {
    let provider = {
        let guard = slot().read().ok()?;
        guard.as_ref()?.clone()
    };
    Some(provider())
}

/// Whether the running process is the Professional Edition surface.
///
/// The public Community build deliberately has no provider, while the private
/// `crates/ExclusiveEdition` startup path installs one before the first UI is
/// shown. This is a build/edition capability, not a license entitlement: an
/// unlicensed Professional process still owns the Professional feature
/// surface, and the licensing UI can explain what activation is required.
/// Shared data handling must not use this gate — Community still loads,
/// visualizes, plays back, and saves Note Expression data.
pub fn professional_features_available() -> bool {
    current_edition_info().is_some()
}

fn app_version_slot() -> &'static RwLock<Option<String>> {
    static SLOT: OnceLock<RwLock<Option<String>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

/// Record the running application's package version. Called once by the app
/// layer at startup with its own `env!("CARGO_PKG_VERSION")` — the shared UI
/// crate's own package version differs, so it cannot read the app version
/// itself.
pub fn set_app_version(version: impl Into<String>) {
    if let Ok(mut guard) = app_version_slot().write() {
        *guard = Some(version.into());
    }
}

/// The running application's version string, resolved in priority order:
/// the value the app layer set via [`set_app_version`], then the edition
/// provider's `app_version`, then this crate's own package version as a last
/// resort (only hit in unit tests / standalone UI harnesses).
pub fn app_version() -> String {
    if let Some(version) = app_version_slot().read().ok().and_then(|g| g.clone()) {
        return version;
    }
    if let Some(info) = current_edition_info() {
        return info.app_version;
    }
    env!("CARGO_PKG_VERSION").to_string()
}

// ── License action hand-off ──────────────────────────────────────────────────
//
// The About panel needs to be able to *open* activation, not just describe it —
// a machine already inside a project should not have to close it to enter a key.
// The dialog itself is Professional Edition code the shared crate must not depend
// on, so the app layer installs a handler here, exactly like `crate::account`.

type LicenseActionHandler = Arc<dyn Fn(&mut Window, &mut App) + Send + Sync + 'static>;

fn license_action_slot() -> &'static RwLock<Option<LicenseActionHandler>> {
    static SLOT: OnceLock<RwLock<Option<LicenseActionHandler>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

/// Install the handler that opens license activation. Called once by the app
/// layer of an Professional build during startup.
pub fn set_license_action_handler(handler: LicenseActionHandler) {
    if let Ok(mut guard) = license_action_slot().write() {
        *guard = Some(handler);
    }
}

/// Whether this build can open license activation. Community builds cannot, so
/// the About panel must not offer an action that would do nothing.
pub fn license_action_available() -> bool {
    license_action_slot()
        .read()
        .map(|guard| guard.is_some())
        .unwrap_or(false)
}

/// Open license activation through the installed handler. No-op when unhandled,
/// so a Community build (or a race at startup) simply does nothing.
pub fn dispatch_license_action(window: &mut Window, cx: &mut App) {
    let handler = {
        let Ok(guard) = license_action_slot().read() else {
            return;
        };
        guard.as_ref().cloned()
    };
    if let Some(handler) = handler {
        handler(window, cx);
    }
}

// ── Licensed provider refresh ────────────────────────────────────────────────
//
// A Professional build installs what its license grants (ASIO) at startup. The
// Futureboard Launcher renews or activates a license while Studio is already
// running — it may even start Studio and renew a moment later — and without a
// second look Studio keeps the answer it read at startup until it restarts.
// The app layer installs a handler that re-reads the license; the shared crate
// calls it where an entitlement is about to be shown.

type LicenseRefreshHandler = Arc<dyn Fn() + Send + Sync + 'static>;

fn license_refresh_slot() -> &'static RwLock<Option<LicenseRefreshHandler>> {
    static SLOT: OnceLock<RwLock<Option<LicenseRefreshHandler>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

/// Install the handler that re-reads the license and installs what it now
/// grants. Called once by the app layer of a Professional build at startup.
pub fn set_license_refresh_handler(handler: LicenseRefreshHandler) {
    if let Ok(mut guard) = license_refresh_slot().write() {
        *guard = Some(handler);
    }
}

/// Re-read the license so a renewal or activation made while Studio runs takes
/// effect. Local file and signature work only — call it when a surface that
/// shows entitlements opens, never per frame. No-op in a Community build.
pub fn refresh_licensed_providers() {
    let handler = {
        let Ok(guard) = license_refresh_slot().read() else {
            return;
        };
        guard.as_ref().cloned()
    };
    if let Some(handler) = handler {
        handler();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both halves in one test on purpose: the slots are process-global, so a
    /// separate "nothing is installed" test races whichever test installs one —
    /// which is exactly how the empty-slot assertion used to fail at random.
    #[test]
    fn empty_slots_report_nothing_and_installed_ones_are_read_back() {
        // A build with nothing installed (the Community case) must report
        // nothing rather than fabricate an edition or offer a dead action.
        assert!(current_edition_info().is_none());
        assert!(!license_action_available());
        assert!(!professional_features_available());

        set_license_action_handler(Arc::new(|_window, _cx| {}));
        assert!(license_action_available());

        set_edition_provider(Arc::new(|| EditionInfo {
            edition: "Test",
            app_version: "9.9.9".to_string(),
            license: Some(LicenseDisplay {
                state: LicenseDisplayState::Active,
                licensee: Some("Jane Doe".to_string()),
                entitlements: vec!["asio".to_string()],
                expires_at: None,
            }),
            audio_engine: None,
        }));
        let info = current_edition_info().expect("provider was installed");
        assert!(professional_features_available());
        assert_eq!(info.edition, "Test");
        assert_eq!(info.app_version, "9.9.9");
        assert!(matches!(
            info.license,
            Some(LicenseDisplay {
                state: LicenseDisplayState::Active,
                ..
            })
        ));
    }
}
