// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! # LiteBox Logging Utilities
//!
//! A unified logging facade for LiteBox that abstracts over different logging backends.
//!
//! This crate provides macros for structured logging and tracing spans that work
//! consistently regardless of whether the underlying backend is `log` or `tracing`.
//!
//! ## Features
//!
//! - `backend_log` (default): Uses the [`log`](https://docs.rs/log) crate for logging events.
//!   Spans are emulated by logging events at span entry and exit.
//!
//! - `backend_tracing`: Uses the [`tracing`](https://docs.rs/tracing) crate with full span support.
//!   However, since `tracing` does not natively support `sval` or `serde` key-value capture,
//!   values captured with `:sval` or `:serde` are silently downgraded to their `Debug` (`{:?}`)
//!   representations.
//!
//! When both features are enabled, `backend_tracing` takes precedence.
//!
//! ## Key-Value Capture Modes
//!
//! This crate supports the same capture modes as `log`'s `kv` feature:
//!
//! - `:?` or `:debug` - Capture the value using `Debug`
//! - `:%` or `:display` - Capture the value using `Display`
//! - `:err` - Capture the value using `std::error::Error` (requires `kv_std`)
//! - `:sval` - Capture the value using `sval::Value` (requires `kv_sval`)
//! - `:serde` - Capture the value using `serde::Serialize` (requires `kv_serde`)
//!
//! ## Example
//!
//! ```ignore
//! use litebox_util_log::{info, debug, info_span, instrument};
//!
//! // Simple logging
//! info!("Hello, world!");
//!
//! // Logging with key-value pairs
//! let user_id = 42;
//! info!(user_id:? = user_id; "User logged in");
//!
//! // Using spans (returns a guard, exits when dropped)
//! let _span = info_span!("my_operation", request_id:? = req_id);
//! // ... do work ...
//! debug!("Processing request");
//! // span exits when _span is dropped
//!
//! // Using the instrument attribute macro
//! #[instrument(level = debug, fields(user_id:?))]
//! fn process_user(user_id: u64, data: &str) {
//!     info!("Processing user");
//! }
//! ```

#![no_std]

#[cfg(not(any(feature = "backend_log", feature = "backend_tracing")))]
compile_error!("Either `backend_log` or `backend_tracing` feature must be enabled.");

#[macro_use]
mod macros;

#[cfg(all(feature = "backend_log", not(feature = "backend_tracing")))]
#[macro_use]
mod backend_log;

#[cfg(feature = "backend_tracing")]
#[macro_use]
mod backend_tracing;

pub use litebox_util_log_macros::instrument;

/// Log level that abstracts over backend-specific level types.
///
/// Levels are ordered from most severe to least severe: `Error` > `Warn` > `Info` > `Debug` > `Trace`.
/// This ordering is used by logging implementations to filter messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    /// Serious problems that need immediate attention.
    Error,
    /// Potential issues or unexpected situations.
    Warn,
    /// General informational messages.
    Info,
    /// Debugging information useful during development.
    Debug,
    /// Very verbose debugging, typically disabled in production.
    Trace,
}

#[cfg(all(feature = "backend_log", not(feature = "backend_tracing")))]
pub use backend_log::SpanGuard;

#[cfg(feature = "backend_tracing")]
pub use backend_tracing::SpanGuard;

#[cfg(feature = "tracing_subscriber_init")]
pub use backend_tracing::init_env_filtered_subscriber;

/// Converts a [`log::Record`] into the compact host-console format.
///
/// Formats the record as `[LEVEL] message key=value ...\n` into `writer`.
#[cfg(feature = "backend_log")]
pub fn format_record<W: core::fmt::Write>(
    writer: &mut W,
    record: &log::Record<'_>,
) -> core::fmt::Result {
    struct FieldVisitor<'a, W>(&'a mut W);

    impl<W: core::fmt::Write> log::kv::VisitSource<'_> for FieldVisitor<'_, W> {
        fn visit_pair(
            &mut self,
            key: log::kv::Key<'_>,
            value: log::kv::Value<'_>,
        ) -> Result<(), log::kv::Error> {
            write!(self.0, " {key}={value}")?;
            Ok(())
        }
    }

    write!(writer, "[{}] {}", record.level(), record.args())?;
    record
        .key_values()
        .visit(&mut FieldVisitor(writer))
        .map_err(|_| core::fmt::Error)?;
    writeln!(writer)
}

/// Internal module exposing backend types for use by exported macros.
///
/// This module is public only because macros need access to backend types at the
/// call site. It is not part of the public API and should not be used directly.
/// Breaking changes to this module are not considered semver violations.
#[doc(hidden)]
pub mod __private {
    #[cfg(all(feature = "backend_log", not(feature = "backend_tracing")))]
    pub use log;
    #[cfg(all(feature = "backend_log", not(feature = "backend_tracing")))]
    pub use log::Level;

    #[cfg(feature = "backend_tracing")]
    pub use tracing;
    #[cfg(feature = "backend_tracing")]
    pub use tracing::Level;
}

/// Registers the function used to enter (`true`) / leave (`false`) a scope in which the calling
/// thread must allocate from private (non-shared) memory. The runner sets this when its global
/// allocator can place heap data in memory shared between forked guest processes; unset (the
/// default) it does nothing. See [`PrivateAllocGuard`].
pub fn set_private_alloc_hook(hook: fn(bool)) {
    PRIVATE_ALLOC_HOOK.store(hook as usize, core::sync::atomic::Ordering::Release);
}

static PRIVATE_ALLOC_HOOK: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// While alive, the calling thread's heap allocations come from private memory.
///
/// Wrap any code that grows a `static` (or `thread_local!`) collection: with kernel state shared
/// across a native `fork()`, such a collection's nodes would otherwise be reachable from BOTH the
/// parent's and the child's copy of the static, and each process would then mutate the other's
/// nodes.
pub struct PrivateAllocGuard(());

impl PrivateAllocGuard {
    /// Enters a private-allocation scope.
    #[must_use]
    pub fn new() -> Self {
        let f = PRIVATE_ALLOC_HOOK.load(core::sync::atomic::Ordering::Acquire);
        if f != 0 {
            // SAFETY: only `set_private_alloc_hook` stores here, always a valid `fn(bool)`.
            let hook: fn(bool) = unsafe { core::mem::transmute::<usize, fn(bool)>(f) };
            hook(true);
        }
        Self(())
    }
}

impl Default for PrivateAllocGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PrivateAllocGuard {
    fn drop(&mut self) {
        let f = PRIVATE_ALLOC_HOOK.load(core::sync::atomic::Ordering::Acquire);
        if f != 0 {
            // SAFETY: as in `new`.
            let hook: fn(bool) = unsafe { core::mem::transmute::<usize, fn(bool)>(f) };
            hook(false);
        }
    }
}
