//! Per-plug-in editor quirks.
//!
//! Some VST3 vendors ship editors that don't follow the SDK's "attach returns,
//! UI is ready" contract. UAD Native (uaudio_*.vst3, "UADx 1176" et al.) hosts
//! a Chromium/CEF runtime that initializes asynchronously, so the host has to:
//!
//! - Initialize COM as STA on the editor thread before `attached()`.
//! - Wait 100–3000 ms for the WebView child windows to materialize before
//!   declaring the editor blank.
//! - Pump messages aggressively while waiting.
//! - Prefer the owned tool-window fallback (or at least allow it) when the
//!   WS_CHILD path stays blank — GPUI's DirectComposition surface often
//!   composites over WebView children.
//!
//! Keep this table small and generic; matching is intentionally loose so a
//! single entry covers a vendor family. Generic plug-ins fall through to
//! [`PluginEditorQuirk::default()`] which preserves the SDK-correct behaviour.

use std::path::Path;

use crate::native_editor::PluginEditorPresentationMode;

/// Browser/WebView runtime bundled inside a VST3 plug-in's editor.
///
/// Modern plug-ins frequently render their editor with an embedded browser
/// engine and ship the runtime DLLs/resources inside the `.vst3` bundle. The
/// loader DLLs resolve their dependents from their own directory, so the host
/// must put that directory on the DLL search path before `createView`/
/// `attached`. This enum is keyed off bundled marker files — never vendor
/// names — so the compatibility layer is generic, not UAD-specific.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginEditorRuntimeKind {
    /// No bundled browser runtime — a normal native-UI plug-in (FabFilter etc.).
    Native,
    /// Microsoft Edge WebView2 (`WebViewLoader.dll`, fixed-version runtimes).
    WebView2,
    /// Chromium Embedded Framework (`libcef.dll` + cef paks).
    Cef,
    /// Raw Chromium content shell (`chrome_elf.dll`, snapshot/pak files).
    Chromium,
    /// Browser engine detected via resource files but engine flavor unknown.
    BrowserUnknown,
}

impl PluginEditorRuntimeKind {
    /// Stable diagnostic label (matches the C++ `daux_editor_runtime_kind_name`).
    pub fn as_str(self) -> &'static str {
        match self {
            PluginEditorRuntimeKind::Native => "Native",
            PluginEditorRuntimeKind::WebView2 => "WebView2",
            PluginEditorRuntimeKind::Cef => "Cef",
            PluginEditorRuntimeKind::Chromium => "Chromium",
            PluginEditorRuntimeKind::BrowserUnknown => "BrowserUnknown",
        }
    }

    /// `true` for any non-native (browser/WebView) editor runtime.
    pub fn is_browser_based(self) -> bool {
        !matches!(self, PluginEditorRuntimeKind::Native)
    }
}

#[cfg(target_arch = "aarch64")]
const RUNTIME_ARCH_SUBDIR: &str = "win-arm64";
#[cfg(not(target_arch = "aarch64"))]
const RUNTIME_ARCH_SUBDIR: &str = "win-x64";

/// Candidate sub-directories (relative to the bundle root) to probe for a
/// bundled browser runtime. Bounded — no recursion.
const BUNDLE_SCAN_DIRS: &[&str] = &[
    "",
    "Contents/Resources",
    "Contents/x86_64-win",
    // A 32-bit bundle, which the 32-bit plug-in bridge loads.
    "Contents/x86-win",
    "Contents/Resources/WebView2",
    "Contents/Resources/CEF",
    "Contents/Resources/Chromium",
    "Contents/Resources/Browser",
    "Contents/Resources/runtimes",
    "Contents/Resources/bin",
];

