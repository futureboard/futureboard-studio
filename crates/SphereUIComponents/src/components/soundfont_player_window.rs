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
//! A MIDI track's bank/program selection sets the preset of the part on its
//! channel ([`soundfont_program_parts`]): the window shows it, the engine
//! plays it through the part (so a kit on any channel loads as a kit), and
//! picking a preset for such a part in the browser writes the MIDI track's
//! program instead, so the two never disagree.
//!
//! In sixteen-channel mode each part can be fed from here: the `+` on a
//! channel creates a MIDI track that plays it, or puts an existing MIDI track
//! on it ([`SoundfontMidiRoute`]); the edit goes through the same undo and
//! engine path as the Inspector's MIDI Out.
//!
//! The window reads the `.sf2` itself only for its metadata (bank name and
//! presets), off the UI thread. It never plays: the keyboard and the Test
//! button send MIDI preview through [`SoundfontPlayerPreview`] to the engine,
//! the same path the piano roll uses, so what you hear here is what the track
//! plays back.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, size, App, AppContext, Bounds, Context, Entity, FocusHandle, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, Render, Styled, Subscription,
    UniformListScrollHandle, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle,
    WindowKind,
};

use crate::components::context_menu::{context_menu_overlay, ContextMenuEntry};
use crate::components::soundfont_player_mdi::{
    browser_rows, default_parts, soundfont_player_panel, SoundfontPlayerCallbacks,
    SoundfontPlayerPanelState, CHANNELS, SOUNDFONT_MULTI_TITLE, SOUNDFONT_PLAYER_MDI_TITLE,
};
use crate::components::text_input::{
    bind_mouse_selection, text_field_with_callbacks_and_ime, TextInputState,
};
use crate::components::timeline::timeline_state::{
    CreateTrackOptions, InputMonitorMode, MidiOutputChannelMode, SoundfontChannel,
    SoundfontChannels, SoundfontPlayerMode, SoundfontPlayerSettingsState, TimelineState,
    TrackOutputRouting, TrackState, TrackType,
};
use crate::components::timeline::Timeline;
use crate::components::title_bar::external_window_titlebar;
use crate::soundfont_player::{
    SoundfontEnvelope, SoundfontPlayer, SoundfontPlayerError, SoundfontPlayerSettings,
    SoundfontPresetInfo, SoundfontRenderQuality,
};
use crate::theme::Colors;
use sphere_midi_service::program::{MidiPatchFormat, MidiProgramSelection, XG_DRUM_BANK_MSB};

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
    /// Every MIDI track of the project, for putting one on a channel.
    pub midi_tracks: Vec<SoundfontMidiTrackChoice>,
    /// Per channel, the track whose program sets that part's preset.
    pub program_parts: [Option<SoundfontProgramPart>; CHANNELS],
}

/// SoundFont bank of the drum kits.
const DRUM_BANK: i32 = 128;

/// A part whose preset a track's bank/program selection sets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundfontProgramPart {
    pub track_id: String,
    pub track_name: String,
    pub preset: (i32, i32),
}

/// The SoundFont preset `selection` picks on `channel` (0-based) of a
/// Soundfont Multi, or `None` when it picks no program. The synthesizer takes
/// the bank from Bank Select MSB alone, so the MSB is the SoundFont bank; a
/// drum selection (channel 10 in GM and GS, the drum banks in XG) is the kit
/// of that number in the drum bank.
pub fn soundfont_preset_for_program(
    selection: MidiProgramSelection,
    channel: u8,
) -> Option<(i32, i32)> {
    let selection = selection.sanitized();
    let program = i32::from(selection.program?);
    if selection.is_drum_bank(channel.min(15) + 1) {
        return Some((DRUM_BANK, program));
    }
    let bank = selection.bank().map_or(0, |(msb, _)| i32::from(msb));
    Some((bank, program))
}

