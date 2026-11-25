//! Runtime tracks: spatial and monitor state, input sources, sends, preview mode and meters.

use super::*;

/// Mark the buses that keep their channels' places, from `snapshot`. Only in
/// a spatial mix: in stereo nothing is placed, so nothing is kept. Control
/// thread; runs before latency planning, which reads the flags through
/// [`spatial_field_target`].
pub fn apply_spatial_keeps_inputs(
    tracks: &mut [RuntimeTrack],
    snapshot: &crate::types::EngineSpatialSnapshot,
) {
    let spatial = snapshot.format.is_spatial();
    for track in tracks.iter_mut() {
        track.spatial_state.keeps_inputs = spatial
            && crate::audio_graph::is_routing_track_type(&track.track_type)
            && snapshot
                .sources
                .iter()
                .any(|source| source.keeps_inputs && source.track_id == track.id);
    }
}

/// The bus whose field `track_index` is placed into, when it is routed into a
/// bus that keeps its channels' places and that bus sums into the master.
/// Such a channel reaches the master directly — through the field, at the
/// bus's fader — so the render path places it there and latency planning
/// counts it as a direct path. Resolved indices only; cheap.
#[inline]
pub fn spatial_field_target(tracks: &[RuntimeTrack], track_index: usize) -> Option<usize> {
    let track = tracks.get(track_index)?;
    if crate::audio_graph::is_master_track_type(&track.track_type) {
        return None;
    }
    let target = track.output_track_index.filter(|&t| t != track_index)?;
    let bus = tracks.get(target)?;
    (bus.spatial_state.keeps_inputs
        && crate::audio_graph::is_routing_track_type(&bus.track_type)
        && bus.output_track_index.is_none())
    .then_some(target)
}

/// One channel's spatial state beyond its saved placement.
#[derive(Debug, Clone)]
pub struct RuntimeTrackSpatial {
    /// Where the channel is heard this block: `spatial_params` with any
    /// spatial automation lane applied. Set by the fader stage, read where the
    /// channel is placed. A copy, so the render path never allocates for it.
    pub heard: solfege_spatialaudio::SourceParams,
    /// A bus or return set to keep its channels' places (snapshot flag). Only
    /// acted on in a spatial mix, by a bus that sums into the master.
    pub keeps_inputs: bool,
    /// The field such a bus gathers: every channel routed into it rendered
    /// where it sits in the room, in the mix's own channel layout. Added to the
    /// master's spatial bus at the bus's fader. `None` everywhere else.
    /// Preallocated by [`RuntimeProject::ensure_spatial`].
    pub field: Option<solfege_spatialaudio::SpatialBus>,
    /// The field gain the bus ended its last block on, so the next one ramps.
    pub field_gain: f32,
    /// This block's fader gain (volume × VCA, automation included), set by
    /// the fader stage for the field to follow.
    pub fader_gain: f32,
}

impl Default for RuntimeTrackSpatial {
    fn default() -> Self {
        Self {
            heard: solfege_spatialaudio::SourceParams::default(),
            keeps_inputs: false,
            field: None,
            field_gain: 1.0,
            fader_gain: 1.0,
        }
    }
}

/// The spatial mix of a graph.
///
/// In a stereo project this is inert (`format` stereo, a two-channel bus
/// nobody writes). In a spatial one, every channel that sums into the master
/// is rendered by its own [`solfege_spatialaudio::SpatialSource`] into `bus`
/// instead of being panned, and the master stage takes the bus from there:
/// a binaural bus is already the stereo mix; a surround bus goes out discretely
/// when the device has the channels and Master owns channels 0/1, and is
/// otherwise heard through `virtual_speakers` (headphones) or the ITU fold.
#[derive(Debug, Clone)]
pub struct RuntimeSpatial {
    pub format: solfege_spatialaudio::SpatialFormat,
    pub room: solfege_spatialaudio::RoomSettings,
    pub fold: solfege_spatialaudio::MonitorFold,
    pub bus: solfege_spatialaudio::SpatialBus,
    pub virtual_speakers: Option<solfege_spatialaudio::VirtualSpeakers>,
    /// The room's late tail, added to what reaches headphones: the binaural
    /// mix, and a surround mix heard through the virtual speakers.
    pub tail: Option<solfege_spatialaudio::RoomTail>,
    pub sample_rate: u32,
    /// Each speaker feed's peak (or the two ears of a binaural mix) since the
    /// display last read it, after the master fader. Shared with the control
    /// side's copy of the graph like the track meters.
    pub meter: Arc<RuntimeSpeakerMeter>,
    /// An offline render writing every speaker to its own channel of a
    /// multichannel file: the surround bus goes out discretely whatever the
    /// Control Room would do with a device (the exporter never runs it).
    pub offline_discrete: bool,
}

