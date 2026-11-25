//! Events, the fixed-capacity buffers devices read and write, and the note
//! bookkeeping every device shares.

/// What a [`MidiEvent`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    NoteOn,
    NoteOff,
    /// A controller. `data` is the controller number as the engine encodes it
    /// (MIDI CC `0..=127`, `128` channel pressure, `129` pitch bend, `130`
    /// program change); `value` is normalized `0.0..=1.0`.
    Control,
}

impl Kind {
    /// Order of events that share an offset: releases first, so a note that
    /// ends where the next one starts is never cut by its own successor.
    fn rank(self) -> u8 {
        match self {
            Kind::NoteOff => 0,
            Kind::Control => 1,
            Kind::NoteOn => 2,
        }
    }
}

/// One MIDI event inside a block. Same fields as the engine's
/// `Vst3MidiEvent`, so the engine converts without looking anything up.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MidiEvent {
    /// Sample offset inside the block.
    pub offset: u32,
    pub kind: Kind,
    pub channel: u8,
    /// Note number for notes, controller number for [`Kind::Control`].
    pub data: u8,
    /// Velocity (`0.0..=1.0`) for notes, value for controllers.
    pub value: f32,
}

impl MidiEvent {
    pub const fn note_on(offset: u32, channel: u8, key: u8, velocity: f32) -> Self {
        Self {
            offset,
            kind: Kind::NoteOn,
            channel,
            data: key,
            value: velocity,
        }
    }

    pub const fn note_off(offset: u32, channel: u8, key: u8) -> Self {
        Self {
            offset,
            kind: Kind::NoteOff,
            channel,
            data: key,
            value: 0.0,
        }
    }

    pub const fn control(offset: u32, channel: u8, controller: u8, value: f32) -> Self {
        Self {
            offset,
            kind: Kind::Control,
            channel,
            data: controller,
            value,
        }
    }

    const EMPTY: Self = Self::note_off(0, 0, 0);
}

/// Events one block can carry through a device. Generous for a MIDI stream
/// (a dense block is a few dozen events); past it, events are counted as
/// dropped rather than reallocating on the audio thread.
pub const MAX_EVENTS: usize = 512;

/// A block's events, sorted by offset, in a fixed array.
#[derive(Clone)]
pub struct EventBuffer {
    events: [MidiEvent; MAX_EVENTS],
    len: usize,
    dropped: u32,
}

impl Default for EventBuffer {
    fn default() -> Self {
        Self {
            events: [MidiEvent::EMPTY; MAX_EVENTS],
            len: 0,
            dropped: 0,
        }
    }
}

impl std::fmt::Debug for EventBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.as_slice()).finish()
    }
}

impl EventBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Events that did not fit since this buffer was created.
    pub fn dropped(&self) -> u32 {
        self.dropped
    }

    pub fn as_slice(&self) -> &[MidiEvent] {
        &self.events[..self.len]
    }

    /// Append, keeping the buffer sorted by offset (then note-off, control,
    /// note-on). Events normally arrive in order, so this is a compare and a
    /// store; an out-of-order event is moved into place.
    pub fn push(&mut self, event: MidiEvent) -> bool {
        if self.len == MAX_EVENTS {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        let key = (event.offset, event.kind.rank());
        let mut at = self.len;
        while at > 0 {
            let prev = &self.events[at - 1];
            if (prev.offset, prev.kind.rank()) <= key {
                break;
            }
            at -= 1;
        }
        self.events.copy_within(at..self.len, at + 1);
        self.events[at] = event;
        self.len += 1;
        true
    }

    /// Replace the contents with `other`'s.
    pub fn copy_from(&mut self, other: &EventBuffer) {
        self.events[..other.len].copy_from_slice(other.as_slice());
        self.len = other.len;
    }
}

/// Which notes a device has sounding at its output, so it can always release
/// them (bypass, removal, transport stop) and so two of its inputs mapped to
/// the same output note sound it once.
#[derive(Clone)]
pub struct NoteTracker {
    counts: [u8; 16 * 128],
    sounding: u32,
}

