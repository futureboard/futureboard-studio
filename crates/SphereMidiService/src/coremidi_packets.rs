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

/// Per-connection state the CoreMIDI read proc works with. Lives here, not in
/// `macos_coremidi`, so the read path below runs in tests on every host.
pub(crate) struct InputCallbackState {
    tx: crate::MidiInputSender,
    /// Index of the device in the enabled set, resolved to its id and name by
    /// the draining side so the read proc never clones a string.
    source: u32,
    /// The connection's stream state: running status, a message cut at a
    /// packet boundary and an in-progress SysEx all carry into the next packet
    /// list. Only the read proc touches it; `busy` turns "CoreMIDI never calls
    /// one port's read proc concurrently" into a checked condition instead of
    /// an assumption.
    parser: std::cell::UnsafeCell<crate::MidiStreamParser>,
    busy: std::sync::atomic::AtomicBool,
}

impl InputCallbackState {
    /// Called on the control thread when the port is opened; allocates the
    /// SysEx buffer here so the read proc never does.
    pub(crate) fn new(tx: crate::MidiInputSender, source: u32) -> Self {
        Self {
            tx,
            source,
            parser: std::cell::UnsafeCell::new(crate::MidiStreamParser::new(
                crate::MAX_INPUT_SYSEX_BYTES,
            )),
            busy: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

// SAFETY: the only non-`Sync` field is `parser`, and it is reached solely
// through `read_packet_list`, which first takes `busy` with an atomic swap (a
// try-lock) and releases it after its last use, so no two threads ever hold a
// reference to it at once.
unsafe impl Sync for InputCallbackState {}

/// What one read-proc invocation produced, for the input diagnostics.
pub(crate) type PacketListCounts = crate::MidiInputCounts;

/// The body of the CoreMIDI read proc: parse every packet of the list at
/// `list` as one continuous stream, decode the messages and queue the events.
///
/// # Safety
/// As [`for_each_packet`].
pub(crate) unsafe fn read_packet_list(
    state: &InputCallbackState,
    list: *const u8,
    align_to_4: bool,
) -> PacketListCounts {
    use std::sync::atomic::Ordering;
    let mut counts = PacketListCounts::default();
    if state.busy.swap(true, Ordering::Acquire) {
        // A concurrent call on the same port. CoreMIDI does not do this; if it
        // ever did, dropping the list (counted) beats a data race.
        counts.dropped += 1;
        return counts;
    }
    // SAFETY: `busy` was false and is now ours, so no other reference to the
    // parser exists until it is released below.
    let parser = unsafe { &mut *state.parser.get() };
    let received_at = std::time::Instant::now();

    // SAFETY: forwarded from the caller.
    unsafe {
        for_each_packet(list, align_to_4, |data| {
            crate::deliver_midi_input(
                parser,
                &state.tx,
                state.source,
                data,
                received_at,
                &mut counts,
            );
        });
    }
    state.busy.store(false, Ordering::Release);
    counts
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
        // payload has to arrive whole for the stream parser to unpack it.
        let payload: &[u8] = &[0x90, 60, 100, 0x90, 64, 100, 0x90, 67, 100];
        for align in LAYOUTS {
            assert_eq!(collect(&[payload], align), vec![payload.to_vec()]);
        }

        let mut parser = crate::MidiStreamParser::new(crate::MAX_INPUT_SYSEX_BYTES);
        let mut events = Vec::new();
        parser.push(payload, |message| {
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

    // ---------------------------------------------------------------------
    // Issue #408: the reported cases, driven through `read_packet_list` — the
    // exact code the CoreMIDI read proc runs — with packet lists laid out
    // byte-for-byte as CoreMIDI lays them out.
    // ---------------------------------------------------------------------

    use crate::MidiInputEvent;

    fn input_state() -> (InputCallbackState, crate::MidiInputReceiver) {
        let bell = std::sync::Arc::new(crate::MidiInputDoorbell::default());
        let (tx, rx) = crate::midi_input_queue(&bell);
        (InputCallbackState::new(tx, 0), rx)
    }

    /// Run one read-proc invocation over `packets`, on both stride rules
    /// (state is per call, so each layout gets its own connection).
    fn deliver(packets: &[&[u8]]) -> Vec<(Vec<MidiInputEvent>, PacketListCounts)> {
        LAYOUTS
            .iter()
            .map(|&align| {
                let (state, rx) = input_state();
                let list = build_packet_list(packets, align);
                // SAFETY: well-formed list built with `align`, outlives the call.
                let counts = unsafe { read_packet_list(&state, list.as_ptr(), align) };
                (rx.try_iter().map(|m| m.event).collect(), counts)
            })
            .collect()
    }

    fn note_on(note: u8, velocity: u8) -> MidiInputEvent {
        MidiInputEvent::NoteOn {
            note,
            velocity,
            channel: 0,
        }
    }

    fn cc(controller: u8, value: u8) -> MidiInputEvent {
        MidiInputEvent::ControlChange {
            controller,
            value,
            channel: 0,
        }
    }

    #[test]
    fn issue_408_note_on_then_control_change_in_one_packet() {
        for (events, counts) in deliver(&[&[0x90, 60, 100, 0xB0, 7, 90]]) {
            assert_eq!(events, vec![note_on(60, 100), cc(7, 90)]);
            assert_eq!(counts.malformed, 0);
        }
    }

    #[test]
    fn issue_408_a_status_byte_ends_an_unfinished_note_on_instead_of_becoming_its_velocity() {
        // The Note On lost its velocity byte. Per MIDI 1.0 a status byte always
        // starts a new message: the CC must decode, the two orphaned bytes are
        // malformed, and no note may be invented.
        for (events, counts) in deliver(&[&[0x90, 60, 0xB0, 7, 90]]) {
            assert_eq!(events, vec![cc(7, 90)], "no phantom note, CC kept");
            assert_eq!(counts.malformed, 2);
        }
    }

    #[test]
    fn issue_408_a_clock_byte_inside_a_note_on_does_not_shift_the_following_cc() {
        // Real-time bytes may appear between the data bytes of any message.
        for (events, counts) in deliver(&[&[0x90, 60, 0xF8, 100, 0xB0, 7, 90]]) {
            assert_eq!(events, vec![note_on(60, 100), cc(7, 90)]);
            assert_eq!(counts.malformed, 0);
            assert_eq!(counts.ignored, 1, "the clock is a message of its own");
        }
    }

    #[test]
    fn issue_408_a_running_status_note_cut_short_by_a_cc() {
        for (events, counts) in deliver(&[&[0x90, 60, 100, 62, 0xB0, 7, 90]]) {
            assert_eq!(events, vec![note_on(60, 100), cc(7, 90)]);
            assert_eq!(counts.malformed, 1);
        }
    }

    #[test]
    fn issue_408_a_note_on_whose_velocity_is_in_the_next_packet() {
        // CoreMIDI documents complete messages per packet, but a driver that
        // splits one must not turn the next status byte into data.
        for (events, _) in deliver(&[&[0x90, 60], &[100, 0xB0, 7, 90]]) {
            assert_eq!(events, vec![note_on(60, 100), cc(7, 90)]);
        }
    }

    #[test]
    fn issue_408_decode_rejects_a_status_byte_in_a_data_position() {
        assert_eq!(crate::decode_midi_bytes(&[0x90, 60, 0xB0]), None);
        assert_eq!(crate::decode_midi_bytes(&[0xB0, 0x90, 7]), None);
        assert_eq!(crate::decode_midi_bytes(&[0xE0, 0, 0xF8]), None);
    }

    /// A 1 KB SysEx (F0 7D, 1,021 data bytes, F7).
    fn one_kb_sysex() -> Vec<u8> {
        let mut sysex = vec![0xF0, 0x7D];
        sysex.extend((0..1_021u32).map(|i| (i % 0x80) as u8));
        sysex.push(0xF7);
        assert_eq!(sysex.len(), 1_024);
        sysex
    }

    #[test]
    fn issue_408_a_1kb_sysex_in_several_packets_of_one_list_is_one_message() {
        let sysex = one_kb_sysex();
        let mut packets: Vec<&[u8]> = sysex.chunks(256).collect();
        packets.push(&[0x90, 60, 100]);
        for (events, counts) in deliver(&packets) {
            assert_eq!(counts.malformed, 0, "a valid SysEx is not malformed");
            assert_eq!(counts.messages, 2, "one SysEx and one Note On");
            assert_eq!(counts.ignored, 1);
            assert_eq!(events, vec![note_on(60, 100)]);
        }
    }

    #[test]
    fn issue_408_a_1kb_sysex_across_separate_read_proc_calls_is_one_message() {
        let sysex = one_kb_sysex();
        for align in LAYOUTS {
            let (state, rx) = input_state();
            let mut total = PacketListCounts::default();
            for chunk in sysex.chunks(256) {
                let list = build_packet_list(&[chunk], align);
                // SAFETY: well-formed list built with `align`, outlives the call.
                let counts = unsafe { read_packet_list(&state, list.as_ptr(), align) };
                total.messages += counts.messages;
                total.malformed += counts.malformed;
                total.ignored += counts.ignored;
            }
            let list = build_packet_list(&[&[0x90, 60, 100]], align);
            // SAFETY: as above.
            let counts = unsafe { read_packet_list(&state, list.as_ptr(), align) };
            assert_eq!(total.malformed, 0, "align_to_4={align}");
            assert_eq!(total.messages, 1, "align_to_4={align}");
            assert_eq!(counts.malformed, 0);
            assert_eq!(
                rx.try_iter().map(|m| m.event).collect::<Vec<_>>(),
                vec![note_on(60, 100)]
            );
        }
    }

    /// Frame `packets` the way the read proc does, collecting the raw
    /// messages (so SysEx content can be checked, which the event queue does
    /// not carry).
    fn frame(packets: &[&[u8]]) -> (Vec<Vec<u8>>, usize) {
        let mut parser = crate::MidiStreamParser::new(crate::MAX_INPUT_SYSEX_BYTES);
        let mut messages = Vec::new();
        let mut malformed = 0;
        for packet in packets {
            malformed += parser
                .push(packet, |message| messages.push(message.to_vec()))
                .malformed;
        }
        (messages, malformed)
    }

    #[test]
    fn issue_408_sysex_reassembled_across_packets_has_its_exact_bytes() {
        let sysex = one_kb_sysex();
        let packets: Vec<&[u8]> = sysex.chunks(300).collect();
        assert_eq!(frame(&packets), (vec![sysex.clone()], 0));
    }

    #[test]
    fn issue_408_a_clock_inside_sysex_is_delivered_and_not_part_of_the_dump() {
        let (messages, malformed) = frame(&[&[0xF0, 0x7D, 0x01, 0xF8, 0x02], &[0x03, 0xF7]]);
        assert_eq!(
            messages,
            vec![vec![0xF8], vec![0xF0, 0x7D, 0x01, 0x02, 0x03, 0xF7]]
        );
        assert_eq!(malformed, 0);
    }

    #[test]
    fn issue_408_the_input_queue_is_bounded_while_nothing_drains() {
        // Over a million events with nobody draining. The queue must hold at
        // most a fixed number and count every event it could not take.
        let mut packet = Vec::new();
        for i in 0..1_000u32 {
            packet.extend_from_slice(&[0x90, (i % 128) as u8, 100]);
        }
        let list = build_packet_list(&[&packet], PACKETS_ARE_4_BYTE_ALIGNED);
        let (state, rx) = input_state();
        let mut dropped = 0u64;
        let mut sent = 0u64;
        for _ in 0..1_100 {
            // SAFETY: well-formed list, outlives the call.
            let counts =
                unsafe { read_packet_list(&state, list.as_ptr(), PACKETS_ARE_4_BYTE_ALIGNED) };
            dropped += counts.dropped;
            sent += counts.events;
        }
        let queued = rx.try_iter().count() as u64;
        assert_eq!(sent, 1_100_000);
        assert!(
            queued <= 65_536,
            "{queued} events queued: the queue is unbounded"
        );
        assert_eq!(queued + dropped, sent, "every event not queued is counted");
    }

    #[test]
    fn issue_408_queuing_input_does_not_allocate_on_the_input_thread() {
        let mut packet = Vec::new();
        for i in 0..500u32 {
            packet.extend_from_slice(&[0x90, (i % 128) as u8, 100]);
        }
        let list = build_packet_list(&[&packet], PACKETS_ARE_4_BYTE_ALIGNED);
        let (state, rx) = input_state();
        // SAFETY: well-formed list, outlives the calls.
        unsafe { read_packet_list(&state, list.as_ptr(), PACKETS_ARE_4_BYTE_ALIGNED) };
        let _ = rx.try_iter().count();
        let before = crate::alloc_counter::this_thread();
        // SAFETY: as above.
        let counts = unsafe { read_packet_list(&state, list.as_ptr(), PACKETS_ARE_4_BYTE_ALIGNED) };
        let allocations = crate::alloc_counter::this_thread() - before;
        assert_eq!(counts.events, 500);
        assert_eq!(
            allocations, 0,
            "{allocations} heap allocations for 500 events"
        );
        drop(rx);
    }

    #[test]
    fn reassembling_a_split_sysex_does_not_allocate_on_the_input_thread() {
        let sysex = one_kb_sysex();
        let packets: Vec<&[u8]> = sysex.chunks(256).collect();
        let list = build_packet_list(&packets, PACKETS_ARE_4_BYTE_ALIGNED);
        let (state, _rx) = input_state();
        let before = crate::alloc_counter::this_thread();
        // SAFETY: well-formed list, outlives the call.
        let counts = unsafe { read_packet_list(&state, list.as_ptr(), PACKETS_ARE_4_BYTE_ALIGNED) };
        assert_eq!(crate::alloc_counter::this_thread() - before, 0);
        assert_eq!((counts.messages, counts.malformed), (1, 0));
    }
}
