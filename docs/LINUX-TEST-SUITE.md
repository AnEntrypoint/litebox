# The Linux-host build and test suite (this container) - full notes

Companion to `AGENTS.md`'s "Linux-host test suite" section, which summarizes this file. Written
2026-10-07 against this container: kernel 6.17, **`no /dev/net/tun`**, `CapEff: 0000000000000000`
(no capabilities), ~4.2 GB of 7.8 GB RAM available, `rustc 1.98.0` / `rustfmt 1.9.0` on the
`stable` channel pinned by `rust-toolchain.toml`.

Standing rule behind every fix below: **here a test binary is a HOST process, so a panic, an abort
or an OOM kill ends the whole run, not one guest** -- the same reason guest-reachable code in the
shim answers an errno instead of panicking.

## Linux-host test suite (this container) - separate stream from the Windows harness above

`cargo test` in THIS container (kernel 6.17, **no `/dev/net/tun`, `CapEff: 0000000000000000`**, ~4.2 GB of 7.8 GB). **Here a test binary is a HOST process, so a panic, an abort or an OOM kill ends the whole run, not one guest.** **GREEN: `cargo build`, `cargo check --all-targets` and `cargo clippy --all-targets --all-features` on the DEFAULT MEMBERS (what CI runs), `-p litebox_runner_optee_on_linux_userland --all-targets`, and every suite below.**

