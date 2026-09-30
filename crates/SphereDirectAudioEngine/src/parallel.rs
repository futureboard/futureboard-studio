//! Multi-core processing for the render pass.
//!
//! The expensive part of a block is each track's insert chain: its instrument
//! and its plug-ins. Chains of tracks that do not feed each other are
//! independent, so the render pass hands a batch of them to this pool and the
//! audio thread works through the batch alongside the pool's threads. Summing
//! into buses and the master, sends, meters and taps stay on the audio thread,
//! in the order they always ran, so a block renders bit-identically with the
//! pool on or off.
//!
//! Realtime rules, on the dispatching (audio) thread:
//!
//! * no allocation and no lock: the pool is a `static`, its threads are
//!   spawned from Settings on the control thread, and a batch is one pointer
//!   to a descriptor on the caller's stack;
//! * wakeups are `Thread::unpark` (a futex/`WakeByAddress` call), and the
//!   workers spin briefly before parking so the second pass of a block usually
//!   finds them awake;
//! * the pool is claimed with a compare-exchange, never waited for. A second
//!   render (an offline bounce while playback runs) that finds it busy renders
//!   its batch on its own thread instead.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread::Thread;

/// Most threads a block can be processed on, the audio thread included.
pub const MAX_PROCESSING_THREADS: usize = 32;
const MAX_WORKERS: usize = MAX_PROCESSING_THREADS - 1;
/// Upper bound for "Auto": past this, wake-up cost outgrows what a typical
/// project can hand out per block.
const AUTO_THREAD_CAP: usize = 16;
/// Polls of the generation counter before a worker parks. A few tens of
/// microseconds: long enough to bridge the gap between the two passes of one
/// block, short enough not to burn a core between callbacks.
const SPIN_BEFORE_PARK: u32 = 4_000;

type RunFn = unsafe fn(*const (), usize);

#[derive(Clone, Copy)]
struct Batch {
    run: RunFn,
    ctx: *const (),
    count: usize,
}

unsafe fn run_nothing(_: *const (), _: usize) {}

struct Pool {
    /// Worker handles by slot. Filled on the control thread, read by the
    /// dispatcher to wake them. A slot is never emptied: shrinking the pool
    /// only lowers `workers`, and a worker past the count stays parked.
    threads: [OnceLock<Thread>; MAX_WORKERS],
    /// Workers spawned so far. Control thread only.
    spawned: Mutex<usize>,
    enabled: AtomicBool,
    /// Workers that take part in a batch.
    workers: AtomicUsize,
    /// Held by the one render currently dispatching.
    busy: AtomicBool,
    /// Bumped once per batch; workers wait for it to move.
    generation: AtomicU64,
    /// True while the current batch accepts workers.
    open: AtomicBool,
    /// Written only while `busy` is held, `open` is false and no worker is
    /// inside a batch; read only by a worker that saw `open`.
    batch: UnsafeCell<Batch>,
    next: AtomicUsize,
    completed: AtomicUsize,
    /// Workers currently inside a batch. The dispatcher does not return until
    /// this drains, so no worker can outlive the descriptor on its stack.
    in_batch: AtomicUsize,
    panicked: AtomicBool,
}

// SAFETY: `batch` is the only non-atomic state. It is written by the one
// thread holding `busy`, while `open` is false and `in_batch` is zero, and read
// only by workers that registered in `in_batch` and then saw `open` — see
// `dispatch` and `participate`.
unsafe impl Sync for Pool {}

/// Batches that went to the pool, for tests that need to know it was used.
#[cfg(test)]
pub(crate) static DISPATCHED: AtomicU64 = AtomicU64::new(0);

static POOL: Pool = Pool {
    threads: [const { OnceLock::new() }; MAX_WORKERS],
    spawned: Mutex::new(0),
    enabled: AtomicBool::new(false),
    workers: AtomicUsize::new(0),
    busy: AtomicBool::new(false),
    generation: AtomicU64::new(0),
    open: AtomicBool::new(false),
    batch: UnsafeCell::new(Batch {
        run: run_nothing,
        ctx: std::ptr::null(),
        count: 0,
    }),
    next: AtomicUsize::new(0),
    completed: AtomicUsize::new(0),
    in_batch: AtomicUsize::new(0),
    panicked: AtomicBool::new(false),
};

/// Multi-core processing as it is configured right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MulticoreStatus {
    pub enabled: bool,
    /// Threads a block is processed on, the audio thread included. `1` when
    /// multi-core processing is off.
    pub threads: usize,
}

/// Threads "Auto" resolves to on this machine: every core but one, so the UI
/// and the plug-in host keep a core of their own, capped at a count a block
/// can actually use.
pub fn auto_processing_threads() -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    let threads = if cores <= 2 { cores } else { cores - 1 };
    threads.clamp(1, AUTO_THREAD_CAP)
}

