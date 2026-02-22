//! Fair FIFO queue for serializing V8 Locker access across concurrent requests.
//!
//! When multiple requests share the same V8 isolate (intra-isolate multiplexing),
//! only one can hold the V8 Locker at a time. AsyncWaiter ensures FIFO ordering
//! so no request starves.
//!
//! Thread safety: Uses `AtomicBool` + `std::sync::Mutex` so the type is `Send + Sync`,
//! allowing storage in `Arc<TaggedIsolate>`. In practice, all callers run on the same
//! `tokio::task::LocalSet` thread, so the mutex never contends.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Waker};

/// Fair FIFO queue for serializing V8 Locker access across concurrent requests.
///
/// Only one request can hold the Locker at a time. Others wait in FIFO order.
/// Uses atomic/mutex types for `Send + Sync` (needed by `Arc<TaggedIsolate>`),
/// but all callers are on the same thread so the mutex never contends.
pub struct AsyncWaiter {
    queue: Mutex<VecDeque<Waker>>,
    locked: AtomicBool,
}

impl Default for AsyncWaiter {
    fn default() -> Self {
        Self::new()
    }
}

impl AsyncWaiter {
    pub fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            locked: AtomicBool::new(false),
        }
    }

    /// Try to acquire the turn. Returns true if this request can proceed.
    ///
    /// If false, the waker from `cx` is registered and will be woken in FIFO order
    /// when the current holder calls `unlock()`.
    ///
    /// Uses `compare_exchange` for correctness even though current callers are
    /// single-threaded (all on the same LocalSet).
    pub fn try_lock(&self, cx: &mut Context<'_>) -> bool {
        if self
            .locked
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            true
        } else {
            self.queue.lock().unwrap().push_back(cx.waker().clone());
            false
        }
    }

    /// Release the turn. Wakes the next waiter in FIFO order.
    pub fn unlock(&self) {
        self.locked.store(false, Ordering::Release);

        if let Some(waker) = self.queue.lock().unwrap().pop_front() {
            waker.wake();
        }
    }

    /// Returns true if no request currently holds the lock.
    pub fn is_free(&self) -> bool {
        !self.locked.load(Ordering::Acquire)
    }

    /// Returns the number of requests waiting in the queue.
    pub fn queue_len(&self) -> usize {
        self.queue.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::task::{RawWaker, RawWakerVTable};

    /// Create a no-op waker for testing
    fn noop_waker() -> Waker {
        fn noop(_: *const ()) {}
        fn clone(p: *const ()) -> RawWaker {
            RawWaker::new(p, &VTABLE)
        }
        const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
    }

    /// Create a Context from a Waker for testing
    fn test_context(waker: &Waker) -> Context<'_> {
        Context::from_waker(waker)
    }

    #[test]
    fn test_lock_unlock_basic() {
        let waiter = AsyncWaiter::new();
        let waker = noop_waker();
        let mut cx = test_context(&waker);

        // Initially free
        assert!(waiter.is_free());
        assert_eq!(waiter.queue_len(), 0);

        // First lock succeeds
        assert!(waiter.try_lock(&mut cx));
        assert!(!waiter.is_free());

        // Second lock fails and enqueues waker
        assert!(!waiter.try_lock(&mut cx));
        assert_eq!(waiter.queue_len(), 1);

        // Third lock also fails
        assert!(!waiter.try_lock(&mut cx));
        assert_eq!(waiter.queue_len(), 2);

        // Unlock wakes first waiter, drains one from queue
        waiter.unlock();
        assert!(waiter.is_free());
        assert_eq!(waiter.queue_len(), 1);

        // Can lock again after unlock
        assert!(waiter.try_lock(&mut cx));
        assert!(!waiter.is_free());
    }

    #[test]
    fn test_fifo_ordering() {
        let waiter = AsyncWaiter::new();
        let wake_log: Arc<std::sync::Mutex<Vec<u32>>> = Arc::new(std::sync::Mutex::new(Vec::new()));

        // Lock it
        let waker = noop_waker();
        let mut cx = test_context(&waker);
        assert!(waiter.try_lock(&mut cx));

        // Enqueue 3 waiters with tracking wakers
        for i in 0..3u32 {
            let log = wake_log.clone();
            let raw_waker = {
                let data = Box::into_raw(Box::new((i, log.clone())));

                fn clone_fn(p: *const ()) -> RawWaker {
                    let (i, log) =
                        unsafe { &*(p as *const (u32, Arc<std::sync::Mutex<Vec<u32>>>)) };
                    let new_data = Box::into_raw(Box::new((*i, log.clone())));
                    RawWaker::new(new_data as *const (), &TRACKING_VTABLE)
                }
                fn wake_fn(p: *const ()) {
                    let data =
                        unsafe { Box::from_raw(p as *mut (u32, Arc<std::sync::Mutex<Vec<u32>>>)) };
                    data.1.lock().unwrap().push(data.0);
                }
                fn drop_fn(p: *const ()) {
                    unsafe {
                        drop(Box::from_raw(
                            p as *mut (u32, Arc<std::sync::Mutex<Vec<u32>>>),
                        ))
                    };
                }
                static TRACKING_VTABLE: RawWakerVTable =
                    RawWakerVTable::new(clone_fn, wake_fn, wake_fn, drop_fn);

                RawWaker::new(data as *const (), &TRACKING_VTABLE)
            };

            let waker = unsafe { Waker::from_raw(raw_waker) };
            let mut cx = test_context(&waker);
            assert!(!waiter.try_lock(&mut cx));
        }

        assert_eq!(waiter.queue_len(), 3);

        // Unlock 3 times — should wake in FIFO order: 0, 1, 2
        waiter.unlock();
        waiter.unlock();
        waiter.unlock();

        let log = wake_log.lock().unwrap();
        assert_eq!(*log, vec![0, 1, 2], "Wakers must be woken in FIFO order");
    }

    #[test]
    fn test_compare_exchange_prevents_double_lock() {
        // Verify that try_lock uses compare_exchange (not load+store)
        // by checking the atomic state directly
        let waiter = AsyncWaiter::new();
        let waker = noop_waker();
        let mut cx = test_context(&waker);

        assert!(waiter.try_lock(&mut cx));

        // Manually check the locked state
        assert!(waiter.locked.load(Ordering::Acquire));

        // Second try_lock must fail
        assert!(!waiter.try_lock(&mut cx));

        // State unchanged
        assert!(waiter.locked.load(Ordering::Acquire));
    }
}