- **NOW: `cargo test -p litebox_shim_linux --lib` = `ok. 222 passed; 0 failed; 1 ignored`** (`finished in 47.38s`); `--release` = `221 passed; 0 failed`. **The 222/221 gap is `process.rs`'s `#[cfg(all(target_os = "linux", debug_assertions))]` `test_sigint_with_custom_handler` - intentional.** Also `-p litebox` `69 passed`, `-p litebox_common_linux` `10`, `-p litebox_platform_linux_userland` `3`.
- **A MISSING HOST CAPABILITY IS A SKIP, NEVER A FAILURE OR A HANG** (`syscalls/tests.rs`: `tun_device_available()` opens `/dev/net/tun`, `host_program_available("diod")` scans `$PATH`). Before: the four `test_tun_blocking_*` ran past 60 s and two `transport.rs` 9P tests failed; after: `init_tun_platform` prints `SKIPPED: this host has no usable TUN device` and returns `None`.
- **`LinuxShimBuilder::build` RUNS ON AN 8 MiB THREAD, NOT ON LIBTEST'S 2 MiB** (`tests.rs` `BUILD_STACK_SIZE`): `GlobalState` is built BY VALUE on the stack - 796,024 bytes plus a temporary per big field - so a debug build overflowed and killed the binary. **RULE: a giant stack frame goes on its own thread with a real stack.**
- **THE LINUX ARENA IS RECLAIMED - `create_shared_kernel_state` NOW RECYCLES ITS BLOCKS** (`litebox_platform_linux_userland/src/lib.rs`: thread-local `PENDING_ARENA_BLOCKS` drained at `create`, `ARENA_POOL` re-used at the next `alloc`, `ReclaimableHandle` drops the state BEFORE returning its blocks). One `build` costs ~47.9 MiB: the suite peaked at **5107 MiB / 5322 MiB and was SIGKILLed at 204 of 221**; **after: 232.5 MiB.** Blocks are attributed by "allocated on this thread since the last `create`" - sound because every `shared_kernel_arena_alloc_bytes` caller is a fixed-capacity table in `build`'s `GlobalState { .. }` literal (the only other caller is `Network::new` in `litebox/src/net/tests.rs`).
- **A LOCK TABLE WITH NO MEMORY TO HOLD IT DEGRADES, NEVER PANICS** (`file.rs`): `SharedFlockTable`/`SharedRecordLockTable` hold `Option<&Region>`; when neither the shared arena (exhausted) nor the process heap (refused) can hold the region, `new_in(None, _)` yields a table taking the "cannot key this lock" path `excludes == false` already took - `lock`/`apply` `None`, `unlock` false, `conflicting_claim` `Unavailable`, `release_process` a no-op, `region_addr` 0 (which `FlockHolderInner` already reads as "nothing to release"). Both `.expect("fallback allocation failed")` are gone. **Proved by `file.rs::degraded_lock_table_tests` (2 tests), each with a CONTROL on a live table so its `None`s cannot be a mis-shaped request.**
- **THREE EXPECTATIONS WERE WRONG, NOT THE SHIM - each checked against REAL LINUX HERE**: `getcwd` has NO trailing slash except `/` -> `file.rs` `"/test_chdir_dir"`/`"/rel_parent/rel_child"`/`"/rel_parent"`; a write to a pty slave whose master is closed is **EIO, not EPIPE** -> `pty.rs` `master_close_surfaces_eio_on_slave_write`. Before: `217 passed; 3 failed`.
- **THREE CRATES' BUILDS WERE BROKEN (two stopped `cargo test` entirely)**, all the same class - a struct literal outlived by a new field: `E0063: missing field 'owner' in initializer of 'RawMutex'` (`litebox_platform_linux_userland/src/lib.rs`, fixed with `super::RawMutex::new()`), 29 x `MockPlatform: SystemInfoProvider is not satisfied` (`litebox/src/net/tests.rs`; the mock now implements only the two methods with no default: `get_syscall_entry_point` -> 0, `get_vdso_address` -> `None`), and `E0063: missing field 'symlink_target'` in two `litebox_packager` `TarEntry` literals - both name a regular file just read with `std::fs::read`, so `None`.
- **`cargo test --release` DID NOT COMPILE**: `run_test_thread` was `#[cfg(debug_assertions)]` on the `ThreadProvider` trait default and all three platform impls, so every `cfg(test)` caller in a release build hit E0576. **RULE: a trait method that `cfg(test)` code in a DEPENDENT crate calls must not be gated on `debug_assertions`.**
- **`mm::tests::test_vmm_mapping`'s growth expectation was stale (ONE-DIRECTIONAL)**: `resize_mapping` folds a grown mapping's delta back into the allocation's OWN entry, so 2 entries, not the 3 asserted. **Distinct private allocations still never coalesce** - what `VmArea`'s never-equal `PartialEq` buys.
- **TWO HOST GAPS HAD TO BE CLOSED WITH SUDO before `cargo test --workspace` could build at all** (plain `apt-get` fails: `E: Could not open lock file /var/lib/dpkg/lock-frontend - open (13: Permission denied)`): `libssl-dev` + `pkg-config` (`openssl-sys v0.9.116`: `Could not find directory of OpenSSL installation`, reached by `litebox_packager` -> `oci-client` -> `reqwest` -> `native-tls`) and `libclang-dev` (bindgen in `litebox_platform_linux_kernel`: `Unable to find libclang`); run `sudo -n apt-get update` first (a stale index 404s).
- **`cargo check --workspace` (ALL members) FAILS HERE BY DESIGN: `litebox_runner_lvbs` `error[E0152]: found duplicate lang item panic_impl`, and `litebox_runner_snp`.** Both are excluded from `default-members` because they need a custom target (`litebox_runner_lvbs/x86_64_vtl1.json`, `litebox_runner_snp/target.json`) plus `-Z build-std` and a nightly. **Never "fix" them for the host target; CI builds them in their own jobs.**
- **A BINARY OR EXAMPLE IN `default-members` MUST COMPILE ON EVERY TARGET, EVEN IF IT CAN ONLY RUN ON ONE.** `litebox_presenter` (5 x E0432/E0433) and `litebox_platform_windows_userland/examples/presenter_{bench,smoke}` broke `cargo build`/`--all-targets` on Linux because `litebox_presenter_protocol` and the WHOLE `litebox_platform_windows_userland` crate are `#![cfg(all(target_os = "windows", target_arch = "x86_64"))]` - an empty crate and no `presentation` module here. **Fix = a TARGET DISPATCHER**: `litebox_presenter/src/main.rs` dispatches to `mod win` (the real program, moved to `src/win.rs`) and each example wraps its body in `mod imp`, so the Windows body compiles only where it could run and every other target gets a `main` that says so (`./target/debug/litebox-presenter` -> that message, rc=1).
- **`cargo fmt --all -- --check` IS RED HERE ON 209 PRE-EXISTING HUNKS in ~30 untouched files**: `rust-toolchain.toml` pins only `channel = "stable"`, and this container's stable (rustc 1.98 / rustfmt 1.9.0) restyles `let Some(x) = y else { continue };` onto several lines and expands one-line struct literals. **NEVER run `cargo fmt --all`** - it rewrites ~30 unrelated files into this version's style. Format only your own hunks, matching the diff `cargo fmt --all -- --check` shows.


