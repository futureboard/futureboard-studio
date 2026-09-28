//! A single-slot, wait-free hand-off cell for moving a heap-allocated value
//! between exactly two threads without a blocking lock.
//!
//! Modeled on `rodharerist`'s own IR hand-off cell: one `AtomicPtr` swap per
//! side, no ring buffer, no external dependency. `put` (the producer) and
//! `take` (the consumer) must each only ever be called from their own single
//! thread; the cell itself does not enforce that - see [`crate::PadChannel`]
//! for which side calls which method.

use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

pub(crate) struct HandoffCell<T> {
    slot: AtomicPtr<T>,
}

impl<T> HandoffCell<T> {
    pub(crate) fn new() -> Self {
        Self {
            slot: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// Store `value` in the slot. If a previous value was sitting there
    /// unread, it is returned so the caller can drop it - safe on the
    /// producer's own thread, since an unread value was never touched by the
    /// consumer.
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

// The cell only ever moves a `Box<T>` across the swap; `Send` on `T` is
// enough for the cell itself to be `Send + Sync` (no `&T` is ever shared).
unsafe impl<T: Send> Send for HandoffCell<T> {}
unsafe impl<T: Send> Sync for HandoffCell<T> {}

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
}
