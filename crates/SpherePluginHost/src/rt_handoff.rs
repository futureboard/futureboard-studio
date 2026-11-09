//! Hand a realtime thread an immutable "plan" built elsewhere, and take every
//! plan it lets go of back to the thread that built it.
//!
//! The plug-in host's audio producer serves every insert from one plan: the
//! mapped regions and the runtime behind each (built-in DSP, Audio Unit, VST3
//! voice), resolved by the host's main thread whenever an insert is loaded,
//! attached or removed. The producer must never be the thread that frees what
//! a plan holds: dropping the last handle on a built-in's DSP frees its
//! buffers and loaded models, on an Audio Unit disposes it, on a VST3 voice
//! terminates the plug-in, and on a region unmaps it. So the producer never
//! drops a plan: it swaps the newest one in and sends the one it replaced back
//! over a bounded queue, and the main thread drops it there.
//!
//! Both queues are bounded `crossbeam` array channels, created up front: the
//! producer's side is a `try_recv` and a `try_send`, wait-free and
//! allocation-free. When the return queue is full the producer simply keeps
//! its current plan for another block instead of dropping anything.

use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};

/// Plans that may be waiting for the producer at once. The main thread keeps
/// at most one more of its own (the newest) until there is room.
const PUBLISH_DEPTH: usize = 4;
/// Plans the producer may have handed back before the main thread collects
/// them. While it is full the producer keeps the plan it has.
const RETIRE_DEPTH: usize = 16;

/// Builds the two ends. The producer starts on `T::default()` (an empty plan:
/// nothing to free) until the first publish.
pub fn plan_handoff<T: Send + Default>() -> (PlanPublisher<T>, PlanConsumer<T>) {
    let (publish_tx, publish_rx) = bounded(PUBLISH_DEPTH);
    let (retire_tx, retire_rx) = bounded(RETIRE_DEPTH);
    (
        PlanPublisher {
            publish: publish_tx,
            retired: retire_rx,
            unsent: None,
        },
        PlanConsumer {
            current: T::default(),
            publish: publish_rx,
            retire: retire_tx,
        },
    )
}

/// The non-realtime end: publishes plans and frees the ones the producer
/// retired.
pub struct PlanPublisher<T> {
    publish: Sender<T>,
    retired: Receiver<T>,
    /// The newest plan when the publish queue was full; sent on the next call.
    unsent: Option<T>,
}

impl<T> PlanPublisher<T> {
    /// Queue `plan` for the producer. Plans are whole snapshots, so a plan
    /// still waiting here when a newer one arrives is dropped, on this thread.
    pub fn publish(&mut self, plan: T) {
        self.unsent = Some(plan);
        self.flush();
    }

    /// Free every plan the producer has handed back (on this thread), and
    /// send a plan that found the publish queue full. Returns how many plans
    /// were freed.
    pub fn collect_retired(&mut self) -> usize {
        let mut freed = 0;
        while let Ok(plan) = self.retired.try_recv() {
            drop(plan);
            freed += 1;
        }
        self.flush();
        freed
    }

    /// Whether a published plan has not reached the producer's queue yet.
    pub fn has_unsent(&self) -> bool {
        self.unsent.is_some()
    }

    /// Whether the producer has taken every plan published so far.
    pub fn is_adopted(&self) -> bool {
        self.unsent.is_none() && self.publish.is_empty()
    }

    fn flush(&mut self) {
        let Some(plan) = self.unsent.take() else {
            return;
        };
        match self.publish.try_send(plan) {
            Ok(()) => {}
            // The producer has not caught up: keep the newest for later.
            Err(TrySendError::Full(plan)) => self.unsent = Some(plan),
            // The producer is gone (shutdown); free it here.
            Err(TrySendError::Disconnected(plan)) => drop(plan),
        }
    }
}