impl Default for RuntimeSpatial {
    fn default() -> Self {
        Self {
            format: solfege_spatialaudio::SpatialFormat::Stereo,
            room: solfege_spatialaudio::RoomSettings::default(),
            fold: solfege_spatialaudio::MonitorFold::default(),
            bus: solfege_spatialaudio::SpatialBus::new(2, 0),
            virtual_speakers: None,
            tail: None,
            sample_rate: 48_000,
            meter: Arc::default(),
            offline_discrete: false,
        }
    }
}

/// Peak of every channel of the spatial bus, as the Room window meters it.
#[derive(Debug, Default)]
pub struct RuntimeSpeakerMeter {
    peaks: [AtomicU32; solfege_spatialaudio::MAX_CHANNELS],
}

impl RuntimeSpeakerMeter {
    /// Publish one block's peaks: a lock-free max per channel.
    #[inline]
    pub fn store(&self, channel: usize, peak: f32) {
        if let Some(slot) = self.peaks.get(channel) {
            slot.fetch_max(peak_bits(peak), Ordering::Relaxed);
        }
    }

    /// The first `channels` peaks since the last take, and start the next
    /// window. For the one display reader.
    pub fn take(&self, channels: usize) -> Vec<f32> {
        self.peaks
            .iter()
            .take(channels)
            .map(|slot| f32_load(slot.swap(0, Ordering::Relaxed)))
            .collect()
    }
}

impl RuntimeSpatial {
    /// Allocates the bus (and, for surround, the virtual speakers) at the
    /// callback's block capacity. Control thread only.
    pub fn from_snapshot(snapshot: &crate::types::EngineSpatialSnapshot, sample_rate: u32) -> Self {
        let format = snapshot.format;
        if !format.is_spatial() {
            return Self {
                sample_rate,
                ..Self::default()
            };
        }
        let virtual_speakers = match format {
            solfege_spatialaudio::SpatialFormat::Surround(layout) => {
                Some(solfege_spatialaudio::VirtualSpeakers::new(
                    layout,
                    sample_rate,
                    DEFAULT_AUDIO_BLOCK_CAPACITY,
                ))
            }
            _ => None,
        };
        let room = snapshot.room.sanitized();
        let tail = Some(solfege_spatialaudio::RoomTail::new(&room, sample_rate))
            .filter(solfege_spatialaudio::RoomTail::is_audible);
        Self {
            format,
            room,
            fold: snapshot.fold,
            bus: solfege_spatialaudio::SpatialBus::new(
                format.channel_count(),
                DEFAULT_AUDIO_BLOCK_CAPACITY,
            ),
            virtual_speakers,
            tail,
            sample_rate,
            meter: Arc::default(),
            offline_discrete: false,
        }
    }

    /// Whether channels are being spatialised.
    #[inline]
    pub fn active(&self) -> bool {
        self.format.is_spatial()
    }

    /// Meter every channel of the bus at `gain` (the master fader). Realtime:
    /// reads the preallocated bus, one atomic max per channel.
    #[inline]
    pub fn publish_meter(&self, frames: usize, gain: f32) {
        for channel in 0..self.bus.channels() {
            let peak = self
                .bus
                .channel(channel, frames)
                .iter()
                .fold(0.0f32, |peak, sample| peak.max(sample.abs()));
            self.meter.store(channel, peak * gain.abs());
        }
    }

    /// Channels a surround bus writes to the device directly, or `None` when it
    /// must be folded to channels 0/1 instead: the device is too small, or
    /// Master does not own the first pair outright (the Control Room is
    /// monitoring, or Master is routed elsewhere).
    pub fn discrete_channels(
        &self,
        device_channels: usize,
        monitor: &RuntimeMonitor,
    ) -> Option<usize> {
        let solfege_spatialaudio::SpatialFormat::Surround(layout) = self.format else {
            return None;
        };
        let count = layout.channel_count();
        let master_owns_first_pair = self.offline_discrete
            || (monitor.hardware_owner == crate::monitor::HardwareOutputOwner::MasterDirect
                && monitor.master_output == Some((0, 1)));
        (count > 2 && device_channels >= count && master_owns_first_pair).then_some(count)
    }
}

