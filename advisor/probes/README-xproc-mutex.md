# Cross-process mutex probes (ADVISORY-002 Track B, step 2)

Live-verification tooling for
`litebox_platform_windows_userland/src/xproc_sync.rs`, the cross-process mutex that
`ADVISORY-002-d-zero-fork.md` §3.2 identifies as a hard prerequisite for genuine
`D == 0` fork semantics.

These are throwaway diagnostics in this directory's established style, not crate
tests. Compiled `.exe` outputs are deliberately not committed (no probe binary in
this directory is).

## Why the primitive is needed at all

Every native address- or TID-based wait on Windows is process-local *by design*, so
the existing `RawMutex` (built on `WaitOnAddress`/`WakeByAddressSingle`) cannot work
across a real process boundary. `clone_probe.c` had already established the
load-bearing negative result empirically on this host: keyed events do **not**
rendezvous across processes; both sides time out.

The only cross-process wake is therefore through a shared kernel object, which makes
the design forced rather than chosen: an atomic state word in the shared section for
the uncontended fast path, plus a genuine kernel `Event` used only when contended.

## The probes

| probe | what it establishes |
|---|---|
| `xproc_mutex_probe.c` | The protocol itself, under ordinary `CreateProcessW`. Two real processes, a pagefile-backed section mapped into both, 8-16 threads each hammering one lock-protected counter. |
| `xproc_rust_probe/` | The same, against the **actual shipped Rust implementation** rather than a C reimplementation of the protocol. |
| `xproc_rust_probe/src/bin/bench.rs` | Isolated uncontended cost, and a head-to-head against a named kernel `Mutex` object (the simpler alternative design). |
| `xproc_rust_probe/src/bin/guard_check.rs` | That the primitive's two deliberate panics actually fire, and that `try_lock` really excludes. |
| `xproc_mutex_clone_probe.c` | The protocol under `RtlCloneUserProcess`, which is the mechanism Track B step 5 actually intends to use. |

## Building and running

```sh
# C probes (mingw; the `gcc` on PATH may be tcc, which lacks _mm_pause)
x86_64-w64-mingw32-gcc -O2 -o xproc_mutex_probe.exe xproc_mutex_probe.c
x86_64-w64-mingw32-gcc -O2 -o xproc_mutex_clone_probe.exe xproc_mutex_clone_probe.c
./xproc_mutex_probe.exe          # spawns its own child
./xproc_mutex_clone_probe.exe    # clones itself

# Rust probe (its own workspace, so a root `cargo build` never picks it up)
cd xproc_rust_probe && cargo build --release
./target/release/xproc_rust_probe.exe
./target/release/bench.exe
./target/release/guard_check.exe
```

`xproc_mutex_probe.c` is tunable via the environment, which is how the contention
regimes below were produced: `LITEBOX_XPM_THREADS`, `LITEBOX_XPM_ITERS`,
`LITEBOX_XPM_SPIN` (0 disables the spin, forcing the kernel Event path) and
`LITEBOX_XPM_HOLD` (artificially lengthens the critical section). The Rust probe
takes `XPM_THREADS` and `XPM_ITERS`.

## What was measured (Windows 11 10.0.26200, 2026-09-06)

**Correctness.** 43 consecutive passing runs of the Rust probe (up to 24 threads per
process), 25 of the C probe across three contention regimes, and 12 of the clone
probe. Zero mutual-exclusion violations, zero lost wakeups, zero lost updates
throughout.

Two independent invariants are checked on every run, deliberately, because a
counter-only check is weaker than it looks (two racing increments can still total
correctly by luck):

1. A **non-atomic** read-modify-write of a shared counter totals exactly
   `procs * threads * iters`. This is correct only if the lock genuinely excludes.
2. A critical-section occupancy word incremented on entry is observed as exactly `1`
   by every holder.

**Address independence.** Under `CreateProcessW` the same section maps at *different*
addresses in each process (measured: `0x2330_4460_0000` parent vs `0x18B1_E6B0_0000`
child), which is why nothing in the shared word may be keyed by its own address. This
is precisely the failure class `ADVISORY-001` §3N root-caused in glibc's safe-linked
tcache. A bare `AtomicU32` has no such dependence.

Under `RtlCloneUserProcess` the section is at the *same* address in both, by
construction. The design is correct under both, which is what makes it independent of
which copy mechanism Track B eventually adopts.

**Shared, not privatised.** In the clone probe the counter reaching its full expected
total is itself the proof that a section view mapped *before* the clone stays
genuinely shared rather than being copy-on-write privatised. Had it been privatised,
each process would have counted only its own half.

**Performance**, uncontended, single thread, 2M iterations:

| | ns per acquire+release pair |
|---|---|
| `CrossProcessMutex` (this hybrid) | **~5** |
| named kernel `Mutex` object | ~840 (roughly 170x slower) |
| `std::sync::Mutex` (in-process reference, not cross-process) | ~5 |

The kernel-`Mutex` column is why the extra complexity of the hybrid is worth it: it
is the other genuinely correct cross-process design and it is simpler, but every
acquire *and* every release round-trips through the kernel. The hybrid is
indistinguishable from `std::sync::Mutex` on the uncontended path while remaining
correct across a real process boundary.

Contended, two processes hammering one lock: roughly 100-700 ns per pair depending on
thread count and critical-section length. The 200-iteration spin resolves about 99.8%
of contended acquires without entering the kernel (319,666 fast vs 334 blocked out of
320,000 in a representative run); setting `LITEBOX_XPM_SPIN=0` pushes 14,114 of the
same 320,000 through `WaitForSingleObject`, still correct but with far more kernel
work for no benefit.

## Known limitation

The lock is **not robust to holder death** (same as a POSIX non-robust
`pthread_mutex_t` in shared memory, and unlike a kernel `Mutex`, which reports
`WAIT_ABANDONED`). This is a deliberate trade documented in the module itself,
along with the shape of the fix should it ever be needed.
