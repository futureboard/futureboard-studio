//! AAudio stream states and the teardown wait built on them.
//!
//! Kept apart from the `dlopen` bindings in `aaudio` so the logic that decides
//! when a callback context may be freed builds and is unit-tested on the host,
//! not only on a device.

/// The idle `aaudio_stream_state_t` values from `AAudio.h`. The running ones
/// (starting 3, started 4, pausing 5, flushing 7, stopping 9) are everything
/// [`callbacks_settled`] does not list.
pub const STATE_OPEN: i32 = 2;
pub const STATE_PAUSED: i32 = 6;
pub const STATE_FLUSHED: i32 = 8;
pub const STATE_STOPPED: i32 = 10;
pub const STATE_CLOSED: i32 = 12;
pub const STATE_DISCONNECTED: i32 = 13;

/// Whether a stream in `state` has stopped running its data callback.
///
/// Anything not known to be idle counts as possibly running, so an unexpected
/// state errs towards keeping the callback context alive.
pub fn callbacks_settled(state: i32) -> bool {
    matches!(
        state,
        STATE_OPEN
            | STATE_PAUSED
            | STATE_FLUSHED
            | STATE_STOPPED
            | STATE_CLOSED
            | STATE_DISCONNECTED
    )
}

/// Follows a stream from `state` until its callback has settled, asking
/// `wait_for_change` (one bounded `AAudioStream_waitForStateChange`) for the
/// next state at most `max_waits` times.
///
/// Returns whether it settled. On `false` the caller must not free anything a
/// callback could still be reading.
pub fn wait_until_settled(
    mut state: i32,
    max_waits: u32,
    mut wait_for_change: impl FnMut(i32) -> i32,
) -> bool {
    for _ in 0..max_waits {
        if callbacks_settled(state) {
            return true;
        }
        state = wait_for_change(state);
    }
    callbacks_settled(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATE_STARTING: i32 = 3;
    const STATE_STARTED: i32 = 4;
    const STATE_PAUSING: i32 = 5;
    const STATE_FLUSHING: i32 = 7;
    const STATE_STOPPING: i32 = 9;

    #[test]
    fn idle_states_are_settled_and_running_ones_are_not() {
        for state in [
            STATE_OPEN,
            STATE_PAUSED,
            STATE_FLUSHED,
            STATE_STOPPED,
            STATE_CLOSED,
            STATE_DISCONNECTED,
        ] {
            assert!(callbacks_settled(state), "state {state}");
        }
        for state in [
            STATE_STARTING,
            STATE_STARTED,
            STATE_PAUSING,
            STATE_FLUSHING,
            STATE_STOPPING,
            // Uninitialized, unknown, closing and anything newer: assume it
            // may run.
            0,
            1,
            11,
            99,
        ] {
            assert!(!callbacks_settled(state), "state {state}");
        }
    }

    #[test]
    fn a_stopping_stream_is_followed_until_it_has_stopped() {
        let mut states = [STATE_STOPPING, STATE_STOPPED].into_iter();
        let mut waits = 0;
        let settled = wait_until_settled(STATE_STARTED, 10, |_| {
            waits += 1;
            states.next().unwrap()
        });
        assert!(settled);
        assert_eq!(waits, 2);
    }

    #[test]
    fn an_already_stopped_stream_does_not_wait() {
        let settled = wait_until_settled(STATE_STOPPED, 10, |_| panic!("no wait expected"));
        assert!(settled);
    }

    #[test]
    fn a_stream_that_never_stops_is_reported_unsettled() {
        // The wait keeps timing out with the callback still running: the
        // caller must keep the context alive instead of freeing it.
        let mut waits = 0;
        let settled = wait_until_settled(STATE_STARTED, 5, |state| {
            waits += 1;
            state
        });
        assert!(!settled);
        assert_eq!(waits, 5);
    }

    #[test]
    fn the_last_wait_is_still_checked() {
        let settled = wait_until_settled(STATE_STOPPING, 1, |_| STATE_STOPPED);
        assert!(settled);
    }
}
