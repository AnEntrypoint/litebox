# AGENTS.md archive — detail drained 2026-09-10

`AGENTS.md` crossed its 30KB compaction threshold. This file holds the reference-grade detail that
was drained out of it so the main document could stay a current-state summary with one-line facts and
pointers. Nothing here is historical narrative for its own sake — each section is the detail a
session actually needs once it starts touching that subsystem, and `AGENTS.md` points at it by name.

Everything below carries its proving commit sha or `file:line`. The older pass-by-pass narrative
lives in `docs/AGENTS_ARCHIVE_2026-09-03.md` and `docs/AGENTS_ARCHIVE_2026-09-05.md`.

## Cross-process fork: what crosses the fd boundary, and the exact deviations

`try_cross_process_fork` lives at `litebox_shim_linux/src/syscalls/process.rs:2524-2814`. Each fd
kind is carried by a different mechanism, and each carries a deviation from real `fork()` semantics
that matters when a guest depends on sharing rather than copying:

- **Pipes** bridge over a real inheritable Windows pipe. The sender half landed first (`5af4208`),
  the receiver half after (`021951e`), gated on `ForkPipeBridge::owners() == 1` — i.e. the parent
  must have let go of its own copy. `617da12` added the descriptor-independent pipe-end handles and
  the shim API over them that this relies on.
- **Regular files** are reopened in the child at the recorded path and seek offset, with creation
  flags stripped. **The offset is copied, not shared** — real `fork()` shares the file position, so
  a guest where parent and child both advance one descriptor will diverge here. `47e9c2e`, which
  also fixed `F_DUPFD` losing an fd's recorded path (without the path there is nothing to reopen).
- **eventfds** are recreated with the **counter value copied, not shared** (`b65b2ff`). This took
  eventfd-blocked forks from 12 of 23 to 0 of 34.
- **Close-on-exec fds** are dropped rather than refusing the fork (`ed74c28`, which took refusals
  from 34 of 34 down to 5 of 34). The deviation: a child that uses a CLOEXEC fd *before* calling
  `execve` gets `EBADF`.
- **Unix sockets** are the one remaining blocking kind on a real `debian-xfce` boot. There is no
  shared AF_UNIX namespace between guest processes, so there is nothing to carry.

Child bootstrap:

- The parent serializes its whole writable filesystem layer to a temp tar per fork and the child
  imports it (`edbe5e9`), measured at roughly 1.4ms per MB.
- An OCI-booted child receives the image *reference* plus pre-resolved layer digests, not a
  materialised rootfs (`780a85c`, which also fixed a cross-process child on the `--oci-image` path
  having no rootfs at all).
- `LITEBOX_PUBLISH` is blanked in children (`780a85c`) — without it every child raced to re-bind the
  host's published ports.
- `spawn_suspended()` gained `CREATE_NO_WINDOW` (`fe421ac`); before that, every cross-process child
  flashed a console window.

Overrides and gates worth knowing: `LITEBOX_PROCESS_FORK` enables the path at all (read at the first
statement of `spawn_cross_process_fork_child`,
`litebox_platform_windows_userland/src/lib.rs:9758`); `LITEBOX_PROCESS_FORK_IGNORE_FDS` overrides the
fd-kind eligibility scan (`process.rs:2639-2798`); `THREAD_BASED_FORK_ONLY` (`process.rs:2560`,
`564af3f`) excludes `Xvfb` and `dbus-daemon` by `comm`, because a live unix listening socket cannot
be served from a fork-time filesystem snapshot — when it was allowed, the measured outcome was
`XVFB_FAILED`.

## Cross-process fork: the per-fork cost measurement history

The original symptom was roughly 3.5-5 seconds of overhead per fork even with every OCI layer at
`[cache] HIT`. Three stacked fixes, each independently measured:

1. `1199ab6` — the runtime OCI path was fetching and parsing the image-config blob that nothing
   reads. Removing it took a fork from ~4.7s to ~4.1s.
2. `cc7986f` — the child was re-running `pull_image_manifest`, an unconditional HTTPS round-trip
   costing 2.2-3.1s, which was 99%+ of the remaining per-fork cost. Passing pre-resolved digests
   through `FORK_CHILD_OCI_LAYER_DIGESTS_ENV_VAR` skips it: ~4.1s to ~1.2s per fork, 10 forks in
   12.2s. For scale, the 17-layer cache-check loop that runs after the manifest fetch is 3-15ms —
   200-700x smaller than the fetch it follows.
3. `ce5648f` — `fork_verify::is_readable` called `VirtualQuery` once per 4KB page on the parent side
   while copying the child's memory. `VirtualQuery`'s cost scales with total committed memory (it is
   a VAD-tree walk), so this dominated on large address spaces. `fork_verify::readable_region` now
   caches the queried region's bounds across consecutive pages: one 173MB group went from 23.5-25.4s
   to 400-650ms, roughly 40-60x.

