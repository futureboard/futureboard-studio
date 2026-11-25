//! A bounded single-producer / single-consumer queue of control messages,
//! sized once. The control thread pushes (under the engine's control lock, so
//! there is one producer); the audio thread peeks and pops at the start of
//! each block. Neither side allocates, locks or waits.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

/// What the control thread asks the audio thread to do, in order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Message {
    /// Set wire parameter `index` to `value`.
    Param(u32, f32),
    /// Adopt the whole state waiting in the engine's state slot.
    State,
    /// Clear every tail and meter.
    Reset,
    /// Clear the sticky clip indicators.
    ClearClip,
}

pub(crate) struct Queue {
    slots: Box<[UnsafeCell<Message>]>,
    /// Next slot the consumer reads; only the consumer stores it.
    head: AtomicUsize,
    /// Next slot the producer writes; only the producer stores it.
    tail: AtomicUsize,
}

// SAFETY: a slot is written only by the producer while it is outside
// `head..tail`, and read only by the consumer while it is inside; the
// release/acquire pairs on `head` and `tail` order those accesses.
unsafe impl Sync for Queue {}
unsafe impl Send for Queue {}

impl Queue {
    pub(crate) fn new(capacity: usize) -> Self {
        let capacity = capacity.max(2);
        Self {
            slots: (0..capacity)
                .map(|_| UnsafeCell::new(Message::Reset))
                .collect(),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    /// Producer: queue `message`; `false` when full.
    pub(crate) fn push(&self, message: Message) -> bool {
        let tail = self.tail.load(Ordering::Relaxed);
        let next = (tail + 1) % self.slots.len();
        if next == self.head.load(Ordering::Acquire) {
            return false;
        }
        // SAFETY: `tail` is outside the consumer's range until published.
        unsafe { *self.slots[tail].get() = message };
        self.tail.store(next, Ordering::Release);
        true
    }

    /// Consumer: the oldest message, left in the queue.
    pub(crate) fn peek(&self) -> Option<Message> {
        let head = self.head.load(Ordering::Relaxed);
        if head == self.tail.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: `head` is inside the published range.
        Some(unsafe { *self.slots[head].get() })
    }

    /// Consumer: drop the message [`Self::peek`] returned.
    pub(crate) fn pop(&self) {
        let head = self.head.load(Ordering::Relaxed);
        if head != self.tail.load(Ordering::Acquire) {
            self.head
                .store((head + 1) % self.slots.len(), Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_come_out_in_order_until_full() {
        let queue = Queue::new(4);
        assert!(queue.push(Message::Param(1, 0.5)));
        assert!(queue.push(Message::Reset));
        assert!(queue.push(Message::State));
        assert!(!queue.push(Message::ClearClip), "one slot stays free");
        assert_eq!(queue.peek(), Some(Message::Param(1, 0.5)));
        assert_eq!(queue.peek(), Some(Message::Param(1, 0.5)), "peek leaves it");
        queue.pop();
        assert_eq!(queue.peek(), Some(Message::Reset));
        queue.pop();
        assert!(queue.push(Message::ClearClip));
        queue.pop();
        assert_eq!(queue.peek(), Some(Message::ClearClip));
        queue.pop();
        assert_eq!(queue.peek(), None);
        queue.pop();
    }

    #[test]
    fn a_producer_and_a_consumer_on_two_threads_lose_nothing() {
        let queue = std::sync::Arc::new(Queue::new(64));
        let producer = {
            let queue = queue.clone();
            std::thread::spawn(move || {
                for i in 0..20_000u32 {
                    while !queue.push(Message::Param(i, i as f32)) {
                        std::thread::yield_now();
                    }
                }
            })
        };
        let mut expected = 0u32;
        while expected < 20_000 {
            if let Some(message) = queue.peek() {
                assert_eq!(message, Message::Param(expected, expected as f32));
                queue.pop();
                expected += 1;
            } else {
                std::thread::yield_now();
            }
        }
        producer.join().unwrap();
    }
}