/// Inspect a `.vst3` bundle and classify the editor's browser/WebView runtime.
///
/// Mirrors the C++ `daux_detect_editor_runtime` detection so the Rust UI layer
/// can label the editor / surface meaningful errors without re-implementing the
/// scan. Detection is purely file-presence based, so it is cheap and safe to
/// call before opening the editor (never on the audio thread).
pub fn detect_plugin_editor_runtime(plugin_path: &Path) -> PluginEditorRuntimeKind {
    let arch_native_rel = format!("runtimes/{RUNTIME_ARCH_SUBDIR}/native");
    let bare_arch_native_rel = format!("{RUNTIME_ARCH_SUBDIR}/native");

    let mut found_webview2 = false;
    let mut found_cef = false;
    let mut found_chromium = false;
    let mut found_browser = false;

    for rel in BUNDLE_SCAN_DIRS {
        let base = if rel.is_empty() {
            plugin_path.to_path_buf()
        } else {
            plugin_path.join(rel)
        };
        if !base.is_dir() {
            continue;
        }

        // WebView2 fixed-version runtime: WebViewLoader.dll directly or under
        // runtimes/win-{arch}/native (and the bare win-{arch}/native).
        for native_rel in ["", arch_native_rel.as_str(), bare_arch_native_rel.as_str()] {
            let native_dir = if native_rel.is_empty() {
                base.clone()
            } else {
                base.join(native_rel)
            };
            if !native_dir.is_dir() {
                continue;
            }
            if native_dir.join("WebViewLoader.dll").is_file()
                || native_dir.join("Microsoft.Web.WebView2.Core.dll").is_file()
            {
                found_webview2 = true;
            }
        }

        let has_libcef = base.join("libcef.dll").is_file();
        let has_chrome_elf = base.join("chrome_elf.dll").is_file();
        let has_cef_pak = base.join("cef.pak").is_file()
            || base.join("cef_100_percent.pak").is_file()
            || base.join("cef_200_percent.pak").is_file();
        let has_icu = base.join("icudtl.dat").is_file();
        let has_v8 = base.join("snapshot_blob.bin").is_file()
            || base.join("v8_context_snapshot.bin").is_file();
        let has_respak = base.join("resources.pak").is_file();

        if has_libcef || has_cef_pak {
            found_cef = true;
        }
        if has_chrome_elf && !has_libcef && !has_cef_pak {
            found_chromium = true;
        }
        if has_icu || has_v8 || has_respak {
            found_browser = true;
        }
    }

    if found_webview2 {
        PluginEditorRuntimeKind::WebView2
    } else if found_cef {
        PluginEditorRuntimeKind::Cef
    } else if found_chromium {
        PluginEditorRuntimeKind::Chromium
    } else if found_browser {
        PluginEditorRuntimeKind::BrowserUnknown
    } else {
        PluginEditorRuntimeKind::Native
    }
}

/// Per-editor host-mode override. Mirrors
/// [`PluginEditorPresentationMode`] with an `Auto` variant meaning "let the
/// resolver pick" — used by the default quirk so we don't override the
/// existing C++ host-kind resolver (`embed_resolve_host_kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginEditorHostMode {
    /// Pick whichever mode `embed_resolve_host_kind` decides.
    Auto,
    /// Force WS_CHILD region under the GPUI HWND.
    ChildHwndEmbed,
    /// Force WS_POPUP|WS_EX_TOOLWINDOW owned by the GPUI HWND.
    OwnedToolWindowFallback,
}

