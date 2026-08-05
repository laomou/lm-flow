//! Graph public facade.
//!
//! The implementation is split by responsibility:
//! build-time validation, runtime scheduling, Pollers, diagnostics, and lifecycle support.

mod runtime;

pub use runtime::*;
