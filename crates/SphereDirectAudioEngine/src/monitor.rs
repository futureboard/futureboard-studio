//! Control Room / Listen Bus — the monitoring path that sits *after* the
//! master bus and feeds the physical monitoring output.
//!
//! ```txt
//! Tracks / Instruments / Aux / Returns / Groups
//!     -> Master Bus (+ master inserts, master fader)
//!     -> Monitor Source Router
//!     -> Listen (PFL/AFL) override
//!     -> Monitor Insert Chain
//!     -> Monitor Control (mono -> dim -> gain -> mute)
//!     -> Selected hardware output pair
//! ```
//!
//! Two properties are structural rather than enforced by checks:
//!
//! * **Playback-only.** Every stage here runs inside the realtime device
//!   callback (`backend::render::fill_output_f32_inner`). Offline export, stem
//!   export, and recording all render through
//!   [`crate::engine::render_project_block_interleaved_with_taps`], which never
//!   enters the device callback — so no Control Room stage can reach exported
//!   or recorded audio. (Requirement: monitor gain/FX must not affect export.)
//!
//! * **No duplicate output path.** The Control Room *replaces* the samples on
//!   its target output pair instead of summing into them, and the master mix is
//!   written to the device buffer exactly once by the graph. The same hardware
//!   output therefore can never carry both the raw master feed and the
//!   monitored feed.
//!
//! The monitor bus is deliberately **not** bound to a hardware input. A
//! hardware input is only one selectable source among several, and is never the
//! default — see [`MonitorSource`].

/// Where the Control Room takes its signal from when no Listen (PFL/AFL) is
/// engaged.
///
/// [`MonitorSource::MasterBus`] is the default and is what a DAW monitors
/// almost all of the time: the complete internal mix — audio tracks,
/// instrument tracks, aux/send buses, return channels, group buses, and master
/// insert processing — because it is tapped from the master bus output rather
/// than from any individual source.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum MonitorSource {
    /// The full master bus output, post master inserts and master fader.
    #[default]
    MasterBus,
    /// A group / aux / return bus, tapped post-fader.
    Bus(String),
    /// One track tapped *before* its channel fader (but after its inserts).
    TrackPreFader(String),
    /// One track tapped *after* its channel fader and channel processing.
    TrackAfterFader(String),
    /// A hardware capture input. Never the default: selecting this is an
    /// explicit user action, and until it is selected the engine does not open
    /// or read a capture device on the Control Room's behalf at all.
    HardwareInput(String),
}

impl MonitorSource {
    /// Stable tag used for snapshot/persistence round-trips.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::MasterBus => "master",
            Self::Bus(_) => "bus",
            Self::TrackPreFader(_) => "track-pfl",
            Self::TrackAfterFader(_) => "track-afl",
            Self::HardwareInput(_) => "hardware-input",
        }
    }

    /// The referenced entity id, if this variant names one.
    pub fn target_id(&self) -> Option<&str> {
        match self {
            Self::MasterBus => None,
            Self::Bus(id)
            | Self::TrackPreFader(id)
            | Self::TrackAfterFader(id)
            | Self::HardwareInput(id) => Some(id.as_str()),
        }
    }

    /// Rebuild from a `(tag, id)` pair. Unknown tags fall back to the master
    /// bus so a corrupt or future-versioned project still monitors correctly
    /// rather than going silent.
    pub fn from_parts(tag: &str, id: Option<&str>) -> Self {
        match (tag, id) {
            ("bus", Some(id)) => Self::Bus(id.to_string()),
            ("track-pfl", Some(id)) => Self::TrackPreFader(id.to_string()),
            ("track-afl", Some(id)) => Self::TrackAfterFader(id.to_string()),
            ("hardware-input", Some(id)) => Self::HardwareInput(id.to_string()),
            _ => Self::MasterBus,
        }
    }

    /// True when this source reads a capture device. The engine uses this to
    /// decide whether the Control Room needs live input at all — requirement:
    /// the microphone must not be activated unless explicitly selected.
    pub fn needs_hardware_input(&self) -> bool {
        matches!(self, Self::HardwareInput(_))
    }
}

/// Which stage performs the physical output write.
///
/// Compiled on the control thread from the project's Master / Monitor output
/// assignments and handed to the runtime already decided, so the callback never
/// reasons about ownership — it just does what it was told. Exactly one stage
/// writes, which is what keeps the master feed and its monitored copy from
/// summing into one destination at +6 dB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HardwareOutputOwner {
    /// The Control Room is out of the path; Master writes its own destination.
    MasterDirect,
    /// The Control Room owns the write. Master's direct feed is silenced on any
    /// channel the monitoring destination does not cover.
    #[default]
    MonitorControlRoom,
    /// Nothing resolves. The device buffer is silent — never a fallback pair.
    None,
}