/// Control Room state carried by the render graph.
///
/// Lives on [`RuntimeProject`] so the audio callback reads a resolved,
/// index-based configuration with no id lookups, but it is only *consumed* by
/// the realtime device callback. The offline exporter renders the same graph
/// and simply never runs the Control Room stage, which is what keeps monitor
/// gain, dim, mono, and monitor inserts out of exported and recorded audio.
#[derive(Debug, Clone, Default)]
pub struct RuntimeMonitor {
    /// Selected source when no Listen is engaged. Defaults to the master bus.
    pub source: crate::monitor::MonitorSource,
    /// [`Self::source`] resolved to a track index for `Bus` / `TrackPreFader` /
    /// `TrackAfterFader`. `None` for `MasterBus` and `HardwareInput`.
    pub source_track_index: Option<usize>,
    /// Which stage of `source_track_index` to tap.
    pub source_stage: Option<crate::monitor::TapStage>,
    pub control: crate::monitor::MonitorControl,
    pub output: crate::monitor::MonitorOutputTarget,
    /// Which stage owns the physical write this configuration. Resolved on the
    /// control thread; the callback only obeys it.
    pub hardware_owner: crate::monitor::HardwareOutputOwner,
    /// Master's own destination as resolved `(left, right)` device channels.
    /// `None` means Master has no hardware destination at all.
    ///
    /// The graph always renders the master mix into device channels 0/1, so
    /// this is where that feed is *moved to* when Master owns the write, and
    /// which channels are silenced when the Control Room owns it instead. A
    /// plain integer pair, so applying it allocates nothing.
    pub master_output: Option<(u16, u16)>,
    /// Monitor-only insert chain. Never reached by export.
    pub inserts: Vec<RuntimeInsert>,
    /// The listening simulation (Virtual Speaker), after the monitor inserts
    /// and before the monitor control. Built with the graph; `None` only in
    /// a default-constructed monitor.
    pub simulator: Option<Box<solfege_spatialaudio::ListeningSimulator>>,
    /// Scratch holding the routed non-master source for this block.
    pub source_l: Vec<f32>,
    pub source_r: Vec<f32>,
    /// Scratch accumulating every engaged PFL/AFL tap for this block.
    pub listen_l: Vec<f32>,
    pub listen_r: Vec<f32>,
    /// Set by the graph pass when at least one channel contributed a Listen
    /// tap this block. When false the Control Room plays `source` — which for
    /// the default configuration is the master bus.
    pub listen_active: bool,
    /// Set by the graph pass when the routed source tap actually captured.
    /// Distinguishes "the selected bus produced silence" from "the selected
    /// bus no longer exists", which must fall back to the master bus.
    pub source_captured: bool,
}

impl RuntimeMonitor {
    /// Grow the Control Room scratch buffers to hold `frames`. Control-thread
    /// only — the callback never resizes and instead degrades to plain master
    /// output if capacity is short.
    pub fn ensure_block_capacity(&mut self, frames: usize) {
        if self.source_l.len() >= frames {
            return;
        }
        self.source_l.resize(frames, 0.0);
        self.source_r.resize(frames, 0.0);
        self.listen_l.resize(frames, 0.0);
        self.listen_r.resize(frames, 0.0);
    }

    /// Whether the callback can run the Control Room for a block of `frames`.
    pub fn has_block_capacity(&self, frames: usize) -> bool {
        self.source_l.len() >= frames
            && self.source_r.len() >= frames
            && self.listen_l.len() >= frames
            && self.listen_r.len() >= frames
    }

    /// Zero the per-block scratch and clear the capture flags. Called once at
    /// the top of every render block.
    pub fn begin_block(&mut self, frames: usize) {
        self.listen_active = false;
        self.source_captured = false;
        for i in 0..frames.min(self.listen_l.len()) {
            self.listen_l[i] = 0.0;
            self.listen_r[i] = 0.0;
            self.source_l[i] = 0.0;
            self.source_r[i] = 0.0;
        }
    }
}

/// A channel's Dual pan on the audio thread.
///
/// `settings` is what the mixer asked for (live-updated by
/// `SetTrackDualPan`); `matrix` is the 2x2 gain, volume included, that the
/// fader applied at the end of the last block that ran the matrix path, in
/// [`crate::dsp::gain::dual_pan_matrix`]'s layout. `engaged` says whether the
/// last block did run it: the fader uses it to ramp into and out of Dual from
/// the balance gains instead of jumping. Plain `Copy` data, no allocation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RuntimeDualPan {
    pub settings: crate::types::EngineDualPan,
    pub matrix: [f32; 4],
    pub engaged: bool,
}

impl Default for RuntimeDualPan {
    fn default() -> Self {
        Self::new(crate::types::EngineDualPan::default())
    }
}

impl RuntimeDualPan {
    pub fn new(settings: crate::types::EngineDualPan) -> Self {
        Self {
            settings: settings.sanitized(),
            matrix: [1.0, 0.0, 0.0, 1.0],
            engaged: false,
        }
    }

