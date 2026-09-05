// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Shared helpers for LiteBox's dev-only tooling crates (`dev_bench`, `dev_tests`).

use std::path::PathBuf;

use anyhow::{Result, anyhow};

/// Walks up from the current directory to find the workspace root (identified by a `target`
/// directory), then `chdir`s into it.
pub fn project_root() -> Result<PathBuf> {
    let mut dir = std::env::current_dir().ok().unwrap();
    loop {
        if dir.join("target").is_dir() {
            std::env::set_current_dir(&dir)?;
            eprintln!("Changed working directory to project root: {}", dir.display());
            return Ok(dir);
        }
        if !dir.pop() {
            return Err(anyhow!("Could not find project root"));
        }
    }
}