## `dev_tests`: the hygiene crate that gates the whole suite

`cargo test` builds and runs crates in dependency order and STOPS at the first failing one, so
`dev_tests` -- a 4-test crate of repo-hygiene checks -- decides whether any other suite runs at all.
It was red here on 3 of its 4 tests, all drift accumulated by earlier sessions, never by one change.
**NOW: `cargo test -p dev_tests --lib` = `ok. 4 passed; 0 failed`.**

- **`boilerplate::copyright_header` REQUIRES A HEADER ON EVERY NON-GITIGNORED FILE IN THE TREE**, by
  extension (`dev_tests/src/boilerplate.rs` `HEADERS_REQUIRED_PREFIX`), and the walker is
  gitignore-based, so a file committed under `advisor/`, `tools/`, `docs/` or `dev_tools/` is
  checked like any source file. It was failing on 144 files (131 missing a header, 7 extension-less,
  6 of an extension the table did not know). **RULE: any file added to this repo needs its
  extension's header, or a `SKIP_FILES` entry with a reason.**
- The test can write the headers itself: **`AUTO_INCLUDE_HEADERS=1 cargo test -p dev_tests --lib
  boilerplate`** prepends the required prefix, but it (a) still bails at the end, (b) REFUSES any
  file whose text already mentions "opyright"/"icensed", and (c) does NOT replace an existing
  shebang -- it prepends `#! /bin/bash` above a file's own `#!/bin/sh`, which silently changes the
  interpreter. So the 131 were written by hand: the mandated prefix REPLACES line 1 when line 1 is a
  shebang (62 files: 45 `#!/bin/sh`, 11 `#!/usr/bin/env python3`, 4 `#!/bin/bash`,
  2 `#!/usr/bin/bash`), and is otherwise prepended. The `.sh` files now run under bash, as the
  mandated prefix dictates.
- The residuals went into the tables: `mjs` gets the `js` header; `xml`/`plist`/`gz` get `""`
  (no header required, like `json`/`txt`); and `SKIP_FILES` gained the 4 compiled ELF probe binaries
  under `advisor/probes/` (build output beside their `.c`, like the existing `test-bins` entries),
  the 3 extension-less s6 service files in `tools/webtop/custom-services.d/` (s6 only scans
  extension-less names there), and one vendored minified front-end bundle that already mentions
  licensing.
- **`ratchet::{globals,transmutes,maybe_uninit}` COUNT LINES MATCHING A HEURISTIC PER DIRECTORY
  PREFIX, AND THE COUNTS ARE HARDCODED** (`dev_tests/src/ratchet.rs`). A count that GREW fails the
  test; so does any `.rs` file with a non-zero count that no prefix covers. Both were true here:
  uncovered `litebox_util_log/`, `litebox_runner_linux_userland/`,
  `litebox_runner_linux_on_windows_userland/`; and `litebox/` 8 -> 25 transmutes, `litebox/` 10 -> 45,
  `litebox_platform_linux_userland/` 9 -> 20, `litebox_platform_windows_userland/` 19 -> 112,
  `litebox_shim_linux/` 1 -> 35 globals. **RULE: bump the number and WRITE WHY, in the comment above
  the entry, naming the files the new hits are in** -- the file already does this for every earlier
  bump. Do not shrink real code to satisfy a ratchet.
