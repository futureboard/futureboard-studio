//! When to tell the studio that a hosted plug-in's own state may have changed
//! ([`crate::ipc::HostEvent::PluginStateTouched`]).
//!
//! The bridges raise a per-instance flag from the plug-in's callbacks (a VST3
//! `performEdit`, a VST2 `audioMasterAutomate`, a CLAP `mark_dirty`, ...). The
//! host's UI thread takes those flags and feeds them here; this decides which
//! of them become a report, and when:
//!
//! * Only while the instance's editor is open. The user edits a plug-in through
//!   its editor; outside it the same callbacks mostly come from the plug-in
//!   settling after a state restore or echoing host automation, and counting
//!   those would leave a freshly opened project "unsaved".
//! * Not in the first [`EDITOR_SETTLE`] after the editor opened, which is when
//!   some editors push their current values back through the host on attach.
//! * At most once per [`REPORT_INTERVAL`] per instance, leading edge first and
//!   a trailing report for anything that arrived in between, so a knob drag is
//!   a handful of frames rather than one per callback, and the last edit of a
//!   drag is never swallowed.
//! * Once more when the editor closes if a touch is still waiting, or when the
//!   format cannot report its edits at all (Audio Units), whose state may have
//!   changed with no way to tell.
//!
//! Plain data and `Instant`s, driven by the host's UI loop: no threads, no I/O.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Shortest spacing between two reports for one instance.
pub const REPORT_INTERVAL: Duration = Duration::from_millis(250);

/// Touches this soon after an editor opened are the editor settling, not the
/// user: they are dropped.
pub const EDITOR_SETTLE: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy)]
struct EditorSession {
    opened_at: Instant,
    /// Whether this instance's format reports its own edits. When it does not,
    /// closing the editor is reported unconditionally.
    reports_edits: bool,
    last_report: Option<Instant>,
    /// A touch arrived since the last report.
    pending: bool,
}

/// Per-instance editor sessions and their unreported touches. See the module
/// docs for the rules.
#[derive(Debug, Default)]
pub struct StateTouchTracker {
    sessions: HashMap<String, EditorSession>,
}

