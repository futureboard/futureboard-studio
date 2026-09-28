//! Listening simulation: the mix as it would sound somewhere else — in a car,
//! on a phone, through a PA, in a living room.
//!
//! A monitoring aid, not an effect: it runs on the Control Room's output only
//! and never reaches an export. Each [`ListeningProfile`] is a model of one
//! playback system in the space it is heard in, built from the parts that
//! make it sound the way it does:
//!
//! * **The loudspeakers' response** — what they cannot reproduce (a phone
//!   has no bass below ~500 Hz), where they ring, and the room's bass that
//!   the early response is too short to hold (a car cabin's lift, a
//!   bedroom's mode). A cascade of RBJ biquads.
//! * **How hard they are driven.** Small drivers compress and distort a
//!   little; a gentle soft clipper stands in for them.
//! * **The space** ([`crate::environment`]): the room's geometry, the
//!   listener and every loudspeaker at their real positions — a car's
//!   woofers low in the doors and tweeters on the dash, a PA across a club
//!   — turned into an impulse response by the image-source method, heard
//!   through the measured head, and convolved without latency
//!   ([`crate::convolver`]); then the room's late reverberation, with the
//!   decay time and level its size and surfaces give it.
//! * **The limiter** every real system has at its output, so a loud master
//!   is squeezed rather than clipped.
//!
//! On headphones ([`ListeningDevice::Headphones`]) everything arrives through
//! the head, from where it comes from. On speakers the direct sound plays
//! from its own side and the reflections are spread between the two real
//! loudspeakers — the room you are sitting in supplies the rest.
//!
//! Every profile is level-matched to the unprocessed mix on pink noise
//! (`trim_db`, checked by a test), so switching compares balance, not
//! loudness. The systems are models drawn from typical published
//! measurements of each kind, not measurements of one product.
//!
//! Realtime: the rooms are worked out once per sample rate, off the audio
//! thread, and shared; a simulator allocates in [`ListeningSimulator::new`]
//! only. Changing system or device on the audio thread swaps responses and
//! recomputes filter coefficients behind a short fade, so it never clicks.

use serde::{Deserialize, Serialize};

use std::sync::{Arc, Mutex};

use crate::convolver::{Ffts, PartitionedIr, StreamingConvolver};
use crate::environment::{Loudspeaker, Room, Surface};
use crate::hrtf::HrirSet;
use crate::room_tail::RoomTail;

/// The playback systems the simulator can put the mix on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum ListeningProfile {
    /// A sedan, heard from the driver's seat.
    #[default]
    Car,
    /// A phone's own speaker, held in the hand.
    Phone,
    /// Laptop speakers at arm's length.
    Laptop,
    /// A flat-panel TV's down-firing speakers across a living room.
    Television,
    /// A single-box Bluetooth speaker.
    BluetoothSpeaker,
    /// A club PA from the dance floor.
    ClubPa,
    /// A large hall's PA from the middle of the audience.
    ConcertHall,
    /// Near-field monitors in a treated room: the reference.
    Studio,
    /// Hi-fi speakers in a furnished living room.
    LivingRoom,
    /// Small speakers in a small, bare bedroom.
    Bedroom,
}

/// How the profiles are grouped for choosing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ListeningGroup {
    Devices,
    Car,
    Live,
    Rooms,
}

impl ListeningGroup {
    pub const ALL: [ListeningGroup; 4] = [Self::Car, Self::Devices, Self::Live, Self::Rooms];

    pub fn name(self) -> &'static str {
        match self {
            Self::Devices => "Devices",
            Self::Car => "Car",
            Self::Live => "Live",
            Self::Rooms => "Rooms",
        }
    }
}

impl ListeningProfile {
    pub const ALL: [ListeningProfile; 10] = [
        Self::Car,
        Self::Phone,
        Self::Laptop,
        Self::Television,
        Self::BluetoothSpeaker,
        Self::ClubPa,
        Self::ConcertHall,
        Self::Studio,
        Self::LivingRoom,
        Self::Bedroom,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Car => "Car",
            Self::Phone => "Phone Speaker",
            Self::Laptop => "Laptop",
            Self::Television => "TV",
            Self::BluetoothSpeaker => "Bluetooth Speaker",
            Self::ClubPa => "Club PA",
            Self::ConcertHall => "Concert Hall",
            Self::Studio => "Studio Monitors",
            Self::LivingRoom => "Living Room",
            Self::Bedroom => "Bedroom",
        }
    }

    /// One line on what the listener is hearing through.
    pub fn description(self) -> &'static str {
        match self {
            Self::Car => "Sedan, driver's seat: cabin bass lift, left door close, right far",
            Self::Phone => "Built-in speaker in the hand: mono, no bass, a tiny driver",
            Self::Laptop => "Two small drivers at arm's length: thin, narrow",
            Self::Television => "Down-firing TV speakers across a living room",
            Self::BluetoothSpeaker => "One box, mono, tuned-up bass, a little compressed",
            Self::ClubPa => "From the dance floor: subs, horns, a live room",
            Self::ConcertHall => "Mid-audience in a big hall: distant, long reverb",
            Self::Studio => "Near-field monitors in a treated room",
            Self::LivingRoom => "Hi-fi speakers in a furnished room with a bass mode",
            Self::Bedroom => "Small speakers in a small, bare, boomy room",
        }
    }

    pub fn group(self) -> ListeningGroup {
        match self {
            Self::Car => ListeningGroup::Car,
            Self::Phone | Self::Laptop | Self::Television | Self::BluetoothSpeaker => {
                ListeningGroup::Devices
            }
            Self::ClubPa | Self::ConcertHall => ListeningGroup::Live,
            Self::Studio | Self::LivingRoom | Self::Bedroom => ListeningGroup::Rooms,
        }
    }

    /// Whether the system plays everything from one speaker.
    pub fn is_mono(self) -> bool {
        self.model()
            .room
            .loudspeakers
            .iter()
            .all(|speaker| speaker.channel == 0)
    }

    /// A stable token for settings files.
    pub fn token(self) -> &'static str {
        match self {
            Self::Car => "car",
            Self::Phone => "phone",
            Self::Laptop => "laptop",
            Self::Television => "tv",
            Self::BluetoothSpeaker => "bluetooth-speaker",
            Self::ClubPa => "club-pa",
            Self::ConcertHall => "concert-hall",
            Self::Studio => "studio",
            Self::LivingRoom => "living-room",
            Self::Bedroom => "bedroom",
        }
    }

    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.token() == token)
    }

    fn model(self) -> &'static Model {
        match self {
            Self::Car => &CAR,
            Self::Phone => &PHONE,
            Self::Laptop => &LAPTOP,
            Self::Television => &TELEVISION,
            Self::BluetoothSpeaker => &BLUETOOTH,
            Self::ClubPa => &CLUB_PA,
            Self::ConcertHall => &CONCERT_HALL,
            Self::Studio => &STUDIO,
            Self::LivingRoom => &LIVING_ROOM,
            Self::Bedroom => &BEDROOM,
        }
    }
}