- **THE RATCHETS COUNT THIS FILE'S OWN COMMENTS TOO** (`dev_tests/` is itself a prefix), so a
  justification comment must not spell out another ratchet's keyword: writing "`MaybeUninit`" in a
  transmutes comment moved `ratchet_maybe_uninit` from 1 to 2. Say what the code does instead.
- Measure a count the way the test measures it before changing it: line-based, `//`-stripped for
  transmutes, and `static `/`pub... static ` at line start for globals (a `thread_local!` counts as
  a global too).

## Second pass: signal delivery, glibc's `termios2`, and three harness traps

Everything above was green when this pass started; what followed were the failures that only
surfaced once the suite could actually run. Each is recorded here with the mechanism and the
proof, because each one looks like a guest bug and is not.

### 1. Every signal delivered through the synthesized sigreturn trampoline hung

`litebox_platform_linux_userland`'s fault path (`exception_signal_handler` ->
`signal_handler_exit_guest` -> `copy_signal_context` -> `set_signal_return`) hands control to the
guest's handler by pushing a synthesized sigreturn trampoline: a fresh `mmap` whose **offset 0**
is `mov eax, SYS_rt_sigreturn; syscall`. So the guest returns to `rip == mmap_base`.

The host's SIGSEGV/SIGTRAP handler has to tell a real fault from the `ICEBP;HLT` (`F1 F4`) pair
`litebox_syscall_rewriter` emits at unpatched syscall sites, and it does that by reading
`*(rip - 1)`. **That read is only safe when `rip - 1` sits on the same page as `rip`** -- the one
page already known to be mapped, since fetching (or nearly fetching) from it is what trapped. A
fault whose RIP sits at offset 0 of its page reads into whatever precedes that page, which is
very often not mapped at all, so the probe takes a **second fault inside the handler, with
SIGSEGV blocked**: the nested fault can neither be handled nor reported, and the thread is simply
lost.

That is exactly where the trampoline sits, so *every* delivery through it died this way --
live-caught as the guest handler running and returning, then a hard hang with RSS climbing past
1.7 GB and no further fault, syscall or log line. `sys_rt_sigreturn` was never reached, because
the probe read `rip - 1` and nothing after it ever ran.

Fix (`litebox_platform_linux_userland/src/lib.rs`, `is_syscall_trap`):

```rust
let is_syscall_trap = rip >= 1
    && rip & 0xfff != 0
    && unsafe { *(rip.wrapping_sub(1) as *const u8) == 0xF1 && *(rip as *const u8) == 0xF4 };
```

The only case this now declines is an `ICEBP;HLT` pair split across a page boundary (`F1` the
last byte of one page, `F4` the first byte of the next), which the rewriter never emits -- it
writes the pair whole.

**BOTH WAYS**, by a guest that installs handlers for several signals and prints from them
(`/tmp/p4.c`): before, the run hangs after the handler's first line; after, it prints
`A: pause returned` and exits 0.

### 2. glibc 2.42+ `tcgetattr` is `TCGETS2` (`0x802c_542a`), not `TCGETS` (`0x5401`)

`test_runner_with_python_repl_pty` was failing with **zero bytes** of output -- not a partial
banner, not a traceback. A guest `python3` under a pty never entered interactive mode, which
traces to `isatty()`, which is `tcgetattr(fd, &t) == 0`.

The decisive pair of probes: a raw `syscall(SYS_ioctl, fd, 0x5401 /* TCGETS */, buf)` on the same
pty returned **0**, while glibc's `tcgetattr` returned **-1 / EINVAL (22)**. Disassembling this
container's glibc shows why -- `tcgetattr` no longer issues `TCGETS`; it issues a `syscall` with
`esi = 0x802c542a` and a **44-byte `struct termios2`**:

