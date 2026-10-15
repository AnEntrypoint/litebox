// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! # LiteBox
//!
//! > A security-focused library OS.
//!
//! LiteBox exposes a [`nix`](https://docs.rs/nix)/[`rustix`](https://docs.rs/rustix)-like interface
//! "above" when it is provided a `Platform` interface "below".
//!
//! To use LiteBox, you must provide a type that implements the [`platform::Provider`] trait; then,
//! one obtains a Rust-friendly POSIX-like interface (i.e., "nix-like" interface) via the rest of
//! the modules in this crate.

#![allow(
    deprecated,
    reason = "fetch_update: try_update is nightly-only on the toolchains that build the no_std and LVBS jobs, and fetch_update is what the rest accept"
)]
#![no_std]

extern crate alloc;

pub mod event;
pub mod fd;
pub mod fs;
pub mod mm;
pub mod net;
pub mod path;
pub mod pipes;
pub mod platform;
pub mod shim;
pub mod sync;
pub mod tls;

// The core [`LiteBox`] object itself, re-exported here publicly, just to keep management of the
// code cleaner.
mod litebox;
pub use litebox::LiteBox;

// Explicitly-private, the utilities are not exposed to users of LiteBox, and are intended entirely
// to contain implementation-internal code.
mod utilities;

// Public utilities that might be used in other LiteBox crates.
pub mod utils;
