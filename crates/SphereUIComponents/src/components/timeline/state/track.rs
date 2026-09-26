use super::*;

pub use sphere_soundfont_player::{SoundfontEnvelope, SoundfontRenderQuality};

/// Per-channel Listen state. Mirrors the engine's `ListenMode`; the engine
/// stays authoritative for where each tap sits relative to the fader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListenMode {
    #[default]
    Off,
    /// Pre-Fader Listen — level independent of the channel fader.
    Pfl,
    /// After-Fader Listen — follows the channel fader.
    Afl,
}

impl ListenMode {
    pub fn is_active(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Engine-side equivalent, so the UI never re-derives the mapping.
    pub fn to_engine(self) -> DirectAudio::monitor::ListenMode {
        match self {
            Self::Off => DirectAudio::monitor::ListenMode::Off,
            Self::Pfl => DirectAudio::monitor::ListenMode::Pfl,
            Self::Afl => DirectAudio::monitor::ListenMode::Afl,
        }
    }
}

/// One complete set of built-in Soundfont Player settings for a track, as
/// published by the player window. Grouped rather than passed positionally so
/// adding a control to the panel cannot silently transpose two arguments at the
/// one call site.
#[derive(Debug, Clone, PartialEq)]
pub struct SoundfontPlayerSettingsState {
    pub path: Option<String>,
    pub preset: Option<(i32, i32)>,
    pub volume: f32,
    pub reverb_chorus: bool,
    pub polyphony: usize,
    pub envelope: SoundfontEnvelope,
    pub quality: SoundfontRenderQuality,
}

impl Default for SoundfontPlayerSettingsState {
    fn default() -> Self {
        Self {
            path: None,
            preset: None,
            volume: 1.0,
            reverb_chorus: true,
            polyphony: 64,
            envelope: SoundfontEnvelope::default(),
            quality: SoundfontRenderQuality::default(),
        }
    }
}

impl SoundfontPlayerSettingsState {
    /// Clamps every value into the range the engine and the `.sf2` player
    /// accept, so a stored track can never hold something playback would reject.
    pub fn sanitized(self) -> Self {
        Self {
            volume: self.volume.clamp(0.0, 1.0),
            polyphony: self.polyphony.clamp(1, 256),
            envelope: self.envelope.sanitized(),
            ..self
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackType {
    Audio,
    Midi,
    Instrument,
    /// Sub-mix bus — other tracks route their output here for grouped
    /// processing before the master. Phase 3.
    Bus,
    /// FX return — receives sends from other tracks (aux/reverb returns).
    /// Phase 3.
    Return,
    /// Arrangement group container. Child membership is visual/project state
    /// and does not implicitly change the child's audio routing.
    Group,
    Master,
    /// Reference/preview video lane. Picture is decoded by the Video Player
    /// window; the container's audio track plays through the ordinary engine
    /// graph, so the lane has a real fader, meter, and mixer strip like any
    /// other channel. A project holds at most one (see
    /// [`TrackType::is_singleton`]).
    Video,
}

impl TrackType {
    /// `true` for routing tracks (Bus/Return) that receive audio from other
    /// tracks rather than hosting clips directly.
    pub fn is_routing(self) -> bool {
        matches!(self, TrackType::Bus | TrackType::Return | TrackType::Group)
    }

    /// Bus/Return live in the mixer only — they never occupy arrangement lanes.
    pub fn is_mixer_only(self) -> bool {
        matches!(self, TrackType::Bus | TrackType::Return)
    }

    /// `true` for track types a project may hold only one of.
    pub fn is_singleton(self) -> bool {
        matches!(self, TrackType::Master | TrackType::Video)
    }
}

/// Per-track edit/display mode. `Clips` is normal clip editing; `Automation`
/// switches the lane to automation editing — points/line are drawn inside the
/// same track lane and clips are dimmed behind. View state: it never reaches
/// the engine, and toggling it does not mark the project dirty, but a save
/// keeps it (the v54 view section).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackLaneMode {
    Clips,
    Automation,
}

impl Default for TrackLaneMode {
    fn default() -> Self {
        TrackLaneMode::Clips
    }
}

/// What a track's clips are anchored to when the tempo changes.
///
/// Positions are always *stored* in beats — this decides what is held constant
/// when the tempo map moves underneath them:
///
/// - [`Musical`](Self::Musical): the beat is held. A clip on bar 5 stays on
///   bar 5 and its wall-clock position moves. This is how a DAW normally
///   behaves and is the default for every track type.
/// - [`Linear`](Self::Linear): the wall-clock time is held. A clip at 1:23.000
///   stays at 1:23.000 and its bar position moves. This is what dialogue, sound
///   effects, and anything locked to picture need.
///
/// Available on every track type: a tempo change moves audio, MIDI, instrument
/// and video lanes alike, so any of them can need to be pinned to the clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrackTimebase {
    #[default]
    Musical,
    Linear,
}

impl TrackTimebase {
    /// Stable persistence tag. Never renumber — these are written into project
    /// files.
    pub fn to_tag(self) -> u8 {
        match self {
            Self::Musical => 0,
            Self::Linear => 1,
        }
    }