Causes investigated and explicitly **ruled out** — do not re-derive these:

- Re-deriving the in-memory rootfs (`pull_layers_in_memory` re-reading and re-merging the cached
  layers) and cold-starting `WindowsUserland::new()` were the first suspects and are both wrong:
  `78040e3` measured the base-rootfs merge at 390-660ms and `Platform::new()` as near-instant, and
  ruled both out by name.
- Writable-layer growth: `6d4248b` added the size/timing marker and `c208e12` measured 69ms at 9KB
  versus 235ms at 167MB — real, but far too small to explain the gap.
- A Windows Defender scan-gate, and a pathological `fork_verify` healing loop, were both earlier
  explanations for the "15 real minutes, 7 guest-seconds of progress" full-stack observation. Both
  are superseded by the measurements above.

Result: a full `webtop_stack.sh` boot under `LITEBOX_PROCESS_FORK=1` reaches `NGINX_CONFIGURED` and
`NGINX_STARTED` in under a minute, against never reaching them in 15+ minutes before. Use the gated,
zero-cost-when-unset `LITEBOX_DIAG_FORK_TIMING=1` (`cf8a370`, `6d4248b`) for any future per-fork
cost question rather than adding fresh probes.

## Cross-process fork: the correctness-soundness retraction chain

This conclusion was reached, retracted, and re-reached, and the chain is worth keeping so nobody
re-runs it:

1. `82f3a34` claimed cross-process fork showed the SAME tcache corruption as the thread-based path.
   The evidence was shim-level "eligible" and "copy plan" log lines, which fire *before* the platform
   call is made.
2. `4021d45` retracted that by instrumenting the first line of `spawn_cross_process_fork_child`
   directly: `LITEBOX_PROCESS_FORK` was `None` on every run and there were zero
   `[process_fork_diag] task-resume-probe` lines. The repro had never taken the cross-process path.
3. `71a4bcd` re-ran it with the variable genuinely set: zero corruption across every completed fork,
   with `x=$(echo hi)` capturing `hi` every time, against the thread-based default's 100%
   `malloc(): unaligned tcache chunk detected` rate on the identical repro.

Corroborated since by `ce5648f` (10/10 clean `bashfork_repro`), `060ccc3` (3/3) and `6e86a40` (still
zero). ADVISORY-001 section 3N's tcache-safe-linking analysis describes the **thread-based** path's
defect only.

A second, related false conclusion came from the same trap: a superseded post-duplication assert was
emitting `fd_complexity.beyond_stdio == 0` as though it gated the decision, 67 times per boot against
3 forks that had actually taken the path. `b2c89fc` demoted it to `debug!` and recorded that it was
asserting something false. The real decision has no `beyond_stdio` gate at all.

## Cross-process fork: reading the logs

On an identity (`D == 0`) fork, `fork_verify` emits "stale CODE pointer detected, translating and
resuming" with `translated_rip == rip`. In one measured run that was 84,319 of roughly 90,400 log
lines, with zero real translations, bounded per child by `MAX_IDENTITY_VERIFICATION_STEPS = 4096`.
It is wasteful (up to ~4096 single-step traps per long-enough child) but not corrupting. Anyone
reading a cross-process fork log without knowing this will spend the session chasing it.

Also: every fork child is a fresh re-exec whose `init_logging()` resets elapsed time to ~0, so
timestamps from a parent log and a child log are not on the same clock. Never subtract across them
(`4b95600`, `1199ab6`).

## The 2026-09-08 defect set that made the default-configured desktop render

`8c07f51` is the summary commit. Seven independent litebox defects, distinct from the 2026-09-07 set
(which is in memory `mem-f17269d5777055d3-3326`):

- `d89bebd` — `ThreadHandle::interrupt` called `env::var_os` between Suspend and Resume, taking
  ntdll's environment lock while another thread held it, deadlocking the whole guest.
- `694bb93` — unknown socket options returned `EINVAL` instead of `ENOPROTOOPT`, which made
  GdkPixbuf → glycin → D-Bus image decoding fail for **every** format, not just one.
- `133d3d4` — `O_NOATIME` hit an `unimplemented!()` and panicked the host.
- `6860300` — `fork_verify` could heal outside the guest address space, writing into litebox's own
  code or a loaded module.
- `62e3c79` — `allocate_pages` panicked on OOM instead of reporting it to the guest.
- `d22a916` — writable `MAP_SHARED` mappings of ordinary files were rejected outright.
- `5a13f2c` — read-only `MAP_SHARED` file mappings did not actually route through the shared object,
  which is what left MATE without its `top` toplevel, i.e. no Applications menu.

