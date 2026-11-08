//! Walking an incoming CoreMIDI `MIDIPacketList`.
//!
//! Pure pointer arithmetic with no CoreMIDI calls, kept out of
//! `macos_coremidi` so the traversal builds and is unit-tested on every host,
//! not only on a Mac.
//!
//! Layout, from `MIDIServices.h`: `MIDIPacketList` is `{ UInt32 numPackets;
//! MIDIPacket packet[1]; }` and `MIDIPacket` is `{ MIDITimeStamp timeStamp;
//! UInt16 length; Byte data[256]; }`, both under `#pragma pack(4)`. The
//! `data[256]` is nominal: `length` is the real payload size, and a packet in a
//! list CoreMIDI hands to a read proc can be far longer than 256 bytes (a bulk
//! SysEx dump). The next packet starts right after the real payload, which is
//! what `MIDIPacketNext` computes.

/// Offset of the first `MIDIPacket` in a `MIDIPacketList` (4, not 8:
/// `MIDIPacket` is `pack(4)`).
pub(crate) const PACKET_LIST_FIRST_PACKET: usize = 4;
/// Offset of `MIDIPacket::length`.
pub(crate) const PACKET_LENGTH_OFFSET: usize = 8;
/// Offset of `MIDIPacket::data`.
pub(crate) const PACKET_DATA_OFFSET: usize = 10;

/// Whether `MIDIPacketNext` rounds each packet up to a 4-byte boundary on this
/// target: it does on ARM and packs packets end to end on Intel.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) const PACKETS_ARE_4_BYTE_ALIGNED: bool =
    cfg!(any(target_arch = "aarch64", target_arch = "arm"));

/// Address of the packet that follows the one at `packet` carrying `length`
/// payload bytes. Mirrors `MIDIPacketNext`: the full `length` is stepped over,
/// never a clamped one, or every later packet is read from the wrong offset.
pub(crate) const fn next_packet_address(packet: usize, length: usize, align_to_4: bool) -> usize {
    let end = packet + PACKET_DATA_OFFSET + length;
    if align_to_4 { (end + 3) & !3 } else { end }
}