/// The selection that picks `preset` on `channel` (0-based): the inverse of
/// [`soundfont_preset_for_program`], keeping `current`'s format when it can
/// say the same thing.
pub fn program_for_soundfont_preset(
    current: MidiProgramSelection,
    channel: u8,
    (bank, patch): (i32, i32),
) -> MidiProgramSelection {
    let program = Some(patch.clamp(0, 127) as u8);
    let drum_channel = channel == 9;
    if bank >= DRUM_BANK {
        // GM and GS put the kits on channel 10; anywhere else only XG's drum
        // bank says "kit".
        if drum_channel && current.format != MidiPatchFormat::Xg {
            return MidiProgramSelection {
                format: current.format,
                program,
                bank_msb: 0,
                bank_lsb: 0,
            };
        }
        return MidiProgramSelection {
            format: MidiPatchFormat::Xg,
            program,
            bank_msb: XG_DRUM_BANK_MSB,
            bank_lsb: 0,
        };
    }
    let bank = bank.clamp(0, 125) as u8;
    let format = if drum_channel {
        // A melodic sound on channel 10: XG chooses drums by bank, not by
        // channel.
        MidiPatchFormat::Xg
    } else {
        match current.format {
            MidiPatchFormat::Gm if bank == 0 => MidiPatchFormat::Gm,
            MidiPatchFormat::Gm => MidiPatchFormat::Gs,
            format => format,
        }
    };
    MidiProgramSelection {
        format,
        program,
        bank_msb: bank,
        bank_lsb: 0,
    }
}

/// Per channel of `instrument_id`'s Soundfont Multi, the track whose program
/// selection sets that part: a MIDI track playing it (or the instrument track
/// itself) on that channel. The last such track in track order wins, as it
/// would for the channel of a hardware module.
pub fn soundfont_program_parts(
    state: &TimelineState,
    instrument_id: &str,
) -> [Option<SoundfontProgramPart>; CHANNELS] {
    let mut parts: [Option<SoundfontProgramPart>; CHANNELS] = Default::default();
    let Some(instrument) = state.find_track(instrument_id) else {
        return parts;
    };
    if !instrument.builtin_soundfont_player
        || instrument.soundfont_mode != SoundfontPlayerMode::Multi
    {
        return parts;
    }
    for track in &state.tracks {
        if state.effective_instrument_track_id(&track.id).as_deref() != Some(instrument_id) {
            continue;
        }
        // The channel its selection goes out on, as for any instrument.
        let channel = track.routing.midi_channel.unwrap_or(1).clamp(1, 16) - 1;
        if let Some(preset) = soundfont_preset_for_program(track.routing.program, channel) {
            parts[channel as usize] = Some(SoundfontProgramPart {
                track_id: track.id.clone(),
                track_name: track.name.clone(),
                preset,
            });
        }
    }
    parts
}

/// `track`'s parts as they play: a part a program selection sets takes that
/// preset. Unchanged for a track that is no Soundfont Multi.
pub fn effective_soundfont_channels(
    state: &TimelineState,
    track: &TrackState,
) -> SoundfontChannels {
    let mut channels = track.soundfont_channels;
    for (part, program) in channels
        .iter_mut()
        .zip(soundfont_program_parts(state, &track.id))
    {
        if let Some(program) = program {
            part.preset = Some(program.preset);
        }
    }
    channels
}

/// A MIDI track as the channel menu offers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundfontMidiTrackChoice {
    pub id: String,
    pub name: String,
    /// It plays this instrument now.
    pub plays_here: bool,
    /// Its fixed output channel, 0-based; `None` when each note keeps its own.
    pub channel: Option<u8>,
}

impl SoundfontMidiTrackChoice {
    fn plays_channel(&self, channel: u8) -> bool {
        self.plays_here && self.channel == Some(channel)
    }
}

/// A change to which MIDI tracks play an instrument, asked for by the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoundfontMidiRoute {
    /// New MIDI tracks, each playing one channel (0-based) under its name.
    NewTracks(Vec<(u8, String)>),
    /// An existing MIDI track plays `channel` (0-based) of the instrument.
    Assign { track_id: String, channel: u8 },
    /// The MIDI track stops playing the instrument.
    Disconnect { track_id: String },
    /// The track whose program sets part `channel` (0-based) selects `preset`.
    Program {
        track_id: String,
        channel: u8,
        preset: (i32, i32),
    },
}