impl PluginEditorHostMode {
    pub fn to_presentation(self) -> Option<PluginEditorPresentationMode> {
        match self {
            PluginEditorHostMode::Auto => None,
            PluginEditorHostMode::ChildHwndEmbed => {
                Some(PluginEditorPresentationMode::ChildHwndEmbed)
            }
            PluginEditorHostMode::OwnedToolWindowFallback => {
                Some(PluginEditorPresentationMode::OwnedToolWindowFallback)
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct PluginEditorQuirk {
    /// Diagnostic label.
    pub name: &'static str,
    /// Lowercased substring match against vendor (if known). `None` = wildcard.
    pub vendor_match: Option<&'static str>,
    /// Lowercased substring match against the plug-in binary path. `None` = wildcard.
    pub path_match: Option<&'static str>,
    /// Lowercased substring match against display name. `None` = wildcard.
    pub name_match: Option<&'static str>,
    /// Preferred presentation. `Auto` = let the resolver decide.
    pub preferred_host_mode: PluginEditorHostMode,
    /// Skip the immediate "no visible UI" failure; rely on delayed-ready polling.
    pub delayed_ready_check: bool,
    /// Force `CoInitializeEx(STA)` on the editor thread before attaching.
    /// Already enabled by default in the C++ backend; quirks can disable it
    /// if a future vendor regression demands.
    pub requires_sta_com: bool,
    /// Run extra message-pump ticks during the delayed-ready window.
    pub extra_message_pump: bool,
    /// Plug-in bundles its own WebView2 runtime. The host adds the bundle-local
    /// `WebViewLoader.dll` directory to the DLL search path during editor attach.
    pub plugin_webview_based: bool,
}

impl Default for PluginEditorQuirk {
    fn default() -> Self {
        Self {
            name: "generic",
            vendor_match: None,
            path_match: None,
            name_match: None,
            preferred_host_mode: PluginEditorHostMode::Auto,
            delayed_ready_check: true,
            requires_sta_com: true,
            extra_message_pump: false,
            plugin_webview_based: false,
        }
    }
}

/// Built-in quirks. Evaluated in order; the first match wins.
const BUILT_IN_QUIRKS: &[PluginEditorQuirk] = &[
    PluginEditorQuirk {
        name: "UAD Native (uaudio_)",
        vendor_match: None,
        path_match: Some("uaudio_"),
        name_match: None,
        preferred_host_mode: PluginEditorHostMode::Auto,
        delayed_ready_check: true,
        requires_sta_com: true,
        extra_message_pump: true,
        plugin_webview_based: true,
    },
    PluginEditorQuirk {
        name: "UAD Native (UADx)",
        vendor_match: Some("universal audio"),
        path_match: None,
        name_match: Some("uadx"),
        preferred_host_mode: PluginEditorHostMode::Auto,
        delayed_ready_check: true,
        requires_sta_com: true,
        extra_message_pump: true,
        plugin_webview_based: true,
    },
];

fn matches_substring(haystack: &str, needle: Option<&str>) -> bool {
    match needle {
        None => true,
        Some(n) => haystack.contains(n),
    }
}

/// Match a plug-in against the quirk table. `vendor` and `name` are optional —
/// scanner/registry data may not always include them. Returns the matching
/// quirk's *clone* (or the generic default).
pub fn match_quirk(
    plugin_path: &Path,
    name: Option<&str>,
    vendor: Option<&str>,
) -> PluginEditorQuirk {
    let path_lc = plugin_path.to_string_lossy().to_ascii_lowercase();
    let name_lc = name.unwrap_or("").to_ascii_lowercase();
    let vendor_lc = vendor.unwrap_or("").to_ascii_lowercase();
    for q in BUILT_IN_QUIRKS {
        if matches_substring(&path_lc, q.path_match)
            && matches_substring(&name_lc, q.name_match)
            && matches_substring(&vendor_lc, q.vendor_match)
        {
            return q.clone();
        }
    }
    PluginEditorQuirk::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn matches_uad_path() {
        let q = match_quirk(
            &PathBuf::from("C:/Program Files/Common Files/VST3/uaudio_ua_1176.vst3"),
            Some("UADx 1176"),
            Some("Universal Audio"),
        );
        assert_eq!(q.name, "UAD Native (uaudio_)");
        assert!(q.delayed_ready_check);
        assert!(q.extra_message_pump);
        assert!(q.plugin_webview_based);
    }

    #[test]
    fn generic_for_fabfilter() {
        let q = match_quirk(
            &PathBuf::from("C:/Program Files/Common Files/VST3/FabFilter Pro-Q 3.vst3"),
            Some("Pro-Q 3"),
            Some("FabFilter"),
        );
        assert_eq!(q.name, "generic");
        assert!(!q.plugin_webview_based);
    }

    #[test]
    fn runtime_kind_native_for_missing_bundle() {
        let kind = detect_plugin_editor_runtime(&PathBuf::from(
            "C:/Program Files/Common Files/VST3/__nonexistent_plugin__.vst3",
        ));
        assert_eq!(kind, PluginEditorRuntimeKind::Native);
        assert!(!kind.is_browser_based());
    }

    #[test]
    fn runtime_kind_labels_are_stable() {
        assert_eq!(PluginEditorRuntimeKind::WebView2.as_str(), "WebView2");
        assert_eq!(PluginEditorRuntimeKind::Cef.as_str(), "Cef");
        assert_eq!(PluginEditorRuntimeKind::Chromium.as_str(), "Chromium");
        assert_eq!(
            PluginEditorRuntimeKind::BrowserUnknown.as_str(),
            "BrowserUnknown"
        );
        assert_eq!(PluginEditorRuntimeKind::Native.as_str(), "Native");
    }
}
