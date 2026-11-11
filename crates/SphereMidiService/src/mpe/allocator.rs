use crate::expression::NoteId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum MpeChannelExhaustionPolicy {
    #[default]
    OldestReleased,
    OldestActive,
    LowestVelocity,
    HighestNote,
    LowestNote,
    RejectNewNote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChannelSlot {
    note_id: NoteId,
    pitch: u8,
    velocity: u8,
    order: u64,
    held: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MpeChannelAllocation {
    pub channel: u8,
    pub stolen_note: Option<NoteId>,
    pub stolen_pitch: Option<u8>,
    pub reset_required: bool,
}

/// Fixed-size member-channel allocator.  It owns only transport assignment;
/// expression itself remains attached to the note id in the project model.
#[derive(Debug, Clone)]
pub struct MpeChannelAllocator {
    member_channel_start: u8,
    member_channel_end: u8,
    policy: MpeChannelExhaustionPolicy,
    slots: [Option<ChannelSlot>; 16],
    released_channels: [bool; 16],
    next_order: u64,
}

impl MpeChannelAllocator {
    pub fn new(member_channel_start: u8, member_channel_end: u8) -> Self {
        Self {
            member_channel_start: member_channel_start.min(15),
            member_channel_end: member_channel_end.min(15),
            policy: MpeChannelExhaustionPolicy::default(),
            slots: [None; 16],
            released_channels: [false; 16],
            next_order: 0,
        }
    }

    pub fn with_policy(mut self, policy: MpeChannelExhaustionPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn policy(&self) -> MpeChannelExhaustionPolicy {
        self.policy
    }

    pub fn set_policy(&mut self, policy: MpeChannelExhaustionPolicy) {
        self.policy = policy;
    }

    pub fn allocate(
        &mut self,
        note_id: NoteId,
        pitch: u8,
        velocity: u8,
    ) -> Option<MpeChannelAllocation> {
        let channel = (self.member_channel_start..=self.member_channel_end)
            .find(|&channel| self.slots[channel as usize].is_none())
            .or_else(|| self.select_exhausted_channel())?;
        self.next_order = self.next_order.wrapping_add(1);
        let was_released = self.released_channels[channel as usize];
        let previous = self.slots[channel as usize].replace(ChannelSlot {
            note_id,
            pitch,
            velocity,
            order: self.next_order,
            held: true,
        });
        // A release marker belongs to the old voice. Clear it as soon as the
        // channel is claimed so a later active-voice steal does not keep
        // reporting a stale release forever.
        self.released_channels[channel as usize] = false;
        Some(MpeChannelAllocation {
            channel,
            stolen_note: previous.map(|slot| slot.note_id),
            stolen_pitch: previous.map(|slot| slot.pitch),
            reset_required: previous.is_some() || was_released,
        })
    }

    /// Mark a note as released but still held by a sustain policy.  A later
    /// call to [`Self::release`] makes the channel reusable.
    pub fn mark_released(&mut self, note_id: NoteId) -> Option<u8> {
        let channel = self.channel_for_note(note_id)?;
        if let Some(slot) = self.slots[channel as usize].as_mut() {
            slot.held = false;
        }
        self.released_channels[channel as usize] = true;
        Some(channel)
    }

    pub fn release(&mut self, note_id: NoteId) -> Option<u8> {
        let channel = self.channel_for_note(note_id)?;
        self.slots[channel as usize] = None;
        self.released_channels[channel as usize] = true;
        Some(channel)
    }

    pub fn release_channel(&mut self, channel: u8) -> Option<NoteId> {
        if channel < self.member_channel_start || channel > self.member_channel_end {
            return None;
        }
        let slot = self.slots.get_mut(channel as usize)?.take()?;
        self.released_channels[channel as usize] = true;
        Some(slot.note_id)
    }

    pub fn channel_for_note(&self, note_id: NoteId) -> Option<u8> {
        (self.member_channel_start..=self.member_channel_end).find(|&channel| {
            self.slots[channel as usize].is_some_and(|slot| slot.note_id == note_id)
        })
    }

    pub fn active_note(&self, channel: u8) -> Option<NoteId> {
        self.slots
            .get(channel as usize)
            .and_then(|slot| slot.map(|slot| slot.note_id))
    }

    pub fn pitch_for_channel(&self, channel: u8) -> Option<u8> {
        self.slots
            .get(channel as usize)
            .and_then(|slot| slot.map(|slot| slot.pitch))
    }

    pub fn reset(&mut self) {
        self.slots = [None; 16];
        self.released_channels = [false; 16];
        self.next_order = 0;
    }

    fn select_exhausted_channel(&self) -> Option<u8> {
        let mut oldest_released: Option<(u8, ChannelSlot)> = None;
        let mut selected: Option<(u8, ChannelSlot)> = None;
        for channel in self.member_channel_start..=self.member_channel_end {
            let Some(slot) = self.slots[channel as usize] else {
                continue;
            };
            if !slot.held && oldest_released.is_none_or(|(_, current)| slot.order < current.order) {
                oldest_released = Some((channel, slot));
            }
            if selected.is_none_or(|(_, current)| self.is_better_candidate(slot, current)) {
                selected = Some((channel, slot));
            }
        }
        if let Some((channel, _)) = oldest_released {
            return Some(channel);
        }
        if self.policy == MpeChannelExhaustionPolicy::RejectNewNote {
            return None;
        }
        selected.map(|(channel, _)| channel)
    }

    fn is_better_candidate(&self, candidate: ChannelSlot, current: ChannelSlot) -> bool {
        match self.policy {
            MpeChannelExhaustionPolicy::OldestReleased
            | MpeChannelExhaustionPolicy::OldestActive => candidate.order < current.order,
            MpeChannelExhaustionPolicy::LowestVelocity => {
                (candidate.velocity, candidate.order) < (current.velocity, current.order)
            }
            MpeChannelExhaustionPolicy::HighestNote => {
                (candidate.pitch, std::cmp::Reverse(candidate.order))
                    > (current.pitch, std::cmp::Reverse(current.order))
            }
            MpeChannelExhaustionPolicy::LowestNote => {
                (candidate.pitch, candidate.order) < (current.pitch, current.order)
            }
            MpeChannelExhaustionPolicy::RejectNewNote => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn released_channels_are_reused_before_stealing() {
        let mut allocator = MpeChannelAllocator::new(1, 2);
        assert_eq!(allocator.allocate(1, 60, 100).unwrap().channel, 1);
        assert_eq!(allocator.allocate(2, 64, 100).unwrap().channel, 2);
        allocator.mark_released(1);
        let allocation = allocator.allocate(3, 67, 100).unwrap();
        assert_eq!(allocation.channel, 1);
        assert_eq!(allocation.stolen_note, Some(1));
    }

    #[test]
    fn reject_policy_only_rejects_when_no_released_slot_exists() {
        let mut allocator =
            MpeChannelAllocator::new(1, 1).with_policy(MpeChannelExhaustionPolicy::RejectNewNote);
        allocator.allocate(1, 60, 100).unwrap();
        assert!(allocator.allocate(2, 62, 100).is_none());
        allocator.mark_released(1);
        assert!(allocator.allocate(2, 62, 100).is_some());
    }
}