/// Hands each packet's payload in the packet list at `list` to `visit`, in
/// order.
///
/// Reads through raw offsets rather than a packed struct, so no reference to an
/// unaligned field is ever formed.
///
/// # Safety
/// `list` must point at a live `MIDIPacketList` holding `numPackets`
/// well-formed packets, each followed by its full `length` bytes of payload, as
/// CoreMIDI guarantees for a read proc. `align_to_4` must match the layout the
/// list was built with ([`PACKETS_ARE_4_BYTE_ALIGNED`] for CoreMIDI's own).
pub(crate) unsafe fn for_each_packet(
    list: *const u8,
    align_to_4: bool,
    mut visit: impl FnMut(&[u8]),
) {
    unsafe {
        let num_packets = (list as *const u32).read_unaligned();
        if num_packets == 0 {
            return;
        }
        let mut packet = list.add(PACKET_LIST_FIRST_PACKET);
        for index in 0..num_packets {
            let length = (packet.add(PACKET_LENGTH_OFFSET) as *const u16).read_unaligned() as usize;
            visit(std::slice::from_raw_parts(
                packet.add(PACKET_DATA_OFFSET),
                length,
            ));
            if index + 1 < num_packets {
                // Advance by the same distance `next_packet_address` computes, but
                // through the pointer so it keeps the list's provenance.
                let next = next_packet_address(packet as usize, length, align_to_4);
                packet = packet.add(next - packet as usize);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4-byte-aligned allocation of the exact byte length of a
    /// `MIDIPacketList`, without trailing padding to a 4-byte boundary.
    struct PacketListBuf {
        ptr: *mut u8,
        len: usize,
        layout: std::alloc::Layout,
    }

    impl PacketListBuf {
        fn new(bytes: &[u8]) -> Self {
            let layout = std::alloc::Layout::from_size_align(bytes.len().max(1), 4).unwrap();
            // SAFETY: `layout` has non-zero size and 4-byte power-of-two alignment.
            let ptr = unsafe { std::alloc::alloc(layout) };
            if ptr.is_null() {
                std::alloc::handle_alloc_error(layout);
            }
            // SAFETY: `ptr` is valid for `bytes.len()` bytes and does not overlap `bytes`.
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            }
            Self {
                ptr,
                len: bytes.len(),
                layout,
            }
        }

        fn as_ptr(&self) -> *const u8 {
            self.ptr
        }

        fn len(&self) -> usize {
            self.len
        }
    }

    impl Drop for PacketListBuf {
        fn drop(&mut self) {
            // SAFETY: `self.ptr` was allocated with `self.layout` in `Self::new`.
            unsafe {
                std::alloc::dealloc(self.ptr, self.layout);
            }
        }
    }

    /// Build a `MIDIPacketList` byte-for-byte the way CoreMIDI does.
    ///
    /// The offsets and the stride rule are written out as literals taken from
    /// `MIDIServices.h` rather than from this module's constants, so the tests
    /// fail if those constants drift: `numPackets` at 0, the first packet at
    /// **4**, `timeStamp` at +0, `length` at +8, `data` at +10, and each
    /// following packet 4-byte aligned on ARM or butted directly against the
    /// previous payload on Intel. The buffer ends exactly where the last
    /// payload does, so a walk that strays past it reads outside the
    /// allocation.
    fn build_packet_list(packets: &[&[u8]], align_to_4: bool) -> PacketListBuf {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(packets.len() as u32).to_ne_bytes());
        for (index, payload) in packets.iter().enumerate() {
            assert!(
                !align_to_4 || bytes.len() % 4 == 0,
                "packets start 4-byte aligned"
            );
            bytes.extend_from_slice(&0x1234_5678_9abc_def0u64.to_ne_bytes());
            bytes.extend_from_slice(&(payload.len() as u16).to_ne_bytes());
            bytes.extend_from_slice(payload);
            if align_to_4 && index + 1 < packets.len() {
                while bytes.len() % 4 != 0 {
                    bytes.push(0);
                }
            }
        }
        PacketListBuf::new(&bytes)
    }

    fn collect(packets: &[&[u8]], align_to_4: bool) -> Vec<Vec<u8>> {
        let list = build_packet_list(packets, align_to_4);
        let mut seen = Vec::new();
        // SAFETY: `list` is a well-formed packet list built above with the
        // same alignment rule, and it outlives the walk.
        unsafe {
            for_each_packet(list.as_ptr(), align_to_4, |data| {
                seen.push(data.to_vec());
            });
        }
        seen
    }

    fn owned(packets: &[&[u8]]) -> Vec<Vec<u8>> {
        packets.iter().map(|packet| packet.to_vec()).collect()
    }

    /// Both stride rules, whatever this host is: the ARM rule is what Apple
    /// Silicon uses, and it cannot be exercised by running on Intel or Windows
    /// otherwise.
    const LAYOUTS: [bool; 2] = [false, true];

    #[test]
    fn a_packet_list_is_walked_at_the_layout_core_midi_actually_uses() {
        // The regression this layout guards: reading the first packet through a
        // naturally aligned `#[repr(C)]` struct put it at offset 8, so `length`
        // landed on the third data byte and every payload came back as noise.
        for align in LAYOUTS {
            assert_eq!(
                collect(&[&[0x90, 60, 100]], align),
                vec![vec![0x90, 60, 100]]
            );
        }
    }

    #[test]
    fn every_packet_in_a_multi_packet_list_is_read() {
        let packets: &[&[u8]] = &[&[0x90, 60, 100], &[0x80, 60, 0], &[0xB0, 7, 90]];
        for align in LAYOUTS {
            assert_eq!(
                collect(packets, align),
                owned(packets),
                "align_to_4={align}"
            );
        }
    }

    #[test]
    fn packets_of_uneven_length_keep_the_walk_aligned() {
        let packets: &[&[u8]] = &[&[0xF8], &[0x90, 60, 100], &[0xC0, 5], &[0x90, 64, 100]];
        for align in LAYOUTS {
            assert_eq!(
                collect(packets, align),
                owned(packets),
                "align_to_4={align}"
            );
        }
    }

    #[test]
    fn a_packet_longer_than_256_bytes_is_read_whole_and_stepped_over() {
        // Issue #222: a bulk SysEx packet was clamped to 256 bytes both for its
        // payload and for the step to the next packet, so the notes after it
        // were read from inside the SysEx (and past the end of the list).
        let mut sysex = vec![0xF0, 0x7D];
        sysex.extend((0..1_000u32).map(|i| (i % 0x80) as u8));
        sysex.push(0xF7);
        let mut odd_sysex = sysex.clone();
        odd_sysex.insert(2, 0x01); // 1,004 bytes: exercises the ARM padding
        let packets: &[&[u8]] = &[&sysex, &[0x90, 60, 100], &odd_sysex, &[0x80, 60, 0]];
        for align in LAYOUTS {
            assert_eq!(
                collect(packets, align),
                owned(packets),
                "align_to_4={align}"
            );
        }
    }

    #[test]
    fn the_next_packet_follows_the_full_payload() {
        // `MIDIPacketNext` on Intel: data offset + length, unclamped.
        assert_eq!(next_packet_address(4, 300, false), 4 + 10 + 300);
        assert_eq!(next_packet_address(4, 3, false), 17);
        // On ARM the same end is rounded up to the next 4-byte boundary.
        assert_eq!(next_packet_address(4, 300, true), 316);
        assert_eq!(next_packet_address(4, 3, true), 20);
        assert_eq!(next_packet_address(4, 2, true), 16);
    }

    #[test]
    fn a_coalesced_packet_reaches_the_splitter_intact() {
        // CoreMIDI puts messages that share a timestamp in one packet; the
        // payload has to arrive whole for `split_midi_messages` to unpack it.
        let payload: &[u8] = &[0x90, 60, 100, 0x90, 64, 100, 0x90, 67, 100];
        for align in LAYOUTS {
            assert_eq!(collect(&[payload], align), vec![payload.to_vec()]);
        }

        let mut running = None;
        let mut events = Vec::new();
        crate::split_midi_messages(payload, &mut running, |message| {
            if let Some(event) = crate::decode_midi_bytes(message) {
                events.push(event);
            }
        });
        assert_eq!(events.len(), 3, "the whole chord must decode");
    }

    #[test]
    fn an_empty_packet_list_is_a_no_op() {
        for align in LAYOUTS {
            assert!(collect(&[], align).is_empty());
        }
    }

    #[test]
    fn arm_walk_does_not_step_past_an_unaligned_final_packet() {
        // Issue #258: on ARM (`align_to_4 = true`), advancing `packet` after
        // the last packet rounds up to the next 4-byte boundary, which is 1-3
        // bytes past the end of the `MIDIPacketList` allocation whenever the
        // final packet's end (`4 + 10 + len` from the start of the packet) is
        // not a multiple of 4.
        for tail in [
            &[0xF8][..],
            &[0xC0, 5][..],
            &[0x90, 60, 100][..],
            &[0xF0, 0x7D, 0x01, 0xF7][..],
        ] {
            for prefix in [&[][..], &[&[0x90, 60, 100][..]][..]] {
                let mut packets = prefix.to_vec();
                packets.push(tail);
                let list = build_packet_list(&packets, true);
                let base = list.as_ptr() as usize;
                let end = base + list.len();
                let mut seen = Vec::new();
                // SAFETY: `list` is an exact-length allocation of `list.len()`
                // bytes built with the ARM layout (`align_to_4 = true`).
                unsafe {
                    for_each_packet(list.as_ptr(), true, |data| {
                        let slice_end = data.as_ptr() as usize + data.len();
                        assert!(slice_end <= end, "packet slice exceeded allocation");
                        seen.push(data.to_vec());
                    });
                }
                assert_eq!(seen, owned(&packets));
            }
        }
        // Specifically verify that a 3-byte Note On as the final packet leaves
        // an exact list length not divisible by 4 (4 + 10 + 3 = 17), where
        // `next_packet_address` rounds to 20 (3 bytes past `list.len()`).
        let single_note = build_packet_list(&[&[0x90, 60, 100]], true);
        assert_eq!(single_note.len(), 17);
        assert_eq!(next_packet_address(PACKET_LIST_FIRST_PACKET, 3, true), 20);
    }
}
