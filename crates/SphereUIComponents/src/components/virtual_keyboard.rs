use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use gpui::{App, Context, WindowId};

use sphere_midi_service::VirtualKeyboardEvent;

pub type VirtualKeyboardEventSink =
    Arc<dyn Fn(VirtualKeyboardEvent, &mut App) -> bool + Send + Sync + 'static>;

const DEFAULT_OCTAVE: i8 = 4;
const DEFAULT_VELOCITY: u8 = 96;
const DEFAULT_CHANNEL: u8 = 0;
const BASE_WHITE_NOTE: i16 = 60;
const MIN_OCTAVE: i8 = 0;
const MAX_OCTAVE: i8 = 8;
const VELOCITY_STEP: u8 = 8;

/// `FUTUREBOARD_VIRTUAL_KEYBOARD_DEBUG=1` traces every musical-typing key event
/// (raw key, normalized key, mapped note, octave, velocity, ignored reason).
/// Cached on first read; never logged in the audio path.
fn vkbd_log(message: &str) {
    eprintln!("[VirtualKeyboard] {message}");
}

fn vkbd_debug() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var_os("FUTUREBOARD_VIRTUAL_KEYBOARD_DEBUG").is_some())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MusicalTypingKey {
    pub key: &'static str,
    pub semitone: i8,
    pub is_black: bool,
}

pub const WHITE_KEYS: [MusicalTypingKey; 10] = [
    MusicalTypingKey {
        key: "a",
        semitone: 0,
        is_black: false,
    },
    MusicalTypingKey {
        key: "s",
        semitone: 2,
        is_black: false,
    },
    MusicalTypingKey {
        key: "d",
        semitone: 4,
        is_black: false,
    },
    MusicalTypingKey {
        key: "f",
        semitone: 5,
        is_black: false,
    },
    MusicalTypingKey {
        key: "g",
        semitone: 7,
        is_black: false,
    },
    MusicalTypingKey {
        key: "h",
        semitone: 9,
        is_black: false,
    },
    MusicalTypingKey {
        key: "j",
        semitone: 11,
        is_black: false,
    },
    MusicalTypingKey {
        key: "k",
        semitone: 12,
        is_black: false,
    },
    MusicalTypingKey {
        key: "l",
        semitone: 14,
        is_black: false,
    },
    MusicalTypingKey {
        key: ";",
        semitone: 16,
        is_black: false,
    },
];

pub const BLACK_KEYS: [MusicalTypingKey; 7] = [
    MusicalTypingKey {
        key: "w",
        semitone: 1,
        is_black: true,
    },
    MusicalTypingKey {
        key: "e",
        semitone: 3,
        is_black: true,
    },
    MusicalTypingKey {
        key: "t",
        semitone: 6,
        is_black: true,
    },
    MusicalTypingKey {
        key: "y",
        semitone: 8,
        is_black: true,
    },
    MusicalTypingKey {
        key: "u",
        semitone: 10,
        is_black: true,
    },
    MusicalTypingKey {
        key: "o",
        semitone: 13,
        is_black: true,
    },
    MusicalTypingKey {
        key: "p",
        semitone: 15,
        is_black: true,
    },
];

/// A physical key normalized into a stable internal action *before* any note
/// math runs. Produced by [`classify_key`] so the rest of the controller never
/// matches on raw platform key strings (which vary by layout / IME / shift).
/// Unknown keys classify to `None` and are ignored, never panicked on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MusicalKey {
    /// A piano key. `semitone` is the offset above the base-octave C and is
    /// unique per physical key, so it doubles as the `pressed_keys` identity
    /// (octave-independent: the actual MIDI note is resolved at press time).
    Note {
        semitone: i8,
        is_black: bool,
    },
    OctaveDown,
    OctaveUp,
    VelocityDown,
    VelocityUp,
    Sustain,
}

