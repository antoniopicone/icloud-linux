//! Small concurrency helpers the engine is built from.

use std::{
    collections::{BinaryHeap, HashSet},
    hash::Hash,
    sync::{Condvar, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Nothing guarded here can be left half-updated by a panic.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A flag that threads can wait on with a timeout. Used both to stop the
/// engine and to nudge the refresh loop.
#[derive(Debug, Default)]
pub(super) struct Signal {
    raised: Mutex<bool>,
    changed: Condvar,
}

impl Signal {
    pub(super) fn raise(&self) {
        *lock(&self.raised) = true;
        self.changed.notify_all();
    }

    pub(super) fn is_raised(&self) -> bool {
        *lock(&self.raised)
    }

    pub(super) fn clear(&self) {
        *lock(&self.raised) = false;
    }

    /// Wait up to `timeout` for the signal. Returns whether it was raised.
    pub(super) fn wait(&self, timeout: Duration) -> bool {
        let guard = lock(&self.raised);
        let (guard, _) =
            self.changed.wait_timeout_while(guard, timeout, |raised| !*raised).unwrap_or_else(PoisonError::into_inner);
        *guard
    }
}

/// Mutual exclusion per key, without a table that grows forever: a key is
/// present only while somebody holds it.
#[derive(Debug)]
pub(super) struct KeyedLocks<K> {
    held: Mutex<HashSet<K>>,
    released: Condvar,
}

impl<K> Default for KeyedLocks<K> {
    fn default() -> Self {
        Self { held: Mutex::new(HashSet::new()), released: Condvar::new() }
    }
}

pub(super) struct KeyGuard<'a, K: Hash + Eq + Clone> {
    locks: &'a KeyedLocks<K>,
    key: K,
}

impl<K: Hash + Eq + Clone> KeyedLocks<K> {
    pub(super) fn acquire(&self, key: &K) -> KeyGuard<'_, K> {
        let mut held = lock(&self.held);
        while held.contains(key) {
            held = self.released.wait(held).unwrap_or_else(PoisonError::into_inner);
        }
        held.insert(key.clone());
        KeyGuard { locks: self, key: key.clone() }
    }
}

impl<K: Hash + Eq + Clone> Drop for KeyGuard<'_, K> {
    fn drop(&mut self) {
        lock(&self.locks.held).remove(&self.key);
        self.locks.released.notify_all();
    }
}

/// Items that become available at a given time; consumers block until the
/// earliest is due or the queue is closed.
#[derive(Debug)]
pub(super) struct DelayQueue<T: Ord> {
    inner: Mutex<QueueInner<T>>,
    changed: Condvar,
}

#[derive(Debug)]
struct QueueInner<T: Ord> {
    heap: BinaryHeap<std::cmp::Reverse<(Instant, u64, T)>>,
    closed: bool,
    seq: u64,
}

impl<T: Ord> Default for DelayQueue<T> {
    fn default() -> Self {
        Self {
            inner: Mutex::new(QueueInner { heap: BinaryHeap::new(), closed: false, seq: 0 }),
            changed: Condvar::new(),
        }
    }
}

impl<T: Ord> DelayQueue<T> {
    pub(super) fn push(&self, item: T, delay: Duration) {
        let mut inner = lock(&self.inner);
        inner.seq += 1;
        let seq = inner.seq;
        inner.heap.push(std::cmp::Reverse((Instant::now() + delay, seq, item)));
        self.changed.notify_one();
    }

    pub(super) fn close(&self) {
        lock(&self.inner).closed = true;
        self.changed.notify_all();
    }

    pub(super) fn len(&self) -> usize {
        lock(&self.inner).heap.len()
    }

    /// Block until an item is due. `None` once the queue is closed.
    pub(super) fn pop(&self) -> Option<T> {
        let mut inner = lock(&self.inner);
        loop {
            if inner.closed {
                return None;
            }
            let now = Instant::now();
            match inner.heap.peek() {
                None => {
                    inner = self.changed.wait(inner).unwrap_or_else(PoisonError::into_inner);
                }
                Some(std::cmp::Reverse((due, _, _))) if *due <= now => {
                    return inner.heap.pop().map(|std::cmp::Reverse((_, _, item))| item);
                }
                Some(std::cmp::Reverse((due, _, _))) => {
                    let wait = *due - now;
                    inner = self.changed.wait_timeout(inner, wait).unwrap_or_else(PoisonError::into_inner).0;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
    };

    use super::*;

    #[test]
    fn a_signal_wakes_waiters_and_can_be_cleared() {
        let signal = Arc::new(Signal::default());
        assert!(!signal.wait(Duration::from_millis(5)));
        let raiser = Arc::clone(&signal);
        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            raiser.raise();
        });
        assert!(signal.wait(Duration::from_secs(5)));
        handle.join().unwrap();
        assert!(signal.is_raised());
        signal.clear();
        assert!(!signal.is_raised());
    }

    #[test]
    fn keyed_locks_serialise_the_same_key_but_not_different_keys() {
        let locks = Arc::new(KeyedLocks::<String>::default());
        let inside = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        let workers: Vec<_> = (0..6)
            .map(|_| {
                let (locks, inside, max_seen) = (Arc::clone(&locks), Arc::clone(&inside), Arc::clone(&max_seen));
                thread::spawn(move || {
                    let _guard = locks.acquire(&"same".to_owned());
                    let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                    max_seen.fetch_max(now, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(5));
                    inside.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }
        assert_eq!(max_seen.load(Ordering::SeqCst), 1);

        let _a = locks.acquire(&"a".to_owned());
        let _b = locks.acquire(&"b".to_owned()); // does not block
        assert_eq!(lock(&locks.held).len(), 2);
    }

    #[test]
    fn keyed_locks_do_not_leak_keys() {
        let locks = KeyedLocks::<String>::default();
        for i in 0..100 {
            drop(locks.acquire(&format!("k{i}")));
        }
        assert!(lock(&locks.held).is_empty());
    }

    #[test]
    fn a_delay_queue_yields_items_in_due_order_and_not_early() {
        let queue = DelayQueue::<&str>::default();
        queue.push("late", Duration::from_millis(60));
        queue.push("soon", Duration::from_millis(5));
        let start = Instant::now();
        assert_eq!(queue.pop(), Some("soon"));
        assert_eq!(queue.pop(), Some("late"));
        assert!(start.elapsed() >= Duration::from_millis(55), "must not return before the delay");
    }

    #[test]
    fn a_delay_queue_keeps_insertion_order_for_equal_due_times() {
        let queue = DelayQueue::<u32>::default();
        for n in [3, 1, 2] {
            queue.push(n, Duration::ZERO);
        }
        // Equal delays are ordered by insertion, not by value.
        assert_eq!((queue.pop(), queue.pop(), queue.pop()), (Some(3), Some(1), Some(2)));
    }

    #[test]
    fn closing_a_delay_queue_releases_blocked_consumers() {
        let queue = Arc::new(DelayQueue::<u8>::default());
        let consumer = {
            let queue = Arc::clone(&queue);
            thread::spawn(move || queue.pop())
        };
        thread::sleep(Duration::from_millis(20));
        queue.close();
        assert_eq!(consumer.join().unwrap(), None);
    }
}
