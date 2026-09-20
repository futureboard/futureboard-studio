use gpui::{KeyDownEvent, Keystroke};

/// Canonicalize an authored accelerator ("Ctrl+Shift+S") into a stable token.
pub fn canonical_accel(accel: &str) -> Option<String> {
    let mut ctrl = false;
    let mut shift = false;
    let mut alt = false;
    let mut base: Option<String> = None;
    for raw in accel.split('+') {
        let part = raw.trim().to_ascii_lowercase();
        match part.as_str() {
            "" => {}
            "ctrl" | "control" | "cmd" | "command" | "meta" | "super" => ctrl = true,
            "shift" => shift = true,
            "alt" | "option" | "opt" => alt = true,
            other => base = Some(canonical_key(other)),
        }
    }
    Some(join_token(ctrl, shift, alt, base?))
}

pub fn canonical_event(event: &KeyDownEvent) -> Option<String> {
    canonical_keystroke(&event.keystroke)
}

/// True when the keystroke is a bare modifier press. Those arrive as their own
/// key events on some platforms and must never terminate a chord recording.
pub fn is_modifier_only_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "control"
            | "ctrl"
            | "shift"
            | "alt"
            | "option"
            | "opt"
            | "platform"
            | "cmd"
            | "command"
            | "super"
            | "meta"
            | "win"
            | "function"
            | "fn"
            | "capslock"
    )
}

pub fn canonical_keystroke(keystroke: &Keystroke) -> Option<String> {
    let m = &keystroke.modifiers;
    let ctrl = m.control || m.platform;
    let shift = m.shift;
    let alt = m.alt;
    let key = keystroke.key.to_ascii_lowercase();
    if key.is_empty() || is_modifier_only_key(&key) {
        return None;
    }
    let base = canonical_key(&key);
    if base.is_empty() {
        return None;
    }
    Some(join_token(ctrl, shift, alt, base))
}

/// True when accelerators should render with macOS symbols (⌘⌥⇧⌃, glyph keys,
/// no `+` separator). Honors `FUTUREBOARD_ACCEL_STYLE=mac|pc` so tests and the
/// preferences preview can force a style; otherwise follows the host OS.
pub fn use_mac_accel_style() -> bool {
    match std::env::var("FUTUREBOARD_ACCEL_STYLE").ok().as_deref() {
        Some("mac") | Some("macos") => true,
        Some("pc") | Some("win") | Some("windows") | Some("linux") => false,
        _ => cfg!(target_os = "macos"),
    }
}

/// Display an accelerator token for the human. Platform-aware:
///
/// * macOS  — Apple symbols joined with no separator: `⌘⇧S`, `⌥⌘K`, `⌘⌫`.
///   The canonical token folds Ctrl and Cmd together (see [`canonical_keystroke`]),
///   so the primary modifier renders as `⌘` — the convention every macOS app
///   (and cross-platform editors) uses for a `Ctrl+`-authored binding.
/// * others — words joined with `+`: `Ctrl+Shift+S`.
///
/// The token modifier order is `alt` → `ctrl` → `shift` (from [`join_token`]);
/// this presents them in each platform's conventional order.
pub fn format_accel_display(token: &str) -> String {
    format_accel_display_with(token, use_mac_accel_style())
}

/// Style-explicit core of [`format_accel_display`]. Kept separate so tests and
/// the preferences preview can render either style deterministically without
/// touching process-global env state.
pub fn format_accel_display_with(token: &str, mac: bool) -> String {
    if mac {
        format_accel_display_mac(token)
    } else {
        format_accel_display_pc(token)
    }
}

/// PC/Linux style: `Ctrl+Alt+Shift+Key`.
fn format_accel_display_pc(token: &str) -> String {
    let lower = token.to_ascii_lowercase();
    let mut rest = lower.as_str();
    let mut parts = Vec::new();
    if rest.starts_with("alt+") {
        parts.push("Alt".to_string());
        rest = &rest[4..];
    }
    if rest.starts_with("ctrl+") {
        parts.push("Ctrl".to_string());
        rest = &rest[5..];
    }
    if rest.starts_with("shift+") {
        parts.push("Shift".to_string());
        rest = &rest[6..];
    }
    parts.push(format_key_display(rest, false));
    parts.join("+")
}

/// macOS style: Apple modifier symbols in Apple order (⌃⌥⇧⌘), no separator,
/// key rendered as a glyph where one exists.
fn format_accel_display_mac(token: &str) -> String {
    let lower = token.to_ascii_lowercase();
    let mut rest = lower.as_str();
    let mut alt = false;
    let mut cmd = false;
    let mut shift = false;
    if rest.starts_with("alt+") {
        alt = true;
        rest = &rest[4..];
    }
    if rest.starts_with("ctrl+") {
        // The primary modifier — Cmd on macOS.
        cmd = true;
        rest = &rest[5..];
    }
    if rest.starts_with("shift+") {
        shift = true;
        rest = &rest[6..];
    }
    // Apple's canonical order: Control, Option, Shift, Command, then the key.
    let mut out = String::new();
    if alt {
        out.push('⌥');
    }
    if shift {
        out.push('⇧');
    }
    if cmd {
        out.push('⌘');
    }
    out.push_str(&format_key_display(rest, true));
    out
}