/// What the listener is actually wearing or facing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum ListeningDevice {
    /// Headphones: the simulated speakers are placed around the head.
    #[default]
    Headphones,
    /// Real speakers: only the simulated speakers' timing and level are
    /// kept; the real room does the rest.
    Speakers,
}

impl ListeningDevice {
    pub const ALL: [ListeningDevice; 2] = [Self::Headphones, Self::Speakers];

    pub fn name(self) -> &'static str {
        match self {
            Self::Headphones => "Headphones",
            Self::Speakers => "Speakers",
        }
    }

    pub fn token(self) -> &'static str {
        match self {
            Self::Headphones => "headphones",
            Self::Speakers => "speakers",
        }
    }

    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|d| d.token() == token)
    }
}

/// The simulator's controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SimulationSettings {
    pub enabled: bool,
    pub profile: ListeningProfile,
    pub device: ListeningDevice,
}

// ── Models ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
enum Kind {
    HighPass,
    LowPass,
    Peak,
    LowShelf,
    HighShelf,
}

#[derive(Debug, Clone, Copy)]
struct Band {
    kind: Kind,
    hz: f32,
    gain_db: f32,
    q: f32,
}

const fn band(kind: Kind, hz: f32, gain_db: f32, q: f32) -> Band {
    Band {
        kind,
        hz,
        gain_db,
        q,
    }
}

#[derive(Debug)]
struct Model {
    /// The loudspeakers' own response (and what the space does to the bass
    /// that the image-source window is too short to hold).
    eq: &'static [Band],
    /// Soft-clip drive, `0` clean: how hard a small driver is pushed.
    drive: f32,
    /// Where it all is: the room, the listener, the loudspeakers.
    room: Room,
    /// The late reverberation against what the diffuse-field formula gives,
    /// dB: a furnished room is not the ideal diffuse box the formula
    /// assumes. Set so each system's clarity (C50, mid band) lands on the
    /// values measured in rooms of its kind — see `acoustics_table`.
    tail_db: f32,
    /// Level match against the dry mix on pink noise, per device
    /// (headphones, speakers).
    trim_db: [f32; 2],
}

use Kind::{HighPass as HP, HighShelf as HS, LowPass as LP, LowShelf as LS, Peak as PK};

const BUTTERWORTH_Q: f32 = std::f32::consts::FRAC_1_SQRT_2;
const FULL: [f32; 3] = [1.0, 1.0, 1.0];

const fn speaker(
    position: [f32; 3],
    channel: usize,
    bands: [f32; 3],
    directivity: [f32; 3],
) -> Loudspeaker {
    Loudspeaker {
        position,
        channel,
        bands,
        directivity,
    }
}

// How each kind of loudspeaker beams (low, mid, high), as the exponent of a
// cardioid-like pattern: `Q = 2p + 1`. Small drivers are nearly
// omnidirectional until the treble; waveguides and horns hold a narrow beam
// from the mids up.
const BEAM_DOOR_WOOFER: [f32; 3] = [0.0, 0.5, 1.2];
const BEAM_DASH_TWEETER: [f32; 3] = [0.0, 0.6, 1.6];
const BEAM_PHONE: [f32; 3] = [0.0, 0.2, 0.8];
const BEAM_LAPTOP: [f32; 3] = [0.0, 0.3, 1.0];
const BEAM_TV: [f32; 3] = [0.0, 0.3, 0.7];
const BEAM_BLUETOOTH: [f32; 3] = [0.0, 0.3, 0.9];
const BEAM_PA: [f32; 3] = [0.5, 2.5, 4.0];
const BEAM_STUDIO: [f32; 3] = [0.2, 1.3, 2.2];
const BEAM_HIFI: [f32; 3] = [0.2, 1.4, 2.2];

/// A furnished living room: the TV, the Bluetooth speaker and the hi-fi
/// share it.
const LIVING_WALLS: Surface = Surface([0.25, 0.36, 0.33]);
const LIVING_FLOOR: Surface = Surface([0.2, 0.4, 0.5]);
const LIVING_CEILING: Surface = Surface([0.08, 0.06, 0.06]);
const LIVING_SIZE: [f32; 3] = [5.0, 6.0, 2.7];

static CAR: Model = Model {
    eq: &[
        band(HP, 32.0, 0.0, BUTTERWORTH_Q),
        // Cabin gain: a closed box a couple of metres long lifts the bass.
        band(LS, 90.0, 6.0, 0.7),
        // Door-panel and seat dips.
        band(PK, 180.0, -4.0, 1.2),
        band(PK, 1_100.0, -2.0, 1.0),
        band(HS, 9_000.0, -3.0, 0.7),
        band(LP, 16_000.0, 0.0, BUTTERWORTH_Q),
    ],
    drive: 0.0,
    room: Room {
        // A sedan's cabin, the driver on the left.
        size: [1.45, 2.1, 1.15],
        listener: [0.40, 1.0, 0.95],
        // Glass, door trim and seat backs; carpet and seats; the headliner.
        walls: Surface([0.15, 0.2, 0.25]),
        floor: Surface([0.25, 0.45, 0.6]),
        ceiling: Surface([0.3, 0.55, 0.65]),
        early_s: 0.05,
        scattering: 0.3,
        loudspeakers: &[
            // Woofers low in the front doors, tweeters in the dash corners.
            speaker([0.03, 1.45, 0.35], 0, [1.0, 1.0, 0.25], BEAM_DOOR_WOOFER),
            speaker([0.12, 1.95, 0.85], 0, [0.0, 0.35, 1.0], BEAM_DASH_TWEETER),
            speaker([1.42, 1.45, 0.35], 1, [1.0, 1.0, 0.25], BEAM_DOOR_WOOFER),
            speaker([1.33, 1.95, 0.85], 1, [0.0, 0.35, 1.0], BEAM_DASH_TWEETER),
        ],
    },
    tail_db: 0.0,
    trim_db: [-12.2, -13.4],
};

static PHONE: Model = Model {
    eq: &[
        // A 4th-order roll-off: a phone speaker has nothing under ~500 Hz.
        band(HP, 550.0, 0.0, 0.54),
        band(HP, 550.0, 0.0, 1.31),
        band(PK, 1_000.0, 3.0, 1.0),
        band(PK, 2_800.0, 6.0, 1.4),
        band(PK, 6_000.0, -4.0, 1.5),
        band(LP, 12_000.0, 0.0, BUTTERWORTH_Q),
    ],
    drive: 0.12,
    room: Room {
        // Held in the hand, in an ordinary room.
        size: [3.5, 4.0, 2.5],
        listener: [1.75, 1.6, 1.2],
        walls: Surface([0.2, 0.3, 0.3]),
        floor: Surface([0.12, 0.3, 0.4]),
        ceiling: Surface([0.05, 0.05, 0.05]),
        early_s: 0.05,
        scattering: 0.6,
        loudspeakers: &[speaker([1.75, 1.93, 1.05], 0, FULL, BEAM_PHONE)],
    },
    tail_db: -7.2,
    trim_db: [6.5, 8.3],
};

