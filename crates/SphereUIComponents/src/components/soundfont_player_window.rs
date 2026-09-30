//! Floating "Soundfont Player" utility window.
//!
//! Hosts the built-in Soundfont Player instrument of one track (see
//! `TrackState::builtin_soundfont_player`), in either of its modes: one
//! instrument, or sixteen parts on the MIDI channels (see
//! [`crate::soundfont_player::SoundfontPlayerMode`]).
//!
//! The track is the authority. The window observes the timeline and draws the
//! track's settings as they are — an undo, or a MIDI track routed to this
//! instrument in the meantime, shows up here without reopening the window —
//! and every edit goes back through [`SoundfontPlayerTrackUpdate`], the one
//! path that records undo and reaches the engine.
//!
//! The window reads the `.sf2` itself only for its metadata (bank name and
//! presets), off the UI thread. It never plays: the keyboard and the Test
//! button send MIDI preview through [`SoundfontPlayerPreview`] to the engine,
//! the same path the piano roll uses, so what you hear here is what the track
//! plays back.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, AppContext, Bounds, Context, Entity, FocusHandle, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Render, Styled, Subscription, UniformListScrollHandle, Window,
    WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, div, px, size,
};

use crate::components::soundfont_player_mdi::{
    CHANNELS, SOUNDFONT_MULTI_TITLE, SOUNDFONT_PLAYER_MDI_TITLE, SoundfontPlayerCallbacks,
    SoundfontPlayerPanelState, browser_rows, default_parts, soundfont_player_panel,
};
use crate::components::text_input::{
    TextInputState, bind_mouse_selection, text_field_with_callbacks_and_ime,
};
use crate::components::timeline::Timeline;
use crate::components::timeline::timeline_state::{
    MidiOutputChannelMode, SoundfontChannel, SoundfontPlayerMode, SoundfontPlayerSettingsState,
    TimelineState, TrackType,
};
use crate::components::title_bar::external_window_titlebar;
use crate::soundfont_player::{
    SoundfontEnvelope, SoundfontPlayer, SoundfontPlayerError, SoundfontPlayerSettings,
    SoundfontPresetInfo, SoundfontRenderQuality,
};
use crate::theme::Colors;

pub const SOUNDFONT_PLAYER_WINDOW_WIDTH: f32 = 920.0;
/// Tall enough to open with the preset card, the envelope and the output all
/// visible above the keyboard. Shorter than this the instrument column
/// scrolls; the header, browser and keyboard stay put.
pub const SOUNDFONT_PLAYER_WINDOW_HEIGHT: f32 = 700.0;
/// The browser plus the narrowest the channel rack's controls fit in.
pub const SOUNDFONT_PLAYER_WINDOW_MIN_WIDTH: f32 = 760.0;
pub const SOUNDFONT_PLAYER_WINDOW_MIN_HEIGHT: f32 = 500.0;

const PREVIEW_VELOCITY: u8 = 100;
/// Notes the Test button auditions: a C major triad, low enough to be clear on
/// a bass or pad preset and high enough not to disappear on a lead.
const TEST_CHORD: [u8; 3] = [60, 64, 67];
/// How long the Test button holds its chord. Long enough to judge a slow
/// attack or a pad, short enough that the button is not a mode the user has to
/// exit — and Stop is there for the impatient.
const TEST_CHORD_HOLD: Duration = Duration::from_millis(2_200);

/// One MIDI preview gesture from this window, addressed to the track that owns
/// the built-in player.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoundfontPlayerPreview {
    NoteOn {
        channel: u8,
        pitch: u8,
        velocity: u8,
    },
    NoteOff {
        channel: u8,
        pitch: u8,
    },
    AllNotesOff,
}

#[derive(Debug, Clone)]
pub struct SoundfontPlayerTrackUpdate {
    pub track_id: String,
    pub settings: SoundfontPlayerSettingsState,
}

/// A track's Soundfont Player settings: the shape the timeline stores and the
/// window publishes back, so the panel cannot drift out of step with what a
/// track actually holds.
pub type SoundfontPlayerTrackState = SoundfontPlayerSettingsState;