    pub fn from_tag(tag: u8) -> Self {
        match tag {
            1 => Self::Linear,
            _ => Self::Musical,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Musical => "Musical",
            Self::Linear => "Linear",
        }
    }

    /// Short glyph for the track-header toggle. Paired with a colour change, so
    /// the state never rests on hue alone.
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Musical => "♪",
            Self::Linear => "⏱",
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            Self::Musical => Self::Linear,
            Self::Linear => Self::Musical,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackState {
    pub id: String,
    pub name: String,
    pub track_type: TrackType,
    /// ARA plug-in processing this track, if any.
    ///
    /// Set means every audio clip on the track is rendered by the plug-in
    /// instead of read from its source file, so this and the ARA session must
    /// be changed together — see `layout::ara_ops`.
    pub ara: Option<crate::components::timeline::state::clip::AraTrackBinding>,
    /// What this track's clips hold constant when the tempo changes — their
    /// bar position ([`TrackTimebase::Musical`]) or their wall-clock position
    /// ([`TrackTimebase::Linear`]).
    pub timebase: TrackTimebase,
    /// Parent arrangement group, if any. Only `TrackType::Group` ids are valid.
    pub parent_group_id: Option<String>,
    /// Folder presentation state. Meaningful only for `TrackType::Group`.
    pub group_collapsed: bool,
    pub color: gpui::Rgba,
    /// Manual/base normalized fader position in `0.0..=1.0`. `1.0` is the top of
    /// the fader (≈ +6 dB) and `0.0` is the bottom (≈ -60 dB). See
    /// `Volume::norm_to_db`. This is the value the user sets directly and the
    /// value persisted as `volume_norm`; Track Volume automation does NOT write
    /// here — it drives [`Self::volume_effective`] instead.
    pub volume: f32,
    /// Automation-evaluated effective volume at the current playhead. UI-only and
    /// not persisted — recomputed from the Track Volume automation lane on
    /// playback ticks, seeks, and point edits (see
    /// [`TimelineState::recompute_effective_volumes`]). Equals [`Self::volume`]
    /// whenever automation read is off or there is no active volume automation.
    pub volume_effective: f32,
    /// Whether Track Volume automation drives the effective volume / display.
    /// Persisted with the track (v32); `true` for older projects so automated
    /// ones follow their curves on load.
    pub volume_automation_read: bool,
    /// Pan position in `-1.0..=1.0`. `-1.0` is hard left, `+1.0` is hard right.
    pub pan: f32,
    pub muted: bool,
    pub solo: bool,
    pub armed: bool,
    /// Input monitoring mode (Off / Auto / Input).
    pub input_monitor: InputMonitorMode,
    /// Pre/After-Fader Listen state, routed to the Control Room's listen bus.
    ///
    /// Monitoring only: engaging Listen never changes what this channel
    /// contributes to the master mix, so it cannot affect an export or a
    /// recording. Session state — not persisted with the project.
    pub listen: ListenMode,
    /// Latest peak meter levels in `0.0..=1.0`. Currently a static placeholder
    /// per track; will be driven by the audio engine when that lands.
    pub meter_level_l: f32,
    pub meter_level_r: f32,
    /// Held peak levels (slow release) driving the peak-hold tick. UI-only.
    pub meter_peak_hold_l: f32,
    pub meter_peak_hold_r: f32,
    /// Latched clip indicator — set when the engine peak reached/exceeded
    /// 0 dBFS, auto-cleared once the held peak falls back. UI-only.
    pub meter_clip: bool,
    pub clips: Vec<ClipState>,
    pub automation_lanes: Vec<AutomationLaneState>,
    /// Per-track edit mode (Clip vs Automation). View state, saved in the
    /// project's view section (v54).
    pub lane_mode: TrackLaneMode,
    /// Which automation target the lane editor is currently focused on. Drives
    /// which lane renders/edits while in [`TrackLaneMode::Automation`]. Saved
    /// with the lane mode when it names one of the track's lanes.
    pub selected_automation_target: Option<AutomationTarget>,
    /// Insert (effect) plugin chain — ordered. Audio flows through these
    /// in order before volume/pan/sends in the runtime. The UI stores
    /// only descriptor + transient state; the runtime owns the actual
    /// plugin processor.
    pub inserts: Vec<InsertSlotState>,
    /// Canonical MIDI destination for this instrument track — the
    /// `plugin_instance_id` of the first enabled instrument insert (e.g.
    /// `insert-track-1-1`). Set when a VSTi is assigned; used for piano
    /// preview, clip playback, and external-bridge routing.
    pub instrument_plugin_instance_id: Option<String>,
    /// `true` when this Instrument track's sound source is the in-app built-in
    /// Soundfont Player rather than a hosted VSTi. Not a plugin insert — the
    /// built-in player never goes through the VST3/CLAP/AU/LV2 bridge or plugin
    /// registry, so this is a plain marker, not `inserts`. Persisted with the
    /// rest of the player (v28): a track with a saved soundfont reopens with it.
    pub builtin_soundfont_player: bool,
    /// Absolute `.sf2` path loaded into the built-in Soundfont Player.
    pub soundfont_path: Option<String>,
    /// Selected SoundFont preset `(bank, patch)` for channel 1 playback.
    pub soundfont_preset: Option<(i32, i32)>,
    pub soundfont_volume: f32,
    pub soundfont_reverb_chorus: bool,
    pub soundfont_polyphony: usize,
    /// Amp envelope applied to the built-in player's output. Default is the
    /// bypassed envelope — see `SoundfontEnvelope`.
    pub soundfont_envelope: SoundfontEnvelope,
    /// Internal synthesis oversampling for the built-in player.
    pub soundfont_quality: SoundfontRenderQuality,
    /// Native Solfege instrument state. Mutually exclusive with the built-in
    /// Soundfont Player and VSTi instrument insert for Instrument tracks.
    pub solfege: Option<crate::solfege::SolfegeTrackState>,
    /// Aux sends to Bus/Return tracks (Phase 3). Empty for most tracks.
    pub sends: Vec<SendSlotState>,
    /// Persisted routing choices. Device discovery is not wired yet, so device
    /// variants are preserved but not created by the Inspector.
    pub routing: TrackRoutingState,
    /// Recorded passes on this track, oldest first. See [`TrackTake`] — a take
    /// is one of this track's own clips plus the record of which pass made it,
    /// so an inactive take is a muted clip and nothing more exotic.
    pub takes: Vec<TrackTake>,
    /// Whether the take sub-lane is open in the track header. Opens itself on
    /// the first pass that overlaps another; closed is the resting state for a
    /// track nobody is comping.
    pub takes_expanded: bool,
}

impl TrackState {
    /// `true` when this track has an enabled Track Volume automation lane that
    /// actually carries points — i.e. automation can resolve a value.
    pub fn has_active_volume_automation(&self) -> bool {
        self.automation_lanes.iter().any(|l| {
            l.enabled && matches!(l.target, AutomationTarget::TrackVolume) && !l.points.is_empty()
        })
    }