/// The realtime end, owned by the producer thread.
pub struct PlanConsumer<T: Default> {
    current: T,
    publish: Receiver<T>,
    retire: Sender<T>,
}

impl<T: Default> PlanConsumer<T> {
    /// Adopt the newest published plan and return it. Wait-free and
    /// allocation-free, and never drops a plan: each one it replaces goes back
    /// to the publisher. With the return queue full it keeps the plan it has.
    pub fn refresh(&mut self) -> &T {
        while !self.retire.is_full() {
            let Ok(next) = self.publish.try_recv() else {
                break;
            };
            let previous = std::mem::replace(&mut self.current, next);
            // This thread is the only sender and the queue was not full, so
            // this cannot fail while the publisher lives. If it is gone the
            // process is shutting down: leak the plan rather than free it on
            // the realtime thread.
            if let Err(error) = self.retire.try_send(previous) {
                std::mem::forget(error.into_inner());
            }
        }
        &self.current
    }

    /// The plan in use, without looking for a newer one.
    pub fn current(&self) -> &T {
        &self.current
    }
}

impl<T: Default> Drop for PlanConsumer<T> {
    /// The producer is exiting: hand its plan back rather than free it here,
    /// and every published plan it never picked up too — dropping the last
    /// receiver would otherwise make the channel discard (free) them on this
    /// thread. A plan that finds the return queue full is leaked instead.
    fn drop(&mut self) {
        let last = std::mem::take(&mut self.current);
        if let Err(error) = self.retire.try_send(last) {
            std::mem::forget(error.into_inner());
        }
        while let Ok(waiting) = self.publish.try_recv() {
            if let Err(error) = self.retire.try_send(waiting) {
                std::mem::forget(error.into_inner());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_alloc::count_heap_events;
    use std::sync::{Arc, Mutex};
    use std::thread::{self, ThreadId};

    /// Records the thread that drops it.
    #[derive(Default)]
    struct Tracked {
        id: u32,
        dropped_on: Option<Arc<Mutex<Vec<(u32, ThreadId)>>>>,
    }

    impl Drop for Tracked {
        fn drop(&mut self) {
            if let Some(log) = &self.dropped_on {
                log.lock().unwrap().push((self.id, thread::current().id()));
            }
        }
    }

    fn tracked(id: u32, log: &Arc<Mutex<Vec<(u32, ThreadId)>>>) -> Tracked {
        Tracked {
            id,
            dropped_on: Some(log.clone()),
        }
    }

    #[test]
    fn the_producer_adopts_the_newest_plan() {
        let (mut publisher, mut consumer) = plan_handoff::<Vec<u32>>();
        assert!(consumer.refresh().is_empty());
        publisher.publish(vec![1]);
        publisher.publish(vec![1, 2]);
        assert_eq!(consumer.refresh(), &vec![1, 2]);
        assert_eq!(consumer.current(), &vec![1, 2]);
        // The empty initial plan and `[1]` came back to the publisher.
        assert_eq!(publisher.collect_retired(), 2);
    }

    /// #295/#296: a removed insert's last handle is released by the thread
    /// that collects retired plans, never by the producer that used it.
    #[test]
    fn a_replaced_plan_is_dropped_on_the_publishing_thread() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (mut publisher, consumer) = plan_handoff::<Tracked>();
        let main_thread = thread::current().id();
        publisher.publish(tracked(1, &log));

        let producer_log = log.clone();
        let producer = thread::spawn(move || {
            let mut consumer = consumer;
            // Plan 1 (if it is seen at all) is replaced by plan 2 below.
            let started = std::time::Instant::now();
            while consumer.refresh().id != 2 {
                assert!(started.elapsed().as_secs() < 10, "plan 2 never arrived");
                thread::yield_now();
            }
            let dropped_here = producer_log.lock().unwrap().len();
            (consumer, dropped_here, thread::current().id())
        });
        publisher.publish(tracked(2, &log));
        let (consumer, dropped_on_producer, producer_thread) = producer.join().unwrap();
        assert_eq!(dropped_on_producer, 0, "the producer dropped a plan");

        // Plan 1 (and the empty initial plan) wait for the publisher.
        assert_eq!(publisher.collect_retired(), 2);
        assert_eq!(*log.lock().unwrap(), vec![(1, main_thread)]);

        // An exiting producer hands its last plan back too.
        drop(consumer);
        assert_eq!(publisher.collect_retired(), 1);
        let log = log.lock().unwrap();
        assert_eq!(log[1], (2, main_thread));
        assert!(log.iter().all(|(_, thread)| *thread != producer_thread));
    }

    /// An exiting producer must not free plans it was sent but never took:
    /// they go back to the publisher with its current plan.
    #[test]
    fn an_exiting_producer_hands_back_plans_it_never_picked_up() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (mut publisher, mut consumer) = plan_handoff::<Tracked>();
        let main_thread = thread::current().id();
        publisher.publish(tracked(1, &log));
        consumer.refresh();
        publisher.publish(tracked(2, &log));
        publisher.publish(tracked(3, &log));
        let producer_thread = thread::spawn(move || {
            drop(consumer);
            thread::current().id()
        })
        .join()
        .unwrap();
        assert!(log.lock().unwrap().is_empty(), "freed on the producer");
        // The empty initial plan, plan 1 (current), plans 2 and 3 (waiting).
        assert_eq!(publisher.collect_retired(), 4);
        let mut log = log.lock().unwrap().clone();
        log.sort_by_key(|(id, _)| *id);
        assert_eq!(
            log,
            vec![(1, main_thread), (2, main_thread), (3, main_thread)]
        );
        assert!(log.iter().all(|(_, thread)| *thread != producer_thread));
    }

    #[test]
    fn a_full_return_queue_keeps_the_current_plan_instead_of_dropping_it() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (mut publisher, mut consumer) = plan_handoff::<Tracked>();
        // Fill the return queue without the publisher collecting.
        for id in 0..RETIRE_DEPTH as u32 {
            publisher.publish(tracked(id, &log));
            consumer.refresh();
        }
        assert!(consumer.retire.is_full());
        publisher.publish(tracked(100, &log));
        assert_ne!(consumer.refresh().id, 100, "swapped with nowhere to retire");
        assert!(log.lock().unwrap().is_empty(), "nothing was freed");

        publisher.collect_retired();
        assert_eq!(consumer.refresh().id, 100);
    }

