//! Bounded inter-thread queues (TECH_SPEC §4.1, §4.3, task T1.3).
//!
//! Two primitives live here:
//!
//! * [`SpscRing`] — a lock-free single-producer/single-consumer ring. The CPAL
//!   callback is the only producer and it must never allocate or block, so the
//!   ring is fully preallocated and pushing past capacity only bumps an overflow
//!   counter.
//! * [`BoundedQueue`] — a mutex/condvar queue for the dispatcher's downstream
//!   lanes. It keeps one FIFO so that `SegmentStart`, audio, `Gap` and
//!   `SegmentEnd` are always observed in order, but it only accepts *audio*
//!   while the queue is below `audio_capacity`. The slots above that line are
//!   reserved for control messages, which is what makes them droppable-never
//!   (TECH_SPEC §4.3).

use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// Lock-free single-producer/single-consumer ring of `T: Copy` values.
///
/// The producer only advances `head`, the consumer only advances `tail`, and the
/// slot between them is the single synchronisation point (Release on publish,
/// Acquire on read). Capacity is rounded up to a power of two so the index wrap
/// is a mask instead of a modulo.
pub struct SpscRing<T: Copy> {
    slots: Box<[UnsafeCell<MaybeUninit<T>>]>,
    mask: usize,
    head: AtomicUsize,
    tail: AtomicUsize,
    overflow: AtomicU64,
}

// SAFETY: `T: Copy` means values are plain bit patterns that are never dropped,
// and the head/tail protocol guarantees the producer and the consumer never
// touch the same slot at the same time.
unsafe impl<T: Copy + Send> Send for SpscRing<T> {}
unsafe impl<T: Copy + Send> Sync for SpscRing<T> {}

impl<T: Copy> SpscRing<T> {
    /// Creates a ring holding at least `min_capacity` values.
    pub fn with_capacity(min_capacity: usize) -> Self {
        let capacity = min_capacity.max(1).next_power_of_two();
        let slots = (0..capacity)
            .map(|_| UnsafeCell::new(MaybeUninit::uninit()))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        SpscRing {
            slots,
            mask: capacity - 1,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            overflow: AtomicU64::new(0),
        }
    }

    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Number of values currently queued.
    pub fn len(&self) -> usize {
        let head = self.head.load(Ordering::Acquire);
        let tail = self.tail.load(Ordering::Acquire);
        head.wrapping_sub(tail)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Values that can still be pushed before the ring reports overflow.
    pub fn free_space(&self) -> usize {
        self.capacity() - self.len()
    }

    /// Number of values rejected because the ring was full.
    pub fn overflow_count(&self) -> u64 {
        self.overflow.load(Ordering::Relaxed)
    }

    /// Producer side. Returns `false` (and counts an overflow) when full.
    pub fn try_push(&self, value: T) -> bool {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= self.slots.len() {
            self.overflow.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let index = head & self.mask;
        // SAFETY: the slot is not reachable by the consumer until `head` is
        // published below, so writing it here cannot race.
        unsafe { (*self.slots[index].get()).write(value) };
        self.head.store(head.wrapping_add(1), Ordering::Release);
        true
    }

    /// Producer side. Returns `false` without counting an overflow.
    pub fn try_push_quiet(&self, value: T) -> bool {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= self.slots.len() {
            return false;
        }
        let index = head & self.mask;
        // SAFETY: as in `try_push`.
        unsafe { (*self.slots[index].get()).write(value) };
        self.head.store(head.wrapping_add(1), Ordering::Release);
        true
    }

    /// Consumer side.
    pub fn try_pop(&self) -> Option<T> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if tail == head {
            return None;
        }
        let index = tail & self.mask;
        // SAFETY: `head` was published after the producer wrote this slot, so the
        // value is initialised and no longer written by the producer.
        let value = unsafe { (*self.slots[index].get()).assume_init_read() };
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some(value)
    }
}

/// Marks a queue element as audio or control (TECH_SPEC §4.3).
pub trait QueueItem {
    fn is_control(&self) -> bool;
}

/// Result of a bounded wait for the next item.
#[derive(Debug, PartialEq, Eq)]
pub enum PopOutcome<T> {
    Item(T),
    /// The wait expired without an item; the caller may check its watchdog.
    Empty,
    /// The producer closed the queue.
    Closed,
}

struct Inner<T> {
    items: VecDeque<T>,
    closed: bool,
}

/// Bounded FIFO with capacity reserved for control messages.
///
/// `audio_capacity` is the length at which [`BoundedQueue::push_audio`] starts
/// rejecting; the queue itself holds `audio_capacity + control_capacity` items.
pub struct BoundedQueue<T> {
    inner: Mutex<Inner<T>>,
    not_empty: Condvar,
    audio_capacity: usize,
    total_capacity: usize,
    high_water: AtomicU64,
    control_in_flight: AtomicU64,
}

impl<T> BoundedQueue<T> {
    pub fn new(audio_capacity: usize, control_capacity: usize) -> Self {
        let audio_capacity = audio_capacity.max(1);
        BoundedQueue {
            inner: Mutex::new(Inner {
                items: VecDeque::with_capacity(audio_capacity + control_capacity.max(1)),
                closed: false,
            }),
            not_empty: Condvar::new(),
            audio_capacity,
            total_capacity: audio_capacity + control_capacity.max(1),
            high_water: AtomicU64::new(0),
            control_in_flight: AtomicU64::new(0),
        }
    }