    /// The normalized volume the UI fader / readout should display: the
    /// automation-evaluated effective value when automation read is active and a
    /// volume lane exists, otherwise the manual/base value. Faders still WRITE
    /// the base via [`TimelineState::set_track_volume`] — this is display only,
    /// so an automation-follow repaint can never be mistaken for a user edit.
    pub fn display_volume(&self) -> f32 {
        if self.volume_automation_read && self.has_active_volume_automation() {
            self.volume_effective
        } else {
            self.volume
        }
    }

    pub fn instrument_insert(&self) -> Option<&InsertSlotState> {
        if self.track_type == TrackType::Instrument {
            self.inserts.first()
        } else {
            None
        }
    }

    pub fn instrument_insert_mut(&mut self) -> Option<&mut InsertSlotState> {
        if self.track_type == TrackType::Instrument {
            self.inserts.first_mut()
        } else {
            None
        }
    }

    pub fn effect_inserts(&self) -> &[InsertSlotState] {
        if self.track_type == TrackType::Instrument {
            self.inserts.get(1..).unwrap_or(&[])
        } else {
            self.inserts.as_slice()
        }
    }

    pub fn effect_inserts_mut(&mut self) -> &mut [InsertSlotState] {
        if self.track_type == TrackType::Instrument {
            let start = self.inserts.len().min(1);
            &mut self.inserts[start..]
        } else {
            self.inserts.as_mut_slice()
        }
    }
}

#[derive(Debug, Clone)]
pub struct CreateTrackOptions {
    pub track_type: TrackType,
    pub name: String,
    pub color: gpui::Rgba,
    pub volume: f32,
    pub pan: f32,
    pub armed: bool,
    pub input_monitor: InputMonitorMode,
}

impl TimelineState {
    // ── Identity helpers ─────────────────────────────────────────────────────