    #[test]
    fn a_plan_published_onto_a_full_queue_is_sent_later() {
        let (mut publisher, mut consumer) = plan_handoff::<Vec<u32>>();
        for id in 0..(PUBLISH_DEPTH as u32 + 2) {
            publisher.publish(vec![id]);
        }
        assert!(publisher.has_unsent());
        let newest = PUBLISH_DEPTH as u32 + 1;
        // Drain what is queued, then let the publisher send the newest.
        while consumer.refresh() != &vec![newest] {
            publisher.collect_retired();
        }
        assert!(!publisher.has_unsent());
    }

    /// #318: the per-block plan refresh neither allocates nor frees, whether
    /// or not a new plan is waiting.
    #[test]
    fn refreshing_the_plan_does_not_touch_the_heap() {
        let (mut publisher, mut consumer) = plan_handoff::<Vec<u32>>();
        publisher.publish(vec![1, 2, 3]);
        let (_, events) = count_heap_events(|| {
            let _ = consumer.refresh();
        });
        assert_eq!(events, 0, "swap allocated or freed");
        let (_, events) = count_heap_events(|| {
            for _ in 0..1000 {
                let _ = consumer.refresh();
            }
        });
        assert_eq!(events, 0, "idle refresh allocated or freed");
        assert_eq!(consumer.current(), &vec![1, 2, 3]);
        assert_eq!(publisher.collect_retired(), 1);
    }
}
