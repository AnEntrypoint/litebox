// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

use clap::Parser as _;
use litebox_runner_linux_userland::CliArgs;

#[global_allocator]
static ALLOC: litebox_platform_linux_userland::shared_heap::SharedHeap =
    litebox_platform_linux_userland::shared_heap::SharedHeap::new();

fn main() -> anyhow::Result<()> {
    litebox_runner_linux_userland::run(CliArgs::parse())
}