/// What the window shows of one track: its settings, and which tracks play
/// which of its channels.
#[derive(Debug, Clone, PartialEq)]
pub struct SoundfontTrackView {
    pub settings: SoundfontPlayerSettingsState,
    pub channel_sources: [Vec<String>; CHANNELS],
    pub per_note_sources: Vec<String>,
}

/// The view of `track_id`'s player in `state`, or `None` when the track is
/// gone. A MIDI track counts as a source when its output is this instrument;
/// the instrument track's own clips count too. Each is listed under the
/// channel its notes go out on, or as per-note when every note keeps its own.
pub fn soundfont_track_view(state: &TimelineState, track_id: &str) -> Option<SoundfontTrackView> {
    let track = state.find_track(track_id)?;
    let settings = SoundfontPlayerSettingsState {
        path: track.soundfont_path.clone(),
        preset: track.soundfont_preset,
        volume: track.soundfont_volume,
        reverb_chorus: track.soundfont_reverb_chorus,
        polyphony: track.soundfont_polyphony,
        envelope: track.soundfont_envelope,
        quality: track.soundfont_quality,
        mode: track.soundfont_mode,
        channels: track.soundfont_channels,
    };
    let mut channel_sources: [Vec<String>; CHANNELS] = Default::default();
    let mut per_note_sources = Vec::new();
    for source in &state.tracks {
        let plays_here = source.id == track_id
            || (source.track_type == TrackType::Midi
                && state.effective_instrument_track_id(&source.id).as_deref() == Some(track_id));
        if !plays_here {
            continue;
        }
        match source.routing.output_channel_mode() {
            MidiOutputChannelMode::Fixed(channel) => {
                channel_sources[channel.raw() as usize % CHANNELS].push(source.name.clone())
            }
            MidiOutputChannelMode::PerNote => per_note_sources.push(source.name.clone()),
        }
    }
    Some(SoundfontTrackView {
        settings,
        channel_sources,
        per_note_sources,
    })
}

type PreviewCb = Arc<dyn Fn(&str, SoundfontPlayerPreview, &mut App) + Send + Sync>;
type UpdateCb = Arc<dyn Fn(SoundfontPlayerTrackUpdate, &mut App) + Send + Sync>;

/// A font's metadata, read off the UI thread.
struct FontInfo {
    bank_name: String,
    presets: Vec<SoundfontPresetInfo>,
}

fn read_font_info(path: &std::path::Path) -> Result<FontInfo, SoundfontPlayerError> {
    let player = SoundfontPlayer::from_path(path, SoundfontPlayerSettings::default())?;
    Ok(FontInfo {
        bank_name: player.bank_name().to_string(),
        presets: player.list_presets(),
    })
}

pub struct SoundfontPlayerWindow {
    track_id: String,
    timeline: Entity<Timeline>,
    _timeline_sync: Subscription,
    on_close: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>,
    on_update_track: UpdateCb,
    on_preview: PreviewCb,
    focus_handle: FocusHandle,
    focused_once: bool,
    search: TextInputState,
    browser_scroll: UniformListScrollHandle,
    /// The font whose metadata the panel shows (or is reading).
    loaded_path: Option<String>,
    /// Bumped per metadata read, so a slow read for a font the track has since
    /// left cannot land over the current one.
    load_generation: u64,
    panel: SoundfontPlayerPanelState,
    /// Bumped whenever an audition starts or is cancelled, so a hold timer that
    /// belongs to an earlier press cannot release the notes of a later one.
    test_generation: u64,
}

