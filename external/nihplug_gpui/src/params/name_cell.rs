//! A parameter display name that can be replaced after construction.
//!
//! `Param::name` returns `&str`, and a host may call it from another thread
//! while the editor renames the parameter. Each published name is leaked and
//! never mutated, so a reader that loaded the pointer holds a valid string
//! for the rest of the process. Renames are rare (a model change), so the
//! leak is a handful of short strings per edit, not a stream.

use std::sync::atomic::{AtomicPtr, Ordering};

struct Frozen {
    ptr: *const u8,
    len: usize,
}

// The bytes are immutable and leaked. Sharing the pointer across threads
// does not alias a mutation.
unsafe impl Send for Frozen {}
unsafe impl Sync for Frozen {}

pub(super) struct NameCell {
    slot: AtomicPtr<Frozen>,
}

impl NameCell {
    pub(super) fn new(name: impl Into<String>) -> Self {
        let cell = Self {
            slot: AtomicPtr::new(std::ptr::null_mut()),
        };
        cell.set(name);
        cell
    }

    pub(super) fn set(&self, name: impl Into<String>) {
        let text = name.into();
        let leaked: &'static str = Box::leak(text.into_boxed_str());
        let frozen = Box::leak(Box::new(Frozen {
            ptr: leaked.as_ptr(),
            len: leaked.len(),
        }));
        // The previous frozen name stays allocated. Freeing it would race
        // a host that still holds the `&str` from `get`.
        self.slot.store(frozen, Ordering::Release);
    }

    pub(super) fn get(&self) -> &str {
        let frozen = self.slot.load(Ordering::Acquire);
        // `new` publishes a name before the cell is observable.
        let frozen = unsafe { &*frozen };
        unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(frozen.ptr, frozen.len)) }
    }
}