static LAPTOP: Model = Model {
    eq: &[
        band(HP, 190.0, 0.0, 0.54),
        band(HP, 190.0, 0.0, 1.31),
        band(PK, 450.0, -2.0, 1.2),
        band(PK, 950.0, 3.0, 1.2),
        band(PK, 3_500.0, 3.0, 1.5),
        band(HS, 8_000.0, -5.0, 0.7),
        band(LP, 15_000.0, 0.0, BUTTERWORTH_Q),
    ],
    drive: 0.05,
    room: Room {
        // On a desk in an office with a tiled ceiling.
        size: [4.0, 5.0, 2.7],
        listener: [2.0, 2.2, 1.2],
        walls: Surface([0.2, 0.26, 0.38]),
        floor: Surface([0.1, 0.25, 0.35]),
        ceiling: Surface([0.2, 0.4, 0.5]),
        early_s: 0.05,
        scattering: 0.6,
        loudspeakers: &[
            speaker([1.85, 2.72, 0.95], 0, FULL, BEAM_LAPTOP),
            speaker([2.15, 2.72, 0.95], 1, FULL, BEAM_LAPTOP),
        ],
    },
    tail_db: -6.6,
    trim_db: [0.9, 2.7],
};

static TELEVISION: Model = Model {
    eq: &[
        band(HP, 85.0, 0.0, 0.54),
        band(HP, 85.0, 0.0, 1.31),
        band(PK, 250.0, -2.0, 1.0),
        band(PK, 2_000.0, 2.0, 1.0),
        // Firing down at the stand: the treble is lost off-axis.
        band(PK, 5_000.0, -4.0, 1.2),
        band(HS, 9_000.0, -5.0, 0.7),
    ],
    drive: 0.0,
    room: Room {
        size: LIVING_SIZE,
        // On the sofa, the TV across the room.
        listener: [2.5, 1.6, 1.05],
        walls: LIVING_WALLS,
        floor: LIVING_FLOOR,
        ceiling: LIVING_CEILING,
        early_s: 0.05,
        scattering: 0.7,
        loudspeakers: &[
            speaker([2.15, 4.4, 0.75], 0, FULL, BEAM_TV),
            speaker([2.85, 4.4, 0.75], 1, FULL, BEAM_TV),
        ],
    },
    tail_db: -10.9,
    trim_db: [-1.8, -0.7],
};

static BLUETOOTH: Model = Model {
    eq: &[
        band(HP, 65.0, 0.0, 0.54),
        band(HP, 65.0, 0.0, 1.31),
        // Tuned up: a passive radiator and DSP bass boost.
        band(LS, 130.0, 5.0, 0.7),
        band(PK, 500.0, -3.0, 1.0),
        band(PK, 2_500.0, 2.0, 1.2),
        band(HS, 8_000.0, -3.5, 0.7),
    ],
    drive: 0.08,
    room: Room {
        size: LIVING_SIZE,
        listener: [2.5, 1.6, 1.05],
        walls: LIVING_WALLS,
        floor: LIVING_FLOOR,
        ceiling: LIVING_CEILING,
        early_s: 0.05,
        scattering: 0.7,
        // On a shelf across the room.
        loudspeakers: &[speaker([2.5, 3.3, 0.9], 0, FULL, BEAM_BLUETOOTH)],
    },
    tail_db: -9.1,
    trim_db: [4.0, 5.4],
};

static CLUB_PA: Model = Model {
    eq: &[
        band(HP, 30.0, 0.0, BUTTERWORTH_Q),
        // Subs.
        band(LS, 75.0, 6.0, 0.7),
        band(PK, 250.0, -2.5, 1.0),
        // Horns.
        band(PK, 2_500.0, 2.0, 1.2),
        band(HS, 9_000.0, -3.0, 0.7),
    ],
    drive: 0.04,
    room: Room {
        // A club: hard floor, a treated ceiling, a crowd between.
        size: [15.0, 20.0, 6.0],
        listener: [7.5, 7.0, 1.7],
        walls: Surface([0.25, 0.3, 0.35]),
        // The crowd: a standing audience soaks up the mids and highs.
        floor: Surface([0.15, 0.6, 0.7]),
        ceiling: Surface([0.3, 0.4, 0.5]),
        early_s: 0.08,
        scattering: 0.5,
        loudspeakers: &[
            speaker([2.0, 17.0, 3.5], 0, FULL, BEAM_PA),
            speaker([13.0, 17.0, 3.5], 1, FULL, BEAM_PA),
        ],
    },
    tail_db: -4.9,
    trim_db: [-5.4, -4.5],
};

static CONCERT_HALL: Model = Model {
    eq: &[
        band(HP, 35.0, 0.0, BUTTERWORTH_Q),
        band(LS, 80.0, 2.0, 0.7),
        band(PK, 2_200.0, 1.5, 1.0),
        band(HS, 7_000.0, -3.0, 0.7),
    ],
    drive: 0.0,
    room: Room {
        // A large hall, mid-audience; the audience soaks up the floor.
        size: [30.0, 45.0, 18.0],
        listener: [15.0, 18.0, 1.2],
        walls: Surface([0.25, 0.3, 0.28]),
        floor: Surface([0.45, 0.75, 0.85]),
        ceiling: Surface([0.12, 0.1, 0.1]),
        early_s: 0.1,
        scattering: 0.6,
        loudspeakers: &[
            speaker([8.0, 40.0, 7.0], 0, FULL, BEAM_PA),
            speaker([22.0, 40.0, 7.0], 1, FULL, BEAM_PA),
        ],
    },
    tail_db: -1.9,
    trim_db: [-3.2, -1.6],
};

static STUDIO: Model = Model {
    eq: &[
        band(HP, 45.0, 0.0, BUTTERWORTH_Q),
        band(LP, 20_000.0, 0.0, BUTTERWORTH_Q),
    ],
    drive: 0.0,
    room: Room {
        // Treated: absorbers on the walls and a cloud overhead; monitors in
        // an equilateral triangle, 1.2 m.
        size: [4.5, 5.5, 3.0],
        listener: [2.25, 2.3, 1.2],
        walls: Surface([0.3, 0.4, 0.45]),
        floor: Surface([0.1, 0.2, 0.3]),
        ceiling: Surface([0.55, 0.8, 0.8]),
        early_s: 0.05,
        scattering: 0.5,
        loudspeakers: &[
            speaker([1.65, 3.34, 1.2], 0, FULL, BEAM_STUDIO),
            speaker([2.85, 3.34, 1.2], 1, FULL, BEAM_STUDIO),
        ],
    },
    tail_db: 0.0,
    trim_db: [-0.2, 1.6],
};