Two network fixes the same path depends on: `537c088` (`close(2)` must flush queued data, not discard
it — the `write(fd,resp); close(fd)` shape silently lost the response, and `--publish` went from HTTP
000 to 200) and `2197d18` (`AF_INET6` sockets exist; `EAFNOSUPPORT` on `listen [::]:80` was why stock
nginx would not start at all).

Two measurement artifacts recorded and retracted in the same investigation, per
`docs/webtop-xfce-code-vs-data-2026-09-08.md:205-229`: "the accepted socket is orphaned one cycle
after accept()" and "nginx was failing" were both wrong — the `HTTP_LOCAL_FAIL` signal was the
launcher calling `wget`, which the image does not ship.

## The black desktop: rewriting data as if it were code

`c7a1fa8`, documented in `a29216b`. The runtime syscall rewriter patched whole `PROT_EXEC` mappings
rather than only real code, corrupting 1984 bytes of `libLLVM.so.19.1`'s `.dynsym`. `ld.so` then
could not resolve a symbol that the file itself defines, so `dlopen` of libLLVM, libgallium and
libGLX_mesa failed roughly three times a second forever, and `xfwm4` never created a single window —
a black desktop with no error that pointed at the cause.

Measured before: 3387 large mappings and 233.7s spent in mmap, with zero windows ever created.
After: 5 mappings, and `WM_S0 owner=0x600014`.

Any older note describing the desktop background as rendering "inconsistently across runs", or as a
non-deterministic race that was investigated but not root-caused, is superseded by this. Related:
`0ac2659` (one streaming decode instead of two materialized ones) and `37885fd` (widen the PIE spread
to 4 GiB, and skip disassembly that cannot find anything).

## OCI packaging and caching internals

- Rewritten layers are cached on disk under `.litebox-cache/`, keyed on the pair
  `(layer digest, litebox_syscall_rewriter::REWRITER_CACHE_VERSION)` — the version component is what
  makes a rewriter change invalidate every existing artifact automatically. Reads are mmap-backed,
  writes are temp-file-then-rename. `litebox_packager/src/oci.rs:330-352,436-493,854`; introduced in
  `00575c0`, streamed write in `ee87135`.
- The runtime path no longer fetches the image-config blob (`1199ab6`, `oci.rs:829-837`), and fork
  children skip the manifest fetch entirely (`cc7986f`).
- Memory history: four independent packager buffer-sizing/streaming bugs were found and fixed —
  `39dd882` (deterministic OOM in `rewrite_layer_elfs`: reserve headroom rather than sizing output to
  exact input length), `a7e11b4` (rewrite each layer's ELFs immediately instead of batching every
  layer first), `ee0fa45` (mmap the decompressed layer instead of holding a resident `Vec`),
  `de095fa` (pre-size pull/decompress buffers instead of growing from `Vec::new()`), plus `69388ae`
  (stream the raw layer pull to a temp file) and `4a6211e` (make cache-miss layers mmap-backed
  immediately). `05ab846` sweeps orphaned `.tmp-*` files from killed runs at the start of every pull.
- Large images now pack: `linuxserver/webtop:alpine-mate` at 2937.9MB / 59,067 entries / 2,641 ELFs
  rewritten, and `ubuntu-xfce` at 119,692 entries / 13.7GB. The residual risk is host-memory
  contention from unrelated processes, not a litebox bug — full detail in memory
  `mem-6c4697ac568ea7be-4487`.
- `tar_ro.rs`'s multi-layer path index is built once at mount time
  (`litebox/src/fs/tar_ro.rs:61,75-80`, `TarIndex::from_layers`), never re-scanned per guest read.
  That build was O(entries²): starting one process against the 2.5GB webtop rootfs went from 17.3s to
  0.35s (`90c010a`).

## Filesystem gaps closed on the way to booting a stock s6-overlay image

`66bd640` is the commit where `/init` on `webtop_seatd.tar` runs with no flags and no stubs — 16
cross-process children, zero uncarriable fds, zero fatal errors, through `preinit`,
`s6-linux-init`, `s6-rc-compile` and `s6-rc-init` into supervision. The gaps closed to get there:

- `ac2ac9c` — fd-relative `faccessat`, seeking past EOF, and two writable-layer propagation bugs.
- `9a667ba` — follow symlinks in intermediate path components; support renaming a directory.
- `66bd640` — FIFOs.
- `90c010a` — a quadratic tar index build, and a corrupting `SEEK_CUR` on read-only-layer files.
- `9798924` — `lstat` could not walk an intermediate symlink.
- `022a54f` — `migrate_file_up` left a stale `Lower` cache entry when the writer's own fd was the
  only one open.
