//! Channel locations and the decoder's output channel order.

/// A loudspeaker location of an AC-3 / E-AC-3 programme (Table 4.3 and Table E.1.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Channel {
    /// Left (also Ch1 of the 1+1 dual mono mode).
    L,
    /// Right (also Ch2 of the 1+1 dual mono mode).
    R,
    /// Centre.
    C,
    /// Low frequency effects.
    Lfe,
    /// Left surround.
    Ls,
    /// Right surround.
    Rs,
    /// Left rear surround.
    Lrs,
    /// Right rear surround.
    Rrs,
    /// Left centre.
    Lc,
    /// Right centre.
    Rc,
    /// Centre surround; also the single surround channel S of the 2/1 and 3/1 modes.
    Cs,
    /// Top surround.
    Ts,
    /// Left surround direct.
    Lsd,
    /// Right surround direct.
    Rsd,
    /// Left wide.
    Lw,
    /// Right wide.
    Rw,
    /// Left vertical height.
    Vhl,
    /// Right vertical height.
    Vhr,
    /// Centre vertical height.
    Vhc,
    /// Left top surround.
    Lts,
    /// Right top surround.
    Rts,
    /// Second low frequency effects channel.
    Lfe2,
}

impl Channel {
    /// Short name ("L", "Rs", "LFE", ...).
    pub fn name(self) -> &'static str {
        match self {
            Channel::L => "L",
            Channel::R => "R",
            Channel::C => "C",
            Channel::Lfe => "LFE",
            Channel::Ls => "Ls",
            Channel::Rs => "Rs",
            Channel::Lrs => "Lrs",
            Channel::Rrs => "Rrs",
            Channel::Lc => "Lc",
            Channel::Rc => "Rc",
            Channel::Cs => "Cs",
            Channel::Ts => "Ts",
            Channel::Lsd => "Lsd",
            Channel::Rsd => "Rsd",
            Channel::Lw => "Lw",
            Channel::Rw => "Rw",
            Channel::Vhl => "Vhl",
            Channel::Vhr => "Vhr",
            Channel::Vhc => "Vhc",
            Channel::Lts => "Lts",
            Channel::Rts => "Rts",
            Channel::Lfe2 => "LFE2",
        }
    }
}

/// Coded channel order of an independent syncframe: the `acmod` channels (Table 4.3) followed
/// by the LFE.
pub fn base_layout(acmod: u8, lfe: bool) -> Vec<Channel> {
    use Channel::*;
    let mut v: Vec<Channel> = match acmod & 7 {
        0 => vec![L, R],
        1 => vec![C],
        2 => vec![L, R],
        3 => vec![L, C, R],
        4 => vec![L, R, Cs],
        5 => vec![L, C, R, Cs],
        6 => vec![L, R, Ls, Rs],
        _ => vec![L, C, R, Ls, Rs],
    };
    if lfe {
        v.push(Lfe);
    }
    v
}

/// Channels named by an E-AC-3 dependent substream `chanmap` (Table E.1.4), in coded order.
pub fn chanmap_channels(chanmap: u16) -> Vec<Channel> {
    use Channel::*;
    const LOCS: [&[Channel]; 16] = [
        &[L],
        &[C],
        &[R],
        &[Ls],
        &[Rs],
        &[Lc, Rc],
        &[Lrs, Rrs],
        &[Cs],
        &[Ts],
        &[Lsd, Rsd],
        &[Lw, Rw],
        &[Vhl, Vhr],
        &[Vhc],
        &[Lts, Rts],
        &[Lfe2],
        &[Lfe],
    ];
    let mut v = Vec::new();
    for (bit, locs) in LOCS.iter().enumerate() {
        if chanmap & (0x8000 >> bit) != 0 {
            v.extend_from_slice(locs);
        }
    }
    v
}

/// Rank of a channel in the output order (see [`output_order`]).
fn rank(c: Channel, has_rear: bool) -> u8 {
    use Channel::*;
    match c {
        L => 0,
        R => 1,
        C => 2,
        Lfe => 3,
        Ls if !has_rear => 4,
        Rs if !has_rear => 5,
        Lrs => 6,
        Rrs => 7,
        Lc => 8,
        Rc => 9,
        Cs => 10,
        Ls => 11,
        Rs => 12,
        Ts => 13,
        Vhl => 14,
        Vhc => 15,
        Vhr => 16,
        Lts => 17,
        Rts => 18,
        Lsd => 19,
        Rsd => 20,
        Lw => 21,
        Rw => 22,
        Lfe2 => 23,
    }
}

/// Sort channel locations into the decoder's output order, which follows the WAV
/// (WAVEFORMATEXTENSIBLE) channel-mask order:
///
/// `L R C LFE` then, without rear surrounds, `Ls Rs` as the back pair; with rear surrounds
/// (7.1) `Lrs Rrs` are the back pair and `Ls Rs` the side pair, giving `L R C LFE Lrs Rrs Ls Rs`.
/// `Lc Rc` (front of centre) and `Cs` (back centre, also the mono surround S of 2/1 / 3/1)
/// come between the back and side pairs; height and other extension channels follow, and the
/// locations without a WAV equivalent (`Lsd Rsd Lw Rw LFE2`) come last.
pub fn output_order(chans: &[Channel]) -> Vec<Channel> {
    let has_rear = chans.contains(&Channel::Lrs);
    let mut v = chans.to_vec();
    v.sort_by_key(|&c| rank(c, has_rear));
    v.dedup();
    v
}

/// WAVEFORMATEXTENSIBLE `dwChannelMask` for channels in [`output_order`]. Locations without a
/// WAV speaker bit contribute nothing.
pub fn wav_channel_mask(chans: &[Channel]) -> u32 {
    use Channel::*;
    let has_rear = chans.contains(&Lrs);
    chans
        .iter()
        .map(|&c| match c {
            L => 0x1,
            R => 0x2,
            C => 0x4,
            Lfe => 0x8,
            Ls if !has_rear => 0x10,
            Rs if !has_rear => 0x20,
            Lrs => 0x10,
            Rrs => 0x20,
            Lc => 0x40,
            Rc => 0x80,
            Cs => 0x100,
            Ls => 0x200,
            Rs => 0x400,
            Ts => 0x800,
            Vhl => 0x1000,
            Vhc => 0x2000,
            Vhr => 0x4000,
            Lts => 0x8000,
            Rts => 0x20000,
            _ => 0,
        })
        .fold(0, |a, b| a | b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Channel::*;

    #[test]
    fn orders_and_masks() {
        let l51 = output_order(&base_layout(7, true));
        assert_eq!(l51, vec![L, R, C, Lfe, Ls, Rs]);
        assert_eq!(wav_channel_mask(&l51), 0x3F);
        let mut v = base_layout(7, true);
        v.extend(chanmap_channels(0x0200));
        let l71 = output_order(&v);
        assert_eq!(l71, vec![L, R, C, Lfe, Lrs, Rrs, Ls, Rs]);
        assert_eq!(wav_channel_mask(&l71), 0x63F);
        // 2/1: the mono surround goes to the back centre
        let l21 = output_order(&base_layout(4, false));
        assert_eq!(l21, vec![L, R, Cs]);
        assert_eq!(wav_channel_mask(&l21), 0x103);
        // chanmap order follows the bits, LFE last
        assert_eq!(
            chanmap_channels(0x8000 | 0x0400 | 0x0001),
            vec![L, Lc, Rc, Lfe]
        );
    }
}
