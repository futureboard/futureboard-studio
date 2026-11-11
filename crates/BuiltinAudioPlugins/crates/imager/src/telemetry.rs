//! The Imager's side spectrum, in the 64-slot telemetry block every built-in
//! can publish beside its meters.
//!
//! The host runs a second analyser over the insert's input — its side,
//! `(L − R) / 2`, where the shared analyser takes the mid — and packs its
//! [`SPECTRUM_BINS`] log-spaced levels into [`SLOTS`] by keeping the louder
//! of each neighbouring pair. Both ends read the layout from here, so the
//! host and the editor cannot drift apart.

use builtin_dsp_core::spectrum::{FLOOR_DB, SPECTRUM_BINS};

/// Slots in the block: one side level per slot, 20 Hz to 20 kHz on the same
/// log axis as the mid spectrum, in dBFS.
pub const SLOTS: usize = 64;
/// Analyser bins folded into one slot.
const FOLD: usize = SPECTRUM_BINS / SLOTS;

const _: () = assert!(SPECTRUM_BINS.is_multiple_of(SLOTS));

/// The block for one side analysis.
pub fn side_block(bins: &[f32; SPECTRUM_BINS]) -> [f32; SLOTS] {
    let mut block = [FLOOR_DB; SLOTS];
    for (slot, pair) in block.iter_mut().zip(bins.chunks_exact(FOLD)) {
        *slot = pair.iter().copied().fold(FLOOR_DB, f32::max);
    }
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_slot_keeps_the_louder_of_its_bins() {
        let mut bins = [FLOOR_DB; SPECTRUM_BINS];
        bins[0] = -40.0;
        bins[1] = -20.0;
        bins[SPECTRUM_BINS - 1] = -6.0;
        let block = side_block(&bins);
        assert_eq!(block[0], -20.0);
        assert_eq!(block[SLOTS - 1], -6.0);
        assert_eq!(block[10], FLOOR_DB);
    }
}