```
kernel termios2 (44): iflag oflag cflag lflag | c_line @16 | c_cc[19] @17..35 | c_ispeed @36 | c_ospeed @40
glibc termios  (60): iflag oflag cflag lflag | c_line @16 | c_cc[32] @17..48 | c_ispeed @52 | c_ospeed @56
```

The shim only knew `TCGETS`/`TCSETS`/`TCSETSW`/`TCSETSF`, so `TCGETS2` fell through the
`Sysno::ioctl` decode as an unrecognized command.

Fix, end to end:

- `litebox_common_linux/src/lib.rs`: `#[repr(C)] struct Termios2` (the 44-byte kernel layout),
  `TCGETS2`/`TCSETS2`/`TCSETSW2`/`TCSETSF2`, `CBAUD`/`CBAUDEX` plus `baud_from_cflag()`
  (index = `c_cflag & CBAUD`, `+15` when `CBAUDEX` is set, over Linux's 31-entry `baud_table`),
  `impl From<Termios> for Termios2` (re-derives `c_ispeed`/`c_ospeed` from `c_cflag`, which the
  shim's stored 36-byte `Termios` does not carry) and `impl From<Termios2> for Termios` (drops
  them), and four new `IoctlArg` variants with their decode arms.
- `litebox_shim_linux/src/syscalls/file.rs`: `stdio_ioctl` and `pty_ioctl` serve the four new
  commands against the same stored `TermiosState`, and `sys_ioctl`'s dispatch group routes them
  to the tty/pty path.

**BOTH WAYS**: guest `tcgetattr` errno 22 -> errno 0 on the same pty; `test_runner_with_python_repl_pty`
FAIL -> ok.

**Trap while writing it**: `stdio_ioctl`'s existing arm is the three-variant or-pattern
`TCSETS | TCSETSW | TCSETSF`. My first edit collapsed it to `TCSETSF` alone, so `TCSETS`/`TCSETSW`
fell to the ENOTTY arm and `file.rs::tcsetsw_and_tcsetsf_round_trip_through_tcgets` went red
(`left: Err(Errno(25))`). The or-pattern is load-bearing.

### 3. `tests/efault.c` did not compile

`test_dynamic_lib_with_rewriter` / `test_static_exec_with_rewriter` failed with
`failed to compile: … error: implicit declaration of function 'abort'`. This container's gcc
rejects implicit declarations; `execve.c` and `thread_exit.c` already had `#include <stdlib.h>`
and `efault.c` did not. One line.

### 4. `tests/loader.rs` shares the 8 MiB build thread

`test_load_exec_dynamic` aborted the whole `loader` binary with
`fatal runtime error: stack overflow` -- which discards every other test's result with it. Same
cause as `syscalls/tests.rs`: `LinuxShimBuilder::build` constructs `GlobalState` **by value on
the stack**, and libtest's test threads get 2 MiB while a real runner's main thread gets 8. The
construction now runs on an explicitly-sized thread (`BUILD_STACK_SIZE = 8 << 20`) and the
built shim is moved back; `tests/loader.rs` is 3 passed.

### 5. The pty helpers' deadline is 60 s, not 10

`tests/common/pty.rs`'s `wait_for_output` and `wait_for_child_exit` waited on a guest python that
has to be rewritten, loaded and started **while libtest runs the rest of the suite on 4 cores**.
Measured: 4.7 s alone, and a timeout with zero bytes whenever three other runners were resident.
A deadline too short for the host is not an assertion about the guest, so it cannot be what
decides the test. Both now use a shared `const DEADLINE = Duration::from_secs(60)`.

### 6. Host RAM decides this suite -- the `signal: 11` is the runner, not the guest

`test_dynamic_lib_with_rewriter` / `test_static_exec_with_rewriter` intermittently died
`signal: 11 (SIGSEGV) (core dumped)`. Ruled out, one at a time:

- not a guest program -- all 28 C files under `tests/` pass sequentially;
- not concurrency in the guest -- 6 concurrent signal runners x 3 rounds, rc=0 every time;
  4 mixed programs x 4 rounds, rc=0;
- not an address-space limit -- `ulimit -v` 4 GB / 2 GB / 1 GB, rc=0 every time;
- not the test -- both pass alone (267 s and 303 s).

What was actually happening: `ps` showed **167 chromium processes** from the foreign `queue.mjs`
chrome storm (not mine to kill), with `MemAvailable` down to ~1.27 GB, plus three leftover
runner processes of mine. Once `MemAvailable` recovered to 3.2-4.3 GB the suite passed 12/12
twice. **A runner is a host process; under host memory pressure it is the first thing the kernel
takes. Check host RAM before believing a runner SIGSEGV.**

### Final state

`cargo test --no-fail-fast` on the default members: **58 targets ok, 0 failed**, including
`-p litebox_shim_linux --lib` 222 passed / 0 failed / 1 ignored,
`-p litebox_runner_linux_userland --test run` 12 passed, `--test loader` 3 passed,
`-p litebox` 69, `-p litebox_common_linux` 10, `-p litebox_platform_linux_userland` 3,
`-p litebox_shim_optee` 10. `cargo build`, `cargo check --all-targets` and
`cargo clippy --all-targets --all-features` all exit 0.

## Third pass: the merge brought a `dev_tests` ratchet failure

After `origin/main` (and `origin/inetfix`) were merged in, `cargo test --no-fail-fast` reported
**`error: 1 target failed: -p dev_tests --lib`** while every other target stayed green.

`dev_tests` is not a guest test at all - it is a **source ratchet**: `dev_tests/src/ratchet.rs`
counts, per crate prefix, the lines matching three heuristics (`transmute`, a line-initial
`static`, `MaybeUninit`) and fails when a count **increases**. Upstream's two new commits had
added one function-local `static` each:

- `litebox/src/net/mod.rs` - `static TICKS: AtomicU32` inside `reclaim_orphaned_listen_queues`
  (the 1-in-512 orphaned-listen-queue sweep) -> `litebox/` 45 -> 46;
- `litebox_platform_windows_userland/src/lib.rs` - `static UNHANDLED: AtomicU32`, throttling the
  `error!` for an exception code the handler does not enumerate -> 112 -> 113.

Two different fixes, chosen on the shape of each site:

- **`TICKS` became a field.** `reclaim_orphaned_listen_queues(&mut self)` already had `&mut self`,
  so the counter belongs on `Network` (`reclaim_tick: u32`, initialized to 0 in `Network::new`),
  which is exactly the shared per-fork-family object the sweep runs over - and being shared is
  better here, since one process's ticks are then every process's. No `static`, no ratchet bump;
  `litebox/` stays at 45.
- **`UNHANDLED` stayed a `static` and the ratchet was bumped to 113.** That arm is inside a free
  function in the exception handler with no `&self` to hang a field on, and the throttle is
  mandatory by this repo's own standing rule (a guest looping on a trap must not bury the log).
  The bump carries a comment, matching how every earlier bump in that file is justified.

**RULE: `dev_tests` reads the SOURCE, not the compiled crate, so it fails on a `cfg`-gated file
too - `litebox_platform_windows_userland` is empty on this host and still counted. Adding a
`static` anywhere under a listed prefix needs a field instead where one is available, or a
justified bump in the same commit.**

## Fourth pass: the tick counter that made an orphan sweep retire a LIVE port

Symptom, after that third pass: `cargo test -p litebox --lib` never finished. Re-run with
`-- --test-threads=1`, the last line was

```
test net::tests::test_bidirectional_tcp_communication_manual ...
```

with one thread at 100% CPU and no `ok`. That test ends in an **unbounded `accept` loop**
(`Err(AcceptError::NoConnectionsReady) => {}`), so a hang there means the port was armed and then
stopped being reachable.

### Mechanism: "no referent recorded" was read as "no LIVE referent"

`Network::reclaim_orphaned_listen_queues` (the 1-in-512 sweep that frees a row whose referents all
died without closing, since a killed process never spends its `refs`) counts referents that are
still alive and retires the row when that count is 0. Its filter drops pid 0:

```rust
let live = remaining.iter().filter(|pid| **pid != 0 && (**pid == me || is_process_alive(**pid))).count();
if live > 0 { continue; }
... self.retire_listen_queue(index);
```

`ListenQueue::record_referent` declines pid 0 as well, and `SystemInfoProvider::current_pid`
returns **0** -- the documented "unknown", and this crate's own `MockPlatform` (its
`SystemInfoProvider` impl only overrides the two methods with no default). So for a port a test
had just armed, `ref_pids` was all zeros, `live` was 0, and the row was retired: `retire_listen_queue`
tears the armed backlog out of the shared socket set. A listening socket destroyed with nothing
ever closed -- the exact deaf-port shape the sweep exists to clean up after.

