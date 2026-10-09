//! Security module for OpenWorkers V8 runtime.
//!
//! This module contains all security-related components to protect against
//! resource abuse and denial-of-service attacks in a multi-tenant environment.
//!
//! ## Components
//!
//! - [`array_buffer_allocator`]: Custom V8 ArrayBuffer allocator with memory limits
//! - [`heap_limit`]: Near-heap-limit callback to prevent V8 OOM crashes
//! - [`timeout_guard`]: Wall-clock timeout enforcement via watchdog thread
//! - [`cpu_enforcer`]: CPU time limit of one request, counted over its turns;
//!   Linux also cuts a turn that runs past it, through a POSIX timer
//! - [`turn`]: which request runs JS on an isolate, so a guard stops only
//!   its own request

#[cfg(not(feature = "sandbox"))]
mod array_buffer_allocator;
mod cpu_enforcer;
mod cpu_timer;
mod heap_limit;
mod timeout_guard;
mod turn;

#[cfg(not(feature = "sandbox"))]
pub use array_buffer_allocator::CustomAllocator;
pub use cpu_enforcer::CpuEnforcer;
pub use cpu_timer::{CpuTimer, get_thread_cpu_time};
pub use heap_limit::{HeapLimitState, install_heap_limit_callback};
pub use timeout_guard::TimeoutGuard;
pub use turn::{Turn, TurnGuard, next_request_id};
