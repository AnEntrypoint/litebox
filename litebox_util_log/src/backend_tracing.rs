// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Tracing backend implementation.
//!
//! This module provides the backend implementation when using the `tracing` crate.
//! Unlike the log backend, this provides full native span support with proper
//! hierarchical context propagation.
//!
//! The macros in this module transform our unified key-value syntax into
//! tracing's native field syntax using a tt-muncher pattern.

#[cfg(feature = "tracing_subscriber_init")]
extern crate std;

/// Installs the standard `tracing-subscriber` bootstrap shared by LiteBox's runner binaries:
/// uptime timestamps, level names, and an `env_var_name`-driven [`tracing_subscriber::EnvFilter`]
/// (`from_env_lossy`, so a missing/invalid value falls back rather than panicking).
///
/// # Panics
///
/// Panics if a global subscriber has already been installed (mirrors
/// `tracing_subscriber::fmt().init()`'s own panic behavior).
#[cfg(feature = "tracing_subscriber_init")]
pub fn init_env_filtered_subscriber(env_var_name: &str) {
    use std::io::IsTerminal as _;
    let inner = tracing_subscriber::fmt()
        .with_timer(tracing_subscriber::fmt::time::uptime())
        .with_level(true)
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(EventStderr::default)
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_env_var(env_var_name)
                .from_env_lossy(),
        )
        .finish();
    tracing::subscriber::set_global_default(PrivateAlloc(inner))
        .expect("a global tracing subscriber was already installed");
}

/// Wraps the fmt subscriber so every call that can touch its per-thread heap state (line buffer,
/// filter scope stack) runs inside a private-allocation scope. See [`set_private_alloc_hook`].
#[cfg(feature = "tracing_subscriber_init")]
struct PrivateAlloc<S>(S);

#[cfg(feature = "tracing_subscriber_init")]
impl<S: tracing::Subscriber + 'static> tracing::Subscriber for PrivateAlloc<S> {
    fn register_callsite(
        &self,
        m: &'static tracing::Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        let _g = crate::PrivateAllocGuard::new();
        self.0.register_callsite(m)
    }
    fn enabled(&self, m: &tracing::Metadata<'_>) -> bool {
        let _g = crate::PrivateAllocGuard::new();
        self.0.enabled(m)
    }
    fn max_level_hint(&self) -> Option<tracing::metadata::LevelFilter> {
        self.0.max_level_hint()
    }
    fn new_span(&self, a: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let _g = crate::PrivateAllocGuard::new();
        self.0.new_span(a)
    }
    fn record(&self, s: &tracing::span::Id, v: &tracing::span::Record<'_>) {
        let _g = crate::PrivateAllocGuard::new();
        self.0.record(s, v);
    }
    fn record_follows_from(&self, s: &tracing::span::Id, f: &tracing::span::Id) {
        self.0.record_follows_from(s, f);
    }
    fn event(&self, e: &tracing::Event<'_>) {
        let _g = crate::PrivateAllocGuard::new();
        self.0.event(e);
    }
    fn enter(&self, s: &tracing::span::Id) {
        let _g = crate::PrivateAllocGuard::new();
        self.0.enter(s);
    }
    fn exit(&self, s: &tracing::span::Id) {
        let _g = crate::PrivateAllocGuard::new();
        self.0.exit(s);
    }
    fn clone_span(&self, s: &tracing::span::Id) -> tracing::span::Id {
        self.0.clone_span(s)
    }
    fn try_close(&self, s: tracing::span::Id) -> bool {
        let _g = crate::PrivateAllocGuard::new();
        self.0.try_close(s)
    }
    unsafe fn downcast_raw(&self, id: core::any::TypeId) -> Option<*const ()> {
        // SAFETY: forwards the same contract to the wrapped subscriber.
        unsafe { self.0.downcast_raw(id) }
    }
}

/// A stderr writer that buffers one formatted log event and emits it as a single `write`.
///
/// `tracing_subscriber::fmt` issues several `Write` calls per event (timestamp, level, target,
/// fields, newline). With guests running as separate host processes sharing one stderr, those
/// pieces from different processes interleave mid-line and a trace becomes unreadable. One
/// `write_all` per event (at most `PIPE_BUF` bytes is atomic on a pipe, and `O_APPEND` files are
/// atomic per call) keeps each event whole.
#[cfg(feature = "tracing_subscriber_init")]
#[derive(Default)]
struct EventStderr(std::vec::Vec<u8>);