/// Per-channel Listen state. PFL and AFL differ only in where the tap sits
/// relative to the channel fader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListenMode {
    #[default]
    Off,
    /// Pre-Fader Listen: taps after the channel's inserts but *before* its
    /// fader, so the monitored level is independent of the fader position.
    Pfl,
    /// After-Fader Listen: taps after the channel's processing *and* its
    /// fader, so the monitored level follows the fader.
    Afl,
}

impl ListenMode {
    pub fn tag(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Pfl => "pfl",
            Self::Afl => "afl",
        }
    }

    pub fn from_tag(tag: &str) -> Self {
        match tag {
            "pfl" => Self::Pfl,
            "afl" => Self::Afl,
            _ => Self::Off,
        }
    }

    pub fn is_active(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// Which stage of a channel a tap reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapStage {
    /// After inserts, before the fader.
    PreFader,
    /// After the fader (and channel preview-mode processing).
    PostFader,
}

/// Attenuation applied by the Dim button, in dB. Matches the conventional
/// console default; a fixed value keeps Dim a predictable reference gesture
/// rather than another level to manage.
pub const DIM_ATTENUATION_DB: f32 = -20.0;

/// Linear gain corresponding to [`DIM_ATTENUATION_DB`].
pub fn dim_gain() -> f32 {
    10.0_f32.powf(DIM_ATTENUATION_DB / 20.0)
}

/// The Control Room's level/shape controls. These affect monitoring only.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MonitorControl {
    /// Linear monitor gain.
    pub gain: f32,
    pub mute: bool,
    pub dim: bool,
    /// Sum to mono for mono-compatibility checking.
    pub mono: bool,
}

impl Default for MonitorControl {
    fn default() -> Self {
        Self {
            gain: 1.0,
            mute: false,
            dim: false,
            mono: false,
        }
    }
}

impl MonitorControl {
    /// Effective linear gain including mute and dim.
    pub fn effective_gain(&self) -> f32 {
        if self.mute {
            return 0.0;
        }
        let dim = if self.dim { dim_gain() } else { 1.0 };
        self.gain.max(0.0) * dim
    }
}

/// Apply the monitor control processor to one stereo block, in place.
///
/// Order is mono-sum → dim → gain → mute, matching a hardware control room:
/// mono folding happens before level so the dim/gain reference does not shift
/// when mono is toggled. Allocation-free and branch-stable; safe to call from
/// the audio callback.
pub fn apply_monitor_control(block_l: &mut [f32], block_r: &mut [f32], control: &MonitorControl) {
    let frames = block_l.len().min(block_r.len());
    if frames == 0 {
        return;
    }
    let gain = control.effective_gain();
    if control.mono {
        for i in 0..frames {
            // -6 dB centre sum keeps a correlated stereo pair at the same
            // apparent level when folded.
            let mono = (block_l[i] + block_r[i]) * 0.5;
            block_l[i] = mono * gain;
            block_r[i] = mono * gain;
        }
    } else {
        for i in 0..frames {
            block_l[i] *= gain;
            block_r[i] *= gain;
        }
    }
}

/// A stereo hardware output pair on the active output device.
///
/// The Control Room targets a *channel pair* rather than a separate device:
/// the backend owns exactly one output stream, so "Headphones" and an
/// alternate "Speaker Set" are additional channel pairs on that stream (Out
/// 3-4, Out 5-6, …), not independent devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorOutputTarget {
    /// Human-readable name shown in the Control Room's Output selector.
    pub name: String,
    /// 0-based index of the left channel; right is `left_channel + 1`.
    pub left_channel: u16,
}

impl Default for MonitorOutputTarget {
    fn default() -> Self {
        Self {
            name: "Out 1-2".to_string(),
            left_channel: 0,
        }
    }
}

impl MonitorOutputTarget {
    pub fn new(name: impl Into<String>, left_channel: u16) -> Self {
        Self {
            name: name.into(),
            left_channel,
        }
    }

    pub fn right_channel(&self) -> u16 {
        self.left_channel.saturating_add(1)
    }

    /// Whether this target fits inside a device with `channels` outputs.
    pub fn fits(&self, channels: usize) -> bool {
        (self.right_channel() as usize) < channels
    }

    /// The channel pair to actually write, clamped into a device that may have
    /// fewer channels than the stored selection expects. Falls back to the
    /// main pair so a monitor selection made against a larger interface never
    /// silences monitoring after switching devices.
    pub fn resolved_pair(&self, channels: usize) -> Option<(usize, usize)> {
        if channels < 2 {
            return None;
        }
        if self.fits(channels) {
            Some((self.left_channel as usize, self.right_channel() as usize))
        } else {
            Some((0, 1))
        }
    }

