//! Spatial audio for Futureboard: surround and binaural.
//!
//! Every source is placed in a **square room** with the listener at its centre
//! ([`RoomPosition`]): `x` runs left to right, `y` back to front, `z` floor to
//! ceiling, each in `-1..=1` (`z` in `0..=1`). The walls are at `±1`. Where the
//! source ends up depends on the project's [`SpatialFormat`]:
//!
//! * **Surround** ([`SpeakerLayout`]: LCR, quad, 5.0/5.1, 7.0/7.1, 7.1.2,
//!   7.1.4). [`SurroundPanner`] turns the position into one gain per speaker
//!   with pairwise 2-D VBAP around the speaker ring — so a source on the wall
//!   between two speakers plays from exactly those two — and spreads it toward
//!   every speaker as it moves in toward the listener, reaching an even,
//!   constant-power bed at the centre. Height layouts crossfade between the
//!   floor ring and the top ring on `z`. The LFE is a send, low-passed at
//!   120 Hz. [`SurroundSource`] renders a channel's audio with those gains,
//!   ramped per block so a moving source never zippers.
//! * **Binaural** (headphones). [`BinauralSource`] renders each source through
//!   the measured MIT KEMAR dummy head (embedded; see `data/`): its time
//!   difference between the ears and its per-ear filters, minimum-phase and
//!   diffuse-field-equalised, interpolated between the measured directions.
//!   Distance attenuates and darkens the source, first-order reflections off
//!   the room's four walls and the room's late tail ([`RoomTail`]) put it
//!   outside the head, and a source at the centre of the room settles inside
//!   it. Nothing is loaded from disk and nothing allocates while rendering.
//!
//! A surround mix is heard on headphones through [`VirtualSpeakers`] (each
//! speaker rendered binaurally where it stands in the room) or folded to
//! stereo with the ITU coefficients ([`fold_down`]).
//!
//! # Data
//!
//! The binaural head is the MIT KEMAR set: Bill Gardner and Keith Martin,
//! "HRTF Measurements of a KEMAR Dummy-Head Microphone", MIT Media Lab
//! Perceptual Computing Technical Report #280, 1994. The authors are to be
//! cited wherever it is used (`data/MIT_KEMAR_NOTICE.md`).
//!
//! # Realtime contract
//!
//! Every processor allocates in `new` and never again: `process` runs on the
//! audio thread with no allocation, locks, I/O or panics on valid input.
//! Buffers are sized for the largest block passed to `new`; longer blocks
//! must be split by the caller.

mod binaural;
mod bus;
mod dsp;
mod format;
mod hrtf;
mod layout;
mod panner;
mod position;
mod room_tail;
mod surround;
mod virtual_speakers;

pub use binaural::{BinauralSource, HEAD_RADIUS_M, SPEED_OF_SOUND_M_S};
pub use bus::SpatialBus;
pub use format::{MonitorFold, SpatialFormat};
pub use layout::{MAX_CHANNELS, Speaker, SpeakerLayout};
pub use panner::SurroundPanner;
pub use position::{RoomPosition, RoomSettings, SourceParams};
pub use room_tail::RoomTail;
pub use surround::SurroundSource;
pub use virtual_speakers::{VirtualSpeakers, fold_down, fold_down_range, speaker_position};

/// A channel's spatialiser for the project's format: what the mixer runs in
/// place of the stereo pan once the project is surround or binaural.
#[derive(Debug, Clone)]
pub enum SpatialSource {
    Surround(SurroundSource),
    Binaural(BinauralSource),
}

impl SpatialSource {
    /// The processor `format` needs, or `None` for stereo (which keeps the
    /// ordinary pan). Allocates; control thread only.
    pub fn for_format(format: SpatialFormat, sample_rate: u32, max_block: usize) -> Option<Self> {
        match format {
            SpatialFormat::Stereo => None,
            SpatialFormat::Binaural => {
                Some(Self::Binaural(BinauralSource::new(sample_rate, max_block)))
            }
            SpatialFormat::Surround(layout) => Some(Self::Surround(SurroundSource::new(
                layout,
                sample_rate,
                max_block,
            ))),
        }
    }

    /// Render one block of a channel's stereo signal at `params` and add it
    /// to `bus`, which must carry the format's channels (two for binaural).
    pub fn process(
        &mut self,
        input_l: &[f32],
        input_r: &[f32],
        params: &SourceParams,
        room: &RoomSettings,
        bus: &mut SpatialBus,
    ) {
        match self {
            Self::Surround(source) => source.process(input_l, input_r, params, bus),
            Self::Binaural(source) => {
                let frames = input_l.len().min(input_r.len());
                let (out_l, out_r) = bus.pair_mut(0, 1, frames);
                source.process(input_l, input_r, params, room, out_l, out_r);
            }
        }
    }

    /// Forget all state (delay lines, filters, ramps) — after a seek or a
    /// graph rebuild, so the next block starts clean.
    pub fn reset(&mut self) {
        match self {
            Self::Surround(source) => source.reset(),
            Self::Binaural(source) => source.reset(),
        }
    }
}