impl SoundfontPlayerWindow {
    pub fn new(
        track_id: String,
        timeline: Entity<Timeline>,
        on_close: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>,
        on_update_track: UpdateCb,
        on_preview: PreviewCb,
        cx: &mut Context<Self>,
    ) -> Self {
        let sync = cx.observe(&timeline, |this, _timeline, cx| this.sync_from_track(cx));
        let mut window = Self {
            track_id,
            timeline,
            _timeline_sync: sync,
            on_close,
            on_update_track,
            on_preview,
            focus_handle: cx.focus_handle(),
            focused_once: false,
            search: TextInputState::new("soundfont-preset-search", cx.focus_handle())
                .with_placeholder("Search presets"),
            browser_scroll: UniformListScrollHandle::new(),
            loaded_path: None,
            load_generation: 0,
            panel: SoundfontPlayerPanelState::default(),
            test_generation: 0,
        };
        window.sync_from_track(cx);
        window
    }

    /// Redraws from the track: settings, channel sources, and — when the
    /// track's font changed — its metadata.
    fn sync_from_track(&mut self, cx: &mut Context<Self>) {
        let Some(view) = soundfont_track_view(&self.timeline.read(cx).state, &self.track_id) else {
            return;
        };
        let path = view.settings.path.clone().filter(|path| !path.is_empty());
        let changed = self.panel.settings != view.settings
            || self.panel.channel_sources != view.channel_sources
            || self.panel.per_note_sources != view.per_note_sources;
        self.panel.settings = view.settings;
        self.panel.channel_sources = view.channel_sources;
        self.panel.per_note_sources = view.per_note_sources;
        if path != self.loaded_path {
            self.load_font_info(path, cx);
            cx.notify();
        } else if changed {
            cx.notify();
        }
    }

    /// Reads `path`'s bank name and presets. Parsing a General MIDI bank takes
    /// long enough to stall a frame, so it runs off the UI thread with the
    /// panel showing its loading state until it lands.
    fn load_font_info(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        self.load_generation = self.load_generation.wrapping_add(1);
        self.loaded_path = path.clone();
        self.panel.status = None;
        let Some(path) = path else {
            self.panel.loading = false;
            self.panel.file_name = None;
            self.panel.bank_name = None;
            self.panel.presets.clear();
            return;
        };
        self.panel.loading = true;
        let generation = self.load_generation;
        let entity = cx.entity().clone();
        cx.spawn(async move |_this, cx| {
            let file = PathBuf::from(&path);
            let result = cx
                .background_spawn(async move { read_font_info(&file) })
                .await;
            let _ = entity.update(cx, |this, cx| {
                if this.load_generation != generation {
                    return;
                }
                this.apply_font_info(&path, result);
                cx.notify();
            });
        })
        .detach();
    }