    /// Standard pairs offered for a device with `channels` outputs. Always
    /// includes the main pair; further pairs are named by their 1-based
    /// channel numbers.
    pub fn available_for_device(channels: usize) -> Vec<Self> {
        let mut targets = vec![Self::default()];
        let mut left = 2u16;
        while (left as usize + 1) < channels {
            targets.push(Self::new(format!("Out {}-{}", left + 1, left + 2), left));
            left += 2;
        }
        targets
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_source_is_the_master_bus_not_a_hardware_input() {
        let source = MonitorSource::default();
        assert_eq!(source, MonitorSource::MasterBus);
        assert!(!source.needs_hardware_input());
    }

    #[test]
    fn only_an_explicit_hardware_input_selection_requests_capture() {
        assert!(!MonitorSource::MasterBus.needs_hardware_input());
        assert!(!MonitorSource::Bus("bus-1".into()).needs_hardware_input());
        assert!(!MonitorSource::TrackPreFader("t1".into()).needs_hardware_input());
        assert!(!MonitorSource::TrackAfterFader("t1".into()).needs_hardware_input());
        assert!(MonitorSource::HardwareInput("in-1".into()).needs_hardware_input());
    }

    #[test]
    fn source_round_trips_through_its_snapshot_parts() {
        for source in [
            MonitorSource::MasterBus,
            MonitorSource::Bus("bus-1".into()),
            MonitorSource::TrackPreFader("track-2".into()),
            MonitorSource::TrackAfterFader("track-3".into()),
            MonitorSource::HardwareInput("Focusrite In 1".into()),
        ] {
            let rebuilt = MonitorSource::from_parts(source.tag(), source.target_id());
            assert_eq!(rebuilt, source);
        }
    }

    #[test]
    fn unknown_source_tag_falls_back_to_master_rather_than_silence() {
        assert_eq!(
            MonitorSource::from_parts("some-future-variant", Some("x")),
            MonitorSource::MasterBus
        );
    }

    #[test]
    fn mute_overrides_gain_and_dim_attenuates() {
        let mut control = MonitorControl {
            gain: 1.0,
            mute: true,
            dim: false,
            mono: false,
        };
        assert_eq!(control.effective_gain(), 0.0);

        control.mute = false;
        assert!((control.effective_gain() - 1.0).abs() < 1.0e-6);

        control.dim = true;
        assert!((control.effective_gain() - dim_gain()).abs() < 1.0e-6);
        // -20 dB is a tenth of the linear amplitude.
        assert!((dim_gain() - 0.1).abs() < 1.0e-4);
    }

    #[test]
    fn mono_folds_both_channels_to_their_average() {
        let mut l = vec![1.0f32, 0.5];
        let mut r = vec![0.0f32, -0.5];
        apply_monitor_control(
            &mut l,
            &mut r,
            &MonitorControl {
                gain: 1.0,
                mute: false,
                dim: false,
                mono: true,
            },
        );
        assert_eq!(l, r);
        assert!((l[0] - 0.5).abs() < 1.0e-6);
        assert!((l[1] - 0.0).abs() < 1.0e-6);
    }

    #[test]
    fn output_target_falls_back_to_the_main_pair_on_a_smaller_device() {
        let headphones = MonitorOutputTarget::new("Out 3-4", 2);
        assert_eq!(headphones.resolved_pair(8), Some((2, 3)));
        // Device switched to a plain stereo interface — monitoring must not
        // vanish just because the stored pair no longer exists.
        assert_eq!(headphones.resolved_pair(2), Some((0, 1)));
        assert_eq!(headphones.resolved_pair(1), None);
    }

    #[test]
    fn available_targets_track_the_device_channel_count() {
        let stereo = MonitorOutputTarget::available_for_device(2);
        assert_eq!(stereo.len(), 1);
        assert_eq!(stereo[0].left_channel, 0);

        let eight = MonitorOutputTarget::available_for_device(8);
        assert_eq!(eight.len(), 4);
        assert_eq!(eight[3].name, "Out 7-8");
        assert_eq!(eight[3].left_channel, 6);
    }

    #[test]
    fn listen_mode_round_trips_and_reports_activity() {
        for mode in [ListenMode::Off, ListenMode::Pfl, ListenMode::Afl] {
            assert_eq!(ListenMode::from_tag(mode.tag()), mode);
        }
        assert!(!ListenMode::Off.is_active());
        assert!(ListenMode::Pfl.is_active());
        assert!(ListenMode::Afl.is_active());
    }
}