- `6e14b71` — `chmod`/`chown`/`set_times` panicked the host on any lower-only directory or character
  device, and recursed unboundedly. See `AGENTS.md`'s errno rule.

This retires three claims older notes carried as fundamental blockers: the s6 `/init` ET_EXEC
single-address-space collision, "only one `python3` can run because it is ET_EXEC", and
`failed to map segment` under concurrent load.

## The remaining architectural gap for a full desktop on the crash-free path

Guest processes share no AF_UNIX, loopback or FIFO namespace. A cross-process fork produces zero
access violations, but Xvfb is then unreachable from its clients because `/tmp/.X11-unix/X1` is not a
shared object, and FIFOs are per-process by the same limitation. A Windows-named-pipe bridge for this
was written and then dropped rather than shipped half-working.

One host-side transport shared by every process of a guest would let the whole desktop run on the
already-crash-free cross-process path. `docs/fork-fs-veh-2026-09-08.md:128-144`, and `66bd640`'s own
"KNOWN LIMITATION" note.

## Host-side crash machinery: what a dump contains and what its fields mean

Paths are `litebox_platform_windows_userland/src/lib.rs`.

**Two different outputs on two different paths.** The first unrecovered host-mode access violation
prints `[diag-unrecov-av-*]` dumps to stderr, **ungated** — no environment variable needed
(`:1950-1956`): a stack walk, the `RECENT_FAULTS` ring (`:789`, printed at `:2231`) and the
`RECOVERY_LOG` (`:2221-2293`). It then calls `RaiseFailFastException` and hands off to WER (`:2326`).
A real OS-written minidump (`MiniDumpWriteDump` via `write_crash_minidump`, `:8889`, `199655b`) is
produced at exactly ONE site: the repeated-identical-fault circuit breaker, more than 64 faults at the
same `rip` (`:2019`). Its flags deliberately exclude full memory
(`MiniDumpWithThreadInfo | …IndirectlyReferencedMemory | …UnloadedModules`), so do not expect a heap
in it.

**`error_code` is synthesized on this platform, and `19740a3` changed what it means** (`:9342-9379`):

- bit0 (Present) is now REAL, derived from `VirtualQuery`/`MEM_COMMIT`. It was hardcoded to 0 before.
  Consequence: **every pre-2026-09-10 inference of the form "not-present, therefore unmapped" drawn
  from `error_code` is unsound** and must be re-derived, not reused.
- bit1 (write) is now set only when `ExceptionInformation[0] == 1`, and bit4 (instruction fetch / DEP)
  only when it is `8`. Previously `!= 0` folded a DEP execute fault into "write". Consequence: a
  pre-fix "write" fault may actually have been an instruction fetch; a pre-fix read (`0x4`) was
  genuinely a read, because read is `ExceptionInformation[0] == 0` both before and after.
- bit2 is always set, and is NOT evidence about ring: `kernel_mode: false` is hardcoded (`:9403`).
  This platform never reports a kernel-mode guest fault.

**`is_in_guest` became tri-state in `c0c1472`** — `true` / `false` / `unknown(no-tls)` (`:786-790`,
printed at `:2239-2243`). Before that change a thread with no TLS slot, i.e. identification simply
failing, printed identically to a confirmed `false` via an `.unwrap_or(false)`. The same commit fixed
`rip` and `rsp` being captured as two separate dereferences of a live OS-owned `CONTEXT` that another
thread's `SuspendThread`/`SetThreadContext` could mutate between them — so they could be torn, from
different moments. Any register-level conclusion recorded before `c0c1472` is void in both directions.

**VEH entry and nesting.** The naked trampoline enters the handler for exactly four exception codes
(`:434-471`, `78dda05`) — `EXCEPTION_ACCESS_VIOLATION`, `EXCEPTION_SINGLE_STEP`,
`EXCEPTION_ILLEGAL_INSTRUCTION`, `0xC0000096` — and returns `EXCEPTION_CONTINUE_SEARCH` for everything
else with no TLS read, no stack swap and no Rust on the path. The two codes that used to do the damage
were `EXCEPTION_STACK_OVERFLOW` and `0xE06D7363`, the MSVC/Rust panic code. Registration is FIRST in
the chain, `AddVectoredExceptionHandler(1, …)` at `:2897`, which landed in `5870ab0` — *before* the
whitelist that makes being first safe.

