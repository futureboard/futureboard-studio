//! Test-only heap counter, so a unit test can assert that a realtime path
//! neither allocates nor frees on the thread that runs it.
//!
//! Installed as the global allocator of this crate's unit-test binary only. It
//! forwards to the system allocator and, while [`count_heap_events`] is
//! running on a thread, counts that thread's allocations, reallocations and
//! frees. Other test threads are not counted.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct CountingAllocator;

thread_local! {
    // `const` initialisers with no destructor: reading them never allocates,
    // which an allocator hook must not do.
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static EVENTS: Cell<u64> = const { Cell::new(0) };
}

#[inline]
fn note_heap_event() {
    let _ = COUNTING.try_with(|counting| {
        if counting.get() {
            let _ = EVENTS.try_with(|events| events.set(events.get() + 1));
        }
    });
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note_heap_event();
        System.alloc(layout)
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note_heap_event();
        System.alloc_zeroed(layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note_heap_event();
        System.realloc(ptr, layout, new_size)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note_heap_event();
        System.dealloc(ptr, layout)
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// Run `f` and return its result with the number of heap allocations,
/// reallocations and frees it made on this thread.
pub(crate) fn count_heap_events<R>(f: impl FnOnce() -> R) -> (R, u64) {
    EVENTS.with(|events| events.set(0));
    COUNTING.with(|counting| counting.set(true));
    let result = f();
    COUNTING.with(|counting| counting.set(false));
    (result, EVENTS.with(Cell::get))
}