    pub fn next_track_id(&self) -> String {
        // Find the highest numeric suffix on "track-N" ids, plus one.
        let mut n = 0u32;
        for t in &self.tracks {
            if let Some(rest) = t.id.strip_prefix("track-") {
                if let Ok(v) = rest.parse::<u32>() {
                    if v > n {
                        n = v;
                    }
                }
            }
        }
        format!("track-{}", n + 1)
    }

    pub fn begin_track_drag(&mut self, track_id: &str, origin_index: usize, y: f32) {
        self.dragging_track_id = Some(track_id.to_string());
        self.drag_origin_index = Some(origin_index);
        self.drag_current_y = y;
        self.drag_target_index = Some(origin_index.min(self.tracks.len()));
    }

    pub fn update_track_drag(&mut self, y: f32) {
        self.drag_current_y = y;
        self.drag_target_index = Some(self.track_insert_index_at_y(y));
    }

    pub fn clear_track_drag(&mut self) {
        self.dragging_track_id = None;
        self.drag_origin_index = None;
        self.drag_current_y = 0.0;
        self.drag_target_index = None;
    }

    pub fn reorder_track(&mut self, track_id: &str, target_index: usize) -> bool {
        let Some(origin_index) = self.tracks.iter().position(|track| track.id == track_id) else {
            self.clear_track_drag();
            return false;
        };
        if self.tracks[origin_index].track_type == TrackType::Group {
            let before: Vec<_> = self.tracks.iter().map(|track| track.id.clone()).collect();
            let target_index = target_index.clamp(0, self.tracks.len());
            let member_indices: Vec<_> = self
                .tracks
                .iter()
                .enumerate()
                .filter_map(|(index, track)| {
                    (track.id == track_id || track.parent_group_id.as_deref() == Some(track_id))
                        .then_some(index)
                })
                .collect();
            let removed_before_target = member_indices
                .iter()
                .filter(|index| **index < target_index)
                .count();
            let mut block = Vec::with_capacity(member_indices.len());
            let mut remaining = Vec::with_capacity(self.tracks.len() - member_indices.len());
            for track in std::mem::take(&mut self.tracks) {
                let belongs =
                    track.id == track_id || track.parent_group_id.as_deref() == Some(track_id);
                if belongs {
                    block.push(track);
                } else {
                    remaining.push(track);
                }
            }
            self.tracks = remaining;
            let insert_index = target_index
                .saturating_sub(removed_before_target)
                .min(self.tracks.len());
            self.tracks.splice(insert_index..insert_index, block);
            self.clear_track_drag();
            return before
                != self
                    .tracks
                    .iter()
                    .map(|track| track.id.clone())
                    .collect::<Vec<_>>();
        }
        let target_index = target_index.clamp(0, self.tracks.len());
        let insert_index = if origin_index < target_index {
            target_index.saturating_sub(1)
        } else {
            target_index
        };
        if insert_index == origin_index {
            self.clear_track_drag();
            return false;
        }

        let track = self.tracks.remove(origin_index);
        let insert_index = insert_index.min(self.tracks.len());
        self.tracks.insert(insert_index, track);
        if let Some(selected) = self.selection.selected_track_id.as_deref() {
            if !self.tracks.iter().any(|track| track.id == selected) {
                self.selection.selected_track_id =
                    self.tracks.get(insert_index).map(|t| t.id.clone());
            }
        }
        self.clear_track_drag();
        true
    }

    /// Create a new audio track with auto-assigned id/color.
    pub fn create_audio_track(&mut self) -> String {
        let name = format!("Audio {}", self.tracks.len() + 1);
        let log_name = name.clone();
        let id = self.create_track(CreateTrackOptions {
            track_type: TrackType::Audio,
            name,
            color: self.track_color_for_index(self.tracks.len()),
            volume: volume::db_to_norm(0.0),
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        });
        eprintln!("[import] created track id={} name={}", id, log_name);
        id
    }