    fn apply_font_info(&mut self, path: &str, result: Result<FontInfo, SoundfontPlayerError>) {
        self.panel.loading = false;
        match result {
            Ok(info) => {
                self.panel.file_name = std::path::Path::new(path)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned());
                self.panel.bank_name = Some(info.bank_name);
                self.panel.presets = info.presets;
                self.panel.status = None;
            }
            Err(error) => {
                self.panel.file_name = None;
                self.panel.bank_name = None;
                self.panel.presets.clear();
                self.panel.status = Some(format!("Could not read the SoundFont: {error}"));
            }
        }
    }

    /// Hands `settings` to the track. The panel shows them at once; the
    /// track's own echo arrives through [`Self::sync_from_track`].
    fn publish(&mut self, settings: SoundfontPlayerSettingsState, cx: &mut Context<Self>) {
        let settings = settings.sanitized();
        self.panel.settings = settings.clone();
        (self.on_update_track)(
            SoundfontPlayerTrackUpdate {
                track_id: self.track_id.clone(),
                settings,
            },
            cx,
        );
        cx.notify();
    }

    fn has_preset(&self, (bank, patch): (i32, i32)) -> bool {
        self.panel
            .presets
            .iter()
            .any(|preset| preset.bank == bank && preset.patch == patch)
    }

    fn browse_soundfont(&mut self, cx: &mut Context<Self>) {
        #[cfg(feature = "native-dialogs")]
        {
            self.panel.status = None;
            let entity = cx.entity().clone();
            cx.spawn(async move |_this, cx| {
                let Some(handle) = rfd::AsyncFileDialog::new()
                    .set_title("Load SoundFont")
                    .add_filter("SoundFont", &["sf2"])
                    .pick_file()
                    .await
                else {
                    return;
                };
                let path = handle.path().to_path_buf();
                let _ = entity.update(cx, |this, cx| {
                    this.panel.loading = true;
                    cx.notify();
                });
                let result = cx
                    .background_spawn({
                        let path = path.clone();
                        async move { read_font_info(&path) }
                    })
                    .await;
                let _ = entity.update(cx, |this, cx| {
                    this.adopt_font(path.to_string_lossy().into_owned(), result, cx);
                });
            })
            .detach();
        }
        #[cfg(not(feature = "native-dialogs"))]
        {
            self.panel.status = Some("Native file dialogs are unavailable in this build.".into());
            cx.notify();
        }
    }

    /// Puts a freshly read font on the track: the player keeps a preset the
    /// new font has, and otherwise starts on its first; the parts start the
    /// way a General MIDI module powers up.
    fn adopt_font(
        &mut self,
        path: String,
        result: Result<FontInfo, SoundfontPlayerError>,
        cx: &mut Context<Self>,
    ) {
        self.load_generation = self.load_generation.wrapping_add(1);
        self.loaded_path = Some(path.clone());
        self.apply_font_info(&path, result);
        if self.panel.file_name.is_none() {
            cx.notify();
            return;
        }
        let mut settings = self.panel.settings.clone();
        settings.path = Some(path);
        if !settings
            .preset
            .is_some_and(|preset| self.has_preset(preset))
        {
            settings.preset = self
                .panel
                .presets
                .first()
                .map(|preset| (preset.bank, preset.patch));
        }
        settings.channels = default_parts(&self.panel.presets, settings.channels);
        self.publish(settings, cx);
    }

    fn set_mode(&mut self, mode: SoundfontPlayerMode, cx: &mut Context<Self>) {
        if self.panel.settings.mode == mode {
            return;
        }
        self.release_all(cx);
        let mut settings = self.panel.settings.clone();
        settings.mode = mode;
        if mode == SoundfontPlayerMode::Multi {
            // The single instrument becomes part 1, so switching does not
            // change what the track's channel-1 notes play.
            if settings.channels[0].preset.is_none() {
                settings.channels[0].preset = settings.preset;
            }
            settings.channels = default_parts(&self.panel.presets, settings.channels);
        }
        self.publish(settings, cx);
    }

    fn select_preset(&mut self, preset: (i32, i32), cx: &mut Context<Self>) {
        if !self.has_preset(preset) {
            return;
        }
        let mut settings = self.panel.settings.clone();
        if settings.mode == SoundfontPlayerMode::Multi {
            settings.channels[self.panel.selected_channel as usize % CHANNELS].preset =
                Some(preset);
        } else {
            settings.preset = Some(preset);
        }
        self.publish(settings, cx);
    }

    fn select_channel(&mut self, channel: u8, cx: &mut Context<Self>) {
        let channel = channel.min(CHANNELS as u8 - 1);
        if self.panel.selected_channel != channel {
            self.release_all(cx);
            self.panel.selected_channel = channel;
            cx.notify();
        }
    }

    fn set_channel(&mut self, channel: u8, part: SoundfontChannel, cx: &mut Context<Self>) {
        let mut settings = self.panel.settings.clone();
        settings.channels[channel as usize % CHANNELS] = part;
        self.publish(settings, cx);
    }

    fn preview(&self, event: SoundfontPlayerPreview, app: &mut App) {
        (self.on_preview)(&self.track_id, event, app);
    }

    /// Presses one panel key. Held until [`Self::note_off`] so a sustained
    /// preset actually sustains, matching the piano roll's key behavior.
    fn note_on(&mut self, pitch: u8, cx: &mut Context<Self>) {
        if !self.panel.is_playable() || self.panel.active_notes.contains(&pitch) {
            return;
        }
        self.panel.active_notes.push(pitch);
        self.preview(
            SoundfontPlayerPreview::NoteOn {
                channel: self.panel.preview_channel(),
                pitch,
                velocity: PREVIEW_VELOCITY,
            },
            cx,
        );
        cx.notify();
    }

    fn note_off(&mut self, pitch: u8, cx: &mut Context<Self>) {
        if !self.panel.active_notes.contains(&pitch) {
            return;
        }
        self.panel.active_notes.retain(|held| *held != pitch);
        if self.panel.active_notes.is_empty() {
            self.panel.testing = false;
        }
        self.preview(
            SoundfontPlayerPreview::NoteOff {
                channel: self.panel.preview_channel(),
                pitch,
            },
            cx,
        );
        cx.notify();
    }

    /// Auditions the current preset through the engine and releases the chord
    /// after [`TEST_CHORD_HOLD`].
    fn start_test(&mut self, cx: &mut Context<Self>) {
        if !self.panel.is_playable() {
            return;
        }
        self.release_all(cx);
        self.test_generation = self.test_generation.wrapping_add(1);
        let generation = self.test_generation;
        let channel = self.panel.preview_channel();
        self.panel.testing = true;
        for pitch in TEST_CHORD {
            self.panel.active_notes.push(pitch);
            self.preview(
                SoundfontPlayerPreview::NoteOn {
                    channel,
                    pitch,
                    velocity: PREVIEW_VELOCITY,
                },
                cx,
            );
        }
        let entity = cx.entity().clone();
        cx.spawn(async move |_this, cx| {
            cx.background_executor().timer(TEST_CHORD_HOLD).await;
            let _ = entity.update(cx, |this, cx| {
                if this.test_generation != generation {
                    return;
                }
                this.release_all(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Releases every note this panel is holding. Also invalidates any pending
    /// audition timer.
    fn release_all(&mut self, app: &mut App) {
        self.test_generation = self.test_generation.wrapping_add(1);
        self.panel.testing = false;
        if self.panel.active_notes.is_empty() {
            return;
        }
        self.panel.active_notes.clear();
        self.preview(SoundfontPlayerPreview::AllNotesOff, app);
    }

    fn shift_octave(&mut self, delta: i32, cx: &mut Context<Self>) {
        self.release_all(cx);
        self.panel.shift_keyboard_octave(delta);
        cx.notify();
    }

    /// Retargets an already-open window at `track_id`. Focusing the OS window
    /// is the caller's job.
    pub fn focus_soundfont_player(&mut self, track_id: String, cx: &mut Context<Self>) {
        if self.track_id != track_id {
            self.release_all(cx);
            self.track_id = track_id;
            self.panel.selected_channel = 0;
            self.loaded_path = None;
        }
        self.sync_from_track(cx);
    }

    fn handle_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search.is_focused(window) {
            return;
        }
        if event.keystroke.key == "escape" {
            self.search.set_value("");
        } else {
            let _ = self.search.handle_key_ime(event, Some(cx));
        }
        self.browser_scroll
            .scroll_to_item(0, gpui::ScrollStrategy::Top);
        cx.notify();
        cx.stop_propagation();
    }

    fn callbacks(&self, cx: &mut Context<Self>) -> SoundfontPlayerCallbacks {
        let entity = cx.entity().clone();
        fn with<T: Clone + 'static>(
            entity: &Entity<SoundfontPlayerWindow>,
            f: impl Fn(&mut SoundfontPlayerWindow, T, &mut Context<SoundfontPlayerWindow>) + 'static,
        ) -> Arc<dyn Fn(&T, &mut Window, &mut App) + 'static> {
            let entity = entity.clone();
            Arc::new(move |value: &T, _window, app: &mut App| {
                let value = value.clone();
                let _ = entity.update(app, |this, cx| f(this, value, cx));
            })
        }
        fn void(
            entity: &Entity<SoundfontPlayerWindow>,
            f: impl Fn(&mut SoundfontPlayerWindow, &mut Context<SoundfontPlayerWindow>) + 'static,
        ) -> Arc<dyn Fn(&mut Window, &mut App) + 'static> {
            let entity = entity.clone();
            Arc::new(move |_window, app: &mut App| {
                let _ = entity.update(app, |this, cx| f(this, cx));
            })
        }
        SoundfontPlayerCallbacks {
            on_browse: void(&entity, |this, cx| this.browse_soundfont(cx)),
            on_set_mode: with(&entity, |this, mode: SoundfontPlayerMode, cx| {
                this.set_mode(mode, cx)
            }),
            on_select_preset: with(&entity, |this, preset: (i32, i32), cx| {
                this.select_preset(preset, cx)
            }),
            on_select_channel: with(&entity, |this, channel: u8, cx| {
                this.select_channel(channel, cx)
            }),
            on_set_channel: with(
                &entity,
                |this, (channel, part): (u8, SoundfontChannel), cx| {
                    this.set_channel(channel, part, cx)
                },
            ),
            on_set_volume: with(&entity, |this, value: f32, cx| {
                let mut settings = this.panel.settings.clone();
                settings.volume = value.clamp(0.0, 1.0);
                this.publish(settings, cx);
            }),
            on_toggle_reverb_chorus: void(&entity, |this, cx| {
                let mut settings = this.panel.settings.clone();
                settings.reverb_chorus = !settings.reverb_chorus;
                this.publish(settings, cx);
            }),
            on_set_polyphony: with(&entity, |this, value: usize, cx| {
                let mut settings = this.panel.settings.clone();
                settings.polyphony = value;
                this.publish(settings, cx);
            }),
            on_set_envelope: with(&entity, |this, envelope: SoundfontEnvelope, cx| {
                let mut settings = this.panel.settings.clone();
                settings.envelope = envelope;
                this.publish(settings, cx);
            }),
            on_set_quality: with(&entity, |this, quality: SoundfontRenderQuality, cx| {
                if this.panel.settings.quality != quality {
                    let mut settings = this.panel.settings.clone();
                    settings.quality = quality;
                    this.publish(settings, cx);
                }
            }),
            on_note_on: with(&entity, |this, pitch: u8, cx| this.note_on(pitch, cx)),
            on_note_off: with(&entity, |this, pitch: u8, cx| this.note_off(pitch, cx)),
            on_test: void(&entity, |this, cx| this.start_test(cx)),
            on_all_notes_off: void(&entity, |this, cx| {
                this.release_all(cx);
                cx.notify();
            }),
            on_shift_octave: with(&entity, |this, delta: i32, cx| this.shift_octave(delta, cx)),
        }
    }
}

// Route platform IME (CJK/Thai composition + candidate-window positioning) to
// the preset search field.
crate::impl_single_input_window_ime!(SoundfontPlayerWindow, search);

impl Render for SoundfontPlayerWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focused_once {
            self.focused_once = true;
            self.focus_handle.focus(window, cx);
        }
        let on_close = self.on_close.clone();
        let entity = cx.entity().clone();
        let callbacks = self.callbacks(cx);
        let search_focused = self.search.is_focused(window);
        let search = text_field_with_callbacks_and_ime(
            &self.search,
            search_focused,
            bind_mouse_selection(entity.clone(), |this| &mut this.search),
            entity.clone(),
        )
        .into_any_element();
        let rows = browser_rows(&self.panel.presets, &self.search.value);
        let title = if self.panel.is_multi() {
            SOUNDFONT_MULTI_TITLE
        } else {
            SOUNDFONT_PLAYER_MDI_TITLE
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .font(crate::theme::ui_font())
            .bg(Colors::surface_window())
            .overflow_hidden()
            .capture_key_down({
                let entity = entity.clone();
                move |event, window, cx| {
                    let _ = entity.update(cx, |this, cx| this.handle_key(event, window, cx));
                }
            })
            .child(div().w(px(0.0)).h(px(0.0)).track_focus(&self.focus_handle))
            .child(external_window_titlebar(
                title,
                "soundfont-player-window-close",
                {
                    let entity = entity.clone();
                    move |window, cx| {
                        // Closing must not leave an auditioned note held on the
                        // engine — nothing would ever send its note-off.
                        let _ = entity.update(cx, |this, cx| this.release_all(cx));
                        on_close(window, cx);
                        window.remove_window();
                    }
                },
            ))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .relative()
                    .child(soundfont_player_panel(
                        &self.panel,
                        callbacks,
                        search,
                        rows,
                        self.browser_scroll.clone(),
                    )),
            )
    }
}

