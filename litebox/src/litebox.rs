// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! A module to house all the code for the top-level [`LiteBox`] object.

use alloc::sync::Arc;

use crate::{
    fd::Descriptors,
    sync::{RawSyncPrimitivesProvider, RwLock},
};

/// A full LiteBox system.
///
/// This manages most of the "global" state within LiteBox, and is often a necessary component to
/// initialize many of LiteBox's subsystems.
///
/// For now, we assume that synchronization support (and the ability to exit) is a hard requirement
/// in every LiteBox based system. In the future, this may be relaxed. Other requirements from the
/// platform are dependent on the particular subsystems.
///
/// **2026-09-17 create-vs-attach note**: `x` stays a plain `Arc` here, deliberately NOT threaded
/// through `litebox::platform::SharedKernelStateProvider` the way
/// `litebox_shim_linux::GlobalState` now is. This crate is the shared, platform-generic base for
/// EVERY runner (Linux native, macOS, optee, snp, lvbs, and the Windows userland cross-process
/// fork target), and adding that bound to `LiteBox<Platform>` itself forces `Platform:
/// SharedKernelStateProvider` onto every generic `Platform: RawSyncPrimitivesProvider` bound
/// throughout this crate that touches `LiteBox` (confirmed live: over 200 downstream
/// `cargo check` errors across `fs/`, `mm/`, `net/`, `pipes.rs`, ...) -- a correctness-neutral
/// (every real platform already implements the trivial default) but very wide mechanical
/// propagation, out of scope for this pass. `LiteBoxX::descriptors` (the shim-wide open-file-
/// description table) therefore stays a per-process-fresh `Arc` even for a
/// `LITEBOX_PROCESS_FORK=1` cross-process fork child; `litebox_shim_linux::GlobalState` (its own,
/// far more contained crate) is where the real create-vs-attach wiring landed instead -- see that
/// crate's `GlobalStateHandle`/`LinuxShimBuilder::build` for the live mechanism and
/// `docs/AGENTS_ARCHIVE_2026-09-17.md` for the full scoping rationale. The
/// `SharedKernelStateProvider`/`SharedKernelStateSlot::LiteBoxX` trait/slot already exist in
/// `crate::platform` for a follow-up pass that wants to take on this wider propagation.
pub struct LiteBox<Platform: RawSyncPrimitivesProvider> {
    pub(crate) x: Arc<LiteBoxX<Platform>>,
}

impl<Platform: RawSyncPrimitivesProvider> LiteBox<Platform> {
    /// Create a new (empty) [`LiteBox`] instance for the given `platform`.
    ///
    /// # Panics
    ///
    /// If the `enforce_singleton_litebox_instance` compilation feature has been enabled, and more
    /// than one instance is made, will panic.
    pub fn new(platform: &'static Platform) -> Self {
        // This check ensures that there is exactly one `LiteBox` instance in the process.
        //
        // LiteBox itself supports having multiple instances (and subsystems correctly make any
        // necessary references to each other correctly, as long as you don't initialize them from
        // _different_ `LiteBox` instances and expect them to automatically work together).
        //
        // However, to ensure that the above nicety is maintained (and due to necessity for some
        // shims), it is helpful to check that there is exactly one singleton `LiteBox` instance.
        //
        // You can choose simply not use this feature if you wish to have multiple `LiteBox`
        // instances, but then you might need to be a little bit more careful as to tracking the
        // instances that are made, rather than being able to maintain a convenient global `LiteBox`
        // instance.
        //
        // Related: #24 would allow for things to become cleaner _internal_ to LiteBox, which
        // reduces the potential footguns for users who do not enable this feature.
        #[cfg(feature = "enforce_singleton_litebox_instance")]
        {
            static LITEBOX_SINGLETON_INITIALIZED: core::sync::atomic::AtomicBool =
                core::sync::atomic::AtomicBool::new(false);

            let previously_initialized =
                LITEBOX_SINGLETON_INITIALIZED.fetch_or(true, core::sync::atomic::Ordering::SeqCst);
            assert!(
                !previously_initialized,
                "In this configuration, there should be only one LiteBox instance ever made.  Failing to make second instance.",
            );
        }

        // Enable lock tracing, using this platform for time keeping and debug
        // prints, if the feature is enabled.
        #[cfg(feature = "lock_tracing")]
        crate::sync::lock_tracing::LockTracker::init(platform);

        let descriptors = RwLock::new(Descriptors::new_from_litebox_creation());

        litebox_util_log::trace!("LiteBox instance initialized");

        Self {
            x: Arc::new(LiteBoxX {
                platform,
                descriptors,
            }),
        }
    }
}

impl<Platform: RawSyncPrimitivesProvider> LiteBox<Platform> {
    /// Clones the handle (a cheap `Arc::clone`, same underlying `LiteBoxX`) -- deliberately not
    /// the ordinary `Clone` trait, to keep this call site-visible/greppable rather than an
    /// implicit `.clone()` a reader could mistake for a real duplication. `pub`, not
    /// `pub(crate)`: `litebox_shim_linux::GlobalStateHandle` legitimately needs its OWN clone of
    /// THIS process's `LiteBox` handle alongside the (possibly cross-process-shared)
    /// `GlobalState` it wraps -- see that struct's doc comment and this struct's own
    /// "2026-09-17 create-vs-attach note" above for why `LiteBox` itself must stay a plain,
    /// per-process `Arc`, never routed through `SharedKernelStateProvider`. Still deliberately
    /// not a blanket `#[derive(Clone)]`: external users outside this trust boundary should keep
    /// constructing a `LiteBox` only via [`Self::new`].
    pub fn clone(&self) -> Self {
        Self {
            x: Arc::clone(&self.x),
        }
    }

    /// Access to the file descriptor table.
    ///
    /// Note: this takes a lock, and thus should ideally not be held on to for too long to prevent
    /// potential deadlocks.
    pub fn descriptor_table(
        &self,
    ) -> impl core::ops::Deref<Target = Descriptors<Platform>> + use<'_, Platform> {
        self.x.descriptors.read()
    }

    /// Mutable access to the file descriptor table.
    ///
    /// Note: this takes a lock, and thus should ideally not be held on to for too long to prevent
    /// potential deadlocks.
    pub fn descriptor_table_mut(
        &self,
    ) -> impl core::ops::DerefMut<Target = Descriptors<Platform>> + use<'_, Platform> {
        self.x.descriptors.write()
    }
}

/// The actual body of [`LiteBox`], containing any components that might be shared.
pub(crate) struct LiteBoxX<Platform: RawSyncPrimitivesProvider> {
    pub(crate) platform: &'static Platform,
    descriptors: RwLock<Platform, Descriptors<Platform>>,
}