    pub fn create_midi_track(&mut self) -> String {
        let name = format!("MIDI {}", self.tracks.len() + 1);
        self.create_track(CreateTrackOptions {
            track_type: TrackType::Midi,
            name,
            color: self.track_color_for_index(self.tracks.len()),
            volume: volume::db_to_norm(0.0),
            pan: 0.0,
            armed: false,
            input_monitor: InputMonitorMode::Off,
        })
    }

    pub fn track_color_for_index(&self, index: usize) -> gpui::Rgba {
        crate::theme::Colors::track_color_for_index(index)
    }

    pub fn create_track(&mut self, options: CreateTrackOptions) -> String {
        let id = self.next_track_id();
        let track_type = options.track_type;
        self.tracks.push(TrackState {
            listen: ListenMode::Off,
            id: id.clone(),
            name: options.name,
            track_type,
            ara: None,
            timebase: TrackTimebase::default(),
            parent_group_id: None,
            group_collapsed: false,
            color: options.color,
            volume: options.volume.clamp(0.0, 1.0),
            volume_effective: options.volume.clamp(0.0, 1.0),
            volume_automation_read: true,
            pan: options.pan.clamp(-1.0, 1.0),
            muted: false,
            solo: false,
            armed: options.armed,
            input_monitor: options.input_monitor,
            meter_level_l: 0.0,
            meter_level_r: 0.0,
            meter_peak_hold_l: 0.0,
            meter_peak_hold_r: 0.0,
            meter_clip: false,
            clips: Vec::new(),
            automation_lanes: Vec::new(),
            lane_mode: TrackLaneMode::Clips,
            selected_automation_target: None,
            inserts: Vec::new(),
            sends: Vec::new(),
            routing: TrackRoutingState::for_track_type(track_type),
            takes: Vec::new(),
            takes_expanded: false,
            instrument_plugin_instance_id: None,
            builtin_soundfont_player: false,
            soundfont_path: None,
            soundfont_preset: None,
            soundfont_volume: 1.0,
            soundfont_reverb_chorus: true,
            soundfont_polyphony: 64,
            soundfont_envelope: SoundfontEnvelope::default(),
            soundfont_quality: SoundfontRenderQuality::default(),
            solfege: None,
        });
        id
    }

    pub fn assign_track_to_group(&mut self, track_id: &str, group_id: &str) -> bool {
        if track_id == group_id {
            return false;
        }
        let Some(group_index) = self
            .tracks
            .iter()
            .position(|track| track.id == group_id && track.track_type == TrackType::Group)
        else {
            return false;
        };
        let Some(track_index) = self.tracks.iter().position(|track| track.id == track_id) else {
            return false;
        };
        if self.tracks[track_index].track_type == TrackType::Group {
            return false;
        }

        let already_grouped = self.tracks[track_index].parent_group_id.as_deref() == Some(group_id);
        let mut track = self.tracks.remove(track_index);
        track.parent_group_id = Some(group_id.to_string());

        let group_index = if track_index < group_index {
            group_index.saturating_sub(1)
        } else {
            group_index
        };
        let mut insert_index = group_index + 1;
        while insert_index < self.tracks.len()
            && self.tracks[insert_index].parent_group_id.as_deref() == Some(group_id)
        {
            insert_index += 1;
        }
        self.tracks.insert(insert_index, track);
        self.clear_track_drag();
        !already_grouped || track_index != insert_index
    }

    pub fn remove_track_from_group(&mut self, track_id: &str) -> bool {
        let Some(track) = self.tracks.iter_mut().find(|track| track.id == track_id) else {
            return false;
        };
        track.parent_group_id.take().is_some()
    }

    pub fn toggle_group_collapsed(&mut self, group_id: &str) -> Option<bool> {
        let group_index = self
            .tracks
            .iter()
            .position(|track| track.id == group_id && track.track_type == TrackType::Group)?;
        let collapsed = !self.tracks[group_index].group_collapsed;
        self.tracks[group_index].group_collapsed = collapsed;
        if collapsed {
            let selected_child_is_hidden = self.selected_range_track_ids().iter().any(|selected| {
                self.tracks.iter().any(|track| {
                    track.id == *selected && track.parent_group_id.as_deref() == Some(group_id)
                })
            });
            if selected_child_is_hidden {
                self.select_track(group_id);
            }
        }
        Some(collapsed)
    }

