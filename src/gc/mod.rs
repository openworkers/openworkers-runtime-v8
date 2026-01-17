//! GC tracking for external memory.
//!
//! This module provides utilities for tracking Rust memory allocations
//! with V8's garbage collector, enabling accurate heap snapshots and
//! informed GC decisions.
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │  JsLock (RAII)                                              │
//! │  ├── v8::Locker (thread safety)                             │
//! │  ├── Stored in isolate slot for Lock::current()             │
//! │  └── Applies deferred memory updates on construction        │
//! └─────────────────────────────────────────────────────────────┘
//!                              │
//!                              ▼
//! ┌─────────────────────────────────────────────────────────────┐
//! │  ExternalMemoryGuard (RAII)                                 │
//! │  ├── Tracks memory amount                                   │
//! │  ├── If lock held → immediate adjust                        │
//! │  └── If no lock → deferred via atomic                       │
//! └─────────────────────────────────────────────────────────────┘
//!                              │
//!                              ▼
//! ┌─────────────────────────────────────────────────────────────┐
//! │  GcTraceable trait                                          │
//! │  └── external_memory_size() → usize                         │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Usage
//!
//! ```ignore
//! use crate::gc::{GcTraceable, ExternalMemoryGuard};
//!
//! struct MyBuffer {
//!     data: Vec<u8>,
//!     _guard: ExternalMemoryGuard,
//! }
//!
//! impl MyBuffer {
//!     fn new(size: usize) -> Self {
//!         let data = vec![0u8; size];
//!         let guard = ExternalMemoryGuard::new(size as i64);
//!         Self { data, _guard: guard }
//!     }
//! }
//! ```

mod external_memory;
mod js_lock;
mod traceable;

pub use external_memory::ExternalMemoryGuard;
pub use js_lock::{JsLock, JsLockRef};
pub use traceable::{GcTraceable, Tracked, tracked_guard};

#[cfg(test)]
mod tests;