#[cfg(feature = "tracing_subscriber_init")]
impl std::io::Write for EventStderr {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(feature = "tracing_subscriber_init")]
impl Drop for EventStderr {
    fn drop(&mut self) {
        if !self.0.is_empty() {
            use std::io::Write as _;
            use std::os::fd::FromRawFd as _;
            // Straight to fd 2 rather than through `std::io::stderr()`: that goes through a
            // process-local lock, which a native-`fork()` child inherits still held whenever a
            // sibling thread of the parent was logging at that instant -- the child then hangs on
            // its first log line. A raw `write` takes no lock and is atomic per event.
            // SAFETY: fd 2 stays open for the whole process; `ManuallyDrop` keeps it that way.
            let mut fd2 = core::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(2) });
            let _ = fd2.write_all(&self.0);
        }
    }
}

impl crate::Level {
    /// Converts this level to the corresponding `tracing::Level`.
    #[doc(hidden)]
    pub const fn to_tracing_level(self) -> tracing::Level {
        match self {
            crate::Level::Error => tracing::Level::ERROR,
            crate::Level::Warn => tracing::Level::WARN,
            crate::Level::Info => tracing::Level::INFO,
            crate::Level::Debug => tracing::Level::DEBUG,
            crate::Level::Trace => tracing::Level::TRACE,
        }
    }
}

/// RAII guard that wraps a tracing span's entered guard.
///
/// This type is returned by span macros (e.g., [`info_span!`](crate::info_span)) when
/// using the `backend_tracing` feature. The span remains "entered" (active) as long
/// as this guard exists. When dropped, the span is exited.
///
/// Unlike the log backend's `SpanGuard`, this provides full tracing semantics
/// including hierarchical span relationships and context propagation.
///
/// # Example
///
/// ```ignore
/// let _guard = info_span!("my_operation");
/// // Span is now entered and active
/// info!("This log is inside the span");
/// // Span exits when _guard goes out of scope
/// ```
pub struct SpanGuard {
    /// The wrapped tracing span guard. Public for macro access but not part of
    /// the public API.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub inner: tracing::span::EnteredSpan,
}

/// Internal macro for tracing backend implementation.
///
/// This macro transforms our unified key-value syntax into tracing's native
/// event syntax. The transformation is handled by [`__tracing_dispatch`].
///
/// Not intended for direct use; called by the public logging macros.
#[doc(hidden)]
#[macro_export]
macro_rules! __log_impl {
    ($level:expr, $($key:ident $(:$cap:tt)? $(= $value:expr)?),+ ; $msg:literal) => {{
        $crate::__tracing_dispatch!(
            [event]
            [$level]
            [$msg]
            []
            [$($key $(:$cap)? $(= $value)?),+]
        )
    }};
    ($level:expr, $msg:literal) => {
        $crate::__private::tracing::event!($crate::Level::to_tracing_level($level), $msg)
    };
}