    /// Mark/unmark a track's instrument as the built-in Soundfont Player.
    /// No-op (and no dirty) for non-Instrument tracks. Returns `true` if the
    /// flag actually changed.
    pub fn set_track_builtin_soundfont_player(&mut self, track_id: &str, enabled: bool) -> bool {
        let Some(track) = self
            .tracks
            .iter_mut()
            .find(|t| t.id == track_id && t.track_type == TrackType::Instrument)
        else {
            return false;
        };
        if track.builtin_soundfont_player == enabled {
            return false;
        }
        track.builtin_soundfont_player = enabled;
        true
    }

    pub fn set_track_soundfont_player_state(
        &mut self,
        track_id: &str,
        settings: SoundfontPlayerSettingsState,
    ) -> bool {
        let Some(track) = self
            .tracks
            .iter_mut()
            .find(|t| t.id == track_id && t.track_type == TrackType::Instrument)
        else {
            return false;
        };
        let settings = settings.sanitized();
        let changed = track.soundfont_path != settings.path
            || track.soundfont_preset != settings.preset
            || (track.soundfont_volume - settings.volume).abs() > f32::EPSILON
            || track.soundfont_reverb_chorus != settings.reverb_chorus
            || track.soundfont_polyphony != settings.polyphony
            || track.soundfont_envelope != settings.envelope
            || track.soundfont_quality != settings.quality;
        if changed {
            track.builtin_soundfont_player = true;
            track.soundfont_path = settings.path;
            track.soundfont_preset = settings.preset;
            track.soundfont_volume = settings.volume;
            track.soundfont_reverb_chorus = settings.reverb_chorus;
            track.soundfont_polyphony = settings.polyphony;
            track.soundfont_envelope = settings.envelope;
            track.soundfont_quality = settings.quality;
        }
        changed
    }

    /// Assign or clear the native Solfege instrument on an Instrument track.
    /// Selecting Solfege also clears the other built-in instrument marker so
    /// the engine snapshot has one unambiguous instrument source.
    pub fn set_track_solfege_engine(
        &mut self,
        track_id: &str,
        state: Option<crate::solfege::SolfegeTrackState>,
    ) -> bool {
        let Some(track) = self
            .tracks
            .iter_mut()
            .find(|t| t.id == track_id && t.track_type == TrackType::Instrument)
        else {
            return false;
        };
        let state = state.map(crate::solfege::SolfegeTrackState::sanitized);
        if track.solfege == state {
            return false;
        }
        track.solfege = state;
        if track.solfege.is_some() {
            track.builtin_soundfont_player = false;
            track.soundfont_path = None;
            track.soundfont_preset = None;
        }
        true
    }

    pub fn selected_audio_track_id(&self) -> Option<String> {
        let selected = self.selection.selected_track_id.as_deref()?;
        self.tracks
            .iter()
            .find(|track| track.id == selected && matches!(track.track_type, TrackType::Audio))
            .map(|track| track.id.clone())
    }

    /// Rename a track. Line breaks and other control characters become
    /// spaces and surrounding whitespace is trimmed ([`sanitize_track_name`]);
    /// an all-whitespace name is ignored (keeps the previous one). Returns
    /// `true` if the stored name actually changed, so callers only mark dirty
    /// on a real edit.
    pub fn rename_track(&mut self, track_id: &str, name: &str) -> bool {
        let cleaned = sanitize_track_name(name);
        if cleaned.is_empty() {
            return false;
        }
        if let Some(t) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            if t.name != cleaned {
                t.name = cleaned;
                self.touch_track_names();
                return true;
            }
        }
        false
    }

    /// Give [`Self::track_names_revision`] a value no arrangement in this
    /// process has had. A surface that copies names out of the timeline keeps
    /// the revision it last copied at, so a fresh value reaches it even when
    /// the whole arrangement was replaced by one that counted its own renames
    /// from the same start.
    pub fn touch_track_names(&mut self) {
        self.track_names_revision = fresh_track_names_revision();
    }

    /// Store `name` exactly as given — no trimming, an empty name included —
    /// so undoing a rename restores whatever the track was called, even a
    /// legacy name a user could not type today. Returns `true` on a change.
    pub fn set_track_name(&mut self, track_id: &str, name: &str) -> bool {
        let Some(track) = self.tracks.iter_mut().find(|t| t.id == track_id) else {
            return false;
        };
        if track.name == name {
            return false;
        }
        track.name = name.to_string();
        self.touch_track_names();
        true
    }