    /// Built already at `settings`' matrix scaled by `gain` (the channel's
    /// volume), so a Dual channel's first block plays at its level with no
    /// ramp — the same seeding the balance smoother gets.
    pub fn seeded(settings: crate::types::EngineDualPan, gain: f32) -> Self {
        let mut dual = Self::new(settings);
        if dual.settings.enabled {
            let m = crate::dsp::gain::dual_pan_matrix(dual.settings.left, dual.settings.right);
            dual.matrix = m.map(|g| g * gain);
            dual.engaged = true;
        }
        dual
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeTrack {
    pub id: String,
    pub track_type: String,
    /// SoundFont voices this track was sounding at the end of the last rendered
    /// block. Written by the audio thread only, summed once per callback into
    /// the shared atomic the transport's load meter reads. A plain field, not an
    /// atomic: nothing outside the callback ever touches it.
    pub active_voices: u32,
    /// Pre/After-Fader Listen state for this channel. Affects the Control Room
    /// only — it never changes what this track contributes to the master mix,
    /// so engaging Listen cannot alter an export or a recording.
    pub listen: crate::monitor::ListenMode,
    pub volume: f32,
    pub pan: f32,
    pub muted: bool,
    pub solo: bool,
    pub record_armed: bool,
    pub monitor_enabled: bool,
    pub input_source: RuntimeTrackInputSource,
    /// Audio Jam publish slot this track's post-fader output feeds, if any.
    ///
    /// Resolved on the control thread and shipped as an index, so the callback
    /// never looks a key up. `None` for every track that is not being shared,
    /// which is almost all of them almost all of the time.
    pub jam_publish_slot: Option<u32>,
    /// Channel pair this track occupies inside the Audio Jam multitrack stream,
    /// if the arrangement is being shared.
    ///
    /// Separate from [`Self::jam_publish_slot`] because the two are different
    /// streams: a track can be shared on its own *and* be a pair inside the
    /// multitrack take, and the receiver sees two independent streams. Like the
    /// slot, the pair is resolved on the control thread and shipped as an index.
    pub jam_multitrack_pair: Option<u32>,
    /// Whether some other track's input reads this track's output, so the
    /// render pass keeps a copy of its post-fader block for the next callback.
    ///
    /// A `bool` rather than a slot table: the copy lives on this track, so
    /// there is nothing to index and nothing to allocate beyond the two buffers
    /// below. Resolved on the control thread whenever an input route changes.
    pub loopback_publish: bool,
    /// This track's post-fader block from the **previous** callback, kept only
    /// while [`Self::loopback_publish`] is set.
    ///
    /// Grown to the block size on first use and never shrunk, so the steady
    /// state is a `copy_from_slice` and nothing else.
    pub loopback_out_l: Vec<f32>,
    pub loopback_out_r: Vec<f32>,
    pub preview_mode: RuntimePreviewMode,
    pub output_track_id: Option<String>,
    /// [`Self::output_track_id`] resolved at build time
    /// ([`RuntimeProject::resolve_indices`]): `Some(index)` only when the id
    /// names an existing non-master track. The render path must never do a
    /// per-block id lookup.
    pub output_track_index: Option<usize>,
    pub inserts: Vec<RuntimeInsert>,
    pub sends: Vec<RuntimeSend>,
    pub automation_lanes: Vec<RuntimeAutomationLane>,
    /// Resolved plugin-parameter automation routes for this track, rebuilt by
    /// [`RuntimeProject::resolve_indices`]. Empty for tracks with no plugin
    /// parameter lanes, so the common render path bails immediately.
    pub plugin_param_automation: Vec<RuntimePluginParamBinding>,
    /// Where this block's automation is read, per stage; stamped at the
    /// start of every block (see [`RuntimeAutomationClock`]).
    pub automation_clock: RuntimeAutomationClock,
    pub meter: Arc<RuntimeTrackMeter>,
    pub meter_peak_l: f32,
    pub meter_peak_r: f32,
    pub meter_sum_sq_l: f32,
    pub meter_sum_sq_r: f32,
    pub callback_insert_log_done: bool,
    pub callback_clip_route_log_done: bool,
    pub block_l: Vec<f32>,
    pub block_r: Vec<f32>,
    /// Send-receive accumulation buffers (Phase 3). Sends from other tracks
    /// sum into these; routing tracks (bus/return) then process this as their
    /// input. Preallocated alongside `block_*` so the audio callback never
    /// allocates. Zeroed at the top of each render block.
    pub recv_l: Vec<f32>,
    pub recv_r: Vec<f32>,
    /// Scratch buffers for built-in SoundFont rendering; preallocated for the
    /// callback block size and mixed into `block_*`.
    pub soundfont_l: Vec<f32>,
    pub soundfont_r: Vec<f32>,
    /// ARA playback renderers assigned to this track.
    ///
    /// Clips these renderers own are skipped by the clip loop: their audio comes
    /// from the plug-in, which read the source samples out of band through the
    /// ARA host callbacks. Resolved on the control thread and shipped inside the
    /// `LoadProject` snapshot, so the callback only iterates.
    pub ara_renderers: Vec<RuntimeAraRenderer>,
    /// Scratch for ARA renderer output plus the silent input those renderers are
    /// fed. Preallocated beside `soundfont_*` and mixed into `block_*`.
    pub ara_l: Vec<f32>,
    pub ara_r: Vec<f32>,
    pub ara_silence: Vec<f32>,
    /// The ARA renderers' share of `plugin_latency_samples`, tracked separately
    /// so replacing the renderer set can subtract exactly what it added instead
    /// of forcing a whole graph rebuild.
    pub ara_latency_samples: u32,
    /// Where this channel sits in the square room when the mix is spatial
    /// (surround or binaural). Live-updated by `SetTrackSpatial`.
    pub spatial_params: solfege_spatialaudio::SourceParams,
    /// The channel's spatialiser, when the mix is spatial and this channel
    /// sums into the master: it replaces the stereo pan on that last hop.
    /// `None` in a stereo mix and for channels routed into a bus (the bus is
    /// placed instead). Preallocated by [`RuntimeProject::ensure_spatial`].
    pub spatial: Option<solfege_spatialaudio::SpatialSource>,
    /// The rest of the channel's spatial state: where automation puts it this
    /// block, and — on a bus that keeps its channels' places — the multichannel
    /// field those channels are rendered into.
    pub spatial_state: RuntimeTrackSpatial,
    /// Per-block MIDI events for the instrument VST3 insert (Phase 2B).
    /// Cleared at the start of `schedule_midi_block`; no steady-path allocation.
    pub midi_block_events: Vec<Vst3MidiEvent>,
    /// Per-block continuous-pitch targets for the Solfege instrument, kept
    /// beside `midi_block_events` rather than inside it because a `Vst3MidiEvent`
    /// is a VST3 wire struct with no room for an absolute frequency, and
    /// widening it would change an FFI type every bridged plugin sees.
    ///
    /// `(sample_offset, note_id, hz)`. Cleared with the MIDI list each block and
    /// reserved to the callback capacity at build time, so the realtime path
    /// only ever pushes into existing capacity.
    pub solfege_pitch_events: Vec<(u32, i32, f32)>,
    /// Per-block articulation selections for the Solfege instrument, alongside
    /// `solfege_pitch_events` and for the same reason.
    ///
    /// `(sample_offset, note_id, voicebank articulation id)`.
    pub solfege_articulation_events: Vec<(u32, i32, u16)>,
    /// MIDI FX between this track's MIDI and its instrument (see
    /// [`RuntimeMidiFx`]). When set, every event for the instrument goes
    /// through `midi_block_events`, never straight to a bridge ring.
    pub midi_fx: Option<Arc<RuntimeMidiFx>>,
    /// Events held over a block in which the MIDI FX chain was locked by an
    /// edit. Reserved with `midi_block_events`.
    pub midi_fx_hold: Vec<Vst3MidiEvent>,
    /// The hosted plug-ins in `midi_fx`, in chain order, with their bridge
    /// sinks (resolved like an insert's, without locking the chain).
    pub midi_fx_plugins: RuntimeMidiFxPlugins,
    /// Index into `inserts` of the first instrument-capable native VST3 insert.
    pub midi_instrument_insert_ix: Option<usize>,
    /// Built-in RustySynth SoundFont instrument for Instrument tracks.
    pub soundfont_player: Option<RuntimeSoundfontPlayer>,
    /// Native Solfege physical/hybrid instrument for Instrument tracks.
    pub solfege_engine: Option<RuntimeSolfegeEngine>,
    /// Sum of enabled insert latencies at build time (Phase V/W reporting).
    pub plugin_latency_samples: u32,
    /// Ring buffers for PDC on post-fader output (preallocated at build).
    pub pdc_delay_l: Vec<f32>,
    pub pdc_delay_r: Vec<f32>,
    pub pdc_write_pos: usize,
    /// Smoothed per-channel fader gain (volume × pan) actually applied to audio.
    /// The applied gain ramps from these values toward the new target across
    /// each block so playback and offline export handle fader automation alike.
    /// Initialized to the build-time target to avoid a startup fade. When
    /// [`RuntimeProject::fader_smoothing`] is off, the target applies directly.
    /// Linear gain the track's VCA group puts on its fader, automation
    /// included; `1.0` outside a VCA. See [`crate::types::EngineTrackSnapshot::vca_gain`].
    pub vca_gain: f32,
    pub smoothed_gain_l: f32,
    pub smoothed_gain_r: f32,
    /// Dual pan: its settings and the matrix the fader last applied. Inert
    /// (and the fader bit-identical to the balance path) while it is off.
    pub dual_pan: RuntimeDualPan,
    /// The return this track's wet/dry/wet inserts
    /// ([`RuntimeInsert::signal_bus`] = [`SIGNAL_BUS_WDW`]) run on, or `None`
    /// while the split is not live: no enabled wet/dry/wet send, or its
    /// return is gone. Those inserts then run in this track's own chain, so a
    /// plug-in left on the W bus is never silenced. Resolved with the other
    /// routing indices ([`crate::latency_graph::resolve_latency_routing_indices`]),
    /// so the render pass and delay compensation read one answer.
    pub wdw_return_index: Option<usize>,
    /// What this track publishes as a sidechain key for another track's
    /// insert; empty unless one keys from it. See [`super::sidechain`].
    pub sidechain_tap: RuntimeSidechainTap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeTrackInputSource {
    None,
    Mono {
        channel: usize,
    },
    Stereo {
        left: usize,
        right: usize,
    },
    /// An Audio Jam stream, resolved to a slot in the engine's
    /// [`crate::jam_bus::JamAudioBus`].
    ///
    /// The slot index is resolved on the control thread and baked into the
    /// runtime snapshot, so the callback never does a string lookup. A track
    /// bound to a jam stream takes no hardware input: the two are different
    /// sources, and mixing both into one track would make a remote performer
    /// double with whatever is plugged into the interface.
    Jam {
        slot: u32,
        mode: crate::jam_bus::JamChannelMode,
    },
    /// Another track's post-fader output — an instrument track feeding an audio
    /// track, so a VSTi can be monitored and recorded as audio without a
    /// bounce. See [`crate::loopback`].
    ///
    /// The source is a **runtime track index**, resolved on the control thread
    /// exactly like a jam slot, so the callback never looks an id up. What it
    /// reads is the block the source rendered in the previous callback; see the
    /// module docs for why that one-block delay is the point rather than a
    /// shortcoming.
    Loopback {
        source_index: u32,
        mode: crate::jam_bus::JamChannelMode,
    },
}

impl RuntimeTrackInputSource {
    pub(crate) fn from_channels(channels: &[u32]) -> Self {
        match channels {
            [] => Self::None,
            [channel] => Self::Mono {
                channel: *channel as usize,
            },
            [left, right, ..] => Self::Stereo {
                left: *left as usize,
                right: *right as usize,
            },
        }
    }

    /// Build the runtime source from a resolved route.
    ///
    /// `jam_slot` is `Some` only when the Audio Connection behind this track
    /// bound a jam device and the bus had a slot for it. An unresolved jam
    /// route yields [`Self::None`] — the track captures nothing and says so,
    /// rather than falling back to an unrelated hardware input.
    pub(crate) fn from_route(channels: &[u32], jam_slot: Option<u32>) -> Self {
        match jam_slot {
            Some(slot) => Self::Jam {
                slot,
                mode: crate::jam_bus::JamChannelMode::from_channels(channels),
            },
            None => Self::from_channels(channels),
        }
    }

    #[inline]
    pub fn is_routable(&self) -> bool {
        !matches!(self, Self::None)
    }

    /// Whether this route reads a remote jam stream rather than the hardware
    /// input bus.
    #[inline]
    pub fn is_jam(&self) -> bool {
        matches!(self, Self::Jam { .. })
    }

    /// Whether this route reads another track's output rather than the hardware
    /// input bus.
    #[inline]
    pub fn is_loopback(&self) -> bool {
        matches!(self, Self::Loopback { .. })
    }

    /// Whether this route takes nothing from the hardware input bus.
    ///
    /// Both virtual sources answer yes, and the render path asks this rather
    /// than naming them one by one: a track fed from somewhere else must not
    /// also sum whatever is plugged into the interface, or a remote performer —
    /// or an instrument — doubles with the room mic.
    #[inline]
    pub fn is_internal(&self) -> bool {
        matches!(self, Self::Jam { .. } | Self::Loopback { .. })
    }

    #[inline]
    pub fn sample_from_latest(&self, latest_l: f32, latest_r: f32) -> (f32, f32) {
        match self {
            Self::None => (0.0, 0.0),
            // A jam or loopback track is not fed by the hardware input bus, so
            // the live input meter has nothing to show for it. Its level comes
            // from the track's own post-fader meter instead.
            Self::Jam { .. } | Self::Loopback { .. } => (0.0, 0.0),
            Self::Mono { .. } => (latest_l, latest_l),
            Self::Stereo { .. } => (latest_l, latest_r),
        }
    }

    #[inline]
    pub fn sample_from_monitor_pair(
        &self,
        latest_l: f32,
        latest_r: f32,
        monitor_source: (u32, u32),
    ) -> (f32, f32) {
        let monitor_source = (monitor_source.0 as usize, monitor_source.1 as usize);
        let pick = |channel: usize| {
            if channel == monitor_source.0 {
                latest_l
            } else if channel == monitor_source.1 {
                latest_r
            } else {
                0.0
            }
        };
        match self {
            Self::None | Self::Jam { .. } | Self::Loopback { .. } => (0.0, 0.0),
            Self::Mono { channel } => {
                let mono = pick(*channel);
                (mono, mono)
            }
            Self::Stereo { left, right } => (pick(*left), pick(*right)),
        }
    }
}

impl RuntimeTrack {
    #[inline]
    pub fn automation_values_at_beat(&self, beat: f64) -> RuntimeTrackAutomationValues {
        let mut values = RuntimeTrackAutomationValues::default();
        for lane in &self.automation_lanes {
            let Some(value) = lane.evaluate_normalized(beat) else {
                continue;
            };
            match lane.target {
                RuntimeAutomationTarget::TrackVolume => {
                    values.volume = Some(volume_norm_to_linear(value));
                }
                RuntimeAutomationTarget::TrackPan => {
                    values.pan = Some((value * 2.0 - 1.0).clamp(-1.0, 1.0));
                }
                RuntimeAutomationTarget::TrackPanLeft => {
                    values.pan_left = Some((value * 2.0 - 1.0).clamp(-1.0, 1.0));
                }
                RuntimeAutomationTarget::TrackPanRight => {
                    values.pan_right = Some((value * 2.0 - 1.0).clamp(-1.0, 1.0));
                }
                RuntimeAutomationTarget::TrackMute => {
                    values.muted = Some(value >= 0.5);
                }
                RuntimeAutomationTarget::Spatial(param) => {
                    values.spatial[param.index()] = Some(value);
                }
                _ => {}
            }
        }
        values
    }
}

impl RuntimeTrack {
    /// Send `s`'s on/off and level at `beat`: its own, unless an automation
    /// lane that has something to say overrides it. Index lookups only — the
    /// lanes were resolved off the audio thread.
    #[inline]
    pub fn send_state_at(&self, s: usize, beat: f64) -> (bool, f32) {
        let Some(send) = self.sends.get(s) else {
            return (false, 0.0);
        };
        let lane_value = |lane: Option<usize>| {
            lane.and_then(|ix| self.automation_lanes.get(ix))
                .and_then(|lane| lane.evaluate_normalized(beat))
        };
        let enabled = lane_value(send.enabled_lane)
            .map(|v| v >= 0.5)
            .unwrap_or(send.enabled);
        let level = lane_value(send.level_lane)
            .map(volume_norm_to_linear)
            .unwrap_or(send.level);
        (enabled, level)
    }
}

/// Point each send and insert at the automation lane that targets it, so the
/// render path reads an index instead of comparing ids. Off the audio thread.
/// An empty or disabled lane still resolves: it says nothing per block
/// ([`RuntimeAutomationLane::evaluate_normalized`]) and the control's own value
/// stands.
pub(crate) fn resolve_send_and_bypass_lanes(track: &mut RuntimeTrack) {
    let (send_lanes, bypass_lanes) =
        lane_indices(&track.automation_lanes, &track.sends, &track.inserts);
    for (send, (level, enabled)) in track.sends.iter_mut().zip(send_lanes) {
        send.level_lane = level;
        send.enabled_lane = enabled;
    }
    for (insert, lane) in track.inserts.iter_mut().zip(bypass_lanes) {
        insert.bypass_lane = lane;
        insert.automation_bypass = None;
    }
}

/// Resolve a track's plugin-parameter automation lanes into compact bindings.
/// Runs off the audio thread (LoadProject / SetPluginBridgeSink); see
/// [`plugin_param_bindings`].
pub(super) fn build_plugin_param_bindings(track: &RuntimeTrack) -> Vec<RuntimePluginParamBinding> {
    plugin_param_bindings(&track.automation_lanes, &track.inserts)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimePreviewMode {
    Stereo,
    Mono,
    Mid,
    Side,
}

impl RuntimePreviewMode {
    #[inline]
    pub fn from_str(value: &str) -> Self {
        match value {
            "mono" => Self::Mono,
            "mid" => Self::Mid,
            "side" => Self::Side,
            _ => Self::Stereo,
        }
    }

    #[inline]
    pub fn from_code(value: f32) -> Self {
        match value as i32 {
            1 => Self::Mono,
            2 => Self::Mid,
            3 => Self::Side,
            _ => Self::Stereo,
        }
    }
}

#[derive(Debug, Default)]
pub struct RuntimeTrackMeter {
    pub(super) peak_l: AtomicU32,
    pub(super) peak_r: AtomicU32,
    pub(super) rms_l: AtomicU32,
    pub(super) rms_r: AtomicU32,
    /// This track's post-fader block goes to the visualizer track tap
    /// (`crate::visualizer_tap::track_tap`). Set by the control side; the
    /// meter is shared with the graph the callback renders, so the flag
    /// reaches it without a command. Read once per block.
    pub(crate) visualizer_tap: AtomicBool,
}

#[derive(Debug, Clone)]
pub struct RuntimeTrackMeterSnapshot {
    pub track_id: String,
    pub peak_l: f32,
    pub peak_r: f32,
    pub rms_l: f32,
    pub rms_r: f32,
}

#[derive(Debug, Clone)]
pub struct RuntimePluginOutputMeterSnapshot {
    pub track_id: String,
    pub insert_id: String,
    pub channel: u8,
    pub peak: f32,
}

/// A block peak as bits that order like the level: a non-negative finite
/// `f32`'s bit pattern grows with its value, so `fetch_max` on the bits is a
/// lock-free max of the levels. Anything else (a NaN from a misbehaving
/// plug-in) counts as silence rather than pinning the meter.
#[inline]
pub(super) fn peak_bits(peak: f32) -> u32 {
    if peak.is_finite() && peak > 0.0 {
        f32_store(peak)
    } else {
        0
    }
}

impl RuntimeTrackMeter {
    /// Publish one block. The peaks are the highest since the meter was last
    /// taken, not this block's: the UI reads at display rate, a block is a few
    /// milliseconds, and storing each block's own peak meant a read saw
    /// whichever block happened to be last — every peak between two reads was
    /// lost, and the bar jumped between whatever it caught. RMS stays the last
    /// block's. Realtime-safe: two atomic max operations and two stores.
    #[inline]
    pub(super) fn store(&self, peak_l: f32, peak_r: f32, rms_l: f32, rms_r: f32) {
        self.peak_l.fetch_max(peak_bits(peak_l), Ordering::Relaxed);
        self.peak_r.fetch_max(peak_bits(peak_r), Ordering::Relaxed);
        self.rms_l.store(f32_store(rms_l), Ordering::Relaxed);
        self.rms_r.store(f32_store(rms_r), Ordering::Relaxed);
    }

    /// Read the meter and start the next peak window: the peaks since the
    /// last take, reset to silence. For the one display reader; diagnostics
    /// use [`Self::load`], which leaves them.
    #[inline]
    pub(crate) fn take(&self, track_id: &str) -> RuntimeTrackMeterSnapshot {
        RuntimeTrackMeterSnapshot {
            track_id: track_id.to_string(),
            peak_l: f32_load(self.peak_l.swap(0, Ordering::Relaxed)),
            peak_r: f32_load(self.peak_r.swap(0, Ordering::Relaxed)),
            rms_l: f32_load(self.rms_l.load(Ordering::Relaxed)),
            rms_r: f32_load(self.rms_r.load(Ordering::Relaxed)),
        }
    }

    #[inline]
    pub(crate) fn load(&self, track_id: &str) -> RuntimeTrackMeterSnapshot {
        RuntimeTrackMeterSnapshot {
            track_id: track_id.to_string(),
            peak_l: f32_load(self.peak_l.load(Ordering::Relaxed)),
            peak_r: f32_load(self.peak_r.load(Ordering::Relaxed)),
            rms_l: f32_load(self.rms_l.load(Ordering::Relaxed)),
            rms_r: f32_load(self.rms_r.load(Ordering::Relaxed)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeSend {
    pub id: String,
    pub return_track_id: String,
    /// [`Self::return_track_id`] resolved to a track index at build time
    /// ([`RuntimeProject::resolve_indices`]) — `None` when the target track
    /// does not exist. The render path must never do a per-block id lookup.
    pub return_track_index: Option<usize>,
    pub level: f32,
    pub enabled: bool,
    /// Pre-fader tap (Phase 3). See [`EngineSendSnapshot::pre_fader`].
    pub pre_fader: bool,
    /// Wet/dry/wet split. The return runs the source inserts whose
    /// [`RuntimeInsert::signal_bus`] is [`SIGNAL_BUS_WDW`].
    pub wdw: bool,
    /// The owning track's Send Level automation lane for this send, resolved
    /// to an index into its `automation_lanes` off the audio thread.
    pub level_lane: Option<usize>,
    /// The owning track's Send On/Off automation lane for this send, likewise.
    pub enabled_lane: Option<usize>,
    /// Delay ring aligning this feed with the return's other feeds
    /// (`RuntimeLatencyGraph::send_pdc_delay`). Sized with the track rings.
    pub pdc: crate::latency_graph::PdcDelayLine,
}