impl Default for NoteTracker {
    fn default() -> Self {
        Self {
            counts: [0; 16 * 128],
            sounding: 0,
        }
    }
}

impl NoteTracker {
    fn slot(channel: u8, key: u8) -> usize {
        usize::from(channel & 0x0F) * 128 + usize::from(key & 0x7F)
    }

    /// Record a note-on. Returns `true` when the note was not sounding, i.e.
    /// the note-on should be sent.
    pub fn on(&mut self, channel: u8, key: u8) -> bool {
        let c = &mut self.counts[Self::slot(channel, key)];
        *c = c.saturating_add(1);
        if *c == 1 {
            self.sounding += 1;
            true
        } else {
            false
        }
    }

    /// Record a note-off. Returns `true` when this released the last holder,
    /// i.e. the note-off should be sent.
    pub fn off(&mut self, channel: u8, key: u8) -> bool {
        let c = &mut self.counts[Self::slot(channel, key)];
        match *c {
            0 => false,
            1 => {
                *c = 0;
                self.sounding -= 1;
                true
            }
            _ => {
                *c -= 1;
                false
            }
        }
    }

    pub fn is_on(&self, channel: u8, key: u8) -> bool {
        self.counts[Self::slot(channel, key)] > 0
    }

    pub fn any(&self) -> bool {
        self.sounding > 0
    }

    /// Note-offs for everything sounding, at `offset`, then forget it all.
    pub fn release_all(&mut self, offset: u32, out: &mut EventBuffer) {
        if self.sounding == 0 {
            return;
        }
        for (slot, count) in self.counts.iter_mut().enumerate() {
            if *count > 0 {
                *count = 0;
                out.push(MidiEvent::note_off(
                    offset,
                    (slot / 128) as u8,
                    (slot % 128) as u8,
                ));
            }
        }
        self.sounding = 0;
    }
}

/// Events a device has promised for later blocks (a note-off at the end of a
/// gate, a delayed note), in the device's own sample clock.
#[derive(Clone)]
pub struct Scheduler {
    due: [u64; Self::CAPACITY],
    events: [MidiEvent; Self::CAPACITY],
    len: usize,
}

impl Default for Scheduler {
    fn default() -> Self {
        Self {
            due: [0; Self::CAPACITY],
            events: [MidiEvent::EMPTY; Self::CAPACITY],
            len: 0,
        }
    }
}

impl Scheduler {
    pub const CAPACITY: usize = 256;

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Hold `event` (its offset ignored) until clock `due`. Returns `false`
    /// when full; a caller scheduling the end of a note it already started
    /// then sends the note-off at once (see [`crate::devices`]), so a full
    /// scheduler shortens a note rather than leaving it hanging.
    pub fn schedule(&mut self, due: u64, event: MidiEvent) -> bool {
        if self.len == Self::CAPACITY {
            return false;
        }
        self.due[self.len] = due;
        self.events[self.len] = event;
        self.len += 1;
        true
    }

    /// Drop pending note-ons (their notes never started) and keep the rest.
    pub fn drop_note_ons(&mut self) {
        let mut kept = 0;
        for i in 0..self.len {
            if self.events[i].kind != Kind::NoteOn {
                self.due[kept] = self.due[i];
                self.events[kept] = self.events[i];
                kept += 1;
            }
        }
        self.len = kept;
    }

    /// Move everything due before `clock + until` into `out`, at its offset
    /// from `clock`, in due order, updating `tracker` as notes start and end.
    /// A device sweeping a block calls this before each input event (with
    /// `until = event offset + 1`) so promised events and new input meet in
    /// time order, then once with the block length.
    pub fn drain_before(
        &mut self,
        clock: u64,
        until: u32,
        out: &mut EventBuffer,
        tracker: &mut NoteTracker,
    ) {
        let end = clock + u64::from(until);
        loop {
            // Earliest due first (note-offs first on a tie), so the tracker
            // sees a note end before a new one on the same key starts.
            let mut pick: Option<usize> = None;
            for i in 0..self.len {
                if self.due[i] >= end {
                    continue;
                }
                let better = match pick {
                    None => true,
                    Some(p) => {
                        (self.due[i], self.events[i].kind == Kind::NoteOn)
                            < (self.due[p], self.events[p].kind == Kind::NoteOn)
                    }
                };
                if better {
                    pick = Some(i);
                }
            }
            let Some(i) = pick else { break };
            let mut event = self.events[i];
            event.offset = self.due[i].saturating_sub(clock) as u32;
            self.len -= 1;
            self.due[i] = self.due[self.len];
            self.events[i] = self.events[self.len];
            emit(event, out, tracker);
        }
    }