static LIVING_ROOM: Model = Model {
    eq: &[
        band(HP, 38.0, 0.0, BUTTERWORTH_Q),
        // The room's first axial mode, and the cancellation above it.
        band(PK, 55.0, 5.0, 4.0),
        band(PK, 110.0, -4.0, 3.0),
        band(HS, 10_000.0, -1.5, 0.7),
    ],
    drive: 0.0,
    room: Room {
        size: LIVING_SIZE,
        listener: [2.5, 2.0, 1.05],
        walls: LIVING_WALLS,
        floor: LIVING_FLOOR,
        ceiling: LIVING_CEILING,
        early_s: 0.05,
        scattering: 0.7,
        loudspeakers: &[
            speaker([1.2, 4.25, 1.0], 0, FULL, BEAM_HIFI),
            speaker([3.8, 4.25, 1.0], 1, FULL, BEAM_HIFI),
        ],
    },
    tail_db: -9.5,
    trim_db: [-1.8, -1.1],
};

static BEDROOM: Model = Model {
    eq: &[
        band(HP, 55.0, 0.0, 0.54),
        band(HP, 55.0, 0.0, 1.31),
        band(PK, 75.0, 6.0, 3.0),
        band(PK, 150.0, -3.0, 3.0),
        band(PK, 300.0, 2.0, 1.5),
        band(HS, 9_000.0, -3.0, 0.7),
    ],
    drive: 0.0,
    room: Room {
        // Small, with hard walls: a wardrobe, the bed and a rug are what
        // absorb.
        size: [3.0, 3.5, 2.5],
        listener: [1.5, 1.1, 1.0],
        walls: Surface([0.2, 0.26, 0.26]),
        floor: Surface([0.2, 0.35, 0.45]),
        ceiling: Surface([0.05, 0.05, 0.05]),
        early_s: 0.05,
        scattering: 0.65,
        loudspeakers: &[
            speaker([0.75, 2.9, 1.0], 0, FULL, BEAM_HIFI),
            speaker([2.25, 2.9, 1.0], 1, FULL, BEAM_HIFI),
        ],
    },
    tail_db: -9.8,
    trim_db: [-1.9, -2.0],
};

const MAX_BANDS: usize = 8;
/// Fade lengths: into and out of the simulation, and through a switch.
const FADE_S: f32 = 0.03;
const SWITCH_OUT_S: f32 = 0.012;
/// On real speakers the late tail is shorter-handed: the listener's own
/// room adds its own.
const SPEAKERS_TAIL_SHARE: f32 = 0.5;
/// The limiter every real system has at its output: nothing leaves the
/// simulation above this, and nothing clips.
const LIMIT_CEILING: f32 = 0.891; // -1 dBFS
const LIMIT_LOOKAHEAD_S: f32 = 0.002;
const LIMIT_RELEASE_S: f32 = 0.08;

// ── Filters ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Coeffs {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

impl Coeffs {
    const IDENTITY: Self = Self {
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
        a1: 0.0,
        a2: 0.0,
    };