impl StateTouchTracker {
    /// Follow the set of open editors: `open` lists every instance whose editor
    /// is open now, and `reports_edits` says whether an instance's format
    /// reports its own edits (asked once, when its editor session starts).
    ///
    /// Returns the instances whose editor closed since the last call and that
    /// need one last report: a touch was still waiting, or the format cannot
    /// report edits. The caller drops the ones no longer loaded — an unload is
    /// the insert going away, not an edit.
    pub fn sync_open_editors<'a>(
        &mut self,
        open: impl Iterator<Item = &'a str> + Clone,
        now: Instant,
        reports_edits: impl Fn(&str) -> bool,
    ) -> Vec<String> {
        let mut closed = Vec::new();
        self.sessions.retain(|id, session| {
            if open.clone().any(|open_id| open_id == id) {
                return true;
            }
            if session.pending || !session.reports_edits {
                closed.push(id.clone());
            }
            false
        });
        for id in open {
            if !self.sessions.contains_key(id) {
                self.sessions.insert(
                    id.to_string(),
                    EditorSession {
                        opened_at: now,
                        reports_edits: reports_edits(id),
                        last_report: None,
                        pending: false,
                    },
                );
            }
        }
        closed
    }

    /// The plug-in raised its touched flag. Ignored when its editor is not
    /// open, and while the editor is still settling.
    pub fn record(&mut self, instance_id: &str, now: Instant) {
        let Some(session) = self.sessions.get_mut(instance_id) else {
            return;
        };
        if now.saturating_duration_since(session.opened_at) < EDITOR_SETTLE {
            return;
        }
        session.pending = true;
    }

    /// The instances to report now, each marked reported.
    pub fn take_due(&mut self, now: Instant) -> Vec<String> {
        let mut due = Vec::new();
        for (id, session) in self.sessions.iter_mut() {
            if !session.pending {
                continue;
            }
            let spaced = session
                .last_report
                .is_none_or(|last| now.saturating_duration_since(last) >= REPORT_INTERVAL);
            if spaced {
                session.pending = false;
                session.last_report = Some(now);
                due.push(id.clone());
            }
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn open(tracker: &mut StateTouchTracker, ids: &[&str], now: Instant) -> Vec<String> {
        tracker.sync_open_editors(ids.iter().copied(), now, |id| !id.starts_with("au"))
    }

    #[test]
    fn touches_count_only_while_the_editor_is_open_and_settled() {
        let t0 = Instant::now();
        let mut tracker = StateTouchTracker::default();
        // No editor: a restore or automation echo is not an edit.
        tracker.record("fx1", t0);
        assert!(tracker.take_due(t0).is_empty());

        open(&mut tracker, &["fx1"], t0);
        // The editor pushing its values back on attach.
        tracker.record("fx1", t0 + ms(100));
        assert!(tracker.take_due(t0 + ms(100)).is_empty());

        tracker.record("fx1", t0 + EDITOR_SETTLE);
        assert_eq!(
            tracker.take_due(t0 + EDITOR_SETTLE),
            vec!["fx1".to_string()]
        );
    }

    #[test]
    fn a_drag_is_throttled_and_its_last_edit_still_reported() {
        let t0 = Instant::now();
        let mut tracker = StateTouchTracker::default();
        open(&mut tracker, &["fx1"], t0);
        let start = t0 + EDITOR_SETTLE;

        tracker.record("fx1", start);
        assert_eq!(tracker.take_due(start).len(), 1, "leading edge");
        tracker.record("fx1", start + ms(50));
        tracker.record("fx1", start + ms(120));
        assert!(
            tracker.take_due(start + ms(120)).is_empty(),
            "inside the interval"
        );
        assert_eq!(
            tracker.take_due(start + REPORT_INTERVAL),
            vec!["fx1".to_string()],
            "trailing report for the edits in between"
        );
        assert!(
            tracker.take_due(start + REPORT_INTERVAL * 3).is_empty(),
            "nothing new"
        );
    }

    #[test]
    fn instances_are_reported_by_their_own_id_only() {
        let t0 = Instant::now();
        let mut tracker = StateTouchTracker::default();
        open(&mut tracker, &["fx1", "fx2"], t0);
        tracker.record("fx2", t0 + EDITOR_SETTLE);
        tracker.record("unknown", t0 + EDITOR_SETTLE);
        assert_eq!(
            tracker.take_due(t0 + EDITOR_SETTLE),
            vec!["fx2".to_string()]
        );
    }

    #[test]
    fn closing_reports_a_waiting_touch_and_every_audio_unit() {
        let t0 = Instant::now();
        let mut tracker = StateTouchTracker::default();
        open(
            &mut tracker,
            &["fx_quiet", "fx_waiting", "fx_reported", "au1"],
            t0,
        );
        let t = t0 + EDITOR_SETTLE;
        tracker.record("fx_reported", t);
        assert_eq!(tracker.take_due(t), vec!["fx_reported".to_string()]);
        tracker.record("fx_waiting", t);
        tracker.record("fx_waiting", t + ms(10));
        tracker.take_due(t);
        tracker.record("fx_waiting", t + ms(20));

        let mut closed = open(&mut tracker, &[], t + ms(30));
        closed.sort();
        assert_eq!(closed, vec!["au1".to_string(), "fx_waiting".to_string()]);
        // Closed sessions are gone: nothing further is reported for them.
        tracker.record("fx_waiting", t + ms(400));
        assert!(tracker.take_due(t + ms(400)).is_empty());
    }

    #[test]
    fn reopening_starts_a_fresh_settle_window() {
        let t0 = Instant::now();
        let mut tracker = StateTouchTracker::default();
        open(&mut tracker, &["fx1"], t0);
        open(&mut tracker, &[], t0 + ms(600));
        let reopened = t0 + ms(700);
        open(&mut tracker, &["fx1"], reopened);
        tracker.record("fx1", reopened + ms(100));
        assert!(tracker.take_due(reopened + ms(100)).is_empty());
        // An editor that stays open keeps its session (and its settle time).
        assert!(open(&mut tracker, &["fx1"], reopened + ms(200)).is_empty());
        tracker.record("fx1", reopened + EDITOR_SETTLE);
        assert_eq!(tracker.take_due(reopened + EDITOR_SETTLE).len(), 1);
    }
}