/// Unified internal macro to dispatch and process key-value pairs for tracing.
///
/// Uses a tt-muncher pattern to transform fields from our unified syntax
/// (e.g., `key:? = value`) into tracing's native syntax (e.g., `key = ?value`).
///
/// The macro processes fields one at a time, accumulating transformed fields
/// until no input remains, then emits the final event or span based on mode.
///
/// Arguments: `[mode] [level] [msg_or_name] [accumulated_fields] [remaining_input]`
///
/// Where `mode` is either `event` or `span`.
#[doc(hidden)]
#[macro_export]
macro_rules! __tracing_dispatch {
    // Field: key:? = value (Debug with explicit value)
    ([$mode:ident] [$level:expr] [$target:tt] [$($acc:tt)*] [$key:ident :? = $value:expr $(, $($rest:tt)*)?]) => {
        $crate::__tracing_dispatch!(
            [$mode] [$level] [$target]
            [$($acc)* $key = ?$value,]
            [$($($rest)*)?]
        )
    };

    // Field: key:debug = value
    ([$mode:ident] [$level:expr] [$target:tt] [$($acc:tt)*] [$key:ident :debug = $value:expr $(, $($rest:tt)*)?]) => {
        $crate::__tracing_dispatch!(
            [$mode] [$level] [$target]
            [$($acc)* $key = ?$value,]
            [$($($rest)*)?]
        )
    };

    // Field: key:% = value (Display with explicit value)
    ([$mode:ident] [$level:expr] [$target:tt] [$($acc:tt)*] [$key:ident :% = $value:expr $(, $($rest:tt)*)?]) => {
        $crate::__tracing_dispatch!(
            [$mode] [$level] [$target]
            [$($acc)* $key = %$value,]
            [$($($rest)*)?]
        )
    };

    // Field: key:display = value
    ([$mode:ident] [$level:expr] [$target:tt] [$($acc:tt)*] [$key:ident :display = $value:expr $(, $($rest:tt)*)?]) => {
        $crate::__tracing_dispatch!(
            [$mode] [$level] [$target]
            [$($acc)* $key = %$value,]
            [$($($rest)*)?]
        )
    };

    // Field: key:err = value (errors use Display)
    ([$mode:ident] [$level:expr] [$target:tt] [$($acc:tt)*] [$key:ident :err = $value:expr $(, $($rest:tt)*)?]) => {
        $crate::__tracing_dispatch!(
            [$mode] [$level] [$target]
            [$($acc)* $key = %$value,]
            [$($($rest)*)?]
        )
    };

    // Field: key:sval = value (fallback to Debug)
    ([$mode:ident] [$level:expr] [$target:tt] [$($acc:tt)*] [$key:ident :sval = $value:expr $(, $($rest:tt)*)?]) => {
        $crate::__tracing_dispatch!(
            [$mode] [$level] [$target]
            [$($acc)* $key = ?$value,]
            [$($($rest)*)?]
        )
    };

    // Field: key:serde = value (fallback to Debug)
    ([$mode:ident] [$level:expr] [$target:tt] [$($acc:tt)*] [$key:ident :serde = $value:expr $(, $($rest:tt)*)?]) => {
        $crate::__tracing_dispatch!(
            [$mode] [$level] [$target]
            [$($acc)* $key = ?$value,]
            [$($($rest)*)?]
        )
    };

    // Field: key = value (no capture mode)
    ([$mode:ident] [$level:expr] [$target:tt] [$($acc:tt)*] [$key:ident = $value:expr $(, $($rest:tt)*)?]) => {
        $crate::__tracing_dispatch!(
            [$mode] [$level] [$target]
            [$($acc)* $key = $value,]
            [$($($rest)*)?]
        )
    };

    // Field: key:cap (shorthand with capture mode) -> delegates to key:cap = key
    ([$mode:ident] [$level:expr] [$target:tt] [$($acc:tt)*] [$key:ident :$cap:tt $(, $($rest:tt)*)?]) => {
        $crate::__tracing_dispatch!(
            [$mode] [$level] [$target]
            [$($acc)*]
            [$key :$cap = $key $(, $($rest)*)?]
        )
    };

    // Field: key (bare identifier) -> delegates to key = key
    ([$mode:ident] [$level:expr] [$target:tt] [$($acc:tt)*] [$key:ident $(, $($rest:tt)*)?]) => {
        $crate::__tracing_dispatch!(
            [$mode] [$level] [$target]
            [$($acc)*]
            [$key = $key $(, $($rest)*)?]
        )
    };

    ([event] [$level:expr] [$msg:literal] [$($acc:tt)*] []) => {
        $crate::__private::tracing::event!($crate::Level::to_tracing_level($level), $($acc)* $msg)
    };
    ([span] [$level:expr] [$name:expr] [$($acc:tt)*] []) => {{
        let span = $crate::__private::tracing::span!($crate::Level::to_tracing_level($level), $name, $($acc)*);
        $crate::SpanGuard { inner: span.entered() }
    }};
}

/// Internal macro for span implementation with tracing backend.
///
/// Creates a tracing span with the given name and fields, enters it, and
/// returns a [`SpanGuard`] wrapping the entered span.
///
/// Not intended for direct use; called by the public span macros.
#[doc(hidden)]
#[macro_export]
macro_rules! __span_impl {
    ($level:expr, $name:expr, $($key:ident $(:$cap:tt)? $(= $value:expr)?),+) => {{
        $crate::__tracing_dispatch!(
            [span]
            [$level]
            [$name]
            []
            [$($key $(:$cap)? $(= $value)?),+]
        )
    }};
    ($level:expr, $name:expr) => {{
        let span = $crate::__private::tracing::span!($crate::Level::to_tracing_level($level), $name,);
        $crate::SpanGuard { inner: span.entered() }
    }};
}