impl SoundfontMidiRoute {
    /// Existing tracks the change edits. Tracks it creates are found by the
    /// undo record itself.
    pub fn edited_tracks(&self) -> Vec<String> {
        match self {
            Self::NewTracks(_) => Vec::new(),
            Self::Assign { track_id, .. }
            | Self::Disconnect { track_id }
            | Self::Program { track_id, .. } => vec![track_id.clone()],
        }
    }

    pub fn history_label(&self) -> &'static str {
        match self {
            Self::NewTracks(parts) if parts.len() > 1 => "Add MIDI Tracks",
            Self::NewTracks(_) => "Add MIDI Track",
            Self::Assign { .. } => "Route MIDI Track",
            Self::Disconnect { .. } => "Disconnect MIDI Track",
            Self::Program { .. } => "Set Program",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SoundfontMidiRouteRequest {
    pub instrument_track_id: String,
    pub route: SoundfontMidiRoute,
}

/// Applies `route` to the instrument track `instrument_id`: the MIDI track
/// goes out to it (MIDI Out → Instrument) on a fixed channel. New tracks take
/// the instrument's colour and sit below the last track already playing it.
/// Returns whether anything changed.
pub fn apply_soundfont_midi_route(
    state: &mut TimelineState,
    instrument_id: &str,
    route: &SoundfontMidiRoute,
) -> bool {
    let Some(instrument) = state.find_track(instrument_id) else {
        return false;
    };
    if instrument.track_type != TrackType::Instrument {
        return false;
    }
    let color = instrument.color;
    let route_to = |state: &mut TimelineState, id: &str, channel: u8| {
        let mut changed = state.set_track_output_routing(
            id,
            TrackOutputRouting::Instrument {
                track_id: instrument_id.to_string(),
            },
        );
        changed |= state.set_track_midi_channel(id, Some(channel.min(15) + 1));
        changed |= state.set_track_midi_output_per_note(id, false);
        changed
    };
    match route {
        SoundfontMidiRoute::NewTracks(parts) => {
            for (channel, name) in parts {
                let insert_at = state
                    .tracks
                    .iter()
                    .rposition(|t| {
                        t.id == instrument_id
                            || state.effective_instrument_track_id(&t.id).as_deref()
                                == Some(instrument_id)
                    })
                    .map_or(state.tracks.len(), |last| last + 1);
                let id = state.create_track(CreateTrackOptions {
                    track_type: TrackType::Midi,
                    name: name.clone(),
                    color,
                    volume: crate::components::timeline::timeline_state::volume::db_to_norm(0.0),
                    pan: 0.0,
                    armed: false,
                    input_monitor: InputMonitorMode::Off,
                });
                route_to(state, &id, *channel);
                let index = state.tracks.len() - 1;
                if insert_at < index {
                    state.move_track_to(index, insert_at);
                }
            }
            !parts.is_empty()
        }
        SoundfontMidiRoute::Assign { track_id, channel } => {
            if state
                .find_track(track_id)
                .is_none_or(|t| t.track_type != TrackType::Midi)
            {
                return false;
            }
            route_to(state, track_id, *channel)
        }
        SoundfontMidiRoute::Disconnect { track_id } => {
            let plays_here = state.find_track(track_id).is_some_and(|t| {
                matches!(&t.routing.output, TrackOutputRouting::Instrument { track_id } if track_id == instrument_id)
            });
            plays_here && state.set_track_output_routing(track_id, TrackOutputRouting::None)
        }
        SoundfontMidiRoute::Program {
            track_id,
            channel,
            preset,
        } => {
            let Some(current) = state.find_track(track_id).map(|t| t.routing.program) else {
                return false;
            };
            let next = program_for_soundfont_preset(current, *channel, *preset);
            state.set_track_program_selection(track_id, next)
        }
    }
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
        // As they play: the parts programs set show those presets.
        channels: effective_soundfont_channels(state, track),
    };
    let mut channel_sources: [Vec<String>; CHANNELS] = Default::default();
    let mut per_note_sources = Vec::new();
    let midi_tracks = state
        .tracks
        .iter()
        .filter(|t| t.track_type == TrackType::Midi)
        .map(|t| SoundfontMidiTrackChoice {
            id: t.id.clone(),
            name: t.name.clone(),
            plays_here: state.effective_instrument_track_id(&t.id).as_deref() == Some(track_id),
            channel: match t.routing.output_channel_mode() {
                MidiOutputChannelMode::Fixed(channel) => Some(channel.raw()),
                MidiOutputChannelMode::PerNote => None,
            },
        })
        .collect();
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
        midi_tracks,
        program_parts: soundfont_program_parts(state, track_id),
    })
}

