//! Security module for OpenWorkers V8 runtime.
//!
//! This module contains all security-related components to protect against
//! resource abuse and denial-of-service attacks in a multi-tenant environment.
//!
//! ## Components
//!
//! - [`array_buffer_allocator`]: Custom V8 ArrayBuffer allocator with memory limits
//! - [`timeout_guard`]: Wall-clock timeout enforcement via watchdog thread
//! - [`cpu_enforcer`]: CPU time limit enforcement via POSIX timers (Linux only)
//!
//! ## Usage
//!
//! ```rust,ignore
//! use openworkers_runtime_v8::security::{
//!     ArrayBufferAllocator, TimeoutGuard, CpuEnforcer
//! };
//!
//! // Memory limits via custom allocator
//! let allocator = ArrayBufferAllocator::new(128 * 1024 * 1024, memory_flag.clone());
//!
//! // Wall-clock timeout (all platforms)
//! let _timeout = TimeoutGuard::new(isolate_handle.clone(), 30_000); // 30s
//!
//! // CPU time limit (Linux only)
//! let _cpu = CpuEnforcer::new(isolate_handle.clone(), 50); // 50ms
//! ```

mod array_buffer_allocator;
mod cpu_enforcer;
mod cpu_timer;
mod timeout_guard;

pub use array_buffer_allocator::CustomAllocator;
pub use cpu_enforcer::CpuEnforcer;
pub use cpu_timer::{CpuTimer, get_thread_cpu_time};
pub use timeout_guard::TimeoutGuard;