    /// The RBJ cookbook designs.
    fn design(b: &Band, rate: f32) -> Self {
        let hz = b.hz.clamp(10.0, rate * 0.45);
        let w0 = 2.0 * std::f32::consts::PI * hz / rate;
        let (sin, cos) = w0.sin_cos();
        let q = b.q.max(0.1);
        let alpha = sin / (2.0 * q);
        let a = 10f32.powf(b.gain_db / 40.0);
        let (b0, b1, b2, a0, a1, a2) = match b.kind {
            Kind::HighPass => (
                (1.0 + cos) / 2.0,
                -(1.0 + cos),
                (1.0 + cos) / 2.0,
                1.0 + alpha,
                -2.0 * cos,
                1.0 - alpha,
            ),
            Kind::LowPass => (
                (1.0 - cos) / 2.0,
                1.0 - cos,
                (1.0 - cos) / 2.0,
                1.0 + alpha,
                -2.0 * cos,
                1.0 - alpha,
            ),
            Kind::Peak => (
                1.0 + alpha * a,
                -2.0 * cos,
                1.0 - alpha * a,
                1.0 + alpha / a,
                -2.0 * cos,
                1.0 - alpha / a,
            ),
            Kind::LowShelf => {
                let s = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) - (a - 1.0) * cos + s),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                    a * ((a + 1.0) - (a - 1.0) * cos - s),
                    (a + 1.0) + (a - 1.0) * cos + s,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                    (a + 1.0) + (a - 1.0) * cos - s,
                )
            }
            Kind::HighShelf => {
                let s = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) + (a - 1.0) * cos + s),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                    a * ((a + 1.0) + (a - 1.0) * cos - s),
                    (a + 1.0) - (a - 1.0) * cos + s,
                    2.0 * ((a - 1.0) - (a + 1.0) * cos),
                    (a + 1.0) - (a - 1.0) * cos - s,
                )
            }
        };
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct BiquadState {
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl BiquadState {
    #[inline]
    fn tick(&mut self, c: &Coeffs, x: f32) -> f32 {
        let y = c.b0 * x + c.b1 * self.x1 + c.b2 * self.x2 - c.a1 * self.y1 - c.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = if y.abs() < 1.0e-20 { 0.0 } else { y };
        self.y1
    }
}

/// A small speaker pushed hard: `tanh`, unity gain for quiet signals.
#[inline]
fn saturate(x: f32, k: f32) -> f32 {
    if k <= 1.0 {
        return x;
    }
    (x * k).tanh() / k
}

// ── The rooms, rendered ─────────────────────────────────────────────────────

/// One system's space, ready to play: its response through the convolver
/// and its late tail.
#[derive(Debug)]
struct Environment {
    ir: PartitionedIr,
    tail_half_size_m: f32,
    rt60_s: f32,
    rt60_high_s: f32,
    tail_level: f32,
}

/// Every system's space on both devices, at one sample rate: worked out once
/// (off the audio thread) and shared by every simulator at that rate.
#[derive(Debug)]
struct Environments {
    ffts: Ffts,
    /// `[profile][device]`, in `ListeningProfile::ALL` order.
    rooms: Vec<[Environment; 2]>,
    max_partitions: usize,
}

impl Environments {
    fn shared(sample_rate: u32) -> Arc<Environments> {
        static SETS: Mutex<Vec<(u32, Arc<Environments>)>> = Mutex::new(Vec::new());
        let mut sets = SETS.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((_, set)) = sets.iter().find(|(rate, _)| *rate == sample_rate) {
            return set.clone();
        }
        let set = Arc::new(Self::build(sample_rate));
        sets.push((sample_rate, set.clone()));
        set
    }

    fn build(sample_rate: u32) -> Self {
        let rate = sample_rate as f32;
        let hrirs = HrirSet::shared(sample_rate);
        let ffts = Ffts::new();
        let rooms: Vec<[Environment; 2]> = ListeningProfile::ALL
            .iter()
            .map(|profile| {
                let room = &profile.model().room;
                [true, false].map(|headphones| {
                    let response = room.response(headphones, &hrirs, rate);
                    Environment {
                        ir: PartitionedIr::new(&response.irs, &ffts),
                        tail_half_size_m: response.tail_half_size_m,
                        rt60_s: response.rt60_s,
                        rt60_high_s: response.rt60_high_s,
                        tail_level: response.tail_level,
                    }
                })
            })
            .collect();
        let max_partitions = rooms
            .iter()
            .flat_map(|pair| pair.iter())
            .map(|e| e.ir.partitions())
            .max()
            .unwrap_or(1);
        Self {
            ffts,
            rooms,
            max_partitions,
        }
    }

    fn get(&self, settings: SimulationSettings) -> &Environment {
        let profile = ListeningProfile::ALL
            .iter()
            .position(|p| *p == settings.profile)
            .unwrap_or(0);
        &self.rooms[profile][device_index(settings.device)]
    }
}

fn device_index(device: ListeningDevice) -> usize {
    match device {
        ListeningDevice::Headphones => 0,
        ListeningDevice::Speakers => 1,
    }
}

// ── The limiter ─────────────────────────────────────────────────────────────

/// A look-ahead peak limiter: the gain it will need is known `lookahead`
/// samples before the peak arrives, so it eases down in time instead of
/// clipping, and eases back up over `release`.
#[derive(Debug, Clone)]
struct Limiter {
    lookahead: usize,
    delay_l: Vec<f32>,
    delay_r: Vec<f32>,
    delay_pos: usize,
    /// Sliding minimum of the gains needed over the look-ahead window: a
    /// monotonic queue of `(sample, gain)`, as a ring.
    queue: Vec<(u64, f32)>,
    queue_head: usize,
    queue_len: usize,
    sample: u64,
    gain: f32,
    attack: f32,
    release: f32,
}

impl Limiter {
    fn new(rate: f32) -> Self {
        let lookahead = ((LIMIT_LOOKAHEAD_S * rate) as usize).max(1);
        Self {
            lookahead,
            delay_l: vec![0.0; lookahead],
            delay_r: vec![0.0; lookahead],
            delay_pos: 0,
            queue: vec![(0, 1.0); lookahead + 2],
            queue_head: 0,
            queue_len: 0,
            sample: 0,
            gain: 1.0,
            attack: 1.0 - (-5.0 / lookahead as f32).exp(),
            release: 1.0 - (-1.0 / (LIMIT_RELEASE_S * rate)).exp(),
        }
    }

    fn reset(&mut self) {
        self.delay_l.fill(0.0);
        self.delay_r.fill(0.0);
        self.delay_pos = 0;
        self.queue_head = 0;
        self.queue_len = 0;
        self.gain = 1.0;
    }

    #[inline]
    fn tick(&mut self, l: f32, r: f32) -> (f32, f32) {
        let peak = l.abs().max(r.abs());
        let need = if peak > LIMIT_CEILING {
            LIMIT_CEILING / peak
        } else {
            1.0
        };
        let cap = self.queue.len();
        // Drop every queued gain this one undercuts, then queue it.
        while self.queue_len > 0 {
            let back = (self.queue_head + self.queue_len - 1) % cap;
            if self.queue[back].1 >= need {
                self.queue_len -= 1;
            } else {
                break;
            }
        }
        let slot = (self.queue_head + self.queue_len) % cap;
        self.queue[slot] = (self.sample, need);
        self.queue_len += 1;
        // Forget what has left the window.
        while self.queue_len > 0
            && self.queue[self.queue_head].0 + (self.lookahead as u64) < self.sample
        {
            self.queue_head = (self.queue_head + 1) % cap;
            self.queue_len -= 1;
        }
        let target = self.queue[self.queue_head].1;
        let coefficient = if target < self.gain {
            self.attack
        } else {
            self.release
        };
        self.gain += (target - self.gain) * coefficient;
        self.sample += 1;

        let out_l = self.delay_l[self.delay_pos] * self.gain;
        let out_r = self.delay_r[self.delay_pos] * self.gain;
        self.delay_l[self.delay_pos] = l;
        self.delay_r[self.delay_pos] = r;
        self.delay_pos = (self.delay_pos + 1) % self.lookahead;
        // The smoothing leaves a sliver; never let it through.
        (
            out_l.clamp(-LIMIT_CEILING, LIMIT_CEILING),
            out_r.clamp(-LIMIT_CEILING, LIMIT_CEILING),
        )
    }
}

// ── The simulator ────────────────────────────────────────────────────────────

/// Where the simulator is between two configurations.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    /// Running `active` (or bypassed, when not enabled).
    Steady,
    /// Fading the current system out before switching to `pending`.
    SwitchingOut,
}

/// The Control Room's listening simulation.
#[derive(Debug, Clone)]
pub struct ListeningSimulator {
    rate: f32,
    /// What is running now, and what was asked for.
    active: SimulationSettings,
    pending: SimulationSettings,
    phase: Phase,
    /// How much of the output is the simulation, `0..=1`, and how much of
    /// the simulation passes (dips to 0 through a switch).
    wet: f32,
    through: f32,
    fade_step: f32,
    switch_step: f32,

    coeffs: [Coeffs; MAX_BANDS],
    bands: usize,
    eq_state: [[BiquadState; MAX_BANDS]; 2],
    drive: f32,
    mono: bool,
    environments: Arc<Environments>,
    convolver: StreamingConvolver,
    tail: RoomTail,
    limiter: Limiter,
    level: f32,

    wet_l: Vec<f32>,
    wet_r: Vec<f32>,
}

impl ListeningSimulator {
    /// A simulator at `sample_rate` for blocks of up to `max_block` (longer
    /// blocks are processed in pieces). Allocates, and the first simulator
    /// at a rate works out every system's room; control thread only.
    pub fn new(sample_rate: u32, max_block: usize) -> Self {
        let rate = sample_rate.max(8_000) as f32;
        let block = max_block.max(64);
        let environments = Environments::shared(sample_rate.max(8_000));
        let convolver =
            StreamingConvolver::new(environments.ffts.clone(), environments.max_partitions);
        let mut simulator = Self {
            rate,
            active: SimulationSettings::default(),
            pending: SimulationSettings::default(),
            phase: Phase::Steady,
            wet: 0.0,
            through: 1.0,
            fade_step: 1.0 / (FADE_S * rate),
            switch_step: 1.0 / (SWITCH_OUT_S * rate),
            coeffs: [Coeffs::IDENTITY; MAX_BANDS],
            bands: 0,
            eq_state: [[BiquadState::default(); MAX_BANDS]; 2],
            drive: 1.0,
            mono: false,
            environments,
            convolver,
            tail: RoomTail::with_capacity(10.0, sample_rate),
            limiter: Limiter::new(rate),
            level: 1.0,
            wet_l: vec![0.0; block],
            wet_r: vec![0.0; block],
        };
        simulator.load(SimulationSettings::default());
        simulator
    }

