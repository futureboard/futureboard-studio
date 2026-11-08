//! A single-slot, wait-free hand-off cell for moving a heap-allocated value
//! between exactly two threads without a blocking lock.
//!
//! This is the crate's own minimal "engine command queue": no ring buffer, no
//! external dependency — just one `AtomicPtr` swap per side. `put` (the
//! producer) and `take` (the consumer) must each only ever be called from
//! their own single thread; the cell itself does not enforce that, so every
//! [`HandoffCell`] field in this crate is documented with which side calls
//! which method (see [`crate::dsp::nam::NamCapture`]).
//!
//! [`RetireBin`] is the way back: a few such slots, so the audio thread can
//! always hand a model it let go of to the control thread without freeing it.

use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

pub(crate) struct HandoffCell<T> {
    slot: AtomicPtr<T>,
}

impl<T> HandoffCell<T> {
    pub(crate) fn new() -> Self {
        Self {
            slot: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// Store `value` in the slot. If a previous value was sitting there unread,
    /// it is returned so the caller can drop it — safe to do on the producer's
    /// own thread, since an unread value was never touched by the consumer.
    pub(crate) fn put(&self, value: Box<T>) -> Option<Box<T>> {
        let new_ptr = Box::into_raw(value);
        let old_ptr = self.slot.swap(new_ptr, Ordering::AcqRel);
        (!old_ptr.is_null()).then(|| unsafe { Box::from_raw(old_ptr) })
    }

    /// Take whatever is in the slot, leaving it empty. Wait-free: a single
    /// atomic swap, safe to call from a realtime thread.
    pub(crate) fn take(&self) -> Option<Box<T>> {
        let ptr = self.slot.swap(ptr::null_mut(), Ordering::AcqRel);
        (!ptr.is_null()).then(|| unsafe { Box::from_raw(ptr) })
    }
}

impl<T> Drop for HandoffCell<T> {
    fn drop(&mut self) {
        let ptr = self.slot.swap(ptr::null_mut(), Ordering::AcqRel);
        if !ptr.is_null() {
            drop(unsafe { Box::from_raw(ptr) });
        }
    }
}

// The cell only ever moves a `Box<T>` across the swap; `Send` on `T` is enough
// for the cell itself to be `Send + Sync` (no `&T` is ever shared).
unsafe impl<T: Send> Send for HandoffCell<T> {}
unsafe impl<T: Send> Sync for HandoffCell<T> {}

/// How many retired models one engine can have waiting for the control side.
///
/// A model reaches the audio side only through `submit`, and every `submit`
/// drains the bin before it queues its model. So from the start of one drain
/// to the start of the next, the models that can be retired are the ones the
/// audio side held when that drain began (the live one, and the one fading
/// out or staged: two), the one queued at that moment, and the one that
/// `submit` then queues: four. Any extra sweep only empties the bin sooner.
pub(crate) const RETIRE_SLOTS: usize = 4;

/// Audio thread → control thread: models the audio side has let go of, for
/// the control thread to free. The audio thread never frees one, and never
/// overwrites one: [`Self::retire`] only fills an empty slot.
pub(crate) struct RetireBin<T> {
    slots: [AtomicPtr<T>; RETIRE_SLOTS],
    /// Models [`Self::retire`] found no slot for and leaked rather than free.
    /// [`RETIRE_SLOTS`] rules that out, so this stays 0.
    leaked: AtomicUsize,
}

impl<T> RetireBin<T> {
    pub(crate) fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| AtomicPtr::new(ptr::null_mut())),
            leaked: AtomicUsize::new(0),
        }
    }

    /// Audio thread: hand `value` to the control side. Wait-free: at most
    /// one compare-exchange per slot. If every slot were full the model is
    /// leaked and counted, never dropped here.
    pub(crate) fn retire(&self, value: Box<T>) {
        let raw = Box::into_raw(value);
        for slot in &self.slots {
            if slot
                .compare_exchange(ptr::null_mut(), raw, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
        // `raw` is deliberately not turned back into a `Box`.
        self.leaked.fetch_add(1, Ordering::Relaxed);
    }

    /// Control thread: take every retired model, for the caller to drop.
    pub(crate) fn drain(&self) -> Retired<T> {
        Retired {
            models: std::array::from_fn(|i| {
                let raw = self.slots[i].swap(ptr::null_mut(), Ordering::AcqRel);
                (!raw.is_null()).then(|| unsafe { Box::from_raw(raw) })
            }),
        }
    }

    /// How many models were leaked because the bin was full. 0 unless the
    /// bound in [`RETIRE_SLOTS`] is broken.
    pub(crate) fn leaked(&self) -> usize {
        self.leaked.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn occupied(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| !slot.load(Ordering::Acquire).is_null())
            .count()
    }
}

impl<T> Drop for RetireBin<T> {
    fn drop(&mut self) {
        drop(self.drain());
    }
}

// Same reasoning as `HandoffCell`: only `Box<T>`s move through the slots.
unsafe impl<T: Send> Send for RetireBin<T> {}
unsafe impl<T: Send> Sync for RetireBin<T> {}

/// Models the audio side retired, handed to the control thread to drop.
/// Dropping this frees them, so drop it where freeing is allowed.
pub struct Retired<T> {
    models: [Option<Box<T>>; RETIRE_SLOTS],
}

impl<T> Retired<T> {
    /// How many models this holds.
    pub fn len(&self) -> usize {
        self.models.iter().flatten().count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_then_take_roundtrips() {
        let cell = HandoffCell::new();
        assert!(cell.put(Box::new(42)).is_none());
        assert_eq!(cell.take(), Some(Box::new(42)));
        assert_eq!(cell.take(), None);
    }

    #[test]
    fn put_returns_previous_unread_value() {
        let cell = HandoffCell::new();
        assert!(cell.put(Box::new(1)).is_none());
        let bumped = cell.put(Box::new(2));
        assert_eq!(bumped, Some(Box::new(1)));
        assert_eq!(cell.take(), Some(Box::new(2)));
    }

    #[test]
    fn drop_reclaims_unread_value() {
        // No leak-checker here, but this exercises the Drop path under Miri/ASan.
        let cell = HandoffCell::new();
        cell.put(Box::new(String::from("leak me not")));
    }

    #[test]
    fn the_retire_bin_hands_back_every_model() {
        let bin = RetireBin::new();
        for i in 0..RETIRE_SLOTS {
            bin.retire(Box::new(i));
        }
        assert_eq!(bin.occupied(), RETIRE_SLOTS);
        let retired = bin.drain();
        assert_eq!(retired.len(), RETIRE_SLOTS);
        assert_eq!(bin.occupied(), 0);
        assert!(bin.drain().is_empty());
        assert_eq!(bin.leaked(), 0);
    }

    /// A full bin must not free on the retiring thread: it leaks and counts.
    #[test]
    fn a_full_retire_bin_leaks_instead_of_freeing() {
        let bin = RetireBin::new();
        for i in 0..RETIRE_SLOTS {
            bin.retire(Box::new(vec![i; 4]));
        }
        crate::alloc_probe::reset();
        crate::alloc_probe::arm();
        bin.retire(Box::new(vec![99; 4]));
        crate::alloc_probe::disarm();
        assert_eq!(crate::alloc_probe::frees(), 0, "the overflow was freed");
        assert_eq!(bin.leaked(), 1);
        assert_eq!(bin.drain().len(), RETIRE_SLOTS, "earlier models are kept");
    }
}