type PreviewCb = Arc<dyn Fn(&str, SoundfontPlayerPreview, &mut App) + Send + Sync>;
type UpdateCb = Arc<dyn Fn(SoundfontPlayerTrackUpdate, &mut App) + Send + Sync>;
type RouteCb = Arc<dyn Fn(SoundfontMidiRouteRequest, &mut App) + Send + Sync>;

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
    on_route_midi: RouteCb,
    /// The MIDI tracks of the project, for the channel menu.
    midi_tracks: Vec<SoundfontMidiTrackChoice>,
    /// Per channel, the track whose program sets the part.
    program_parts: [Option<SoundfontProgramPart>; CHANNELS],
    /// The open channel menu: channel (0-based) and where it was asked for.
    menu: Option<(u8, f32, f32)>,
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
        on_route_midi: RouteCb,
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
            on_route_midi,
            midi_tracks: Vec::new(),
            program_parts: Default::default(),
            menu: None,
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
            || self.panel.per_note_sources != view.per_note_sources
            || self.midi_tracks != view.midi_tracks
            || self.program_parts != view.program_parts;
        self.panel.settings = view.settings;
        self.panel.channel_sources = view.channel_sources;
        self.panel.per_note_sources = view.per_note_sources;
        self.midi_tracks = view.midi_tracks;
        self.panel.program_sources =
            std::array::from_fn(|i| view.program_parts[i].as_ref().map(|p| p.track_name.clone()));
        self.program_parts = view.program_parts;
        self.panel.parts_without_track = self.parts_without_track().len();
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
            let channel = self.panel.selected_channel % CHANNELS as u8;
            // A part a program sets follows that program: change the
            // program, so the track and the part keep agreeing.
            if let Some(owner) = &self.program_parts[channel as usize] {
                let route = SoundfontMidiRoute::Program {
                    track_id: owner.track_id.clone(),
                    channel,
                    preset,
                };
                self.panel.settings.channels[channel as usize].preset = Some(preset);
                self.route_midi(route, cx);
                return;
            }
            settings.channels[channel as usize].preset = Some(preset);
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

    /// The name a new MIDI track for `channel` gets: its part's preset.
    fn part_track_name(&self, channel: u8) -> String {
        let part = self.panel.settings.channels[channel as usize % CHANNELS];
        match self.panel.preset_name(part.preset) {
            Some(name) => format!("Ch {} · {name}", channel + 1),
            None => format!("MIDI Ch {}", channel + 1),
        }
    }

    /// Channels no MIDI track plays yet, muted parts aside.
    fn parts_without_track(&self) -> Vec<u8> {
        (0..CHANNELS as u8)
            .filter(|&channel| {
                !self.panel.settings.channels[channel as usize].mute
                    && !self.midi_tracks.iter().any(|t| t.plays_channel(channel))
            })
            .collect()
    }

    fn route_midi(&mut self, route: SoundfontMidiRoute, cx: &mut Context<Self>) {
        self.menu = None;
        (self.on_route_midi)(
            SoundfontMidiRouteRequest {
                instrument_track_id: self.track_id.clone(),
                route,
            },
            cx,
        );
        cx.notify();
    }

    fn add_part_tracks(&mut self, cx: &mut Context<Self>) {
        let parts: Vec<(u8, String)> = self
            .parts_without_track()
            .into_iter()
            .map(|channel| (channel, self.part_track_name(channel)))
            .collect();
        if !parts.is_empty() {
            self.route_midi(SoundfontMidiRoute::NewTracks(parts), cx);
        }
    }

    fn open_channel_menu(&mut self, channel: u8, x: f32, y: f32, cx: &mut Context<Self>) {
        self.select_channel(channel, cx);
        self.menu = Some((channel.min(CHANNELS as u8 - 1), x, y));
        cx.notify();
    }

    fn channel_menu_entries(&self, channel: u8) -> Vec<ContextMenuEntry> {
        let mut entries = vec![
            ContextMenuEntry::Header(format!("Channel {}", channel + 1)),
            ContextMenuEntry::item("New MIDI Track", "new"),
        ];
        if !self.midi_tracks.is_empty() {
            entries.push(ContextMenuEntry::Separator);
            entries.push(ContextMenuEntry::Header("Play from MIDI track".into()));
            for track in &self.midi_tracks {
                let label = match (track.plays_here, track.channel) {
                    (true, Some(other)) if other != channel => {
                        format!("{}  (Ch {})", track.name, other + 1)
                    }
                    (true, None) => format!("{}  (per note)", track.name),
                    _ => track.name.clone(),
                };
                entries.push(ContextMenuEntry::checked_item(
                    label,
                    format!("assign:{}", track.id),
                    track.plays_channel(channel),
                ));
            }
        }
        let here: Vec<&SoundfontMidiTrackChoice> = self
            .midi_tracks
            .iter()
            .filter(|t| t.plays_channel(channel))
            .collect();
        if !here.is_empty() {
            entries.push(ContextMenuEntry::Separator);
            for track in here {
                entries.push(ContextMenuEntry::item(
                    format!("Disconnect {}", track.name),
                    format!("disconnect:{}", track.id),
                ));
            }
        }
        entries
    }

    fn run_menu_command(&mut self, command: &str, cx: &mut Context<Self>) {
        let Some((channel, _, _)) = self.menu.take() else {
            return;
        };
        let route = if command == "new" {
            Some(SoundfontMidiRoute::NewTracks(vec![(
                channel,
                self.part_track_name(channel),
            )]))
        } else if let Some(id) = command.strip_prefix("assign:") {
            Some(SoundfontMidiRoute::Assign {
                track_id: id.to_string(),
                channel,
            })
        } else {
            command
                .strip_prefix("disconnect:")
                .map(|id| SoundfontMidiRoute::Disconnect {
                    track_id: id.to_string(),
                })
        };
        match route {
            Some(route) => self.route_midi(route, cx),
            None => cx.notify(),
        }
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
            on_channel_menu: with(&entity, |this, (channel, x, y): (u8, f32, f32), cx| {
                this.open_channel_menu(channel, x, y, cx)
            }),
            on_add_part_tracks: void(&entity, |this, cx| this.add_part_tracks(cx)),
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
        let menu = self.menu.map(|(channel, x, y)| {
            let viewport = window.viewport_size();
            let command_target = entity.clone();
            let close_target = entity.clone();
            context_menu_overlay(
                self.channel_menu_entries(channel),
                x,
                y,
                viewport.width.into(),
                viewport.height.into(),
                Arc::new(move |command: &String, _window, cx| {
                    let command = command.clone();
                    let _ =
                        command_target.update(cx, |this, cx| this.run_menu_command(&command, cx));
                }),
                Arc::new(move |_: &(), _window, cx| {
                    let _ = close_target.update(cx, |this, cx| {
                        this.menu = None;
                        cx.notify();
                    });
                }),
            )
        });

        div()
            .relative()
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
            .when_some(menu, |root, menu| root.child(menu))
    }
}