/// Turn multi-core processing on or off. `threads` counts the audio thread;
/// `0` means [`auto_processing_threads`]. Spawns any workers the new count
/// needs, so call it from a control thread, never from the audio callback.
pub fn configure_multicore(enabled: bool, threads: usize) -> MulticoreStatus {
    let total = if threads == 0 {
        auto_processing_threads()
    } else {
        threads.clamp(1, MAX_PROCESSING_THREADS)
    };
    let wanted = if enabled { total - 1 } else { 0 };
    let ready = ensure_workers(wanted);
    POOL.workers.store(ready, Ordering::Relaxed);
    POOL.enabled.store(enabled && ready > 0, Ordering::Relaxed);
    multicore_status()
}

pub fn multicore_status() -> MulticoreStatus {
    let enabled = is_active();
    MulticoreStatus {
        enabled,
        threads: if enabled {
            POOL.workers.load(Ordering::Relaxed) + 1
        } else {
            1
        },
    }
}

/// True when a batch can go to the pool. One relaxed load pair per block.
#[inline]
pub(crate) fn is_active() -> bool {
    POOL.enabled.load(Ordering::Relaxed) && POOL.workers.load(Ordering::Relaxed) > 0
}

/// Spawn workers up to `wanted`; returns how many exist. A failed spawn stops
/// there and the pool runs with the workers it has.
fn ensure_workers(wanted: usize) -> usize {
    let wanted = wanted.min(MAX_WORKERS);
    let mut spawned = POOL.spawned.lock().unwrap_or_else(|e| e.into_inner());
    while *spawned < wanted {
        let slot = *spawned;
        let result = std::thread::Builder::new()
            .name(format!("fb-audio-worker-{}", slot + 1))
            .spawn(move || worker_main(slot));
        match result {
            Ok(handle) => {
                let _ = POOL.threads[slot].set(handle.thread().clone());
                *spawned += 1;
            }
            Err(error) => {
                eprintln!("[DAUx] could not spawn audio worker {}: {error}", slot + 1);
                break;
            }
        }
    }
    (*spawned).min(wanted)
}

fn worker_main(slot: usize) {
    promote_worker_thread();
    let mut seen = POOL.generation.load(Ordering::Acquire);
    loop {
        let mut spins = 0u32;
        loop {
            let generation = POOL.generation.load(Ordering::Acquire);
            if generation != seen {
                seen = generation;
                break;
            }
            if spins < SPIN_BEFORE_PARK {
                spins += 1;
                std::hint::spin_loop();
            } else {
                // Spurious returns are fine: the loop re-reads the counter.
                std::thread::park();
            }
        }
        if slot < POOL.workers.load(Ordering::Relaxed) {
            participate();
        }
    }
}

/// Join the current batch, if one is still open.
fn participate() {
    POOL.in_batch.fetch_add(1, Ordering::SeqCst);
    if POOL.open.load(Ordering::SeqCst) {
        // SAFETY: `open` was stored after `batch` was written, and `batch` is
        // not rewritten until `in_batch` — which counts this worker — drains.
        let batch = unsafe { *POOL.batch.get() };
        run_items(batch);
    }
    POOL.in_batch.fetch_sub(1, Ordering::SeqCst);
}

fn run_items(batch: Batch) {
    loop {
        let index = POOL.next.fetch_add(1, Ordering::Relaxed);
        if index >= batch.count {
            break;
        }
        // A panic must still count the item, or the dispatcher would wait for
        // it forever. Release builds abort on panic, so this only matters in
        // tests and debug runs; the dispatcher re-raises it.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // SAFETY: the dispatcher guarantees `ctx` outlives the batch and
            // that items are independent; each index is claimed exactly once.
            unsafe { (batch.run)(batch.ctx, index) }
        }));
        if outcome.is_err() {
            POOL.panicked.store(true, Ordering::Relaxed);
        }
        POOL.completed.fetch_add(1, Ordering::Release);
    }
}