    /// Apply `settings` at once, with no fade: for a freshly built graph,
    /// before it plays.
    pub fn configure_now(&mut self, settings: SimulationSettings) {
        self.load(settings);
        self.pending = settings;
        self.phase = Phase::Steady;
        self.wet = if settings.enabled { 1.0 } else { 0.0 };
        self.through = 1.0;
    }

    /// Ask for `settings`. Turning on or off fades; a new system or device
    /// fades the old one out and the new one in. Allocation-free.
    pub fn configure(&mut self, settings: SimulationSettings) {
        self.pending = settings;
        let same_sound =
            settings.profile == self.active.profile && settings.device == self.active.device;
        if same_sound {
            self.active.enabled = settings.enabled;
            self.phase = Phase::Steady;
        } else if self.wet <= 0.0 {
            // Nothing of the old system is audible: switch outright.
            self.load(settings);
            self.phase = Phase::Steady;
            self.through = 1.0;
        } else {
            self.phase = Phase::SwitchingOut;
        }
    }

    /// The settings asked for last.
    pub fn settings(&self) -> SimulationSettings {
        self.pending
    }

    /// Whether any of the simulation is being heard.
    pub fn is_audible(&self) -> bool {
        self.wet > 0.0 || self.active.enabled
    }

    fn load(&mut self, settings: SimulationSettings) {
        self.active = settings;
        let model = settings.profile.model();
        self.bands = model.eq.len().min(MAX_BANDS);
        for (i, b) in model.eq.iter().take(MAX_BANDS).enumerate() {
            self.coeffs[i] = Coeffs::design(b, self.rate);
        }
        self.eq_state = [[BiquadState::default(); MAX_BANDS]; 2];
        self.drive = 1.0 + 5.0 * model.drive;
        self.mono = settings.profile.is_mono();
        let environment = self.environments.get(settings);
        let tail_share = match settings.device {
            ListeningDevice::Headphones => 1.0,
            ListeningDevice::Speakers => SPEAKERS_TAIL_SHARE,
        };
        self.tail.set_room_bands(
            environment.tail_half_size_m,
            environment.rt60_s,
            environment.rt60_high_s,
            environment.tail_level * tail_share * 10f32.powf(model.tail_db / 20.0),
        );
        self.level = 10f32.powf(model.trim_db[device_index(settings.device)] / 20.0);
        self.convolver.reset();
        self.limiter.reset();
    }

