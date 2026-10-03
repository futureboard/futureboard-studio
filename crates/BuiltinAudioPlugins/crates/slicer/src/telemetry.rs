//! What the editor is told about playback: the slices sounding, wherever
//! their notes come from (the editor, a track's MIDI, the virtual keyboard).
//!
//! The plug-in host publishes it through the per-pad level block every
//! built-in instrument has in its shared region, [`SLOTS`] `f32`s wide, as
//! one pair per playhead:
//!
//! * `1 + note + 128 · (start number mod 65536)` — exact in an `f32` (under
//!   2²⁴), so a retriggered slice reads as new; `0` for an empty pair;
//! * the playhead's position, a fraction of the sample.

/// Playheads published at once: the newest.
pub const PLAYHEADS: usize = 8;
/// Width of the published block.
pub const SLOTS: usize = 2 * PLAYHEADS;

/// One sounding slice, as the editor draws it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Playhead {
    pub note: u8,
    /// Distinguishes one start of a note from the next.
    pub start: u16,
    /// Where it is reading, as a fraction of the sample.
    pub position: f32,
}

/// Packs `(note, start number, position)` voices, newest first.
pub fn encode(voices: &[(u8, u64, f32)]) -> [f32; SLOTS] {
    let mut slots = [0.0; SLOTS];
    for (pair, &(note, start, position)) in slots.chunks_exact_mut(2).zip(voices) {
        let tag = 1 + (note.min(127) as u32) + 128 * (start % 65_536) as u32;
        pair[0] = tag as f32;
        pair[1] = position;
    }
    slots
}

pub fn decode(slots: &[f32; SLOTS]) -> Vec<Playhead> {
    slots
        .chunks_exact(2)
        .filter(|pair| pair[0] >= 1.0 && pair[0].is_finite() && pair[1].is_finite())
        .map(|pair| {
            let tag = pair[0] as u32 - 1;
            Playhead {
                note: (tag % 128) as u8,
                start: (tag / 128) as u16,
                position: pair[1].clamp(0.0, 1.0),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playheads_round_trip_through_the_block() {
        let voices = [(127, 70_000, 0.25), (48, 3, 1.0)];
        let decoded = decode(&encode(&voices));
        assert_eq!(
            decoded,
            vec![
                Playhead {
                    note: 127,
                    start: (70_000 % 65_536) as u16,
                    position: 0.25,
                },
                Playhead {
                    note: 48,
                    start: 3,
                    position: 1.0,
                },
            ]
        );
        assert!(decode(&[0.0; SLOTS]).is_empty());
    }
}