pub fn open_soundfont_player_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    track_id: String,
    timeline: Entity<Timeline>,
    on_close: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>,
    on_update_track: UpdateCb,
    on_preview: PreviewCb,
    cx: &mut App,
) -> Result<WindowHandle<SoundfontPlayerWindow>, String> {
    let window_bounds = crate::window_position::centered_window_bounds(
        owner_bounds,
        size(
            px(SOUNDFONT_PLAYER_WINDOW_WIDTH),
            px(SOUNDFONT_PLAYER_WINDOW_HEIGHT),
        ),
        cx,
    );
    let mut options = crate::platform_chrome::external_dialog_window_options_partial();
    options.window_bounds = Some(WindowBounds::Windowed(window_bounds));
    options.kind = WindowKind::Floating;
    options.is_resizable = true;
    options.is_minimizable = true;
    options.window_background = WindowBackgroundAppearance::Opaque;
    options.window_min_size = Some(size(
        px(SOUNDFONT_PLAYER_WINDOW_MIN_WIDTH),
        px(SOUNDFONT_PLAYER_WINDOW_MIN_HEIGHT),
    ));
    crate::window_position::apply_owner_display(&mut options, owner_bounds, cx);

    cx.open_window(options, move |_window, cx| {
        cx.new(|cx| {
            SoundfontPlayerWindow::new(
                track_id,
                timeline,
                on_close,
                on_update_track,
                on_preview,
                cx,
            )
        })
    })
    .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::timeline::timeline_state::{
        CreateTrackOptions, InputMonitorMode, TrackOutputRouting,
    };

    fn add(state: &mut TimelineState, track_type: TrackType, name: &str) -> String {
        state.create_track(CreateTrackOptions {
            track_type,
            name: name.to_string(),
            color: gpui::Rgba {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0,
            },
            volume: 1.0,
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        })
    }

    #[test]
    fn the_view_lists_each_routed_track_under_the_channel_it_plays() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let gm = add(&mut state, TrackType::Instrument, "GM");
        let bass = add(&mut state, TrackType::Midi, "Bass");
        let drums = add(&mut state, TrackType::Midi, "Drums");
        let loose = add(&mut state, TrackType::Midi, "Elsewhere");
        let chords = add(&mut state, TrackType::Midi, "Chords");
        for id in [&bass, &drums, &chords] {
            state.set_track_output_routing(
                id,
                TrackOutputRouting::Instrument {
                    track_id: gm.clone(),
                },
            );
        }
        let _ = loose;
        let set_channel = |state: &mut TimelineState, id: &str, channel: u8| {
            let track = state.tracks.iter_mut().find(|t| t.id == id).unwrap();
            track.routing.midi_channel = Some(channel);
        };
        set_channel(&mut state, &bass, 2);
        set_channel(&mut state, &drums, 10);
        state
            .tracks
            .iter_mut()
            .find(|t| t.id == chords)
            .unwrap()
            .routing
            .midi_output_per_note = true;

        let view = soundfont_track_view(&state, &gm).expect("view");
        assert_eq!(
            view.channel_sources[0],
            ["GM"],
            "the instrument's own clips"
        );
        assert_eq!(view.channel_sources[1], ["Bass"]);
        assert_eq!(view.channel_sources[9], ["Drums"]);
        assert_eq!(view.per_note_sources, ["Chords"]);
        assert!(view.channel_sources[4].is_empty());
        assert!(soundfont_track_view(&state, "gone").is_none());
    }
}