/// Run `run(ctx, i)` for every `i` in `0..count`, spread over the pool and the
/// calling thread. Returns `false`, having run nothing, when the pool is off,
/// busy, or the batch is too small to be worth splitting — the caller then
/// runs the items itself.
///
/// # Safety
///
/// `ctx` must stay valid for the call, and `run` must be safe to call for
/// different indices at the same time from different threads: the items must
/// touch disjoint data, and whatever they touch must be `Send`.
pub(crate) unsafe fn dispatch(count: usize, ctx: *const (), run: RunFn) -> bool {
    let workers = POOL.workers.load(Ordering::Relaxed);
    if count < 2 || workers == 0 || !POOL.enabled.load(Ordering::Relaxed) {
        return false;
    }
    if POOL
        .busy
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return false;
    }
    let batch = Batch { run, ctx, count };
    // SAFETY: `busy` is held, and the previous dispatcher left `open` false
    // and `in_batch` drained, so no worker is reading `batch`.
    unsafe { *POOL.batch.get() = batch };
    POOL.next.store(0, Ordering::Relaxed);
    POOL.completed.store(0, Ordering::Relaxed);
    POOL.open.store(true, Ordering::SeqCst);
    POOL.generation.fetch_add(1, Ordering::Release);
    // The calling thread takes items too, so one fewer worker than items is
    // all a batch can use.
    for slot in 0..workers.min(count - 1) {
        if let Some(thread) = POOL.threads[slot].get() {
            thread.unpark();
        }
    }

    run_items(batch);

    let mut spins = 0u32;
    while POOL.completed.load(Ordering::Acquire) < count {
        std::hint::spin_loop();
        spins = spins.wrapping_add(1);
        // A worker preempted mid-item may share this core; let it finish.
        if spins % 1024 == 0 {
            std::thread::yield_now();
        }
    }
    POOL.open.store(false, Ordering::SeqCst);
    // Workers that joined after the last item was claimed leave at once.
    while POOL.in_batch.load(Ordering::SeqCst) != 0 {
        std::hint::spin_loop();
    }
    let panicked = POOL.panicked.swap(false, Ordering::Relaxed);
    POOL.busy.store(false, Ordering::Release);
    #[cfg(test)]
    DISPATCHED.fetch_add(1, Ordering::Relaxed);
    if panicked {
        panic!("a track's processing panicked on an audio worker thread");
    }
    true
}

/// Give a worker the audio thread's scheduling class, so it is not the one
/// thread in a block that the OS lets wait.
fn promote_worker_thread() {
    #[cfg(target_os = "windows")]
    {
        #[link(name = "avrt")]
        extern "system" {
            fn AvSetMmThreadCharacteristicsW(task_name: *const u16, task_index: *mut u32) -> isize;
        }
        let task: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
        let mut task_index = 0u32;
        // SAFETY: `task` is a NUL-terminated UTF-16 string that outlives the
        // call. The handle is kept for the thread's lifetime (never reverted).
        let handle = unsafe { AvSetMmThreadCharacteristicsW(task.as_ptr(), &mut task_index) };
        if handle == 0 {
            eprintln!("[DAUx] audio worker could not join MMCSS 'Pro Audio'");
        }
    }
    #[cfg(target_os = "linux")]
    {
        // Same budget the callback thread gets when the system grants one;
        // without it the worker stays at the default policy.
        // SAFETY: zero-initialised `sched_param` is valid; 0 = this thread.
        unsafe {
            let mut param: libc::sched_param = std::mem::zeroed();
            param.sched_priority = 70;
            let _ = libc::sched_setscheduler(0, libc::SCHED_FIFO, &param);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    struct Squares {
        input: Vec<u32>,
        output: Vec<AtomicU32>,
    }

    unsafe fn square(ctx: *const (), index: usize) {
        let job = unsafe { &*(ctx as *const Squares) };
        let x = job.input[index];
        job.output[index].store(x * x, Ordering::Relaxed);
    }

    #[test]
    fn a_batch_runs_every_item_exactly_once() {
        configure_multicore(true, 4);
        for round in 0..200u32 {
            let count = (round as usize % 37) + 2;
            let job = Squares {
                input: (0..count as u32).map(|i| i + round).collect(),
                output: (0..count).map(|_| AtomicU32::new(u32::MAX)).collect(),
            };
            // SAFETY: each index writes only its own output slot.
            let ran = unsafe { dispatch(count, &job as *const Squares as *const (), square) };
            if !ran {
                // Another test holds the pool; nothing was run.
                assert!(
                    job.output
                        .iter()
                        .all(|o| o.load(Ordering::Relaxed) == u32::MAX)
                );
                continue;
            }
            for (i, out) in job.output.iter().enumerate() {
                let x = i as u32 + round;
                assert_eq!(
                    out.load(Ordering::Relaxed),
                    x * x,
                    "item {i} of round {round}"
                );
            }
        }
    }

    #[test]
    fn a_single_item_is_left_to_the_caller() {
        configure_multicore(true, 2);
        let job = Squares {
            input: vec![3],
            output: vec![AtomicU32::new(0)],
        };
        let ran = unsafe { dispatch(1, &job as *const Squares as *const (), square) };
        assert!(!ran);
    }

    #[test]
    fn auto_leaves_a_core_free_on_machines_that_have_one_to_spare() {
        let threads = auto_processing_threads();
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2);
        assert!(threads >= 1 && threads <= AUTO_THREAD_CAP);
        if cores > 2 {
            assert!(threads < cores);
        }
    }
}
