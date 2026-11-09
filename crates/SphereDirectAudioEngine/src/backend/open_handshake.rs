//! The handshake between a backend's `open()` and the device thread it spawns
//! (WASAPI Exclusive, WDM-KS).
//!
//! `open()` waits a bounded time for the thread's init message, but a driver
//! can take longer than that and still finish. Before this handshake such a
//! late thread went on to run its render loop -- with a runtime clone sharing
//! the live plug-in processors, next to whichever stream the caller fell back
//! to -- and kept the exclusive device (#381). Exactly one side now decides:
//!
//! - the thread [`claim`](OpenHandshake::claim)s once its device runs, before
//!   it touches the runtime or shared state, and sends its init message
//!   straight after;
//! - `open()`, when the wait times out, abandons the open. The thread's claim
//!   then fails, and it releases the device and exits without rendering.
//!
//! When the claim wins that race the message is already on its way, and
//! `open()` takes it instead of failing.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::Duration;

const PENDING: u8 = 0;
const CLAIMED: u8 = 1;
const ABANDONED: u8 = 2;

/// Shared by one `open()` call and the device thread it spawned.
#[derive(Debug, Default)]
pub(crate) struct OpenHandshake {
    state: AtomicU8,
}

impl OpenHandshake {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Device thread, once the device is running and before it renders or
    /// publishes anything: `true` commits the thread to the stream (send the
    /// init message next); `false` means `open()` has given up, so release the
    /// device and exit without touching the runtime.
    pub(crate) fn claim(&self) -> bool {
        self.state
            .compare_exchange(PENDING, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// `open()` gave up on this thread. A thread that sees this need not
    /// finish initialising, and must leave the shared state to the caller.
    pub(crate) fn is_abandoned(&self) -> bool {
        self.state.load(Ordering::Acquire) == ABANDONED
    }

    fn abandon(&self) -> bool {
        self.state
            .compare_exchange(PENDING, ABANDONED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

/// `open()`'s side: wait up to `timeout` for the device thread's init
/// message, abandoning the open if none came. Consumes the receiver, so a
/// message sent after this returns has nowhere to go.
///
/// On `Err` the open has failed and the caller must stop the thread (stop
/// flag and stop event) and hand it to [`reap_device_thread`]. Never blocks
/// past `timeout` on a driver: after a lost race it waits only for the send
/// that directly follows the thread's claim.
pub(crate) fn wait_for_device_thread<T>(
    info_rx: Receiver<Result<T, String>>,
    handshake: &OpenHandshake,
    timeout: Duration,
    backend: &str,
) -> Result<T, String> {
    let exited = || format!("{backend} thread exited before reporting its init");
    match info_rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(RecvTimeoutError::Disconnected) => Err(exited()),
        Err(RecvTimeoutError::Timeout) if handshake.abandon() => Err(format!(
            "{backend} device did not finish initialising within {} s; open abandoned",
            timeout.as_secs()
        )),
        // The thread claimed the stream between the timeout and `abandon`.
        Err(RecvTimeoutError::Timeout) => info_rx.recv().unwrap_or_else(|_| Err(exited())),
    }
}

/// Join a device thread whose open failed or was abandoned, off the
/// caller's thread: a driver call that never returns keeps only the reaper
/// waiting. `then` runs after the join -- close the handles the thread waits
/// on there, never before.
pub(crate) fn reap_device_thread(
    name: &str,
    thread: JoinHandle<()>,
    then: impl FnOnce() + Send + 'static,
) {
    let spawned = std::thread::Builder::new()
        .name(format!("{name}-reap"))
        .spawn(move || {
            let _ = thread.join();
            then();
        });
    if let Err(error) = spawned {
        // The thread is detached and `then` never runs: leaking its handles
        // beats closing one it may still wait on.
        eprintln!("[DAUx] {name}: reaper not started ({error}); thread left detached");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use std::sync::Arc;

    const SHORT: Duration = Duration::from_millis(20);

    #[test]
    fn a_message_in_time_is_returned() {
        let handshake = OpenHandshake::new();
        let (tx, rx) = channel::<Result<u32, String>>();
        assert!(handshake.claim());
        tx.send(Ok(48_000)).unwrap();
        assert_eq!(
            wait_for_device_thread(rx, &handshake, SHORT, "test"),
            Ok(48_000)
        );
    }

    #[test]
    fn a_timeout_abandons_and_the_late_thread_cannot_claim() {
        let handshake = OpenHandshake::new();
        let (tx, rx) = channel::<Result<u32, String>>();
        let result = wait_for_device_thread(rx, &handshake, SHORT, "test");
        assert!(result.unwrap_err().contains("abandoned"));
        assert!(handshake.is_abandoned());
        // The driver finishes after all: no claim, and nobody to tell.
        assert!(!handshake.claim());
        assert!(tx.send(Ok(48_000)).is_err());
    }

    #[test]
    fn a_claim_that_beats_the_abandon_is_waited_for() {
        let handshake = OpenHandshake::new();
        let (tx, rx) = channel::<Result<u32, String>>();
        // Claimed before the wait ends, message not sent yet.
        assert!(handshake.claim());
        let sender = std::thread::spawn(move || {
            std::thread::sleep(SHORT * 3);
            tx.send(Ok(44_100)).unwrap();
        });
        assert_eq!(
            wait_for_device_thread(rx, &handshake, SHORT, "test"),
            Ok(44_100)
        );
        assert!(!handshake.is_abandoned());
        sender.join().unwrap();
    }

    #[test]
    fn a_thread_that_exits_without_reporting_is_an_error() {
        let handshake = OpenHandshake::new();
        let (tx, rx) = channel::<Result<u32, String>>();
        drop(tx);
        let result = wait_for_device_thread(rx, &handshake, SHORT, "test");
        assert!(result.unwrap_err().contains("exited"));
    }

    #[test]
    fn the_reaper_joins_before_running_then() {
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread_finished = Arc::clone(&finished);
        let thread = std::thread::spawn(move || {
            std::thread::sleep(SHORT);
            thread_finished.store(true, Ordering::SeqCst);
        });
        let (done_tx, done_rx) = channel();
        let check = Arc::clone(&finished);
        reap_device_thread("test", thread, move || {
            let _ = done_tx.send(check.load(Ordering::SeqCst));
        });
        assert_eq!(done_rx.recv_timeout(Duration::from_secs(5)), Ok(true));
    }
}