    /// Slot count available to audio pushes.
    pub fn audio_capacity(&self) -> usize {
        self.audio_capacity
    }

    /// Highest queue length seen so far (TECH_SPEC §12 high-water stats).
    pub fn high_water(&self) -> u64 {
        self.high_water.load(Ordering::Relaxed)
    }

    /// Control messages currently queued (TECH_SPEC §12 control-slot usage).
    pub fn control_in_flight(&self) -> u64 {
        self.control_in_flight.load(Ordering::Relaxed)
    }

    pub fn len(&self) -> usize {
        self.inner.lock().map(|g| g.items.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Pushes audio. Fails (returning the value) once the audio slots are used
    /// up; the reserved control slots stay untouched.
    pub fn push_audio(&self, value: T) -> Result<(), T> {
        let Ok(mut guard) = self.inner.lock() else {
            return Err(value);
        };
        if guard.items.len() >= self.audio_capacity {
            return Err(value);
        }
        guard.items.push_back(value);
        self.observe_len(guard.items.len());
        drop(guard);
        self.not_empty.notify_one();
        Ok(())
    }

    /// Pushes a control message. Only fails when the consumer has stopped
    /// draining the queue entirely, which the dispatcher treats as `degraded`.
    pub fn push_control(&self, value: T) -> Result<(), T> {
        let Ok(mut guard) = self.inner.lock() else {
            return Err(value);
        };
        if guard.items.len() >= self.total_capacity {
            return Err(value);
        }
        guard.items.push_back(value);
        self.control_in_flight.fetch_add(1, Ordering::Relaxed);
        self.observe_len(guard.items.len());
        drop(guard);
        self.not_empty.notify_one();
        Ok(())
    }

    fn observe_len(&self, len: usize) {
        self.high_water.fetch_max(len as u64, Ordering::Relaxed);
    }

    /// Blocks up to `timeout` for the next item, control messages included in
    /// arrival order.
    pub fn pop_timeout(&self, timeout: Duration) -> PopOutcome<T> {
        let Ok(mut guard) = self.inner.lock() else {
            return PopOutcome::Closed;
        };
        loop {
            if let Some(item) = guard.items.pop_front() {
                return PopOutcome::Item(item);
            }
            if guard.closed {
                return PopOutcome::Closed;
            }
            match self.not_empty.wait_timeout(guard, timeout) {
                Ok((next, result)) => {
                    guard = next;
                    if result.timed_out() && guard.items.is_empty() {
                        return PopOutcome::Empty;
                    }
                }
                Err(_) => return PopOutcome::Closed,
            }
        }
    }

    /// Marks the queue closed and wakes every waiter.
    pub fn close(&self) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.closed = true;
        }
        self.not_empty.notify_all();
    }
}

/// A flag a consumer sets when its own watchdog fired.
#[derive(Debug, Default)]
pub struct StuckFlag(AtomicBool);

impl StuckFlag {
    pub fn set(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub fn take(&self) -> bool {
        self.0.swap(false, Ordering::Relaxed)
    }

    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn spsc_ring_round_trips_in_order() {
        let ring: SpscRing<u32> = SpscRing::with_capacity(4);
        for i in 0..4 {
            assert!(ring.try_push(i));
        }
        assert_eq!(ring.len(), 4);
        assert!(!ring.try_push(99));
        assert_eq!(ring.overflow_count(), 1);
        for i in 0..4 {
            assert_eq!(ring.try_pop(), Some(i));
        }
        assert_eq!(ring.try_pop(), None);
    }

    #[test]
    fn spsc_ring_reports_free_space() {
        let ring: SpscRing<u8> = SpscRing::with_capacity(3);
        assert_eq!(ring.capacity(), 4);
        assert_eq!(ring.free_space(), 4);
        ring.try_push(1);
        assert_eq!(ring.free_space(), 3);
    }

    #[test]
    fn spsc_ring_keeps_order_across_threads() {
        let ring = Arc::new(SpscRing::with_capacity(1024));
        let producer_ring = Arc::clone(&ring);
        let producer = thread::spawn(move || {
            // The consumer is deliberately slower than this loop, so the ring
            // does fill up; `try_push_quiet` keeps that from being counted as a
            // real overflow (which is what `spsc_ring_round_trips_in_order`
            // checks).
            let mut next = 0u64;
            while next < 50_000 {
                if producer_ring.try_push_quiet(next) {
                    next += 1;
                } else {
                    thread::yield_now();
                }
            }
        });
        let mut received = Vec::new();
        while received.len() < 50_000 {
            if let Some(v) = ring.try_pop() {
                received.push(v);
            } else {
                thread::yield_now();
            }
        }
        producer.join().unwrap();
        assert!(received.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(ring.overflow_count(), 0);
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Msg {
        Audio(u32),
        Control(u32),
    }

    impl QueueItem for Msg {
        fn is_control(&self) -> bool {
            matches!(self, Msg::Control(_))
        }
    }

    #[test]
    fn audio_pushes_stop_at_audio_capacity() {
        let q: BoundedQueue<Msg> = BoundedQueue::new(2, 4);
        assert!(q.push_audio(Msg::Audio(1)).is_ok());
        assert!(q.push_audio(Msg::Audio(2)).is_ok());
        assert_eq!(q.push_audio(Msg::Audio(3)), Err(Msg::Audio(3)));
        assert_eq!(q.high_water(), 2);
    }

    #[test]
    fn control_slots_survive_a_full_audio_lane() {
        let q: BoundedQueue<Msg> = BoundedQueue::new(1, 4);
        q.push_audio(Msg::Audio(1)).unwrap();
        for i in 0..4 {
            assert!(q.push_control(Msg::Control(i)).is_ok());
        }
        assert_eq!(q.push_control(Msg::Control(9)), Err(Msg::Control(9)));
        assert_eq!(q.control_in_flight(), 4);
    }

    #[test]
    fn pop_preserves_control_and_audio_order() {
        let q: BoundedQueue<Msg> = BoundedQueue::new(2, 2);
        q.push_control(Msg::Control(0)).unwrap();
        q.push_audio(Msg::Audio(1)).unwrap();
        q.push_control(Msg::Control(2)).unwrap();
        let mut seen = Vec::new();
        while let PopOutcome::Item(m) = q.pop_timeout(Duration::ZERO) {
            seen.push(m);
        }
        assert_eq!(seen, vec![Msg::Control(0), Msg::Audio(1), Msg::Control(2)]);
    }

    #[test]
    fn pop_timeout_reports_empty_then_closed() {
        let q: BoundedQueue<Msg> = BoundedQueue::new(1, 1);
        assert_eq!(q.pop_timeout(Duration::from_millis(1)), PopOutcome::Empty);
        q.close();
        assert_eq!(q.pop_timeout(Duration::from_millis(1)), PopOutcome::Closed);
    }
}