Per-depth frame sizing, from disassembly rather than guesswork: `VEH_FRAME_STRIDE = 8192` (`:3471`),
`VEH_DEPTH_CAP = 3` (`:3499`), `EXCEPTION_RECORD_RESERVE = 65536` (`:3423`), `EXC_RECORD_SLOTS =
CAP + 1`. The measurement that set the stride: handler prologue 2544 bytes plus `on_single_step` 1384
bytes = 3928 bytes, which does not fit the old 4096 stride once anything else is on the frame. Total
reach below `host_sp` is deliberately unchanged at 32KiB — the extra stride was taken back from the
former 204-slot exception-record region, not added to the total. Past-cap now bails to `.Lsearch`,
counted in `LSEARCH_EXIT_COUNT`, rather than entering the full handler. `5cacf7f` took the stride from
64 bytes to 4096; `0473cc3` is the current truth at 8192.

Other landed VEH work: `dc108fb` stopped arming the process-killer watchdog on the successful-recovery
path (`:1895-1944`, which records the measured evidence — `mate-session` recovered three times and was
then killed by the watchdog, and completes clean with it disabled); `fdf5dd9` and `8ef49b5` resolve
every diagnostic gate once at startup so the single-step handler never queries the environment;
`ff7682b` gave the naked trampoline real SEH unwind info. The canonical narrative is
`docs/veh-exception-handler-design.md` (`6794160`), cited from six places in `lib.rs`.

One removed diagnostic worth knowing about, because older notes tell you to grep for it: the
`diag-guest-exception: cr2 byte dump` no longer exists. `5ec1ee4` removed it because its own unchecked
`from_raw_parts` re-faulted in host mode on the same unbacked address and killed the runner — the
diagnostic was crashing the host in exactly the scenario it existed to debug. See also `97a8f40`.

## The two PRD rows that are true in the tree and block `-Dwarnings`

Both verified by reading the code, not by running anything:

- `litebox-mm-unsafe-op-in-unsafe-fn-breaks-dwarnings` — `litebox/src/mm/mod.rs` has five sites calling
  the `unsafe fn change_page_permissions` (`:1410`) from an `unsafe fn` body with no `unsafe {}` block:
  `make_pages_writable:1429`, `make_pages_executable:1447`, `make_pages_readable:1465`,
  `make_pages_inaccessible:1478`, `make_pages_rwx:1502`. `litebox/Cargo.toml:4` is `edition = "2024"`,
  where `unsafe_op_in_unsafe_fn` is warn-by-default, so `RUSTFLAGS=-Dwarnings` is red on every target.