    /// Process the Control Room's stereo block in place.
    pub fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let frames = left.len().min(right.len());
        let chunk = self.wet_l.len();
        let mut offset = 0;
        while offset < frames {
            let n = (frames - offset).min(chunk);
            self.process_chunk(
                &mut left[offset..offset + n],
                &mut right[offset..offset + n],
            );
            offset += n;
        }
    }

    fn process_chunk(&mut self, left: &mut [f32], right: &mut [f32]) {
        if !self.active.enabled && self.wet <= 0.0 && self.phase == Phase::Steady {
            return;
        }
        let frames = left.len();
        self.render(left, right, frames);

        let target = if self.active.enabled { 1.0 } else { 0.0 };
        for n in 0..frames {
            // Into and out of the simulation.
            if self.wet < target {
                self.wet = (self.wet + self.fade_step).min(1.0);
            } else if self.wet > target {
                self.wet = (self.wet - self.fade_step).max(0.0);
            }
            // Through a switch: out, then (after the switch) back in.
            match self.phase {
                Phase::SwitchingOut => self.through = (self.through - self.switch_step).max(0.0),
                Phase::Steady => self.through = (self.through + self.fade_step).min(1.0),
            }
            let wet = self.wet;
            let sim = wet * self.through;
            left[n] = left[n] * (1.0 - wet) + self.wet_l[n] * sim;
            right[n] = right[n] * (1.0 - wet) + self.wet_r[n] * sim;
        }
        if self.phase == Phase::SwitchingOut && self.through <= 0.0 {
            self.load(self.pending);
            self.phase = Phase::Steady;
        }
    }

    /// The simulation of `left`/`right` into `wet_*`.
    fn render(&mut self, left: &[f32], right: &[f32], frames: usize) {
        let ir = &self.environments.get(self.active).ir;
        for n in 0..frames {
            // Each channel's feed: the system's response, then its drive.
            let (mut l, mut r) = if self.mono {
                (0.5 * (left[n] + right[n]), 0.0)
            } else {
                (left[n], right[n])
            };
            for b in 0..self.bands {
                l = self.eq_state[0][b].tick(&self.coeffs[b], l);
            }
            l = saturate(l, self.drive);
            if !self.mono {
                for b in 0..self.bands {
                    r = self.eq_state[1][b].tick(&self.coeffs[b], r);
                }
                r = saturate(r, self.drive);
            }
            // Out of the loudspeakers, around the room, into the ears.
            let [ear_l, ear_r] = self.convolver.tick([l, r], ir);
            self.wet_l[n] = ear_l;
            self.wet_r[n] = ear_r;
        }
        self.tail
            .process(&mut self.wet_l[..frames], &mut self.wet_r[..frames]);
        for n in 0..frames {
            let (l, r) = self
                .limiter
                .tick(self.wet_l[n] * self.level, self.wet_r[n] * self.level);
            self.wet_l[n] = l;
            self.wet_r[n] = r;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;
    const BLOCK: usize = 512;

    /// Paul Kellet's pink noise, stereo with independent sides.
    fn pink(frames: usize, seed: u32) -> (Vec<f32>, Vec<f32>) {
        let mut state = seed;
        let mut side = |_: ()| {
            let mut b = [0.0f32; 7];
            let mut out = Vec::with_capacity(frames);
            for _ in 0..frames {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let white = (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
                b[0] = 0.99886 * b[0] + white * 0.0555179;
                b[1] = 0.99332 * b[1] + white * 0.0750759;
                b[2] = 0.96900 * b[2] + white * 0.1538520;
                b[3] = 0.86650 * b[3] + white * 0.3104856;
                b[4] = 0.55000 * b[4] + white * 0.5329522;
                b[5] = -0.7616 * b[5] - white * 0.0168980;
                let p = b.iter().sum::<f32>() + white * 0.5362;
                b[6] = white * 0.115926;
                out.push(p * 0.05);
            }
            out
        };
        (side(()), side(()))
    }

    fn run(settings: SimulationSettings, l: &[f32], r: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let mut sim = ListeningSimulator::new(RATE, BLOCK);
        sim.configure_now(settings);
        let (mut l, mut r) = (l.to_vec(), r.to_vec());
        for (cl, cr) in l.chunks_mut(BLOCK).zip(r.chunks_mut(BLOCK)) {
            sim.process(cl, cr);
        }
        (l, r)
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    fn db(x: f32) -> f32 {
        20.0 * x.max(1.0e-12).log10()
    }

    fn on(profile: ListeningProfile, device: ListeningDevice) -> SimulationSettings {
        SimulationSettings {
            enabled: true,
            profile,
            device,
        }
    }

    #[test]
    fn off_is_untouched() {
        let (l, r) = pink(8_192, 1);
        let (ol, or) = run(SimulationSettings::default(), &l, &r);
        assert_eq!(l, ol);
        assert_eq!(r, or);
    }

    /// Every profile, on either device, plays pink noise at the level the
    /// dry mix does (the `trim_db` table), so a switch compares balance.
    #[test]
    fn every_profile_is_level_matched() {
        let (l, r) = pink(RATE as usize * 2, 7);
        let dry = rms(&l).hypot(rms(&r));
        let skip = RATE as usize / 2;
        let mut report = String::new();
        let mut worst = 0.0f32;
        for device in ListeningDevice::ALL {
            for profile in ListeningProfile::ALL {
                let (ol, or) = run(on(profile, device), &l, &r);
                assert!(ol.iter().chain(&or).all(|s| s.is_finite()));
                let wet = rms(&ol[skip..]).hypot(rms(&or[skip..]));
                let off = db(wet / dry);
                worst = worst.max(off.abs());
                report.push_str(&format!("{device:?} {profile:?}: {off:+.1} dB\n"));
            }
        }
        assert!(worst < 1.5, "level match off:\n{report}");
    }

    #[test]
    fn a_phone_is_mono_with_no_bass() {
        // A 60 Hz tone on the left only: a phone plays it from one speaker,
        // barely at all.
        let tone: Vec<f32> = (0..RATE as usize)
            .map(|n| 0.5 * (2.0 * std::f32::consts::PI * 60.0 * n as f32 / RATE as f32).sin())
            .collect();
        let silence = vec![0.0; tone.len()];
        let (ol, _) = run(
            on(ListeningProfile::Phone, ListeningDevice::Speakers),
            &tone,
            &silence,
        );
        let skip = RATE as usize / 4;
        assert!(db(rms(&ol[skip..]) / rms(&tone)) < -25.0);
        // Mono: noise on the left only comes out of both sides alike, since
        // there is one speaker.
        let (noise, _) = pink(RATE as usize, 9);
        for device in ListeningDevice::ALL {
            let (ol, or) = run(on(ListeningProfile::Phone, device), &noise, &silence);
            let balance = db(rms(&ol[skip..]) / rms(&or[skip..]));
            assert!(
                balance.abs() < 1.5,
                "{device:?}: left against right {balance:+.1} dB"
            );
        }
    }

    #[test]
    fn the_driver_hears_the_near_door_first() {
        // In a car the image leans to the near door by arriving first — the
        // cabin's reflections leave the two sides' levels close.
        let first = |left: bool, device| {
            let mut l = vec![0.0f32; RATE as usize / 4];
            let mut r = l.clone();
            if left {
                l[64] = 0.5;
            } else {
                r[64] = 0.5;
            }
            let (ol, or) = run(on(ListeningProfile::Car, device), &l, &r);
            let peak = ol.iter().chain(&or).fold(0.0f32, |m, s| m.max(s.abs()));
            ol.iter()
                .zip(&or)
                .position(|(a, b)| a.abs().max(b.abs()) > 0.2 * peak)
                .unwrap_or(usize::MAX)
        };
        for device in ListeningDevice::ALL {
            let (near, far) = (first(true, device), first(false, device));
            // 0.44 m farther: about 60 samples at 48 kHz.
            assert!(far > near + 40, "{device:?}: near {near} far {far}");
        }
    }

    #[test]
    fn a_hall_rings_on_long_after_a_studio() {
        let ring = |profile| {
            let mut l = vec![0.0f32; RATE as usize * 2];
            l[100] = 1.0;
            let r = l.clone();
            let (ol, _) = run(on(profile, ListeningDevice::Headphones), &l, &r);
            let total: f32 = ol.iter().map(|s| s * s).sum();
            let late: f32 = ol[RATE as usize / 2..].iter().map(|s| s * s).sum();
            late / total
        };
        assert!(ring(ListeningProfile::ConcertHall) > 50.0 * ring(ListeningProfile::Studio));
    }

    #[test]
    fn switching_and_bypassing_never_click() {
        let mut sim = ListeningSimulator::new(RATE, BLOCK);
        let mut prev = 0.0f32;
        let mut phase = 0usize;
        for step in 0..200 {
            if step % 20 == 5 {
                let profile = ListeningProfile::ALL[(step / 20) % ListeningProfile::ALL.len()];
                let device = ListeningDevice::ALL[(step / 40) % 2];
                sim.configure(SimulationSettings {
                    enabled: step % 60 != 45,
                    profile,
                    device,
                });
            }
            let mut l: Vec<f32> = (0..BLOCK)
                .map(|n| {
                    let t = (phase + n) as f32 / RATE as f32;
                    0.5 * (2.0 * std::f32::consts::PI * 220.0 * t).sin()
                })
                .collect();
            let mut r = l.clone();
            phase += BLOCK;
            sim.process(&mut l, &mut r);
            assert!(l.iter().chain(&r).all(|s| s.is_finite() && s.abs() < 4.0));
            let jump = l
                .iter()
                .scan(prev, |p, s| {
                    let d = (s - *p).abs();
                    *p = *s;
                    Some(d)
                })
                .fold(0.0f32, f32::max);
            assert!(jump < 0.35, "click at block {step}: {jump}");
            prev = l[BLOCK - 1];
        }
    }

    #[test]
    #[ignore = "timing"]
    fn cost_of_building_and_running() {
        let start = std::time::Instant::now();
        let _ = Environments::build(RATE);
        println!("rooms built in {:?}", start.elapsed());
        let (l, r) = pink(RATE as usize * 10, 5);
        for device in ListeningDevice::ALL {
            for profile in ListeningProfile::ALL {
                let start = std::time::Instant::now();
                let _ = run(on(profile, device), &l, &r);
                println!(
                    "{device:?} {profile:?}: {:.2}% of a core",
                    start.elapsed().as_secs_f64() / 10.0 * 100.0
                );
            }
        }
    }

    /// Every real system limits its output; the simulation does too, so a
    /// loud master is squeezed and never clipped.
    #[test]
    fn a_loud_master_never_clips() {
        // Pink noise pushed like a loud modern master: driven 12 dB into a
        // soft clip that sits just under full scale.
        let (mut l, mut r) = pink(RATE as usize, 11);
        let peak = l.iter().chain(&r).fold(0.0f32, |m, s| m.max(s.abs()));
        for s in l.iter_mut().chain(r.iter_mut()) {
            *s = (*s / peak * 4.0).tanh() * 0.98;
        }
        for device in ListeningDevice::ALL {
            for profile in ListeningProfile::ALL {
                let (ol, or) = run(on(profile, device), &l, &r);
                let out = ol.iter().chain(&or).fold(0.0f32, |m, s| m.max(s.abs()));
                assert!(
                    out <= LIMIT_CEILING + 1.0e-6,
                    "{device:?} {profile:?}: peak {:+.1} dBFS",
                    db(out)
                );
            }
        }
    }

    /// The room acoustics each system is heard with, measured the way a
    /// room is: on the left ear's impulse response, mid band (500 Hz–2 kHz).
    struct Acoustics {
        /// Schroeder decay from −5 to −25 dB, extrapolated to 60 dB.
        t20_s: f32,
        /// Direct sound (its first 2.5 ms) against everything after.
        drr_db: f32,
        /// Energy in the first 50 ms against the rest.
        c50_db: f32,
    }

    fn acoustics(profile: ListeningProfile, device: ListeningDevice) -> Acoustics {
        let len = RATE as usize * 3;
        let mut l = vec![0.0f32; len];
        l[256] = 0.01;
        let r = l.clone();
        let (ol, _) = run(on(profile, device), &l, &r);
        // Mid band: 500 Hz – 2 kHz.
        let mut hp = [BiquadState::default(); 2];
        let mut lp = [BiquadState::default(); 2];
        let hpc = Coeffs::design(&band(HP, 500.0, 0.0, BUTTERWORTH_Q), RATE as f32);
        let lpc = Coeffs::design(&band(LP, 2_000.0, 0.0, BUTTERWORTH_Q), RATE as f32);
        let ir: Vec<f32> = ol
            .iter()
            .map(|&x| {
                let mut y = x;
                for s in &mut hp {
                    y = s.tick(&hpc, y);
                }
                for s in &mut lp {
                    y = s.tick(&lpc, y);
                }
                y
            })
            .collect();
        let energy: Vec<f64> = ir.iter().map(|s| (*s as f64).powi(2)).collect();
        let peak = energy.iter().cloned().fold(0.0, f64::max);
        let onset = energy.iter().position(|e| *e > peak * 0.01).unwrap_or(0);
        let ms = |t: f32| (t * RATE as f32 / 1000.0) as usize;
        let sum = |a: usize, b: usize| energy[a.min(len)..b.min(len)].iter().sum::<f64>();
        let direct = sum(onset.saturating_sub(ms(0.5)), onset + ms(2.5));
        let after = sum(onset + ms(2.5), len);
        let early = sum(onset.saturating_sub(ms(0.5)), onset + ms(50.0));
        let late = sum(onset + ms(50.0), len);
        // Schroeder backward integral.
        let mut tail = vec![0.0f64; len + 1];
        for n in (0..len).rev() {
            tail[n] = tail[n + 1] + energy[n];
        }
        let total = tail[onset];
        let level = |n: usize| 10.0 * (tail[n] / total).max(1.0e-30).log10();
        let at = |db: f64| (onset..len).find(|&n| level(n) <= db).unwrap_or(len);
        let t20 = (at(-25.0) - at(-5.0)) as f32 / RATE as f32 * 3.0;
        let db10 = |x: f64| 10.0 * x.max(1.0e-30).log10() as f32;
        Acoustics {
            t20_s: t20,
            drr_db: db10(direct / after.max(1.0e-30)),
            c50_db: db10(early / late.max(1.0e-30)),
        }
    }

    /// Each space measures like real rooms of its kind (mid band, on
    /// headphones): reverberation time and clarity inside the ranges
    /// published for them.
    ///
    /// * Car cabins: RT60 55-110 ms by band (IOA 2004, Bay Systems).
    /// * Studio control rooms: 0.2-0.4 s (EBU Tech 3276).
    /// * Homes and offices: the MIT IR Survey (Traer & McDermott, PNAS 2016,
    ///   CC BY 4.0) — medians at 1.5 m: living rooms 0.36 s and C50 +16 dB,
    ///   bedrooms 0.34 s and +14 dB, offices 0.34 s (0.22 s at 8 kHz) and
    ///   +18 dB; carried to each system's listening distance and
    ///   loudspeaker directivity.
    /// * Rock/club venues 0.6-1.3 s empty, drier with a crowd; D50 0.3-0.8
    ///   (Adelman-Larsen et al., JASA 2010).
    /// * Concert halls 1.7-2.3 s, C80 -1 to +3 dB.
    #[test]
    fn rooms_measure_like_real_rooms() {
        use ListeningProfile::*;
        let expect = [
            (Car, (0.04, 0.14), (15.0, 60.0)),
            // Held close, the direct sound dominates the decay curve, so a
            // room of ~0.35 s measures shorter at the ear.
            (Phone, (0.1, 0.5), (18.0, 40.0)),
            (Laptop, (0.1, 0.5), (16.0, 40.0)),
            (Television, (0.28, 0.5), (11.0, 17.0)),
            (BluetoothSpeaker, (0.28, 0.5), (12.0, 19.0)),
            (ClubPa, (0.5, 0.9), (3.0, 9.0)),
            (ConcertHall, (1.6, 2.4), (-4.0, 1.0)),
            (Studio, (0.15, 0.4), (15.0, 40.0)),
            (LivingRoom, (0.28, 0.48), (12.0, 18.0)),
            (Bedroom, (0.25, 0.45), (12.0, 19.0)),
        ];
        for (profile, (rt_lo, rt_hi), (c50_lo, c50_hi)) in expect {
            let a = acoustics(profile, ListeningDevice::Headphones);
            assert!(
                (rt_lo..=rt_hi).contains(&a.t20_s) && (c50_lo..=c50_hi).contains(&a.c50_db),
                "{profile:?}: T20 {:.2} s (want {rt_lo}-{rt_hi}), C50 {:+.1} dB (want {c50_lo}-{c50_hi})",
                a.t20_s,
                a.c50_db
            );
        }
    }

    #[test]
    #[ignore = "diagnostic: `cargo test -p solfege_spatialaudio --release acoustics_table -- --ignored --nocapture`"]
    fn acoustics_table() {
        for device in ListeningDevice::ALL {
            for profile in ListeningProfile::ALL {
                let a = acoustics(profile, device);
                println!(
                    "{device:?} {profile:?}: T20 {:.2} s, DRR {:+.1} dB, C50 {:+.1} dB",
                    a.t20_s, a.drr_db, a.c50_db
                );
            }
        }
    }

    #[test]
    fn tokens_round_trip() {
        for profile in ListeningProfile::ALL {
            assert_eq!(ListeningProfile::from_token(profile.token()), Some(profile));
        }
        for device in ListeningDevice::ALL {
            assert_eq!(ListeningDevice::from_token(device.token()), Some(device));
        }
    }
}
