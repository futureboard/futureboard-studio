use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeymapSource {
    Default,
    User,
    Imported,
    Plugin,
}

impl KeymapSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::User => "User",
            Self::Imported => "Imported",
            Self::Plugin => "Plugin",
        }
    }
}

/// Where a binding answers.
///
/// A `Global` binding answers everywhere; every other scope answers only
/// while its surface has the keyboard, so the same key can mean one thing in
/// the arrangement and another in the MIDI editor (`G` toggles each one's own
/// snap). A scoped key never reuses a global one: the global keys must work
/// whatever has focus, and the conflict check enforces that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum KeymapScope {
    Global,
    Arrangement,
    MidiEditor,
    AudioEditor,
    AutomationEditor,
    Mixer,
    SongTextEditor,
}

impl KeymapScope {
    pub const ALL: [KeymapScope; 7] = [
        KeymapScope::Global,
        KeymapScope::Arrangement,
        KeymapScope::MidiEditor,
        KeymapScope::AudioEditor,
        KeymapScope::AutomationEditor,
        KeymapScope::Mixer,
        KeymapScope::SongTextEditor,
    ];

    /// The key this scope has in a profile file.
    pub fn key(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Arrangement => "arrangement",
            Self::MidiEditor => "midiEditor",
            Self::AudioEditor => "audioEditor",
            Self::AutomationEditor => "automationEditor",
            Self::Mixer => "mixer",
            Self::SongTextEditor => "songTextEditor",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Global => "Global",
            Self::Arrangement => "Arrangement",
            Self::MidiEditor => "MIDI Editor",
            Self::AudioEditor => "Audio Editor",
            Self::AutomationEditor => "Automation",
            Self::Mixer => "Mixer",
            Self::SongTextEditor => "Song Text Editor",
        }
    }

    /// A scope from a file key or a label, case-insensitively. The v1
    /// `"Studio"` context — the only one v1 ever wrote — is not a scope; its
    /// bindings are placed by [`KeymapScope::home_of`].
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        Self::ALL.into_iter().find(|scope| {
            scope.key().eq_ignore_ascii_case(text) || scope.label().eq_ignore_ascii_case(text)
        })
    }

    /// The scopes a key is looked up in, most specific first.
    ///
    /// Automation editing happens on the arrangement, so it is a layer over
    /// the arrangement's keys rather than a surface of its own: the tool keys
    /// still switch back to the pointer, and only the keys automation binds
    /// (Delete, Ctrl+A, …) change meaning.
    pub fn chain(self) -> &'static [KeymapScope] {
        match self {
            Self::Global => &[Self::Global],
            Self::Arrangement => &[Self::Arrangement, Self::Global],
            Self::MidiEditor => &[Self::MidiEditor, Self::Global],
            Self::AudioEditor => &[Self::AudioEditor, Self::Global],
            Self::AutomationEditor => &[Self::AutomationEditor, Self::Arrangement, Self::Global],
            Self::Mixer => &[Self::Mixer, Self::Global],
            Self::SongTextEditor => &[Self::SongTextEditor, Self::Global],
        }
    }

    /// Whether one key bound in both scopes is a conflict. It is in the same
    /// scope, and against Global — a global key must work whatever has focus.
    /// Two editors' own keys never meet, and Automation deliberately
    /// overrides the arrangement keys it layers on.
    pub fn collides_with(self, other: KeymapScope) -> bool {
        self == other || self == Self::Global || other == Self::Global
    }

    /// Where an action lives when a binding does not say: the scope its
    /// command acts on. Used for v1 profiles, which had no scopes.
    pub fn home_of(action: &str) -> Self {
        let prefix = action.split([':', '.']).next().unwrap_or_default();
        match prefix {
            "midi" | "solfege" => Self::MidiEditor,
            "automation" => Self::AutomationEditor,
            "mixer" => Self::Mixer,
            "song_text" => Self::SongTextEditor,
            "clip" | "timeline" | "audio" | "editor" => Self::Arrangement,
            "tools" if action.starts_with("tools:select-") => Self::Arrangement,
            "edit" if !matches!(action, "edit:undo" | "edit:redo") => Self::Arrangement,
            "track" if !action.starts_with("track:add") => Self::Arrangement,
            _ => Self::Global,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeymapProfile {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub extends: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub bindings: Vec<KeyBinding>,
}

impl Default for KeymapProfile {
    fn default() -> Self {
        Self {
            name: String::new(),
            extends: None,
            version: Some("1".to_string()),
            bindings: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KeyBinding {
    pub action: String,
    #[serde(default)]
    pub keys: Vec<String>,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub args: Option<Value>,
    #[serde(default)]
    pub when: Option<String>,
}

/// One action's keys in one scope, after the profile and the user's
/// overrides are layered. Empty `keys` is an explicit "unbound here".
#[derive(Debug, Clone)]
pub struct ResolvedKeyBinding {
    pub action: String,
    pub keys: Vec<String>,
    pub scope: KeymapScope,
    pub args: Option<Value>,
    pub source: KeymapSource,
    pub profile: String,
    pub is_user_override: bool,
}

#[derive(Debug, Clone)]
pub struct KeymapRow {
    /// `scope/action` — one action can have a row in several scopes.
    pub id: String,
    pub action_id: String,
    pub action_label: String,
    pub command: String,
    pub arguments_json: Option<String>,
    pub keystrokes: Vec<String>,
    pub scope: KeymapScope,
    pub source: KeymapSource,
    pub profile: String,
    pub is_user_override: bool,
    pub is_conflict: bool,
    pub conflict_with: Vec<String>,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct KeymapConflict {
    pub keystroke: String,
    pub action: String,
    pub action_label: String,
    pub scope: KeymapScope,
    pub source: KeymapSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileDescriptor {
    pub id: &'static str,
    pub label: &'static str,
    pub builtin: bool,
}

pub const PROFILE_DESCRIPTORS: &[ProfileDescriptor] = &[
    ProfileDescriptor {
        id: "default",
        label: "Default",
        builtin: true,
    },
    ProfileDescriptor {
        id: "futureboard",
        label: "Futureboard",
        builtin: true,
    },
    ProfileDescriptor {
        id: "fl-studio",
        label: "FL Studio",
        builtin: true,
    },
    ProfileDescriptor {
        id: "ableton-live",
        label: "Ableton Live",
        builtin: true,
    },
    ProfileDescriptor {
        id: "cubase",
        label: "Cubase",
        builtin: true,
    },
    ProfileDescriptor {
        id: "pro-tools",
        label: "Pro Tools",
        builtin: true,
    },
    ProfileDescriptor {
        id: "custom",
        label: "Custom",
        builtin: false,
    },
];

pub const USER_OVERRIDES_FILE: &str = "user-overrides.json";