pub fn open_soundfont_player_window(
    owner_bounds: Option<Bounds<gpui::Pixels>>,
    track_id: String,
    timeline: Entity<Timeline>,
    on_close: Arc<dyn Fn(&mut Window, &mut App) + Send + Sync>,
    on_update_track: UpdateCb,
    on_preview: PreviewCb,
    on_route_midi: RouteCb,
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
                on_route_midi,
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
        let bass_choice = view.midi_tracks.iter().find(|t| t.id == bass).unwrap();
        assert!(bass_choice.plays_here && bass_choice.channel == Some(1));
        let loose_choice = view
            .midi_tracks
            .iter()
            .find(|t| t.name == "Elsewhere")
            .unwrap();
        assert!(!loose_choice.plays_here);
    }

    #[test]
    fn a_new_midi_track_plays_its_part_and_sits_below_the_instrument() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let gm = add(&mut state, TrackType::Instrument, "GM");
        let other = add(&mut state, TrackType::Audio, "Vocals");
        assert!(apply_soundfont_midi_route(
            &mut state,
            &gm,
            &SoundfontMidiRoute::NewTracks(vec![
                (2, "Ch 3 · Strings".into()),
                (9, "Ch 10 · Kit".into())
            ]),
        ));
        let names: Vec<&str> = state.tracks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["GM", "Ch 3 · Strings", "Ch 10 · Kit", "Vocals"]);
        let strings = &state.tracks[1];
        assert_eq!(strings.track_type, TrackType::Midi);
        assert_eq!(strings.routing.midi_channel, Some(3));
        assert_eq!(
            state.effective_instrument_track_id(&strings.id).as_deref(),
            Some(gm.as_str())
        );
        let view = soundfont_track_view(&state, &gm).unwrap();
        assert_eq!(view.channel_sources[2], ["Ch 3 · Strings"]);
        assert_eq!(view.channel_sources[9], ["Ch 10 · Kit"]);
        let _ = other;
    }

    #[test]
    fn an_existing_midi_track_moves_to_a_channel_and_can_be_disconnected() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let gm = add(&mut state, TrackType::Instrument, "GM");
        let lead = add(&mut state, TrackType::Midi, "Lead");
        state.set_track_midi_output_per_note(&lead, true);
        let route = SoundfontMidiRoute::Assign {
            track_id: lead.clone(),
            channel: 4,
        };
        assert!(apply_soundfont_midi_route(&mut state, &gm, &route));
        assert!(
            !apply_soundfont_midi_route(&mut state, &gm, &route),
            "already there"
        );
        let view = soundfont_track_view(&state, &gm).unwrap();
        assert_eq!(
            view.channel_sources[4],
            ["Lead"],
            "a fixed channel, not per note"
        );
        assert_eq!(route.edited_tracks(), [lead.clone()]);

        let disconnect = SoundfontMidiRoute::Disconnect {
            track_id: lead.clone(),
        };
        assert!(apply_soundfont_midi_route(&mut state, &gm, &disconnect));
        assert!(state.effective_instrument_track_id(&lead).is_none());
        assert!(!apply_soundfont_midi_route(&mut state, &gm, &disconnect));
    }

    fn multi(state: &mut TimelineState, name: &str) -> String {
        let id = add(state, TrackType::Instrument, name);
        let track = state.tracks.iter_mut().find(|t| t.id == id).unwrap();
        track.builtin_soundfont_player = true;
        track.soundfont_mode = SoundfontPlayerMode::Multi;
        id
    }

    fn selection(format: MidiPatchFormat, msb: u8, program: u8) -> MidiProgramSelection {
        MidiProgramSelection {
            format,
            program: Some(program),
            bank_msb: msb,
            bank_lsb: 0,
        }
    }

    #[test]
    fn a_program_selection_maps_to_its_soundfont_preset() {
        use MidiPatchFormat::{Gm, Gs, Xg};
        assert_eq!(
            soundfont_preset_for_program(selection(Gm, 5, 40), 0),
            Some((0, 40)),
            "GM has no banks"
        );
        assert_eq!(
            soundfont_preset_for_program(selection(Gs, 8, 4), 2),
            Some((8, 4))
        );
        assert_eq!(
            soundfont_preset_for_program(selection(Gm, 0, 25), 9),
            Some((128, 25)),
            "channel 10 kit"
        );
        assert_eq!(
            soundfont_preset_for_program(selection(Xg, 127, 0), 3),
            Some((128, 0)),
            "XG kit anywhere"
        );
        assert_eq!(
            soundfont_preset_for_program(selection(Xg, 0, 0), 9),
            Some((0, 0)),
            "XG melodic on 10"
        );
        assert_eq!(
            soundfont_preset_for_program(MidiProgramSelection::default(), 0),
            None
        );
    }

    #[test]
    fn a_preset_maps_back_to_a_selection_that_picks_it() {
        use MidiPatchFormat::{Gm, Gs, Xg};
        for current in [
            MidiProgramSelection::default(),
            selection(Gs, 3, 1),
            selection(Xg, 0, 9),
        ] {
            for channel in [0u8, 4, 9] {
                for preset in [(0, 0), (0, 73), (8, 4), (128, 0), (128, 25)] {
                    let next = program_for_soundfont_preset(current, channel, preset);
                    assert_eq!(
                        soundfont_preset_for_program(next, channel),
                        Some(preset),
                        "{current:?} ch{channel} {preset:?} -> {next:?}"
                    );
                }
            }
        }
        // A plain GM selection stays GM where GM can say it.
        assert_eq!(
            program_for_soundfont_preset(MidiProgramSelection::default(), 0, (0, 40)).format,
            Gm
        );
    }

    #[test]
    fn a_midi_track_program_sets_the_part_on_its_channel() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let gm = multi(&mut state, "GM");
        let bass = add(&mut state, TrackType::Midi, "Bass");
        let kit = add(&mut state, TrackType::Midi, "Kit");
        for (id, channel) in [(&bass, 2), (&kit, 4)] {
            state.set_track_output_routing(
                id,
                TrackOutputRouting::Instrument {
                    track_id: gm.clone(),
                },
            );
            state.set_track_midi_channel(id, Some(channel));
        }
        state.set_track_program_selection(&bass, selection(MidiPatchFormat::Gs, 8, 33));
        state.set_track_program_selection(&kit, selection(MidiPatchFormat::Xg, 127, 16));

        let parts = soundfont_program_parts(&state, &gm);
        assert_eq!(
            parts[1].as_ref().map(|p| (p.track_name.as_str(), p.preset)),
            Some(("Bass", (8, 33)))
        );
        assert_eq!(parts[3].as_ref().map(|p| p.preset), Some((128, 16)));
        assert!(parts[0].is_none());

        let view = soundfont_track_view(&state, &gm).unwrap();
        assert_eq!(view.settings.channels[1].preset, Some((8, 33)));
        assert!(view.settings.channels[3].is_drum_kit());
        let instrument = state.find_track(&gm).unwrap();
        assert_eq!(
            effective_soundfont_channels(&state, instrument)[1].preset,
            Some((8, 33))
        );
        assert_eq!(
            instrument.soundfont_channels[1].preset, None,
            "the stored part is untouched"
        );

        // The browser writes the program back.
        let route = SoundfontMidiRoute::Program {
            track_id: bass.clone(),
            channel: 1,
            preset: (0, 38),
        };
        assert!(apply_soundfont_midi_route(&mut state, &gm, &route));
        assert_eq!(
            soundfont_program_parts(&state, &gm)[1]
                .as_ref()
                .map(|p| p.preset),
            Some((0, 38))
        );
    }

    #[test]
    fn a_single_mode_player_has_no_program_parts() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let gm = multi(&mut state, "GM");
        state.tracks[0].soundfont_mode = SoundfontPlayerMode::Single;
        state.set_track_program_selection(&gm, selection(MidiPatchFormat::Gm, 0, 5));
        assert!(soundfont_program_parts(&state, &gm)
            .iter()
            .all(Option::is_none));
    }

    #[test]
    fn only_midi_tracks_are_routed_and_only_to_an_instrument_track() {
        let mut state = TimelineState::default();
        state.tracks.clear();
        let gm = add(&mut state, TrackType::Instrument, "GM");
        let audio = add(&mut state, TrackType::Audio, "Vocals");
        let assign_audio = SoundfontMidiRoute::Assign {
            track_id: audio.clone(),
            channel: 0,
        };
        assert!(!apply_soundfont_midi_route(&mut state, &gm, &assign_audio));
        let new = SoundfontMidiRoute::NewTracks(vec![(0, "x".into())]);
        assert!(!apply_soundfont_midi_route(&mut state, &audio, &new));
        assert_eq!(state.tracks.len(), 2);
    }
}