/// Render the base (non-modifier) key. `mac` selects glyphs (↩ ⌫ ⎋ …) where
/// macOS convention uses them; the PC style spells the key out.
fn format_key_display(rest: &str, mac: bool) -> String {
    let named = match rest {
        "space" => Some(if mac { "␣" } else { "Space" }),
        "escape" => Some(if mac { "⎋" } else { "Esc" }),
        "enter" => Some(if mac { "↩" } else { "Enter" }),
        "delete" => Some(if mac { "⌦" } else { "Delete" }),
        "backspace" => Some(if mac { "⌫" } else { "Backspace" }),
        "tab" => Some(if mac { "⇥" } else { "Tab" }),
        "home" => Some(if mac { "↖" } else { "Home" }),
        "end" => Some(if mac { "↘" } else { "End" }),
        "pageup" => Some(if mac { "⇞" } else { "PageUp" }),
        "pagedown" => Some(if mac { "⇟" } else { "PageDown" }),
        "left" => Some(if mac { "←" } else { "Left" }),
        "right" => Some(if mac { "→" } else { "Right" }),
        "up" => Some(if mac { "↑" } else { "Up" }),
        "down" => Some(if mac { "↓" } else { "Down" }),
        _ => None,
    };
    if let Some(named) = named {
        return named.to_string();
    }
    // Function keys: display upper-case (F1..F24), not the canonical "f9".
    if rest.len() >= 2 && rest.starts_with('f') && rest[1..].chars().all(|c| c.is_ascii_digit()) {
        return rest.to_ascii_uppercase();
    }
    if rest.chars().count() == 1 {
        return rest.to_ascii_uppercase();
    }
    rest.to_string()
}

pub fn event_to_accel_string(event: &KeyDownEvent) -> Option<String> {
    canonical_event(event).map(|token| format_accel_display(&token))
}

pub fn keystroke_to_accel_string(keystroke: &Keystroke) -> Option<String> {
    canonical_keystroke(keystroke).map(|token| format_accel_display(&token))
}

/// Convenience for UI call sites that hold an authored accelerator string
/// (e.g. `"Ctrl+1"`) and want the platform-aware display form (`⌘1` on macOS,
/// `Ctrl+1` elsewhere). Falls back to the input unchanged if it can't parse.
pub fn accel_display(accel: &str) -> String {
    canonical_accel(accel)
        .map(|token| format_accel_display(&token))
        .unwrap_or_else(|| accel.to_string())
}

fn join_token(ctrl: bool, shift: bool, alt: bool, base: String) -> String {
    let mut out = String::new();
    if alt {
        out.push_str("alt+");
    }
    if ctrl {
        out.push_str("ctrl+");
    }
    if shift {
        out.push_str("shift+");
    }
    out.push_str(&base);
    out
}

pub fn canonical_key(key: &str) -> String {
    match key {
        "space" | "spacebar" => "space",
        "escape" | "esc" => "escape",
        "enter" | "return" | "numpad_enter" => "enter",
        "delete" | "del" => "delete",
        "backspace" => "backspace",
        "tab" => "tab",
        "home" => "home",
        "end" => "end",
        "pageup" | "page_up" | "pgup" => "pageup",
        "pagedown" | "page_down" | "pgdn" => "pagedown",
        "arrowleft" | "arrow_left" | "left" => "left",
        "arrowright" | "arrow_right" | "right" => "right",
        "arrowup" | "arrow_up" | "up" => "up",
        "arrowdown" | "arrow_down" | "down" => "down",
        "plus" => "=",
        "minus" => "-",
        other => other,
    }
    .to_string()
}

pub fn global_priority(command: &str) -> u8 {
    if command.starts_with("midi:") || command.starts_with("automation:") {
        2
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_accel_normalizes_modifiers() {
        assert_eq!(
            canonical_accel("Ctrl+Shift+S"),
            Some("ctrl+shift+s".to_string())
        );
        assert_eq!(
            canonical_accel("Shift+Space"),
            Some("shift+space".to_string())
        );
    }

    #[test]
    fn format_accel_display_round_trips() {
        let token = canonical_accel("Ctrl+Shift+S").unwrap();
        assert_eq!(format_accel_display_with(&token, false), "Ctrl+Shift+S");
    }

    #[test]
    fn pc_style_spells_out_modifiers_and_named_keys() {
        // Tokens always arrive in canonical alt→ctrl→shift order (join_token).
        assert_eq!(
            format_accel_display_with("alt+ctrl+shift+s", false),
            "Alt+Ctrl+Shift+S"
        );
        assert_eq!(
            format_accel_display_with("ctrl+backspace", false),
            "Ctrl+Backspace"
        );
        assert_eq!(
            format_accel_display_with("shift+enter", false),
            "Shift+Enter"
        );
        assert_eq!(format_accel_display_with("ctrl+7", false), "Ctrl+7");
        assert_eq!(format_accel_display_with("f9", false), "F9");
    }

    #[test]
    fn mac_style_uses_apple_symbols_without_separator() {
        // Primary modifier (canonical `ctrl`) renders as Command.
        assert_eq!(format_accel_display_with("ctrl+s", true), "⌘S");
        // Apple order: Control, Option, Shift, Command — Command sits last,
        // right before the key.
        assert_eq!(format_accel_display_with("ctrl+shift+s", true), "⇧⌘S");
        assert_eq!(format_accel_display_with("alt+ctrl+shift+s", true), "⌥⇧⌘S");
        // Glyph keys.
        assert_eq!(format_accel_display_with("ctrl+backspace", true), "⌘⌫");
        assert_eq!(format_accel_display_with("shift+enter", true), "⇧↩");
        assert_eq!(format_accel_display_with("escape", true), "⎋");
        assert_eq!(format_accel_display_with("alt+left", true), "⌥←");
        assert_eq!(format_accel_display_with("f9", true), "F9");
    }
}
