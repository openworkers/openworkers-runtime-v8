//! GC tracking for external memory and deferred destruction.
//!
//! This module provides utilities for tracking Rust memory allocations
//! with V8's garbage collector, and safe deferred destruction of V8 handles.
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │  JsLock (RAII)                                              │
//! │  ├── v8::Locker (thread safety)                             │
//! │  ├── Stored in isolate slot for Lock::current()             │
//! │  ├── Applies deferred memory updates on construction        │
//! │  └── Processes deferred handle destructions                 │
//! └─────────────────────────────────────────────────────────────┘
//!                              │
//!                              ▼
//! ┌─────────────────────────────────────────────────────────────┐
//! │  DeferredDestructionQueue                                   │
//! │  ├── Thread-safe queue for V8 Global handles                │
//! │  ├── Handles queued when dropped without lock               │
//! │  └── Processed on next lock acquisition                     │
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

mod deferred_destruction;
mod external_memory;
mod js_lock;
mod traceable;

pub use deferred_destruction::DeferredDestructionQueue;
pub use external_memory::ExternalMemoryGuard;
pub use js_lock::{JsLock, JsLockRef};
pub use traceable::{GcTraceable, Tracked, tracked_guard};

#[cfg(test)]
mod tests;