    /// Set a track's color. Returns `true` if it changed.
    pub fn set_track_color(&mut self, track_id: &str, color: gpui::Rgba) -> bool {
        if let Some(t) = self.tracks.iter_mut().find(|t| t.id == track_id) {
            if t.color != color {
                t.color = color;
                return true;
            }
        }
        false
    }

    pub fn find_track(&self, track_id: &str) -> Option<&TrackState> {
        self.tracks.iter().find(|t| t.id == track_id)
    }

    /// Track id -> index, for callers that resolve **many** ids against the
    /// same track list in one pass.
    ///
    /// [`Self::find_track`] is a linear scan, which is right for a single
    /// lookup. Resolving a whole batch with it is O(tracks x lookups): a
    /// project whose instruments expose per-output VSTi channels reaches a few
    /// thousand channels, and the engine publishes one meter for each, so the
    /// meter path alone ran millions of string comparisons on the UI thread at
    /// the display refresh. Building this map once per batch makes that pass
    /// linear instead.
    pub fn track_index_by_id(&self) -> std::collections::HashMap<&str, usize> {
        self.tracks
            .iter()
            .enumerate()
            .map(|(index, track)| (track.id.as_str(), index))
            .collect()
    }

    pub fn delete_track(&mut self, track_id: &str) {
        if let Some(index) = self.tracks.iter().position(|track| track.id == track_id) {
            let deleting_group = self.tracks[index].track_type == TrackType::Group;
            self.tracks.remove(index);
            if deleting_group {
                for track in &mut self.tracks {
                    if track.parent_group_id.as_deref() == Some(track_id) {
                        track.parent_group_id = None;
                    }
                }
            }
            self.track_view_layout.remove_track(track_id);
            self.selection
                .selected_track_ids
                .retain(|selected| selected != track_id);
            if self.selection.selected_track_id.as_deref() == Some(track_id) {
                self.selection.selected_track_id = self
                    .selection
                    .selected_track_ids
                    .last()
                    .cloned()
                    .or_else(|| {
                        self.tracks
                            .get(index.saturating_sub(1))
                            .map(|t| t.id.clone())
                    });
                if self.selection.selected_track_ids.is_empty() {
                    if let Some(primary) = self.selection.selected_track_id.clone() {
                        self.selection.selected_track_ids.push(primary);
                    }
                }
            }
            if self.selection.track_selection_anchor_id.as_deref() == Some(track_id) {
                self.selection.track_selection_anchor_id = self.selection.selected_track_id.clone();
            }
            self.selection.selected_clip_ids.clear();
        }
    }
}

/// A fresh value for [`TimelineState::track_names_revision`]: never 0, never
/// handed out twice in this process.
pub(crate) fn fresh_track_names_revision() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A typed or pasted track name made fit to store: every run of line breaks,
/// tabs and other control characters becomes one space, then surrounding
/// white space is trimmed. A name pasted from several lines becomes one line;
/// Thai vowel and tone marks, combining accents and every other character keep
/// the exact code points typed.
pub fn sanitize_track_name(name: &str) -> String {
    let mut cleaned = String::with_capacity(name.len());
    let mut in_break = false;
    for c in name.chars() {
        if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
            if !in_break {
                cleaned.push(' ');
            }
            in_break = true;
        } else {
            cleaned.push(c);
            in_break = false;
        }
    }
    cleaned.trim().to_string()
}

/// The name a track rename should store, or `None` when it should leave the
/// track alone.
///
/// The draft is sanitized ([`sanitize_track_name`]); a blank draft, or one
/// that cleans up to the name the track has `current`ly, is no rename at all,
/// so it never costs an undo step.
pub fn resolve_track_rename(current: &str, draft: &str) -> Option<String> {
    let cleaned = sanitize_track_name(draft);
    if cleaned.is_empty() || cleaned == current {
        return None;
    }
    Some(cleaned)
}

#[cfg(test)]
mod rename_tests {
    use super::*;

    #[test]
    fn a_rename_trims_the_draft() {
        assert_eq!(
            resolve_track_rename("Audio 1", "  Lead Vox \t"),
            Some("Lead Vox".to_string())
        );
    }

    #[test]
    fn a_blank_draft_keeps_the_name() {
        assert_eq!(resolve_track_rename("Audio 1", ""), None);
        assert_eq!(resolve_track_rename("Audio 1", "   \t "), None);
    }