    /// [`Self::drain_before`] for a whole block of `frames`.
    pub fn drain_block(
        &mut self,
        clock: u64,
        frames: u32,
        out: &mut EventBuffer,
        tracker: &mut NoteTracker,
    ) {
        self.drain_before(clock, frames, out, tracker);
    }

    /// Forget a pending event matching `kind` / `channel` / `data`, if any.
    pub fn cancel(&mut self, kind: Kind, channel: u8, data: u8) -> bool {
        for i in 0..self.len {
            let e = &self.events[i];
            if e.kind == kind && e.channel == channel && e.data == data {
                self.len -= 1;
                self.due[i] = self.due[self.len];
                self.events[i] = self.events[self.len];
                return true;
            }
        }
        false
    }
}

/// Send `event`, keeping `tracker` in step: a duplicate note-on and a
/// note-off for a note still held by another source are swallowed.
pub fn emit(event: MidiEvent, out: &mut EventBuffer, tracker: &mut NoteTracker) {
    match event.kind {
        Kind::NoteOn => {
            if tracker.on(event.channel, event.data) {
                out.push(event);
            }
        }
        Kind::NoteOff => {
            if tracker.off(event.channel, event.data) {
                out.push(event);
            }
        }
        Kind::Control => {
            out.push(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_keeps_offset_order_and_releases_first() {
        let mut b = EventBuffer::new();
        b.push(MidiEvent::note_on(10, 0, 60, 1.0));
        b.push(MidiEvent::note_on(5, 0, 62, 1.0));
        b.push(MidiEvent::note_off(10, 0, 59));
        b.push(MidiEvent::control(10, 0, 64, 1.0));
        let order: Vec<_> = b.as_slice().iter().map(|e| (e.offset, e.kind)).collect();
        assert_eq!(
            order,
            vec![
                (5, Kind::NoteOn),
                (10, Kind::NoteOff),
                (10, Kind::Control),
                (10, Kind::NoteOn)
            ]
        );
    }

    #[test]
    fn a_full_buffer_counts_drops_instead_of_growing() {
        let mut b = EventBuffer::new();
        for i in 0..MAX_EVENTS + 3 {
            b.push(MidiEvent::note_on(i as u32, 0, 60, 1.0));
        }
        assert_eq!(b.len(), MAX_EVENTS);
        assert_eq!(b.dropped(), 3);
    }

    #[test]
    fn tracker_dedupes_and_releases_everything() {
        let mut t = NoteTracker::default();
        assert!(t.on(0, 60));
        assert!(!t.on(0, 60), "second holder does not resend");
        assert!(t.on(1, 60));
        assert!(!t.off(0, 60), "one holder left");
        let mut out = EventBuffer::new();
        t.release_all(7, &mut out);
        assert_eq!(out.len(), 2);
        assert!(
            out.as_slice()
                .iter()
                .all(|e| e.kind == Kind::NoteOff && e.offset == 7)
        );
        assert!(!t.any());
    }

    #[test]
    fn scheduler_emits_what_is_due_and_keeps_the_rest() {
        let mut s = Scheduler::default();
        let mut t = NoteTracker::default();
        t.on(0, 60);
        s.schedule(100, MidiEvent::note_off(0, 0, 60));
        s.schedule(300, MidiEvent::note_on(0, 0, 64, 0.5));
        let mut out = EventBuffer::new();
        s.drain_block(64, 128, &mut out, &mut t);
        assert_eq!(out.as_slice(), &[MidiEvent::note_off(36, 0, 60)]);
        assert_eq!(s.len(), 1);
        s.drop_note_ons();
        assert!(s.is_empty());
    }
}
