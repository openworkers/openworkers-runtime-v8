//! Which request runs JS on an isolate.
//!
//! Requests that share an isolate take turns under its lock. A guard that
//! terminates execution for one request must not stop another one, so it
//! goes through the turn: it terminates only while its own request holds
//! it. A termination left over from the previous holder is cancelled when
//! the next turn begins.
//!
//! The heap and ArrayBuffer limits set one flag for the isolate. The JS that
//! allocates is the JS of the turn, so the flag goes to the request that
//! holds the turn when the turn ends.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use super::CpuEnforcer;

pub struct Turn {
    handle: v8::IsolateHandle,
    holder: Mutex<Option<u64>>,
    memory_limit_hit: Arc<AtomicBool>,
}

impl Turn {
    /// The turn of the isolate of `handle`, whose memory limits set
    /// `memory_limit_hit`.
    pub fn new(handle: v8::IsolateHandle, memory_limit_hit: Arc<AtomicBool>) -> Self {
        Self {
            handle,
            holder: Mutex::new(None),
            memory_limit_hit,
        }
    }

    /// Gives the turn to `request`. The caller holds the isolate lock.
    pub fn begin(&self, request: u64) {
        let mut holder = self.holder.lock().unwrap();
        *holder = Some(request);
        self.handle.cancel_terminate_execution();
    }

    /// Ends the turn, and answers whether a memory limit was hit in it.
    ///
    /// A termination that landed after the turn's JS returned is cancelled
    /// here: the request's guards keep their flags, and JS run outside a
    /// turn, such as the script of a new context, must not die of it.
    pub fn end(&self) -> bool {
        let mut holder = self.holder.lock().unwrap();
        *holder = None;
        self.handle.cancel_terminate_execution();
        self.memory_limit_hit.swap(false, Ordering::SeqCst)
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

/// The turn of a request, from `begin` to drop, with its CPU counted and its
/// memory limit set in `memory_hit`. Take it right after the isolate lock,
/// and drop it before the lock.
pub struct TurnGuard<'a> {
    turn: &'a Turn,
    cpu: Option<&'a CpuEnforcer>,
    memory_hit: &'a AtomicBool,
}

impl<'a> TurnGuard<'a> {
    pub fn begin(
        turn: &'a Turn,
        request: u64,
        cpu: Option<&'a CpuEnforcer>,
        memory_hit: &'a AtomicBool,
    ) -> Self {
        turn.begin(request);

        if let Some(cpu) = cpu {
            cpu.begin_turn();
        }

        Self {
            turn,
            cpu,
            memory_hit,
        }
    }
}

impl Drop for TurnGuard<'_> {
    fn drop(&mut self) {
        if let Some(cpu) = self.cpu {
            cpu.end_turn();
        }

        if self.turn.end() {
            self.memory_hit.store(true, Ordering::SeqCst);
        }
    }
}

/// A new id for each event, which guards and turns refer to.
pub fn next_request_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::pin;

    /// A termination that lands after the turn's JS returned does not stop
    /// the JS run next outside a turn.
    #[test]
    fn a_termination_does_not_outlive_the_turn() {
        crate::platform::get_platform();
        let mut isolate = crate::v8_helpers::new_isolate(Default::default());
        let turn = Turn::new(
            isolate.thread_safe_handle(),
            Arc::new(AtomicBool::new(false)),
        );

        let scope = pin!(v8::HandleScope::new(&mut isolate));
        let mut scope = scope.init();
        let context = v8::Context::new(&scope, Default::default());
        let scope = &mut v8::ContextScope::new(&mut scope, context);

        let run = |scope: &mut v8::PinScope| {
            let code = v8::String::new(scope, "1 + 1").unwrap();
            let script = v8::Script::compile(scope, code, None).unwrap();
            script.run(scope).and_then(|value| value.int32_value(scope))
        };

        turn.begin(1);
        assert!(!turn.terminate(2), "another request cannot end the turn");
        assert!(turn.terminate(1));
        turn.end();

        assert_eq!(run(scope), Some(2), "the termination outlived the turn");
        assert!(!turn.terminate(1), "no turn, nothing to terminate");
    }
}