    #[test]
    fn an_unchanged_draft_is_no_rename() {
        assert_eq!(resolve_track_rename("Bass", "Bass"), None);
        assert_eq!(resolve_track_rename("Bass", "  Bass  "), None);
        // A name that only differs in case is a real rename.
        assert_eq!(
            resolve_track_rename("Bass", "bass"),
            Some("bass".to_string())
        );
    }

    #[test]
    fn thai_and_combining_marks_survive_exactly() {
        // Thai: consonant + above vowel + tone mark, which must stay attached.
        let thai = "เสียงร้อง";
        assert_eq!(
            resolve_track_rename("Audio 1", &format!("  {thai}  ")),
            Some(thai.to_string())
        );
        // A decomposed é (e + U+0301) is not normalised to the precomposed one.
        let decomposed = "Cafe\u{301}";
        assert_eq!(
            resolve_track_rename("Café", decomposed),
            Some(decomposed.to_string())
        );
        // A trailing combining mark is part of the name, not white space.
        assert_eq!(
            resolve_track_rename("ก", "ก\u{E34}"),
            Some("ก\u{E34}".to_string())
        );
    }

    #[test]
    fn renaming_advances_the_names_revision_only_on_a_change() {
        let mut state = TimelineState::default();
        let id = state.create_midi_track();
        let before = state.track_names_revision;
        assert!(!state.set_track_name(&id, &state.tracks[0].name.clone()));
        assert_eq!(state.track_names_revision, before);
        assert!(state.set_track_name(&id, " spaced "));
        assert_eq!(state.tracks[0].name, " spaced ", "stored exactly");
        let after_set = state.track_names_revision;
        assert_ne!(after_set, before);
        assert!(state.rename_track(&id, "Keys"));
        let after_rename = state.track_names_revision;
        assert_ne!(after_rename, after_set);
        assert!(!state.rename_track(&id, "  "));
        assert!(!state.rename_track(&id, " Keys\n"));
        assert_eq!(state.track_names_revision, after_rename);
    }

    /// A surface remembers the revision it last copied names at. A replaced
    /// arrangement must never come back with that same value, or the surface
    /// would keep showing the old project's names.
    #[test]
    fn a_fresh_arrangement_never_repeats_a_names_revision() {
        let mut old = TimelineState::default();
        let id = old.create_midi_track();
        assert!(old.rename_track(&id, "Vox"));
        let seen = old.track_names_revision;
        let mut replacement = TimelineState::default();
        assert_ne!(replacement.track_names_revision, seen);
        assert_ne!(replacement.track_names_revision, 0);
        let loaded = replacement.track_names_revision;
        replacement.touch_track_names();
        assert_ne!(replacement.track_names_revision, loaded);
        assert_ne!(replacement.track_names_revision, seen);
    }

    #[test]
    fn line_breaks_tabs_and_control_characters_become_spaces() {
        assert_eq!(sanitize_track_name("Lead\nVox"), "Lead Vox");
        assert_eq!(sanitize_track_name("Lead\r\nVox"), "Lead Vox");
        assert_eq!(sanitize_track_name("Lead\tVox"), "Lead Vox");
        assert_eq!(sanitize_track_name("\n\tDrums \r\n"), "Drums");
        assert_eq!(sanitize_track_name("A\u{7}\u{1b}B"), "A B");
        assert_eq!(sanitize_track_name("A\u{85}B\u{2028}C\u{2029}D"), "A B C D");
        // Spaces the user typed stay as typed.
        assert_eq!(sanitize_track_name("Lead  Vox"), "Lead  Vox");
        // Format characters are not control characters: a joiner stays.
        assert_eq!(sanitize_track_name("क्\u{200D}ष"), "क्\u{200D}ष");
        assert_eq!(sanitize_track_name("\n\r\t"), "");
    }

    #[test]
    fn a_pasted_multi_line_name_is_stored_on_one_line() {
        assert_eq!(
            resolve_track_rename("Audio 1", "Lead\nVox\n"),
            Some("Lead Vox".to_string())
        );
        assert_eq!(resolve_track_rename("Lead Vox", "Lead\r\nVox"), None);
        assert_eq!(resolve_track_rename("Audio 1", "\n\t"), None);
        let mut state = TimelineState::default();
        let id = state.create_midi_track();
        assert!(state.rename_track(&id, "Bass\tDI\n"));
        assert_eq!(state.tracks[0].name, "Bass DI");
    }
}