/// Normalize a raw platform key string into a [`MusicalKey`].
///
/// We classify from `Keystroke::key` (the layout-stable ASCII-equivalent of the
/// physical key), not from the typed character — the typed glyph differs across
/// keyboard layouts, IME, punctuation, and shifted keys. Spelling variants for
/// the same physical key (`";"`/`"semicolon"`, `"space"`/`"Space"`/`" "`, and
/// upper-case letters from a held Shift) collapse to one canonical token first.
/// Returns `None` for any key not bound to musical typing.
fn classify_key(raw: &str) -> Option<MusicalKey> {
    let token: Cow<str> = match raw {
        ";" | "semicolon" | "Semicolon" => Cow::Borrowed(";"),
        " " | "space" | "Space" | "spacebar" | "Spacebar" => Cow::Borrowed("space"),
        other => Cow::Owned(other.to_ascii_lowercase()),
    };
    match token.as_ref() {
        "z" => Some(MusicalKey::OctaveDown),
        "x" => Some(MusicalKey::OctaveUp),
        "c" => Some(MusicalKey::VelocityDown),
        "v" => Some(MusicalKey::VelocityUp),
        "q" => Some(MusicalKey::Sustain),
        token => WHITE_KEYS
            .iter()
            .chain(BLACK_KEYS.iter())
            .find(|entry| entry.key == token)
            .map(|entry| MusicalKey::Note {
                semitone: entry.semitone,
                is_black: entry.is_black,
            }),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtualKeyboardKeyAction {
    Consumed,
    Pass,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct VirtualKeyboardOutput {
    pub events: Vec<VirtualKeyboardEvent>,
}

impl VirtualKeyboardOutput {
    fn push(&mut self, event: VirtualKeyboardEvent) {
        self.events.push(event);
    }

    pub fn has_events(&self) -> bool {
        !self.events.is_empty()
    }
}

/// Session-level virtual-keyboard / musical-typing state.
///
/// This is the single source of truth shared by every Futureboard window. It is
/// owned by the app-lifetime [`VirtualKeyboardPanel`] entity (not by any window),
/// so settings (octave/velocity/channel/sustain) survive window open/close and
/// notes can be released safely when a window goes away. Windows do not own any
/// of this state — they register/unregister and forward key events in.
#[derive(Debug, Clone)]
pub struct VirtualKeyboardService {
    pub enabled: bool,
    pub octave: i8,
    pub velocity: u8,
    pub channel: u8,
    pub sustain: bool,
    /// Physical note keys currently held, keyed by `(window, semitone)`.
    ///
    /// The semitone is unique per physical key, so the `(window, semitone)`
    /// identity makes OS auto-repeat and re-press produce exactly one NoteOn per
    /// window, lets the same key be held independently in two windows, and lets
    /// key-up / window-close release only the notes a window genuinely holds.
    /// The value is the actual MIDI note dispatched at press time.
    pressed_keys: HashMap<(WindowId, i8), u8>,
    /// Sounding notes (the union across all windows + mouse), used for rendering
    /// and to decide when the *last* holder of a note releases it.
    active_notes: BTreeSet<(u8, u8)>,
    /// Windows that have registered for musical typing. Tracked purely for
    /// lifecycle: a window is unregistered (and its notes released) on close, and
    /// stale window handles are never touched.
    registered_windows: HashSet<WindowId>,
}

impl Default for VirtualKeyboardService {
    fn default() -> Self {
        Self {
            enabled: true,
            octave: DEFAULT_OCTAVE,
            velocity: DEFAULT_VELOCITY,
            channel: DEFAULT_CHANNEL,
            sustain: false,
            pressed_keys: HashMap::new(),
            active_notes: BTreeSet::new(),
            registered_windows: HashSet::new(),
        }
    }
}

impl VirtualKeyboardService {
    /// Register a window as a musical-typing source. Idempotent.
    pub fn register_window(&mut self, window_id: WindowId) {
        if self.registered_windows.insert(window_id) {
            self.log_window("register", window_id);
        }
    }

    /// Unregister a window and release any notes it was still holding. Safe to
    /// call for a window that was never registered (returns no events).
    pub fn unregister_window(&mut self, window_id: WindowId) -> VirtualKeyboardOutput {
        self.registered_windows.remove(&window_id);
        let output = self.release_notes_for_window(window_id);
        self.log_window("unregister", window_id);
        output
    }

    /// Release every note still held by `window_id` (e.g. on window close). A
    /// note only emits NoteOff when no other window still holds it; mouse-held
    /// notes are left untouched.
    pub fn release_notes_for_window(&mut self, window_id: WindowId) -> VirtualKeyboardOutput {
        let mut output = VirtualKeyboardOutput::default();
        let removed: Vec<u8> = self
            .pressed_keys
            .iter()
            .filter(|((win, _), _)| *win == window_id)
            .map(|(_, note)| *note)
            .collect();
        if removed.is_empty() {
            return output;
        }
        self.pressed_keys.retain(|(win, _), _| *win != window_id);
        for note in removed {
            let still_held = self.pressed_keys.values().any(|&held| held == note);
            if !still_held && self.active_notes.remove(&(self.channel, note)) {
                output.push(VirtualKeyboardEvent::NoteOff {
                    note,
                    channel: self.channel,
                });
            }
        }
        if vkbd_debug() {
            eprintln!(
                "[vkbd] release-notes-for-window window={} released={} active_remaining={}",
                window_id.as_u64(),
                output.events.len(),
                self.active_notes.len(),
            );
        }
        output
    }

    fn log_window(&self, action: &str, window_id: WindowId) {
        if vkbd_debug() {
            eprintln!(
                "[vkbd] {action} window={} registered_total={}",
                window_id.as_u64(),
                self.registered_windows.len(),
            );
        }
    }

    /// Resolve a key's semitone to an in-range MIDI note for the current octave.
    /// Returns `None` (ignored, never panics) when the octave shift pushes the
    /// note outside 0..=127.
    pub fn note_for_semitone(&self, semitone: i8) -> Option<u8> {
        let note =
            BASE_WHITE_NOTE + ((self.octave as i16 - DEFAULT_OCTAVE as i16) * 12) + semitone as i16;
        (0..=127).contains(&note).then_some(note as u8)
    }

    /// Convenience wrapper: classify a raw key, then resolve its note (if it is
    /// a note key at all). Retained for callers/tests that work from raw keys.
    pub fn note_for_key(&self, key: &str) -> Option<u8> {
        match classify_key(key)? {
            MusicalKey::Note { semitone, .. } => self.note_for_semitone(semitone),
            _ => None,
        }
    }

    pub fn is_note_active(&self, note: u8) -> bool {
        self.active_notes
            .iter()
            .any(|&(_, active_note)| active_note == note)
    }

    pub fn active_note_values(&self) -> HashSet<u8> {
        self.active_notes
            .iter()
            .map(|&(_, note)| note)
            .collect::<HashSet<_>>()
    }

    pub fn active_count(&self) -> usize {
        self.active_notes.len()
    }

    /// Handle a physical key-down for musical typing.
    ///
    /// `command_modifier` is true when Ctrl/Cmd/Alt/Fn is held — such a chord is
    /// a shortcut, not a musical note, so it is passed through untouched. The
    /// method never panics: unknown/unmapped keys, text-input focus (which also
    /// covers active IME composition, since IME composes into a focused field),
    /// and out-of-range notes all return `Pass` with no events.
    ///
    /// `window_id` identifies the source window so the press can be released
    /// precisely when that key lifts or that window closes.
    pub fn handle_key_down(
        &mut self,
        window_id: WindowId,
        key: &str,
        command_modifier: bool,
        is_repeat: bool,
        text_input_focused: bool,
    ) -> (VirtualKeyboardKeyAction, VirtualKeyboardOutput) {
        let mut output = VirtualKeyboardOutput::default();
        if !self.enabled {
            self.log_ignored(key, "disabled");
            return (VirtualKeyboardKeyAction::Pass, output);
        }
        if text_input_focused {
            self.log_ignored(key, "text-input-focused");
            return (VirtualKeyboardKeyAction::Pass, output);
        }
        if command_modifier {
            self.log_ignored(key, "command-modifier");
            return (VirtualKeyboardKeyAction::Pass, output);
        }
        let Some(musical) = classify_key(key) else {
            self.log_ignored(key, "unmapped");
            return (VirtualKeyboardKeyAction::Pass, output);
        };
        match musical {
            MusicalKey::OctaveDown => {
                if !is_repeat {
                    output.events.extend(self.release_all_notes());
                    self.octave = (self.octave - 1).clamp(MIN_OCTAVE, MAX_OCTAVE);
                }
                self.log_control(key, musical);
                (VirtualKeyboardKeyAction::Consumed, output)
            }
            MusicalKey::OctaveUp => {
                if !is_repeat {
                    output.events.extend(self.release_all_notes());
                    self.octave = (self.octave + 1).clamp(MIN_OCTAVE, MAX_OCTAVE);
                }
                self.log_control(key, musical);
                (VirtualKeyboardKeyAction::Consumed, output)
            }
            MusicalKey::VelocityDown => {
                if !is_repeat {
                    self.velocity = self.velocity.saturating_sub(VELOCITY_STEP).max(1);
                }
                self.log_control(key, musical);
                (VirtualKeyboardKeyAction::Consumed, output)
            }
            MusicalKey::VelocityUp => {
                if !is_repeat {
                    self.velocity = self.velocity.saturating_add(VELOCITY_STEP).min(127);
                }
                self.log_control(key, musical);
                (VirtualKeyboardKeyAction::Consumed, output)
            }
            MusicalKey::Sustain => {
                if !is_repeat {
                    self.sustain = !self.sustain;
                    output.push(VirtualKeyboardEvent::Sustain {
                        down: self.sustain,
                        channel: self.channel,
                    });
                }
                self.log_control(key, musical);
                (VirtualKeyboardKeyAction::Consumed, output)
            }
            MusicalKey::Note { semitone, .. } => {
                let Some(note) = self.note_for_semitone(semitone) else {
                    self.log_ignored(key, "note-out-of-range");
                    return (VirtualKeyboardKeyAction::Pass, output);
                };
                // One NoteOn per physical press *per window*: drop OS auto-repeat
                // and any duplicate down event for a key this window already holds.
                if is_repeat || self.pressed_keys.contains_key(&(window_id, semitone)) {
                    return (VirtualKeyboardKeyAction::Consumed, output);
                }
                self.pressed_keys.insert((window_id, semitone), note);
                // Only the first holder of a given note sounds it; a second window
                // pressing the same note records the hold but does not retrigger.
                if self.active_notes.insert((self.channel, note)) {
                    output.push(VirtualKeyboardEvent::NoteOn {
                        note,
                        velocity: self.velocity,
                        channel: self.channel,
                    });
                    self.log_note_on(key, musical, note);
                }
                (VirtualKeyboardKeyAction::Consumed, output)
            }
        }
    }

    /// Handle a physical key-up from `window_id`. Only note keys this window
    /// genuinely pressed emit a NoteOff, and only when no other window still
    /// holds the same note; control keys, unmapped keys, and never-pressed keys
    /// are no-ops.
    pub fn handle_key_up(&mut self, window_id: WindowId, key: &str) -> VirtualKeyboardOutput {
        let mut output = VirtualKeyboardOutput::default();
        let Some(MusicalKey::Note { semitone, .. }) = classify_key(key) else {
            return output;
        };
        let Some(note) = self.pressed_keys.remove(&(window_id, semitone)) else {
            return output;
        };
        let still_held = self.pressed_keys.values().any(|&held| held == note);
        if !still_held && self.active_notes.remove(&(self.channel, note)) {
            output.push(VirtualKeyboardEvent::NoteOff {
                note,
                channel: self.channel,
            });
        }
        self.log_note_off(key, note);
        output
    }

    fn log_ignored(&self, raw: &str, reason: &str) {
        if vkbd_debug() {
            eprintln!(
                "[vkbd] key-down raw={raw:?} normalized={:?} octave={} velocity={} \
                 ignored reason={reason}",
                classify_key(raw),
                self.octave,
                self.velocity,
            );
        }
    }

    fn log_control(&self, raw: &str, musical: MusicalKey) {
        if vkbd_debug() {
            eprintln!(
                "[vkbd] key-down raw={raw:?} normalized={musical:?} octave={} velocity={} \
                 sustain={}",
                self.octave, self.velocity, self.sustain,
            );
        }
    }

    fn log_note_on(&self, raw: &str, musical: MusicalKey, note: u8) {
        if vkbd_debug() {
            eprintln!(
                "[vkbd] key-down raw={raw:?} normalized={musical:?} note={note} octave={} \
                 velocity={} channel={}",
                self.octave, self.velocity, self.channel,
            );
        }
    }

    fn log_note_off(&self, raw: &str, note: u8) {
        if vkbd_debug() {
            eprintln!(
                "[vkbd] key-up raw={raw:?} note={note} octave={} channel={}",
                self.octave, self.channel,
            );
        }
    }

    pub fn mouse_note_on(&mut self, note: u8) -> VirtualKeyboardOutput {
        let mut output = VirtualKeyboardOutput::default();
        let note = note.min(127);
        if self.active_notes.insert((self.channel, note)) {
            output.push(VirtualKeyboardEvent::NoteOn {
                note,
                velocity: self.velocity,
                channel: self.channel,
            });
        }
        output
    }

    pub fn mouse_note_off(&mut self, note: u8) -> VirtualKeyboardOutput {
        let mut output = VirtualKeyboardOutput::default();
        let note = note.min(127);
        if self.active_notes.remove(&(self.channel, note)) {
            output.push(VirtualKeyboardEvent::NoteOff {
                note,
                channel: self.channel,
            });
        }
        output
    }

    pub fn set_octave(&mut self, octave: i8) -> VirtualKeyboardOutput {
        let mut output = VirtualKeyboardOutput::default();
        output.events.extend(self.release_all_notes());
        self.octave = octave.clamp(MIN_OCTAVE, MAX_OCTAVE);
        output
    }

    pub fn set_velocity(&mut self, velocity: u8) {
        self.velocity = velocity.clamp(1, 127);
    }

    pub fn set_channel(&mut self, channel: u8) -> VirtualKeyboardOutput {
        let mut output = VirtualKeyboardOutput::default();
        output.events.extend(self.release_all_notes());
        self.channel = channel.min(15);
        output
    }

    pub fn toggle_sustain(&mut self) -> VirtualKeyboardOutput {
        self.sustain = !self.sustain;
        let mut output = VirtualKeyboardOutput::default();
        output.push(VirtualKeyboardEvent::Sustain {
            down: self.sustain,
            channel: self.channel,
        });
        output
    }

    pub fn panic(&mut self) -> VirtualKeyboardOutput {
        self.pressed_keys.clear();
        self.active_notes.clear();
        self.sustain = false;
        let mut output = VirtualKeyboardOutput::default();
        output.push(VirtualKeyboardEvent::Panic);
        output
    }

    pub fn release_all_notes(&mut self) -> Vec<VirtualKeyboardEvent> {
        let notes = self.active_notes.iter().copied().collect::<Vec<_>>();
        self.pressed_keys.clear();
        self.active_notes.clear();
        notes
            .into_iter()
            .map(|(channel, note)| VirtualKeyboardEvent::NoteOff { note, channel })
            .collect()
    }

    pub fn release_mouse_notes(&mut self) -> Vec<VirtualKeyboardEvent> {
        let held_notes = self
            .pressed_keys
            .values()
            .map(|note| (self.channel, *note))
            .collect::<HashSet<_>>();
        let notes = self
            .active_notes
            .iter()
            .copied()
            .filter(|note| !held_notes.contains(note))
            .collect::<Vec<_>>();
        for note in &notes {
            self.active_notes.remove(note);
        }
        notes
            .into_iter()
            .map(|(channel, note)| VirtualKeyboardEvent::NoteOff { note, channel })
            .collect()
    }
}

#[derive(Clone)]
pub struct VirtualKeyboardPanelState {
    pub controller: VirtualKeyboardService,
    pub visible: bool,
    pub hint: Option<String>,
    pub target_label: Option<String>,
}

impl Default for VirtualKeyboardPanelState {
    fn default() -> Self {
        Self {
            controller: VirtualKeyboardService::default(),
            visible: false,
            hint: None,
            target_label: None,
        }
    }
}

impl VirtualKeyboardPanelState {
    pub fn close(&mut self) -> VirtualKeyboardOutput {
        self.visible = false;
        let mut output = VirtualKeyboardOutput::default();
        output.events.extend(self.controller.release_all_notes());
        output
    }
}

/// The app-lifetime Virtual Keyboard: the musical-typing service and where its
/// notes go. It draws nothing itself — the modeless window
/// (`virtual_keyboard_window`) is its face — so it outlives that window, and
/// typing keeps its octave, velocity and channel across opening and closing.
pub struct VirtualKeyboardPanel {
    pub state: VirtualKeyboardPanelState,
    pub event_sink: Option<VirtualKeyboardEventSink>,
}

impl VirtualKeyboardPanel {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            state: VirtualKeyboardPanelState::default(),
            event_sink: None,
        }
    }

    /// Octave up or down a step. Held notes are released first, so none is
    /// left sounding at the old pitch.
    pub fn step_octave(&mut self, delta: i8, cx: &mut Context<Self>) {
        let next = self.state.controller.octave.saturating_add(delta);
        let output = self.state.controller.set_octave(next);
        self.flush_output(output, cx);
        cx.notify();
    }

    pub fn step_velocity(&mut self, delta: i8, cx: &mut Context<Self>) {
        let velocity = self.state.controller.velocity;
        let next = if delta < 0 {
            velocity.saturating_sub(VELOCITY_STEP)
        } else {
            velocity.saturating_add(VELOCITY_STEP)
        };
        self.state.controller.set_velocity(next);
        cx.notify();
    }

    pub fn step_channel(&mut self, delta: i8, cx: &mut Context<Self>) {
        let next = (self.state.controller.channel as i8 + delta).clamp(0, 15) as u8;
        let output = self.state.controller.set_channel(next);
        self.flush_output(output, cx);
        cx.notify();
    }

    pub fn toggle_sustain(&mut self, cx: &mut Context<Self>) {
        let output = self.state.controller.toggle_sustain();
        self.flush_output(output, cx);
        cx.notify();
    }

    pub fn mouse_note_on(&mut self, note: u8, cx: &mut Context<Self>) {
        let output = self.state.controller.mouse_note_on(note);
        self.flush_output(output, cx);
        cx.notify();
    }

    pub fn mouse_note_off(&mut self, note: u8, cx: &mut Context<Self>) {
        let output = self.state.controller.mouse_note_off(note);
        self.flush_output(output, cx);
        cx.notify();
    }

    /// End every note held with the mouse (a release anywhere in the window).
    pub fn release_mouse_notes(&mut self, cx: &mut Context<Self>) {
        let mut output = VirtualKeyboardOutput::default();
        output
            .events
            .extend(self.state.controller.release_mouse_notes());
        if output.has_events() {
            self.flush_output(output, cx);
            cx.notify();
        }
    }

    /// Release the notes typed in `window_id` only — that window lost focus
    /// and will not see their key-ups. Notes held from another window, or
    /// with the mouse, keep sounding.
    pub fn release_window_notes(&mut self, window_id: WindowId, cx: &mut Context<Self>) {
        let output = self.state.controller.release_notes_for_window(window_id);
        if output.has_events() {
            self.flush_output(output, cx);
            cx.notify();
        }
    }

    pub fn set_event_sink(&mut self, sink: Option<VirtualKeyboardEventSink>) {
        self.event_sink = sink;
    }

    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        let was_visible = self.state.visible;
        self.state.visible = visible;
        if visible && !was_visible {
            vkbd_log("open");
        } else if !visible && was_visible {
            vkbd_log("close");
            let output = self.state.close();
            self.flush_output(output, cx);
        }
        cx.notify();
    }

    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        let visible = !self.state.visible;
        self.set_visible(visible, cx);
    }

    pub fn set_target_status(&mut self, label: Option<String>, hint: Option<String>) {
        self.state.target_label = label;
        self.state.hint = hint;
    }

    pub fn release_all(&mut self, cx: &mut Context<Self>) {
        let mut output = VirtualKeyboardOutput::default();
        output
            .events
            .extend(self.state.controller.release_all_notes());
        self.flush_output(output, cx);
        cx.notify();
    }

    /// Hard reset: all-notes-off + clear pressed/active/sustain. Used on project
    /// load so no note or sustain survives into the next session. Window
    /// registrations and user settings (octave/velocity/channel) are preserved.
    pub fn panic(&mut self, cx: &mut Context<Self>) {
        let output = self.state.controller.panic();
        self.flush_output(output, cx);
        cx.notify();
    }

    /// Register a musical-typing source window with the service. Idempotent.
    pub fn register_window(&mut self, window_id: WindowId) {
        self.state.controller.register_window(window_id);
    }

    /// Unregister a window and release any notes it still held. Used on window
    /// close so a destroyed window never leaves stuck notes behind. The service
    /// (and its octave/velocity/channel/sustain settings) is left intact.
    pub fn unregister_window(&mut self, window_id: WindowId, cx: &mut Context<Self>) {
        let output = self.state.controller.unregister_window(window_id);
        let handled = output.has_events();
        self.flush_output(output, cx);
        if handled {
            cx.notify();
        }
    }

    pub fn is_musical_key(key: &str) -> bool {
        classify_key(key).is_some()
    }

    pub fn handle_key_down(
        &mut self,
        window_id: WindowId,
        key: &str,
        command_modifier: bool,
        is_repeat: bool,
        text_input_focused: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        // Physical keyboard input is captured only while the panel is visible;
        // otherwise the key falls through to the normal shortcut path. The
        // service decides per-event whether to handle or ignore.
        if !self.state.visible {
            return false;
        }
        // Track the source window so its notes can be released on close even if
        // the window never called `register_window` explicitly.
        self.state.controller.register_window(window_id);
        let (action, output) = self.state.controller.handle_key_down(
            window_id,
            key,
            command_modifier,
            is_repeat,
            text_input_focused,
        );
        self.flush_output(output, cx);
        if matches!(action, VirtualKeyboardKeyAction::Consumed) {
            vkbd_log(&format!("key handled key={key}"));
            cx.notify();
            true
        } else if classify_key(key).is_some() {
            vkbd_log(&format!(
                "key ignored reason=text-input-or-modifier key={key}"
            ));
            false
        } else {
            false
        }
    }

    pub fn handle_key_up(
        &mut self,
        window_id: WindowId,
        key: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.state.visible {
            return false;
        }
        let output = self.state.controller.handle_key_up(window_id, key);
        let is_musical = Self::is_musical_key(key);
        let handled = output.has_events() || is_musical;
        self.flush_output(output, cx);
        if handled {
            vkbd_log(&format!("key handled key={key}"));
            cx.notify();
        }
        handled
    }

    pub fn should_prevent_default_key(key: &str) -> bool {
        Self::is_musical_key(key)
    }

    fn flush_output(&self, output: VirtualKeyboardOutput, cx: &mut App) {
        if let Some(sink) = self.event_sink.as_ref() {
            for event in output.events {
                sink(event, cx);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note_on_count(output: &VirtualKeyboardOutput) -> usize {
        output
            .events
            .iter()
            .filter(|event| matches!(event, VirtualKeyboardEvent::NoteOn { .. }))
            .count()
    }

    /// A stable test window id; [`win2`] is a distinct second window used by
    /// the multi-window tests.
    fn win() -> WindowId {
        WindowId::from(1)
    }

    fn win2() -> WindowId {
        WindowId::from(2)
    }

    /// `(command_modifier, is_repeat, text_input_focused)` all false: a plain
    /// key press from the default test window with the panel enabled/focused.
    fn down(
        controller: &mut VirtualKeyboardService,
        key: &str,
    ) -> (VirtualKeyboardKeyAction, VirtualKeyboardOutput) {
        controller.handle_key_down(win(), key, false, false, false)
    }

    #[test]
    fn key_down_sends_one_note_on_only() {
        let mut controller = VirtualKeyboardService::default();
        let (_, first) = down(&mut controller, "a");
        let (_, second) = down(&mut controller, "a");
        assert_eq!(note_on_count(&first), 1);
        assert_eq!(note_on_count(&second), 0);
    }

    #[test]
    fn key_repeat_does_not_duplicate_note_on() {
        let mut controller = VirtualKeyboardService::default();
        let _ = down(&mut controller, "a");
        // OS auto-repeat (is_repeat = true) must not retrigger.
        let (_, repeat) = controller.handle_key_down(win(), "a", false, true, false);
        assert_eq!(note_on_count(&repeat), 0);
        assert_eq!(controller.active_count(), 1);
    }

    #[test]
    fn key_up_sends_note_off_for_active_key() {
        let mut controller = VirtualKeyboardService::default();
        let _ = down(&mut controller, "a");
        let output = controller.handle_key_up(win(), "a");
        assert_eq!(
            output.events,
            vec![VirtualKeyboardEvent::NoteOff {
                note: 60,
                channel: 0
            }]
        );
        assert_eq!(controller.active_count(), 0);
    }

    #[test]
    fn key_up_for_never_pressed_key_is_a_no_op() {
        let mut controller = VirtualKeyboardService::default();
        // Release without a prior press must not emit a NoteOff or panic.
        let output = controller.handle_key_up(win(), "a");
        assert!(output.events.is_empty());
        assert_eq!(controller.active_count(), 0);
    }

    #[test]
    fn blur_or_close_releases_all_active_notes() {
        let mut state = VirtualKeyboardPanelState::default();
        state.visible = true;
        let _ = state
            .controller
            .handle_key_down(win(), "a", false, false, false);
        let _ = state
            .controller
            .handle_key_down(win(), "s", false, false, false);
        let output = state.close();
        assert_eq!(output.events.len(), 2);
        assert_eq!(state.controller.active_count(), 0);
    }

    #[test]
    fn octave_change_does_not_leave_stale_notes() {
        let mut controller = VirtualKeyboardService::default();
        let _ = down(&mut controller, "a");
        let output = controller.set_octave(5);
        assert_eq!(
            output.events,
            vec![VirtualKeyboardEvent::NoteOff {
                note: 60,
                channel: 0
            }]
        );
        assert_eq!(controller.active_count(), 0);
        assert_eq!(controller.octave, 5);
    }

    #[test]
    fn octave_keys_change_octave_without_stuck_notes() {
        let mut controller = VirtualKeyboardService::default();
        // Hold A, then bump the octave down with Z: the held note is released,
        // nothing is left stuck, and a stale key-up afterwards is harmless.
        let _ = down(&mut controller, "a");
        let (action, output) = down(&mut controller, "z");
        assert_eq!(action, VirtualKeyboardKeyAction::Consumed);
        assert_eq!(
            output.events,
            vec![VirtualKeyboardEvent::NoteOff {
                note: 60,
                channel: 0
            }]
        );
        assert_eq!(controller.octave, 3);
        assert_eq!(controller.active_count(), 0);

        // X bumps the octave back up.
        let (action, _) = down(&mut controller, "x");
        assert_eq!(action, VirtualKeyboardKeyAction::Consumed);
        assert_eq!(controller.octave, 4);

        // The orphaned key-up for A (note already released) is a clean no-op.
        let trailing = controller.handle_key_up(win(), "a");
        assert!(trailing.events.is_empty());
        assert_eq!(controller.active_count(), 0);
    }

    #[test]
    fn semicolon_key_does_not_panic_and_maps_to_a_note() {
        let mut controller = VirtualKeyboardService::default();
        // Both the glyph and the platform name for the key must classify.
        for key in [";", "semicolon"] {
            let mut c = controller.clone();
            let (action, output) = down(&mut c, key);
            assert_eq!(action, VirtualKeyboardKeyAction::Consumed);
            assert_eq!(note_on_count(&output), 1);
        }
        // Sanity: the shared controller is untouched by the clones above.
        assert_eq!(controller.active_count(), 0);
        let _ = down(&mut controller, ";");
        assert_eq!(controller.active_count(), 1);
    }

    #[test]
    fn q_toggles_sustain_without_panic() {
        let mut controller = VirtualKeyboardService::default();
        let (action, output) = down(&mut controller, "q");
        assert_eq!(action, VirtualKeyboardKeyAction::Consumed);
        assert!(controller.sustain);
        assert_eq!(
            output.events,
            vec![VirtualKeyboardEvent::Sustain {
                down: true,
                channel: 0
            }]
        );
        // Auto-repeat must not flip sustain back off.
        let (_, repeat_out) = controller.handle_key_down(win(), "q", false, true, false);
        assert!(controller.sustain);
        assert!(repeat_out.events.is_empty());
        // A fresh press toggles it off again.
        let (_, second) = down(&mut controller, "q");
        assert!(!controller.sustain);
        assert_eq!(
            second.events,
            vec![VirtualKeyboardEvent::Sustain {
                down: false,
                channel: 0
            }]
        );
    }

    #[test]
    fn space_is_not_sustain() {
        let mut controller = VirtualKeyboardService::default();
        let (action, output) = controller.handle_key_down(win(), "space", false, false, false);
        assert_eq!(action, VirtualKeyboardKeyAction::Pass);
        assert!(!controller.sustain);
        assert!(output.events.is_empty());
    }

    #[test]
    fn unmapped_key_is_ignored_without_panic() {
        let mut controller = VirtualKeyboardService::default();
        for key in ["1", "enter", "f1", "arrow_left", "`", "/", ""] {
            let (action, output) = down(&mut controller, key);
            assert_eq!(action, VirtualKeyboardKeyAction::Pass, "key {key:?}");
            assert!(output.events.is_empty(), "key {key:?}");
            // A key-up for the same unmapped key is equally harmless.
            assert!(controller.handle_key_up(win(), key).events.is_empty());
        }
        assert_eq!(controller.active_count(), 0);
    }

    #[test]
    fn command_modifier_passes_through_for_shortcuts() {
        let mut controller = VirtualKeyboardService::default();
        // Ctrl+A (command_modifier = true) is a shortcut, not a note.
        let (action, output) = controller.handle_key_down(win(), "a", true, false, false);
        assert_eq!(action, VirtualKeyboardKeyAction::Pass);
        assert!(output.events.is_empty());
        assert_eq!(controller.active_count(), 0);
    }

    #[test]
    fn text_input_focus_prevents_musical_typing_capture() {
        let mut controller = VirtualKeyboardService::default();
        let (action, output) = controller.handle_key_down(win(), "a", false, false, true);
        assert_eq!(action, VirtualKeyboardKeyAction::Pass);
        assert!(output.events.is_empty());
        assert_eq!(controller.active_count(), 0);
    }

    #[test]
    fn no_selected_or_armed_target_can_show_hint_without_panic() {
        let mut state = VirtualKeyboardPanelState::default();
        state.target_label = None;
        state.hint = Some("Select or arm an instrument track to play.".to_string());
        assert!(state.hint.is_some());
        assert_eq!(state.controller.active_count(), 0);
    }

    #[test]
    fn keys_from_two_windows_are_tracked_independently() {
        let mut service = VirtualKeyboardService::default();
        // Same physical key held in two different windows: one sounding note,
        // and each window can re-press without retriggering its own hold.
        let (_, w1) = service.handle_key_down(win(), "a", false, false, false);
        let (_, w2) = service.handle_key_down(win2(), "a", false, false, false);
        assert_eq!(note_on_count(&w1), 1);
        assert_eq!(
            note_on_count(&w2),
            0,
            "second window must not retrigger note"
        );
        assert_eq!(service.active_count(), 1);

        // Releasing in window 1 keeps the note alive (window 2 still holds it).
        let up1 = service.handle_key_up(win(), "a");
        assert!(up1.events.is_empty(), "note still held by the other window");
        assert_eq!(service.active_count(), 1);

        // Releasing the last holder emits the NoteOff.
        let up2 = service.handle_key_up(win2(), "a");
        assert_eq!(
            up2.events,
            vec![VirtualKeyboardEvent::NoteOff {
                note: 60,
                channel: 0
            }]
        );
        assert_eq!(service.active_count(), 0);
    }

    #[test]
    fn closing_a_window_releases_only_its_notes_without_crash() {
        let mut service = VirtualKeyboardService::default();
        service.register_window(win());
        service.register_window(win2());
        // Window 1 holds A (60), window 2 holds S (62).
        let _ = service.handle_key_down(win(), "a", false, false, false);
        let _ = service.handle_key_down(win2(), "s", false, false, false);
        assert_eq!(service.active_count(), 2);

        // Closing window 2 releases only S; A stays held by window 1.
        let released = service.unregister_window(win2());
        assert_eq!(
            released.events,
            vec![VirtualKeyboardEvent::NoteOff {
                note: 62,
                channel: 0
            }]
        );
        assert_eq!(service.active_count(), 1);
        assert!(service.is_note_active(60));

        // Unregistering a window that holds nothing (or was never registered) is
        // a clean no-op — never panics, emits nothing.
        assert!(service.unregister_window(win2()).events.is_empty());
        assert!(service
            .release_notes_for_window(WindowId::from(99))
            .events
            .is_empty());
    }

    #[test]
    fn shared_note_survives_one_window_close() {
        let mut service = VirtualKeyboardService::default();
        // Both windows hold the same note; closing one must not silence it.
        let _ = service.handle_key_down(win(), "a", false, false, false);
        let _ = service.handle_key_down(win2(), "a", false, false, false);
        let released = service.unregister_window(win2());
        assert!(
            released.events.is_empty(),
            "note still held by the surviving window"
        );
        assert!(service.is_note_active(60));
        assert_eq!(service.active_count(), 1);
    }
}