### Why it only appeared now

The counter was a function-local `static TICKS` shared by every `Network` **in the process**, so
only the first `Network` to tick ever swept on count 0; the third pass turned it into the
`Network::reclaim_tick` field, so now **every** `Network` sweeps on its first tick. In the manual
test the first tick is the explicit `comms()` right after `listen()` + `connect()` -- `Manual`
mode makes `automated_platform_interaction` a no-op, so no earlier call consumes count 0. That is
why `_default` and `_automatic` passed: their first tick lands before `listen()` ever arms a row.
**A per-instance tick counter means tick 0 of every instance sweeps, so every invariant here has
to hold on an instance's first tick, not just on some process's.**

### The fix

A row that records **no** referent pid is a row with no liveness information, not a row with a dead
referent, so the sweep skips it:

```rust
if !remaining.iter().any(|pid| *pid != 0) { continue; }
```

### Both ways

- **With** the guard: `cargo test -p litebox --lib -- net::tests::` = 5 passed, including
  `test_bidirectional_tcp_communication_manual` (0.01 s).
- **Without** it (the condition temporarily `&& !cfg!(all())`): the new
  `test_reclaim_orphaned_listen_queue_needs_a_known_dead_referent` **FAILED** at its first arm
  (`a port nobody closed must stay armed even when this platform cannot name a referent`), and the
  manual test was **still running after 60 s** and had to be killed (exit 124).

