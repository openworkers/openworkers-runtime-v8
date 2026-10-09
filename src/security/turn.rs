//! Which request runs JS on an isolate.
//!
//! Requests that share an isolate take turns under its lock. A guard that
//! terminates execution for one request must not stop another one, so it
//! goes through the turn: it terminates only while its own request holds
//! it. A termination left over from the previous holder is cancelled when
//! the next turn begins.

use std::sync::Mutex;

use super::CpuEnforcer;

pub struct Turn {
    handle: v8::IsolateHandle,
    holder: Mutex<Option<u64>>,
}

impl Turn {
    pub fn new(handle: v8::IsolateHandle) -> Self {
        Self {
            handle,
            holder: Mutex::new(None),
        }
    }

    /// Gives the turn to `request`. The caller holds the isolate lock.
    pub fn begin(&self, request: u64) {
        let mut holder = self.holder.lock().unwrap();
        *holder = Some(request);
        self.handle.cancel_terminate_execution();
    }

    pub fn end(&self) {
        *self.holder.lock().unwrap() = None;
    }

    /// Terminates execution if `request` holds the turn, and answers whether
    /// it did. Otherwise the request sees its guard's flag when its next
    /// turn begins.
    pub fn terminate(&self, request: u64) -> bool {
        let holder = self.holder.lock().unwrap();

        if *holder != Some(request) {
            return false;
        }

        self.handle.terminate_execution();
        true
    }
}

/// The turn of a request, from `begin` to drop, with its CPU counted. Take
/// it right after the isolate lock, and drop it before the lock.
pub struct TurnGuard<'a> {
    turn: &'a Turn,
    cpu: Option<&'a CpuEnforcer>,
}

impl<'a> TurnGuard<'a> {
    pub fn begin(turn: &'a Turn, request: u64, cpu: Option<&'a CpuEnforcer>) -> Self {
        turn.begin(request);

        if let Some(cpu) = cpu {
            cpu.begin_turn();
        }

        Self { turn, cpu }
    }
}

impl Drop for TurnGuard<'_> {
    fn drop(&mut self) {
        if let Some(cpu) = self.cpu {
            cpu.end_turn();
        }

        self.turn.end();
    }
}

/// A new id for each event, which guards and turns refer to.
pub fn next_request_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}