- `litebox-common-linux-not-rustfmt-clean` — four pre-existing diffs: `src/lib.rs:2276` (`Timespec`'s
  long derive list needs wrapping), `src/lib.rs:4378` (the `DRM_IOCTL_SET_CLIENT_CAP` arm should
  collapse to one line), `src/lib.rs:4896` (`Sysno::fallocate`'s `sys_req!` should expand multi-line),
  `src/mm.rs:67` (three `permissions.set` calls plus the `create_pages_with_permissions` binding).

## Input-latency rows: which parts are stale and which are genuinely open

`present-latency-fifo-unbounded-queue-and-abs-motion` has three items and they have diverged:

- Item 1 (present mode is Fifo) is **stale**. It is now Mailbox-preferred with Fifo fallback, queried
  from `caps.present_modes` (`litebox_platform_windows_userland/src/presentation.rs:1064-1080`,
  `5a194f5`/`aa1d0ca`). The `presentation.rs:570` the row cites no longer exists.
- Item 2 (unbounded frame queue) is **already formally refuted** by its own sibling row
  `correction-frame-queue-already-coalesces`, which is marked completed.
- Item 3 is **genuinely open**: mouse deltas are still derived by differencing winit `CursorMoved`
  absolute positions rather than reading `DeviceEvent::MouseMotion`. Lossless sub-pixel accumulation
  mitigates quantization but not winit's event rate. The row should be narrowed to this item, not
  deleted.

`evdev-emits-two-syn-reports-per-mouse-move` is **fully stale** — the fix shape it proposed is exactly
what shipped, as `InputSignal::RelMotion` (`presentation.rs:317-322`) consumed as a single grouped
`SYN_REPORT` (`litebox_shim_linux/src/syscalls/evdev.rs:91-120`, and `:153-160` queues nothing when
both deltas are zero), `59e4ca0`. Resolve it.

## The three correctness bugs the fork perf work exposed

All three were unreachable before the per-fork cost came down far enough for a real stack to get deep
into a boot.

**`060ccc3` — the missing SIGCHLD bridge.** A `LITEBOX_PROCESS_FORK=1` child is a genuinely separate
Windows process that reconstructs its own `Process` from scratch, with no `Arc` back to the real parent.
`Process::prepare_for_exit`'s own `has_live_parent` gate therefore read unconditionally `false` for such
a child, silently skipping the same same-process `SIGCHLD`-delivery step a thread-based child's exit
already performs. A parent blocked the race-free way — mask `SIGCHLD`, then `sigsuspend`/`pause` to wait
for it atomically, which is exactly what busybox ash's plain `wait` builtin does when it has more than
one backgrounded job — hung forever the moment it had any cross-process child, even after that child had
already exited. Nothing was ever going to wake it.

Pinpointed with debug syscall tracing against
`advisor/probes/cross_process_fork_wait_hang_probe.sh`: the parent's last syscall ever was a
non-blocking `sys_wait4(WNOHANG)` correctly returning `Ok(0)`, immediately followed by
`rt_sigprocmask`, then silence. The second half of the mask-then-sigsuspend idiom was never traced
because `sys_pause`/`sys_rt_sigsuspend` have no entry log — not because nothing happened. That
distinction is worth remembering: absence of a trace line is not absence of a syscall.

Fix: `ForkChildVerificationProvider::spawn_cross_process_exit_notifier` (`litebox/src/platform/mod.rs`),
Windows-implemented at `litebox_platform_windows_userland/src/lib.rs:9701` as a spawned thread blocking
on the existing `wait_for_cross_process_exit`, wired into both real `do_clone` cross-process sites via
`arm_cross_process_exit_notifier`. On child exit it pushes the child's `exit_signal` into the parent's
`shared_pending` and calls `interrupt_all_threads()`, mirroring `prepare_for_exit`'s existing
same-process notify step.

**`6e86a40` — `sys_wait4(pid=-1)` checked the wrong registry first.** It consulted
`cross_process_children` only when the thread-based `children` registry was ALREADY empty at call time.
That is backwards for the common shape: a shell forks several plain commands (`mkdir`/`cp`/`sed`,
thread-based) before backgrounding a LATER cross-process fork, leaving `children` non-empty, so the
blocking wait loop never checked the cross-process registry at all. `poll_once`, shared by the `WNOHANG`
and blocking paths, now checks `cross_process_children` first on every invocation rather than once up
front — which is strictly more general, because it also catches a cross-process child that exits *after*
the blocking wait has already begun. Minimal repro: the real `webtop_stack.sh` truncated to its first
148 lines (`head -n 148`, everything through the nginx self-test), which reproduces the stall
deterministically in under a minute.

**`d5cc744` — a claim release deleted a live cross-process memory guard.** `deallocate_pages`
redundantly called `release_claim_range_for_current_thread`. On a partial overlap that dropped a
coalesced `CLAIMED_RANGES` slot whole, silently deleting collision coverage for an entire loaded
library. Reproduced as Xvfb's deterministic `libselinux.so.1` crash: 100% of runs before, 0 of 4 after.

## Cross-process fork eligibility, verbatim

`try_cross_process_fork`, `litebox_shim_linux/src/syscalls/process.rs:2524-2814`. It refuses only for:

- `comm` equal to `Xvfb` or `dbus-daemon` — `THREAD_BASED_FORK_ONLY`, `process.rs:2560`, landed
  `564af3f`. A live unix listening socket cannot be served from a fork-time filesystem snapshot; when it
  was allowed the measured outcome was `XVFB_FAILED`.
- An already-borrowed fd table (`:2584`).
- Any beyond-stdio fd that is not a pipe end, a path-recorded regular file, an eventfd, or
  close-on-exec (`:2639-2798`). Overridable with `LITEBOX_PROCESS_FORK_IGNORE_FDS`.
- An invalid `fs_base`, or a context that cannot be sanitized (`:2820-2828`).

It short-circuits to `try_native_cross_process_fork` when `platform.has_native_fork()` (`:2537`) — the
entire fd-carrying apparatus is Windows-only scaffolding for a syscall the platform lacks.

## The browser-verified desktop: exact working configuration

Source: `f10c5e9`, `docs/webtop-xfce-code-vs-data-2026-09-08.md:374-429`.

- selkies bound with `--addr=0.0.0.0`, listening on port **8081** (not 8082 — `166b5d5` corrected an
  earlier note that said 8082, along with a wrong loopback claim).
- The dashboard is reached over `--publish`, with `/websockets` tunnelled to 8081.
- Everything except the reverse proxy runs inside litebox: Xvfb, the X clients, selkies/pixelflux's x264
  encoder, and MIT-SHM screen capture.
- What Chrome on the host actually showed: xfdesktop's `Home` and `File System` icons, the cursor, and
  live H264 pixels off the guest's X server — reproduced twice on a fresh stack.
- For the MATE variant (`8c07f51`, `181ec68`): panels, menus, wallpaper, icons, the Applications tree
  and a full input round-trip, with an X census of 30 windows, 4 mapped, `WM_S0` owned by `marco`, and
  100.0% non-black pixels.

Selkies in `linuxserver/webtop:alpine-mate` is **pure Python** (pixelflux/pcmflux) with no Node.js
anywhere outside DEV_MODE (`5799e32`, `docs/webtop-alpine-mate-2026-09-07.md:1236-1264`). Any note or
PRD row describing a "selkies node.js WebRTC server" is describing a different stack. Selkies serves one
client at a time and a page reload does not reclaim the slot — a fresh stack is needed per view.

## Desktop status detail

**PI futexes are implemented, not merely refused.** `d35b643` swapped EINVAL for ENOTSUP; `a120c56`
implemented them on the ordinary futex machinery using the kernel's owner-TID / `FUTEX_WAITERS`
protocol, and implemented `sched_get_priority_min`/`max` alongside (1..=99 for FIFO/RR, 0..=0
otherwise); `a473048` replaced the placeholder with a real compare-exchange. The reason this mattered
at all: `EOPNOTSUPP` is itself fatal on glibc via `futex_fatal_error()`, so refusing the op politely
still killed selkies — one line after the cursor had been delivered.

**labwc's `wlr_swapchain_create` SIGABRT is an upstream wlroots legacy-DRM-backend limitation, not a
litebox gap.** Zero DRM ioctls occur anywhere in the crash window (confirmed via a live `labwc -d`
capture), so litebox cannot be the source. It is triggered by xfsettingsd's wlr-output-management
config-apply request reprocessing the output. litebox's DRM device deliberately implements only legacy
`SETCRTC`/`PAGE_FLIP`, by correct design, and that is what forces wlroots onto the path with the
assertion. Guest-side workarounds were investigated and ruled out: xfsettingsd has no plugin-disable
flag, and labwc's `rc.xml` has no output-management suppression. PRD row
`xfce-labwc-swapchain-upstream-wlroots-gap`, status deferred.

**The gdk-pixbuf/GTK image-decode findings are all unverified against a later fix.** The older
per-format results — SVG has no loader shipped (a packaging gap, not a litebox bug), XPM fails
specifically when read from the tar-RO filesystem backend combined with its dlopen'd loader module —
were every one of them measured while `694bb93`'s bug was live: unknown socket options returned
`EINVAL` instead of `ENOPROTOOPT`, which made GdkPixbuf → glycin → D-Bus image decoding fail for EVERY
format. Re-measure before trusting any of them.

**Network fixes the desktop path depends on.** `537c088` — `close(2)` must flush queued data rather
than discard it; the ordinary `write(fd, resp); close(fd)` shape silently lost the response, and
`--publish` went from HTTP 000 to 200. `2197d18` — `AF_INET6` sockets exist; `EAFNOSUPPORT` on
`listen [::]:80` was why stock nginx would not start at all.

**Two measurement artifacts recorded and then retracted** (`docs/webtop-xfce-code-vs-data-2026-09-08.md:205-229`):
"the accepted socket is orphaned one cycle after accept()" and "nginx was failing" were both wrong. The
`HTTP_LOCAL_FAIL` signal behind them was the launcher calling `wget`, which the image does not ship.

**`ubuntu-xfce`'s own blocker, named.** It pulls and packs fine (119,692 entries, 13.7 GB) and has real
XFCE binaries, but Ubuntu ships rust-coreutils, which aborts inside rustix's auxv handling
(`rustix/.../param/auxv.rs:269`, an `unwrap` on an `Err`), taking out `sleep`, `tail` and the DE launch.
`/bin/sleep 1` on its own succeeds (`8c07f51`). `bb46f1a` has since implemented `/proc/self/auxv` and
`AT_EXECFN` — the exact thing that auxv handling wants — so this deserves one re-test, not a fresh
investigation.

## Closed negative results, in full

**Windows CoW-mmap performance.** `try_allocate_cow_pages` is implemented for Windows and has zero
practical effect on real tar-packed execs. Root cause, conclusively established: `MapViewOfFile3`
requires 64KiB file-offset alignment, and real ELF `PT_LOAD` segments are only page-aligned with no
exploitable slack — and that holds regardless of how the tar file's own start is aligned. Do not
re-attempt without a genuinely new approach. Measurement and rejected-alternative detail:
`docs/cow-mmap-fixed-address-design.md`, memory `mem-3e13872ce1ffe95e-2814`.

A real correctness bug in the same path was found and fixed (`bedeb0e`, 2026-09-07): a Windows
`MAP_FIXED` sub-range remap could destroy and zero-fill a shared library's flanking memory, silently
corrupting it. The path became opt-in only, `LITEBOX_COW_MMAP`, default off
(`litebox_shim_linux/src/syscalls/mm.rs:52-68`, consumed at `:444`, re-exported for the runner at
`litebox_shim_linux/src/lib.rs:6406-6409`).

**That default-off is load-bearing, and the flank fix is still contested.** The shipped restoration
(`329174b`; live at `litebox_platform_windows_userland/src/lib.rs:7088-7162`, with the CoW→RW `anon_prot`
translation at `:7092` and the per-flank `VirtualAlloc2` commit at `:7138-7162`) recommits the orphaned
flanks as **anonymous zero-fill**, not original file content, and its own comment at `:7130-7137`
concedes exactly that: it "loses the flanks' original file content — a real, documented … zero-filled
read, which is always memory-safe". But the fault it was fixing was `ld.so` **reading** that flank's
`.gnu.hash`/`.dynsym` at +0x200. Memory-safe is not correct here: opting in trades a loud SIGSEGV for
silently zeroed symbol tables. `19740a3` does not dissolve this — it confirms the classification the
concern rests on, since read/write discrimination is `ExceptionInformation[0]` (`0` read, `1` write, `8`
DEP-execute, `lib.rs:9351,9377`), so an old `error_code=0x4` was a genuine read both before and after
that commit; what `19740a3` overturned was only the fabricated P-bit and the DEP→write misattribution.
gm mutable `cow-flank-zerofill-assumption-contradicted-by-read-fault`.

One stale instruction in that mutable worth correcting before anyone follows it: its experiment recipe
says to grep the log for `diag-guest-exception: cr2 byte dump`. That dump no longer exists — `5ec1ee4`
removed it because its own unchecked `from_raw_parts` re-faulted in host mode on the same unbacked
address and killed the runner. See also `97a8f40`. Separately, `de7e58b` fixed a stale comment in the
shim that denied the Windows CoW implementation exists at all.

**The GUI protocol decision is settled**: DRM/KMS + wgpu, resolved against the X11 and Wayland
alternatives and proven live — guest DRM page-flip pixels rendered into a real host window via
`CREATE_DUMB`/`MAP_DUMB`/`ADDFB2`/`SETCRTC`/`PAGE_FLIP` against `/dev/dri/card0`, observed by the user
across multiple render states. Memory `mem-3c4a9980a884604b-1031`. Do not reopen it as an
X11-vs-Wayland-vs-DRM question.

## Working practices (procedural know-how, pointed at from AGENTS.md)

These are the how-to items that used to sit in AGENTS.md's standing-lessons list. They are real and
earned, but they are procedure rather than hard constraint, so they live here and AGENTS.md names them.

**Build freestanding guest test binaries on the HOST, not with the guest toolchain.** Both the guest's
clang and its gcc are broken as of this writing (the gcc failure is
`gcc: fatal error: cannot execute cc1: posix_spawn: Invalid argument`, itself a real litebox gap). Use:

```
clang --target=x86_64-unknown-linux-gnu -nostdlib -nostdinc -ffreestanding \
      -fno-stack-protector -static -O1
```

which produces a static `ET_EXEC` with raw `syscall` instructions and no libc — exactly the shape that
isolates litebox's syscall emulation from any libc behaviour.

**Inject a new probe or script into a multi-GB layer via a small `--resume-from` overlay tar**, never by
rebuilding the whole layer: `tar cf overlay.tar -C <dir> file`. Two things are load-bearing here — the
path must be a real Windows path, not an MSYS `/tmp/...` one, and `MSYS2_ARG_CONV_EXCL='*'` (or
`MSYS_NO_PATHCONV=1`) must be set. Without them it fails in two different misleading ways: an `ENOENT`
that looks like a missing shebang resolver, or a stack-overflow panic.

**Build general debug and observability tooling proactively while investigating**, not just enough to
explain the bug in front of you. Two concrete instances this project paid for: guest stdout was
interleaved with litebox's own log stream until they were separated, and a component's own stderr kept
getting redirected to a file nobody read — weston's, Xwayland's and xfdesktop's each went unread for a
long stretch before anyone checked.

**Prefer premade, mature libraries over hand-rolled code for well-known problem classes** — OCI clients,
binary-format parsing, unwind-info construction, crash/minidump handling, CLI parsing, serialization.
`docs/premade-library-research.md` holds the audit findings so far. Only hand-roll when research confirms
no existing solution fits a genuinely litebox-specific constraint. Landed examples of this paying off:
`98bbe22` (parse ELF with `object` instead of hand-derived byte offsets), `199655b` (minidumps via the
OS rather than by hand), `c2c2912` (9P wire ints via `zerocopy::byteorder` instead of a hand-rolled
`LeWire`), `f900445` (clap instead of hand-rolled index loops), `cdfcffc` (delegate rdmsr/wrmsr/cr2/cr3
to the `x86_64` crate).

**Use `litebox_packager --oci-image <ref> --output <tar>` rather than any ad-hoc pull script.** The
Python tooling that used to do this — `pull_oci_image.py`, `batch_rewrite_layer.py`,
`fetch_container.py` — is retired (`cfae690`, `9076087`). Do not recreate it.