That new test carries all three arms in one binary, so the guard cannot decay into "never sweep":
no referent recorded -> stays armed; a live referent -> stays armed; **every referent
`mark_dead` on the mock platform -> the row is reclaimed.** The third arm needed a way to make a
pid dead, which `MockPlatform` had no answer for (`is_process_alive` defaults to `true`): it now
has a `dead_pids: RwLock<Vec<u32>>` **field** and a `mark_dead()` helper -- a field, not a
`static`, so `dev_tests`' ratchet does not move.

**RULE: a sweep over shared state may only reclaim what it has EVIDENCE about -- "no information"
is never "evidence of death". A pid a platform cannot name (0) is unknown, and every other reader
of it declines to act rather than guessing.**

### State after this pass

`cargo test --no-fail-fast` = **58 targets ok, 0 failed**; `-p litebox --lib` = **70 passed**
(69 + the new test; `cargo test --release -p litebox --lib` = 69, the debug-only one being the
usual `debug_assertions` gate). `cargo build`, `cargo check --all-targets` and
`cargo clippy --all-targets --all-features` all exit 0.

**TRAP: `cargo test ... 2>&1 | tail -40; echo "TEST=$?"` reports `tail`'s status, not cargo's.**
The first run of this merged tree printed `TEST=0` and `EXIT=0` while the real result was
`error: 1 target failed`. Use `${PIPESTATUS[0]}` (or write rc to the log without a pipe) whenever
a suite's verdict is piped through `tail`.
