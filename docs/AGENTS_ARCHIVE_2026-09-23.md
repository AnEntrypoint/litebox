# litebox — archived pass detail, 70th-76th (2026-09-22/23)

Full narrative for AGENTS.md's condensed pass-history summary. Read AGENTS.md first; this file is
reference detail, never a starting point.

**Pass history (70th-76th, 2026-09-22/23)**: full narrative in the dated archives ("Docs and tooling
map" below). Condensed current-state trail:

- **43rd-61st (FIXED, live-verified)**: both Xvfb SIGSEGVs; D-Bus activation's dropped-CLOEXEC-fd
  bug (`66265d9`); `fd/mod.rs:422` panic (`faa74c6`); per-fork rootfs-rebuild RAM cost (`2d18a4e`);
  the `ssh-agent`/`xfwm4` permanent-freeze class via `RawMutex::WaiterQueue::with_lock`
  (`61c235e`). `DE_FAILED` (no `_NET_SUPPORTING_WM_CHECK`) survived all of it. **62nd-66th**:
  narrowed `xfwm4` to an exact fd/protocol step (X11 census, guest-stderr, `cdb -pv`) — claims
  `WM_S0`, zero stderr, never reaches `setNetSupportedHint`; fixed `SharedUnixConnectQueue::
  cancel`'s slot leak (62nd); REFUTED `/defaults/xfce/` readdir, dbus-daemon babysitter SIGKILL,
  epoll-readiness (pid-filtered traces miss cross-process-forked GDBus siblings), GLX/compositor
  blocker theories. **67th-68th**: `DBUS_FAILED` root-caused+FIXED (`publish_as_container_fs_
  snapshot`'s byte-size regression guard discarded a healthy fresher export; gated the veto on
  THIS process's own prior-adoption failure only, `WRITABLE_LAYER_IMPORT_OK`) — 0 fires across 6
  post-fix boots vs 17 pre-fix. Upstream `xfwm4` pre-hint chain confirmed: `initSettings()`→
  `init_compositor_screen`(no-op)→`sn_init_display`→`myDisplayAddScreen`→`getNetCurrentDesktop`→
  `setUTF8StringHint`→`setNetSupportedHint`. **69th**: first byte-level D-Bus decode (`sys_recvmsg`
  payload-preview, `net.rs::do_recvmsg`) proves `initSettings()`'s call chain succeeds end-to-end,
  but its final `GetAllProperties(.../"/xfwm4/custom")` call re-issued identically every ~10.7s
  forever — matching upstream mechanism `cb_keys_changed`→`keymap_reload()` (GDK `keys-changed`),
  theory not proven. Full narrative for all of this range: `docs/AGENTS_ARCHIVE_2026-09-22.md`.
- **70th**: retrigger independently reproduced (t=9.22/20.47/31.27/42.19s, deltas 10.8-11.3s);
  "`xfwm4` never writes X11" REFUTED — `sys_writev`/`sys_read` log under `syscalls::file`, not
  `net`. Fixed two logging-infra bugs (Standing lessons: line-wrap rejoin, `FlushingStderr` race).
  Reassembled RECV stream parsed 155 Replies+15 PropertyNotify+2 Errors in <1s, incl. a
  `GetKeyboardMapping`-shaped reply — but bytes after didn't parse as valid framing (real parser
  gap, not corrupted input, explained by 71st). GDK upstream source confirms `keys-changed` fires
  ONLY on genuine XKB `XkbNewKeyboardNotify`/`XkbMapNotify`, no internal timer. Did not reach `DE_UP`.
- **71st**: root-caused the 70th-pass desync — `do_read`'s socket branch (`file.rs`, calls
  `GlobalState::receive` directly) is a separate path from `do_recvmsg`; real Xlib/XCB Xtrans uses
  plain `read()`/`write()`, so a `recvmsg`-only capture missed the whole stream including ConnSetup
  (confirmed: captured stream's first bytes don't parse as a live ConnSetup reply). Fixed: new
  `litebox_diag::socket_read` diagnostic on `do_read`'s socket branch, dedicated low-overhead
  target. Three fresh-capture attempts each hit a different obstacle (blanket `file=debug`
  destabilized the boot; `de_only.sh` hit an unrelated `gpg-agent` dead end; a `webtop_stack.sh`
  boot was killed mid-`SELKIES_PORT_SELFTEST_FAILED` polling). XKB-at-retrigger remained OPEN.
- **73rd**: found+fixed the REAL reason the 71st-pass diagnostic captured zero `xfwm4` traffic — the
  `unix` closure gap above (`fc830d1`); live-confirmed (real AF_UNIX D-Bus SASL handshake traffic
  captured for the first time). Added `litebox_shim_linux::syscalls::process=debug`
  (`DIAG_TIMELINE execve`) to directly identify `xfwm4`'s own guest pid. 4/4 captures that pass
  showed `xfwm4` stopping after EXACTLY 2 reads (its D-Bus SASL handshake), attributed at the time
  to RAM/CPU starvation from the fork storm — **partially superseded by the 75th pass: `xfwm4`
  wasn't merely starved, its own xfconf config was invisible to it at all (the writable-layer
  export-path bug, fixed `1d449e6`); RAM/CPU pressure is real but was not the whole story.** XKB
  question left OPEN, unchanged since.
- **74th**: narrowed the diagnostic scope itself (assignment: cut the 73rd pass's own self-inflicted
  capture overhead). Moved all five `DIAG_TIMELINE` sites onto a dedicated `litebox_diag::
  process_timeline` target and added an optional `LITEBOX_DIAG_SOCKET_READ_TARGET` comm filter to
  `litebox_diag::socket_read` — both verified live. RAM still collapsed hard that run (7.85GB free
  at launch → 479MB at t=60s, host process count peaking at 30) — logging overhead was NOT the
  dominant RAM driver. Read the ever-present `[process_fork_diag] globalstate-probe (child):
  rebuilding rootfs from OCI image …` line (99 of them that boot) as evidence the 56th pass's
  rootfs-index cache was still missing repeatedly — **REFUTED by the 75th pass below with direct
  measurement: that line prints unconditionally on every fork regardless of downstream cache
  status, and the cache was actually hitting 100% of the time.** Also hit the xcensus.py
  writable-layer-visibility gap every attempt (`rc=2`, file not found) — **FIXED, 75th pass.**
- **75th**: two real findings, in order of consequence.
  1. **The 74th pass's rootfs-cache-miss theory is REFUTED, with direct measurement.**
     `LITEBOX_DIAG_FORK_TIMING=1` on a real `de_only_xcensus_seed2.tar` boot (`debian-xfce`, same
     harness) shows the per-layer OCI cache AND the 56th pass's merged-rootfs-index cache both
     hitting 100% of the time (`[cache] HIT`/`[diag-mergedidx] HIT` on every single one of ~28-100
     forks sampled across two boots, zero misses) — real per-fork rootfs-related cost is now
     **~83-140ms end to end** (`rootfs layers ready`→`default_fs_multi_layer returned`), far below
     even the 56th pass's own ~2.3-2.5s cache-hit target. The `globalstate-probe (child):
     rebuilding rootfs from OCI image …` line the 74th pass read as a miss signal fires
     UNCONDITIONALLY before either cache is even consulted — a high count of it is not evidence of
     wasted work. The 56th pass's own caching fix stands, fully vindicated; do not re-investigate
     it without new contrary measurement. **This matters for the 76th pass below: it means the
     dominant per-process RAM cost is NOT rootfs-tar parsing time/allocation — whatever is
     producing ~350MB-1.1GB of working set per cross-process-fork child is something else
     (candidates: the merged-index/materialized-file-tree's own resident size once built, or
     litebox's own per-process guest-memory-emulation bookkeeping) — still not decomposed.**
  2. **Root-caused and FIXED a real, deterministic (not racy) writable-layer bug that plausibly
     explains a large share of this whole investigation's "writable-layer-visibility gap"
     symptoms.** `take_cross_process_writable_layer_export`
     (`litebox_platform_windows_userland/src/lib.rs`, called from `sys_wait4`'s cross-process
     branch via `import_cross_process_writable_layer` — the ONLY place a parent ever re-absorbs a
     reaped fork child's filesystem writes) required `FORK_CHILD_TAR_PATH_ENV_VAR`, which is
     deliberately UNSET on every `--oci-image` boot (only ever set for `--initial-files`) — so the
     function returned `None` unconditionally on every OCI-image boot, meaning **a parent NEVER
     imported ANY cross-process fork child's filesystem writes back into its own live state, on
     ANY `--oci-image` boot, ever.** The child's own export-path-naming side (`diag_process_fork_
     task_resume_probe` in the runner crate) already had the correct OCI-image fallback (a
     placeholder `"oci-image"` stem); the parent's read side did not mirror it, so the two sides
     silently computed different export filenames and the parent's read always missed. Confirmed
     live with a minimal, fast repro (`-Z --oci-image ... -- /bin/bash -c 'mkdir -p /tmp/t2; ls -la
     /tmp/t2'`, both `debian:stable-slim` and `linuxserver/webtop:debian-xfce`): before the fix,
     `mkdir` reports exit 0 but the VERY NEXT sibling fork's `ls` deterministically reports `No
     such file or directory` for the identical path — 100% reproducible across 6+ repeated runs,
     not a timing race. **Fix** (`1d449e6`): mirror the child's own `.or_else(FORK_CHILD_OCI_IMAGE_
     ENV_VAR → "oci-image")` fallback on the parent's read side too. Verified: the same minimal
     repro now succeeds 4/4; the real `de_only.sh` harness's own `mkdir -p ~/.config/xfce4/xfconf/
     xfce-perchannel-xml/ && cp /defaults/xfce/*` step (previously invisible to every later
     sibling — `XFCONF_USERDIR`/`XFCONF_XFWM4XML_HEAD` both `No such file or directory`, every
     single pass since this harness existed) now succeeds, and **`xfwm4` launches for the first
     time in this investigation's entire history** — confirmed via `DIAG_TIMELINE execve`
     (`argv0=/usr/bin/xfwm4`), the X11 window count growing 0→1→11 (`XCENSUS_WINDOWS`), and
     `_NET_SUPPORTING_WM_CHECK`'s `xprop` error text advancing from "no such atom on any window"
     to "not found" (the exact 56th-pass forward-progress marker) — reproduced 2/2. Also fixed the
     seed tar's own `/tmp/xcensus.py` visibility gap (a DIFFERENT instance of the same
     export/import-staleness class, still present for a plain `cat > file <<EOF` + later-sibling
     `python3 file` round trip even after the fix above, since that round trip's SOURCE write and
     READ are two more forks either side of the SAME gap) by feeding the census script to `python3`
     via a shell variable + stdin instead of a `/tmp` file (`.wfgy/de_only_xcensus_seed3.tar`,
     disk-only, not checked in) — `XCENSUS_PRE_DE` now returns `rc=0` with real census data instead
     of `rc=2` ENOENT, giving this investigation its first-ever live X11-census ground truth.
     **Not yet reached: `DE_UP`.** Two independent post-fix boots both reached `WM_POLL n=6`
     (`_NET_SUPPORTING_WM_CHECK` still "not found") with 11+ real windows before a RAM crater (free
     RAM fell to 0.3-1.3GB, forcing cleanup) cut the run short — the already-known "process-count
     accumulation" bottleneck (Track B item 1) is now the SOLE remaining blocker on this harness,
     not a filesystem-visibility bug. `xfwm4`'s own X11 traffic capture
     (`LITEBOX_DIAG_SOCKET_READ_TARGET=xfwm4`) still showed only the 2-read D-Bus SASL handshake in
     both post-fix attempts — the RAM crater cut the run before `xfwm4` reached its steady-state
     retrigger loop, so **the XKB-event question remains genuinely open, unchanged, not newly
     answered by this pass.**

## 76th pass (2026-09-23) — RAM-crater mechanism directly captured for the first time; a real,
## partial mitigation landed; the crater is NOT fully solved; per-process RSS is now the prime
## suspect, not fork-tree scheduling alone

**Repro used**: the exact 75th-pass harness, unmodified —
```
$env:LITEBOX_PROCESS_FORK = "1"
$env:LITEBOX_LOG = "warn,litebox_platform_windows_userland::fork_verify=error,litebox_diag::process_timeline=debug"
& .\target\release\litebox_runner_linux_on_windows_userland.exe --env GLIBC_TUNABLES=glibc.malloc.tcache_count=0:glibc.malloc.mxfast=0 --oci-image docker.io/linuxserver/webtop:debian-xfce --resume-from .wfgy/de_only_xcensus_seed3.tar -- /bin/bash /de_only.sh *> combined.log
```
Launched via `Start-Process -RedirectStandardOutput/-RedirectStandardError` (NOT the `*>` form) so a
parallel PowerShell loop could poll `Get-CimInstance Win32_Process`/`Get-Process` working-set numbers
by PID while the boot ran, without needing a second terminal.

### Finding 1 — first-ever direct host-side process-tree capture at the exact crater moment

Polling `Get-CimInstance Win32_Process -Filter "Name='litebox_runner_linux_on_windows_userland.exe'"`
every few seconds (unmodified pre-76th-pass binary, commit `20a57ca`) caught the crater live:
`FreePhysicalMemory` fell from 5.85GB to 0.43-0.65GB inside about 90 seconds, exactly coinciding with
the guest log's `WM_POLL n=7` (an empty `xprop` result — the polling command itself was starting to
fail to even complete). A full process snapshot at that instant
(`.wfgy/pass76_crater_procsnapshot.txt`, 33 lines) shows:

- **33 simultaneous `litebox_runner_linux_on_windows_userland.exe` host processes alive at once**,
  combined working set **≈10.5GB** on a 15.25GB host.
- **A real multi-generation TREE, not a flat list or a simple chain** — up to 4 generations deep
  (e.g. pid 15012 → 19256 → 26212 → 24244 → 23660), with several branches (pid 19256 alone had 7
  direct children, several of which had their own further children).
- **A consistent two-process-per-logical-fork shape**: most branches show a large process
  (350MB-1.1GB working set — the actual re-executed guest-execution instance) immediately followed
  by a small (~6-12MB) child of THAT process, matching a re-exec/relaunch step internal to
  `spawn_cross_process_fork_child`'s own mechanics, not a second independent guest fork.

This is the first time in this investigation's whole history the crater was caught with a REAL
host-side process tree and per-process RSS numbers, rather than inferred from guest-log symptoms or
aggregate `FreePhysicalMemory` alone. It directly confirms AGENTS.md's own pre-existing
"process-count accumulation" framing (Track B item 1, since the 56th pass) with first-hand evidence,
and adds the previously-undocumented TREE-DEPTH dimension: the RAM cost multiplies by branching
factor **and** depth, not just by a flat count of concurrent siblings.

### Finding 2 — an admission-control fix landed, real but only a PARTIAL mitigation

**Fix implemented** (commits: this pass, see AGENTS.md's own pointer once merged): a new plain
`AtomicU32` field, `live_cross_process_fork_children`, added to `litebox_shim_linux`'s `GlobalState`
struct (`litebox_shim_linux/src/lib.rs`) — free-riding on `GlobalState`'s own existing
cross-process-shared-arena placement exactly like `unix_addr_presence`/`shared_pty`, no new shared-
arena wiring needed. Two new `Task` methods (`litebox_shim_linux/src/syscalls/process.rs`):

- `reserve_cross_process_fork_slot`: blocks (via `wait_cx().with_timeout(100ms).sleep()`, the SAME
  real non-busy blocking-wait primitive `sys_clock_nanosleep` itself uses — deliberately NOT a
  `core::hint::spin_loop` busy-wait, which would burn a full host CPU core waiting for another OS
  process's startup, worsening the exact problem this targets) until the counter drops below a cap
  of 6, then reserves a slot. Fails OPEN after 80 x 100ms (~8s) of blocking rather than ever
  deadlocking a fork permanently.
- `release_cross_process_fork_slot`: saturating decrement, called from both `sys_wait4` cross-process
  reap sites (the parent-side "this child is fully done" point — `Process` itself deliberately has no
  `GlobalState` access of its own, by the same isolation discipline as its other fields) and from
  both `spawn_cross_process_fork_child` call sites when the spawn itself returns `None` (undoing an
  optimistic reservation immediately rather than leaving it stuck until a reap that will never come).

Gates both real production cross-process-fork spawn call sites: `Task::try_cross_process_fork`
(`#[cfg(target_arch = "x86_64")]`, the primary path) and `Task::do_clone`'s own separate inline
`spawn_cross_process_fork_child` call (the GPR-snapshot/native-fork fallback attempt). Compiles clean
(`cargo build --release -p litebox_runner_linux_on_windows_userland`, no new warnings).

**Live re-test result, same exact repro, freshly rebuilt binary**: **real, measurable, but NOT
sufficient.** Process count grew more slowly (5 → 10 → 14 → 17 → 20 → 23 → 28 → 29 over ~2 minutes,
vs. the pre-fix run's fast climb to 33 within roughly the same window) and RAM decline was gentler for
the first ~90 seconds (plateaued around 15-20 processes / 2-2.7GB free for several polls, genuinely
different from the pre-fix run's uninterrupted freefall) — **but the run never actually reached even
`WM_POLL n=1`** (slower overall, due to the admission-control blocking itself adding real wall-clock
delay) **and eventually reached the SAME crater magnitude anyway**: 28-29 processes, 0.17-0.31GB free,
forcing the same emergency `Invoke-CimMethod -MethodName Terminate` cleanup
(`.wfgy/pass76_fixed_snapshot_poll12.txt` has the mid-decline snapshot: pid 2344 alone, 1351MB working
set, had spawned 5 further large (600-700MB) children, each with its own small re-exec helper —
i.e. a single subtree independently reached something close to the intended cap of 6).

**Why the cap did not hold globally, best-evidenced explanation (not yet debugger-confirmed)**: the
`Task::wait_cx().with_timeout(100ms).sleep()` retry loop fails OPEN after ~8s specifically so a
missing/delayed `wait4()` can never deadlock every future fork. Under this workload, that is
apparently the COMMON case, not the rare escape hatch: `SharedKernelStateProvider::
attach_shared_kernel_state` (`litebox_platform_windows_userland/src/lib.rs`) DOES correctly persist
its attached offset back into `SHARED_LITEBOXX_OFFSET`/`SHARED_GLOBALSTATE_OFFSET` for re-export to
further descendants (read the function in full before assuming otherwise — an earlier draft of this
same pass mis-read the function as NOT doing this, then found the `offset_cell.store(...)` call a few
lines further down and retracted that theory), so the counter SHOULD be genuinely global across the
whole fork family by design. The much more likely explanation, consistent with both this pass's
numbers and the 75th pass's own `~83-140ms` per-fork timing measurement: **a real XFCE desktop needs
several long-lived daemons alive SIMULTANEOUSLY for its whole runtime (Xvfb, dbus-daemon,
xfce4-session, xfwm4, xfsettingsd, xfce4-panel, Thunar-as-desktop-manager, etc.) — each of which pays
its own ~350MB-1.1GB peak working-set cost ONCE during its own startup, and (ordinary Windows working-
set behavior) never gives that memory back for as long as it stays alive.** These processes are never
`wait4()`'d (they are not supposed to exit), so `reserve_cross_process_fork_slot` blocks against them
correctly, but — being genuinely necessary, long-lived, and more numerous than the cap of 6 — every
admission attempt beyond the first handful just burns its full 8-second budget and then fails open
rather than actually being prevented. **This makes the fix a rate-limiter under sustained legitimate
concurrency, not a true concurrency cap** — real (it measurably slowed the climb and delayed the
crater), but insufficient alone.

**Conclusion, refining Track B item 1's framing**: this is not primarily a fork-tree SCHEDULING
problem (my fix targets exactly that, and only partially helps) and the "process-count accumulation"
name is slightly misleading — the real fix needs to reduce PEAK PER-PROCESS RESIDENT MEMORY itself
(task item 2's branch, not item 3's), since the process count a real desktop needs simultaneously
alive is not, by itself, unreasonable (10-15 daemons is normal for a real Linux XFCE session too).
**Confirmed NOT a lifecycle/leak bug**: both this pass's `Invoke-CimMethod -MethodName Terminate`
cleanups fully recovered RAM immediately (8.07GB and 7.97-8.06GB free respectively) with zero
stragglers either time — every process really was live and doing real (or at least resident) work,
not zombied or leaked.

**Pickup, precise**: (1) decompose the ~350MB-1.1GB per-cross-process-fork-child working set into its
real components — is it the merged/rewritten in-memory rootfs representation itself staying resident
after the ~83-140ms build step, litebox's own per-process guest-memory-emulation bookkeeping (VMA
tracking, page tables, `PageManager` structures), or ordinary Windows loader/image overhead for a
10.7MB `.exe` plus its DLL dependencies? A debug-build `cdb -pv` heap-diff (before vs. after
`default_fs_multi_layer` returns, before vs. after the guest's own `execve` into its real target
binary) on a MINIMAL single-fork repro (not a full desktop boot — too RAM-risky to debug-attach on)
is the natural next tool; `LITEBOX_DIAG_FORK_TIMING=1` already proves the TIME cost is cheap, so this
needs a genuinely separate memory-focused measurement, not a re-read of the timing diagnostic. (2) If
the dominant cost turns out to be the merged rootfs representation, the real fix is making it
genuinely SHARED (read-only, mmap/shared-arena-backed, one physical copy for the whole fork family)
rather than rebuilt into each process's own private heap — a materially bigger design change than
this pass's admission-control fix, but the one likely to actually close the gap. (3) The admission-
control fix itself is safe to keep (fails open, cannot deadlock, real evidence it slows growth) but
should not be mistaken for a complete fix in any future pass — do not re-raise the cap expecular a
bigger number alone will help; the bottleneck is per-process size, not slot count. (4) Neither boot
this pass reached `WM_POLL` at all (admission-control's own added latency pushed the whole run later)
— re-verify the 75th pass's own `WM_POLL n=6`/11-window/`XCENSUS_WINDOWS` progress still reproduces
once whatever fix actually closes the RAM gap lands; this pass did not retest that specifically.
`DE_UP` was NOT reached this pass, and no browser/app verification was attempted (would have required
reaching `DE_UP` first, which did not happen) — chrome-devtools MCP was also unavailable this pass
(`CONNECT_TIMEOUT`, reported to the user, not investigated further as out of scope for this pass).

Evidence files (`.wfgy/`, gitignored, disk-only): `pass76_crater_procsnapshot.txt` (pre-fix, 33-process
crater snapshot), `pass76_fixed_snapshot_poll12.txt` (post-fix, 28-process mid-decline snapshot),
`pass76_boot1.out.log`/`.err.log` (pre-fix full boot log, UTF-16LE), `pass76_boot2_fixed.out.log`/
`.err.log` (post-fix run — note: `litebox_diag::process_timeline=debug` output was NOT observed in
the post-fix child processes' stderr despite being set in the parent's `$env:LITEBOX_LOG`; the
cross-process-fork child environment-block construction in `process_fork.rs` builds a curated env var
list rather than forwarding the parent's full environment, so `LITEBOX_LOG` itself may not propagate
to forked children — not confirmed, but explains the missing `DIAG_TIMELINE`/`execve` correlation this
pass wanted and did not get; worth checking directly in a future pass since it would affect every
prior pass's diagnostic-logging assumptions for anything past the FIRST fork generation).

## 77th pass (2026-09-23) — real memory profile of a single litebox process; a real, measured,
## verified fix landed (2x host-allocator commit waste); confirmed insufficient alone; the
## eager-full-fork-copy mechanism identified as the strongest remaining candidate

**Assignment**: the 76th pass ended with the crater refined to "reduce PEAK PER-PROCESS RESIDENT
MEMORY, not concurrency" but no decomposition of what that memory actually was. This pass's job
was to get a real memory profile of a single litebox guest process and root-cause+fix the real
driver, not guess.

### Method note: `Start-Process -ArgumentList` argument-splitting trap

Every early measurement in this pass was silently wrong until this was found: `Start-Process
-ArgumentList @(...,"-c","sleep 6")` does NOT reliably quote a multi-word array element for the
child's real Win32 command line — `Get-CimInstance Win32_Process | select CommandLine` showed the
guest received `-c sleep 6` as THREE separate argv entries, so `bash -c` ran the script `"sleep"`
alone (`sleep: missing operand`, guest pid 1 exits in ~1-2s instead of actually sleeping). Fix:
explicitly wrap the risky element in literal quotes before building the array, e.g.
`$scriptArg = '"sleep 6"'` then pass `$scriptArg` as the array element — verified via
`Get-CimInstance Win32_Process | select CommandLine` showing `-c "sleep 6"` correctly quoted
before trusting any measurement built on it. Any future pass measuring per-process memory via
`Start-Process` must verify the real received command line the same way BEFORE trusting numbers
built on it — this cost most of this pass's early measurement cycles.

### Finding 1 — clean baseline measurements (`Get-Process WorkingSet64`/`PrivateMemorySize64`,
### release binary, `LITEBOX_PROCESS_FORK=1`, single combined launch+poll PowerShell call to avoid
### cross-turn timing gaps)

| scenario | image | processes | max WS | max Priv (committed) |
|---|---|---|---|---|
| single guest proc, no guest fork | `debian:stable-slim` | root 68.3MB/123.7MB + small re-exec helper 5.7MB/1.3MB | | |
| single guest proc, no guest fork | `debian-xfce` (huge merged rootfs) | root 84.1MB/130.2MB + helper 5.7MB/1.3MB | | |
| `sleep 6 \| cat` (2 real guest forks) | `debian:stable-slim` | 6 processes: 3 "real" (78.2/125.4, 61.6/122.9, 61.6/123.1 MB) + 3 small helpers (5.7-10.3/1.3MB) | | |

Two things established directly: (1) EVERY host process, root or forked, pays a roughly CONSTANT
~120-130MB Priv-committed fixed floor, independent of image size (`debian-xfce`'s much bigger
merged rootfs added only ~16MB over `debian:stable-slim`'s baseline — directly refuting the
"per-process cost scales with rootfs/mergedidx size" theory the 76th pass's own pickup list
raised as its first candidate); (2) a "two-process-per-logical-fork" host process shape is real
and already known (76th pass) but the SMALL member (~5.7-10MB) is not the interesting one — the
~120-130MB "large" member is.

### Finding 2 — the fixed floor traced to a real, concrete bug: 2x commit waste in the host's own
### global allocator

`LITEBOX_DIAG_ALLOC=1` (existing diagnostic, `litebox_platform_windows_userland/src/lib.rs`,
logs every `WindowsUserland::alloc` call — the `#[global_allocator]`'s own OS-backing function)
on the `debian:stable-slim`/`sleep 3` minimal repro showed ~20 allocation events totalling
~108MB committed, ALL before "Pulling OCI image" even prints — i.e. pure Rust-runtime-startup
cost, unrelated to any guest workload. Sizes: 3×8KB, 13×4MiB, 1×8MiB, 3×16MiB.

Live `cdb -pv` (debug build, `target/debug/litebox_runner_linux_on_windows_userland.exe`, per
`AGENTS.md`'s own standing guidance for trustworthy stacks) with a breakpoint on
`kernelbase!VirtualAlloc2` (`.wfgy/pass77_cdb_script.txt`/`_script2.txt`, output
`.wfgy/pass77_cdb_out.log`/`_out2.log`) traced every one of the 13×4MiB hits to the IDENTICAL
call chain: `alloc::raw_vec::RawVec::grow_one` → `Vec<clap_builder::builder::arg::Arg>::push` /
`Vec<clap_builder::builder::arg_group::ArgGroup>::push` → `clap_builder::builder::command::
Command::group` → `litebox_runner_linux_on_windows_userland::impl$13::augment_args` — i.e.
`clap`'s own CLI-argument-definition construction, which runs once per process startup for every
host process this runtime ever creates (root AND every cross-process-fork child, since each is a
freshly `CreateProcess`'d re-invocation of the same binary re-running this same startup burst).

Root cause: `WindowsUserland::alloc` (`litebox_platform_windows_userland/src/lib.rs`, `impl
litebox::mm::allocator::MemoryProvider for WindowsUserland`) computed
`size = max(layout.size().next_pow2(), max(layout.align(), 0x1000) << 1)` — an unconditional 2x
commit inflation, justified by a doc comment inherited from an `mmap`-based platform ("`mmap`
provides no guarantee of alignment, so double the size"). But this function ALREADY constructs a
`MEM_ADDRESS_REQUIREMENTS` extended parameter for `VirtualAlloc2` (for `LowestStartingAddress`/
`HighestEndingAddress`) and that same struct has a native `Alignment: usize` field
(`windows-sys 0.60.2`, confirmed via the vendored crate source) that was set to `0` (unused)
at EVERY ONE of the 4 call sites in this file that construct this struct (confirmed by grep) —
Windows has a direct way to request a correctly-aligned base address and this code was silently
falling back to "commit 2x and hope", never a deliberate choice (no doc comment anywhere claims
`Alignment` was tried and rejected). Grepped and confirmed: `M::alloc`
(`litebox::mm::allocator::MemoryProvider::alloc`) has exactly ONE call site in the whole
codebase — `SafeZoneAllocator::new()`'s buddy-heap rescue closure
(`litebox/src/mm/allocator.rs`) — which ALWAYS passes a self-aligned
`Layout::from_size_align(page_aligned_size, page_aligned_size)`, so `layout.align() ==
layout.size()` on every real call, making `Alignment: layout.align()` both safe and exactly
sufficient.

**Fix** (`621ee1a`, `litebox_platform_windows_userland/src/lib.rs`'s `WindowsUserland::alloc`):
request `size = max(layout.size().next_pow2(), max(layout.align(), 0x1000))` (no `<< 1`) with
`Alignment: max(layout.align(), 0x1000)` set on the `MEM_ADDRESS_REQUIREMENTS`. Kept a defensive
fallback (never observed to trigger) that retries with the old oversized/`Alignment: 0` request
if the OS ever refuses the explicit alignment, so this `#[global_allocator]` — every allocation
in the whole process — cannot start failing outright from this change.

**Verification**:
- Correctness: debug build, `debian:stable-slim`, a script exercising echo/sleep/pipe(fork+exec)/
  `ls | wc -l` (a second real fork+exec chain) — exit 0, all output correct
  (`.wfgy/pass77_correctness.out.log`: `CORRECTNESS_OK_10`, `pipeline_test` round-tripped through
  `cat`, `ls / | wc -l` returned a real count `13`).
- Memory, release build, same measurement methodology as Finding 1:

| scenario | before (Priv) | after (Priv) | reduction |
|---|---|---|---|
| single proc, `debian:stable-slim` | 123.7MB | 76.6MB | -38% |
| single proc, `debian-xfce` | 130.2MB | 84.5MB | -35% |
| forked pipeline root, `debian:stable-slim` | 125.4MB | 77.0MB | -39% |
| forked pipeline child ×2, `debian:stable-slim` | 122.9-123.1MB | 74.5-74.7MB | -39% |

Consistent ~35-40% Priv-committed reduction across every process shape tested, root and forked
alike, as expected (this fix touches the FIXED per-process floor every host process pays, not a
guest-workload-dependent cost).

### Finding 3 — the fix is real but NOT sufficient alone: the full XFCE boot still craters at the
### same magnitude

Same exact 76th-pass repro (`LITEBOX_PROCESS_FORK=1`, `GLIBC_TUNABLES=...`, `--oci-image
docker.io/linuxserver/webtop:debian-xfce --resume-from .wfgy/de_only_xcensus_seed3.tar --
/bin/bash /de_only.sh`, `Start-Process -RedirectStandardOutput/-RedirectStandardError` + a
parallel polling loop with an automatic `Invoke-CimMethod -MethodName Terminate` kill switch at
<1.3GB free), release binary with the 77th-pass fix applied
(`.wfgy/pass77_boot1.out.log`/`.err.log`):

Reached (cleanly reproduced, matching the 75th/76th passes, confirming NO regression from the
fix): `DE_ONLY_START` → `XSOCK_WAIT_DONE` → `DBUS_UP` → `DIAG_ENV` (correct `DISPLAY=:1`) →
`DE_LAUNCHED_DIRECT` → `WM_POLL n=1..4` (`_NET_SUPPORTING_WM_CHECK` "not found", same
forward-progress marker as the 75th pass) → `XCENSUS_ROOTPROP` real census data. Free RAM fell
from 6.98GB to 0.82GB over ~28 polls (~2 minutes) as process count climbed 5→28, at which point
the operator kill fired; `Invoke-CimMethod -MethodName Terminate` fully recovered RAM to 8.17GB
free within 2 seconds, zero stragglers (same "not a leak" conclusion the 76th pass already
established, reconfirmed).

**28 processes at ~0.82GB free is the SAME crater magnitude the 76th pass's own admission-control
fix alone produced (28-29 processes, 0.17-0.31GB free)** — i.e. this pass's real, verified,
~35-40%-per-process fix did NOT meaningfully change the crater's ultimate process-count ceiling
or severity. Arithmetic check: if the crater were driven purely by the FIXED per-process floor
this pass fixed, 28 processes at the OLD ~125MB/process would be ~3.5GB, and at the NEW
~77MB/process would be ~2.15GB — both far short of accounting for the ~7-8GB actually consumed
by the time the crater hits. This confirms Finding 1's own single-fork isolation measurement
(constant ~120-130MB regardless of rootfs size) was measuring only a SMALL, now-partially-fixed
slice of the real per-process cost — the dominant driver is something that scales with what a
REAL running guest program (Xvfb, dbus-daemon once it has real connections, xfwm4, etc.)
actually accumulates, not a fixed startup constant.

### Finding 4 — the eager-full-fork-copy mechanism: the strongest remaining candidate, found by
### direct code reading, NOT yet fixed

Both of litebox's two fork implementations copy a forking process's ENTIRE non-shared memory
footprint into the child, unconditionally, on EVERY single fork call, with no copy-on-write and
no read-only fast path:

- **Thread-based path** — `Vmem::duplicate` (`litebox/src/mm/linux.rs`, ~lines 1679-1719): for
  every VMA that is neither `VM_SHARED` (remapped, not copied) nor `PROT_NONE` (empty, nothing to
  copy), it reads the FULL source region's live bytes (`source_ptr.to_owned_slice`), inserts a
  fresh destination mapping with `populate_pages_immediately = true`, and writes the full byte
  copy in — regardless of whether the region is writable, read-only, or executable-only.
- **Cross-process path** — `copy_one_group` (`litebox_platform_windows_userland/src/
  process_fork.rs`, called from the per-group copy loop around line 2065; that same file's own
  doc comment at lines 2089-2109, "PASS 144", explicitly confirms "`copy_one_group` above commits
  every reservation-group span as blanket `PAGE_READWRITE`... after every group's bytes are
  copied") — copies EVERY reservation group (the coarse span the ELF loader originally reserved
  together, typically covering a whole binary's or shared library's PT_LOAD segments including
  its `.text`/`.rodata`) via `WriteProcessMemory`, unconditionally, then fixes up per-region
  permissions afterward. No distinction is made between a group that is entirely read-only/
  never-written since it was mapped and one that has genuinely diverged.

Why this is the strongest remaining candidate: real Linux `fork()` makes this near-free via COW
— read-only pages (and even writable pages, until actually written) are SHARED between parent and
child, so forking a process that has loaded, say, 40-80MB of shared-library code (glibc, libX11,
libdbus, and — once XFCE's real desktop components run — GTK/cairo/pango/glib) costs almost
nothing per fork. litebox pays the FULL byte-copy cost of that same library code on EVERY fork,
and — worse — litebox's own rootfs backend is ALREADY sharing those exact bytes efficiently
across processes via a cross-process `mmap`'d view (`litebox_packager::oci::read_cached_layer`,
`Cow::Borrowed` over a `Box::leak`'d `memmap2::Mmap`, confirmed by direct code reading,
`litebox_packager/src/oci.rs` ~lines 300-641) — so the ELF loader's INITIAL load of a binary's
segments may well already be reading from an efficiently-shared source, and this fork-copy step
needlessly re-privatizes a private, redundant copy of it into every single forked descendant.
This mechanism scales with what a process has ACTUALLY LOADED (unlike the fixed host-allocator
floor this pass fixed), matching Finding 3's own arithmetic gap, and matches the 76th pass's own
qualitative observation that later-generation/deeper fork-tree processes are the expensive ones.

**Deliberately NOT attempted this pass**: implementing real COW or read-only-region sharing
across a Windows process boundary is a large, correctness-critical redesign of precisely the
subsystem responsible for this whole investigation's worst historical bugs (`ADVISORY-001`
§3N's tcache corruption, the whole Track B fork-fix saga) — attempting it without much more
runway for verification than remained in this pass would risk exactly the kind of regression
this project's own standing discipline exists to prevent. Quantitatively confirming Finding 4
(not just the qualitative code-reading argument above) needs a `cdb`/ETW heap-diff comparing a
REAL daemon's committed-memory total immediately pre-fork vs. immediately post-fork-pre-exec
(e.g. `dbus-launch`→`dbus-daemon`, which really does load real shared libraries before forking)
— the `sleep`-only minimal repro this pass used never loads enough library content for the
effect to show clearly in a single-fork isolation test, which is why Findings 1-2's own
measurements (correctly) showed a roughly constant, small per-process floor.

**`DE_UP` was NOT reached this pass.** chrome-devtools MCP was re-checked (this pass's own tool
listing) and remains `CONNECT_TIMEOUT` — moot this pass since `DE_UP` was never reached, so no
browser/app verification was attempted.

**Pickup, precise**: see `AGENTS.md`'s own Track B item 1 pickup list, item (0), for the exact
next step (quantitative pre/post-fork heap-diff on a real daemon chain) before any fix attempt on
the eager-copy mechanism.

Evidence files (`.wfgy/`, gitignored, disk-only): `pass77_cdb_script.txt`/`_script2.txt` (cdb
breakpoint scripts), `pass77_cdb_out.log`/`_out2.log` (full VirtualAlloc2 call-stack captures),
`pass77_diagalloc.err.log` (`LITEBOX_DIAG_ALLOC=1` startup allocation trace),
`pass77_correctness.out.log` (post-fix correctness verification), `pass77_boot1.out.log`/
`.err.log` (post-fix full XFCE boot attempt, reached `WM_POLL n=4` before the RAM-crater kill).

## 78th pass (2026-09-23) — measured the 77th pass's shared-library-COW theory instead of guessing

Added a permanent `env_flag`-gated diagnostic (`LITEBOX_DIAG_FORK_VMA_BREAKDOWN=1`, off by default,
read-only, zero behavior change) classifying every fork's copied bytes at both call sites
(`litebox_shim_linux/src/syscalls/process.rs`: thread-based `do_clone` and the cross-process
`copy_one_group`-plan site) by reusing `is_file_backed`/`VmFlags` data already carried — no new
bookkeeping. Real result, live `bash -c` fork chain (8+ forks, `ls`/`cat`/`grep`/`sort`/`wc`/`sed`/
`find`, both fork paths cross-validated identical): read-only file-backed bytes are ~29% of copied
bytes (`file_ro_bytes=3690496`/`copied_total=12406784`, 17 regions) — real but MODERATE, not
dominant; ~71% is genuinely anonymous heap/stack data no such fix could skip.

Decision: did NOT implement the skip-copy fix this pass — moderate (not dominant) payoff, `VmArea`
tracks only an `is_file_backed` BOOL with no file/inode/offset identity (needed to safely prove
"same backing the rootfs already `mmap`s", itself a nontrivial addition), and this subsystem's
documented worst-bug history (ADVISORY-001 §3N) makes a rushed fix a bad trade here. Incidentally
reproduced (with the new diagnostic OFF too, so unrelated to it) a pre-existing bug: `ls | wc -l`
inside `bash -c` intermittently SIGSEGVs/SIGABRTs a pipeline child (`free(): invalid pointer`/
signal 11), consistent with the known concurrent-fork tcache-corruption class (ADVISORY-001 §3N) —
not chased further, out of scope. `DE_UP` not reached (no functional change made, so no new boot
attempt).

## 79th pass (2026-09-23) — measured fork-then-immediately-`execve()` before touching the copy loop

Debug build, `LITEBOX_LOG=litebox_shim_linux::syscalls::process=debug`, thread-based path, `bash -c`
loop of `/bin/true`/`/bin/echo`. With the documented `GLIBC_TUNABLES` workaround: 20/20 forks were
plain `fork()` (`CloneFlags(18874368)`, `CLONE_VM` absent — bash never uses `vfork`/`posix_spawn`'s
VM-sharing path for external commands); 20/20 had `execve()` as the literal FIRST syscall, zero
intervening syscalls; fork→`execve` gap averaged 127ms (60-205ms, nothing else happening in that
window) vs. actual post-exec runtime (`execve`→`exit_group`) averaging 23ms — ~85% of every cycle's
wall time is eager-copy, 100% wasted the instant `execve` fires. Incidental finding, SAME repro
WITHOUT the tunable: 44% (7/16) per-fork crash rate (SIGABRT/SIGSEGV before `execve`) — a new,
precise live reconfirmation ADVISORY-001 §3N's tcache-corruption class is still fully live on
`main`, not just historical (0/20 with the tunable). Cross-process fork (`LITEBOX_PROCESS_FORK=1`),
same repro: 20/20 clean but ~830-930ms/cycle (~6x thread-based), dominated by the already-documented
per-child rootfs rebuild (76th pass) not VM-copy — a skip-copy fix's payoff is thread-based-path-only.

Decision: did NOT implement a skip/defer-copy fix. The "peek next syscall, skip copy if `execve`"
shape isn't a static check: the child must execute real instructions (fork-return trampoline, the
`execve` stub itself) before it CAN call `execve`, needing those pages valid at its relocated
address first — real Linux gets this free from hardware page tables, litebox's thread-based path
has no native COW/section-object primitive wired into `VmArea`. A real fix needs genuine per-page
LAZY population via a fault handler (reusing `fork_verify.rs`'s own `AddressRelocations` map) — a
new primitive, not a narrow patch, that must coexist with `fork_verify.rs`'s VEH single-step
healing on the SAME faulting instruction stream. Given this pass's own fresh 44%-per-fork
live-corruption finding in this exact subsystem, layering a second invasive change into the same
path in one sitting was judged unsafe — deferred to its own multi-pass investigation (mirrors 78th
pass declining a smaller-scoped version for the same reason).

`DE_UP` not attempted: mid-session host RAM was additionally consumed by unrelated processes
(chrome ~5.7GB WS, `rustc` ~1.36GB WS — neither litebox), free RAM fell from the session's initial
5.56GB to under 300MB with ZERO litebox processes running — environmental, not a regression. All
litebox processes cleanly terminated, confirmed none left running.

## 80th pass (2026-09-23) — confirmed admission-cap tuning is a dead end for the RAM crater, by direct measurement

Fresh release rebuild with 76th+77th both compiled in (correctness re-verified,
`.wfgy/pass80_correctness*.out.log`), then tried tightening `CROSS_PROCESS_FORK_CONCURRENCY_CAP`
6->3 (`litebox_shim_linux/src/syscalls/process.rs`) and ran two fresh `de_only.sh` boots on that
binary (`Start-Process` + parallel RAM-trajectory polling + an automatic `Invoke-CimMethod Terminate`
kill switch, `.wfgy/pass80_ram_trajectory{,2}.csv`/`pass80_boot{1,2}.out.log`): a lower-headroom
start (5.78GB free) craterd at WM_POLL n=1/t=92s/18 procs/1.14GB free; a higher-headroom start
(~6.8-7GB free) reached WM_POLL n=4/t=140s/25 procs/0.79GB free before the kill switch fired — the
exact same WM_POLL n=4 ceiling the unmodified cap=6 binary already reached in the 77th pass's own
Finding 3 (28-29 procs/0.82GB free, ~120s), just ~20s slower and with fewer procs alive at the
crater instant (18-25 vs 28-33).

Tightening the cap bounds peak INSTANTANEOUS concurrency but not how far the boot gets — REVERTED
to 6 (net diff: a doc comment only) since 3 added latency for zero depth benefit. This directly
confirms the crater is driven by CUMULATIVE committed memory across the boot's whole fork history
(WM_POLL's own loop re-forks `xprop` every 5s regardless of the cap) — i.e. the SAME underlying
cost the 79th pass's eager-fork-copy theory already identified, just observed from the concurrency
angle instead of the per-fork-size angle. Practical effect: admission-control tuning is now CLOSED
as a dead end — the only lever left that could plausibly change the outcome is genuine per-page
lazy population (already scoped as its own dedicated multi-pass investigation, not attempted again
this pass given the same live tcache-corruption risk without `GLIBC_TUNABLES`). `DE_UP` was NOT
reached this pass; chrome-devtools MCP remained `CONNECT_TIMEOUT` (moot, `DE_UP` never fired).

## 81st pass (2026-09-23) — the "whole-batch-deferred" shortcut is not viable either; a second, independent correctness obstacle found, on top of the 79th pass's memory-must-exist-to-execute one

Task: re-examine the 79th pass's declined "peek next syscall, skip copy if `execve`" idea in a
narrower framing — defer the ENTIRE per-fork VMA-copy batch (not per-page) from fork time to
"immediately before the child's first non-`execve` syscall is dispatched", skipping it entirely
when that first syscall IS `execve`. Investigation only, via full code reading — no code changed,
so no rebuild and no live boot were performed (nothing to verify).

Read in full: `Vmem::duplicate` (`litebox/src/mm/linux.rs:1441-1719+`, the thread-based path's
eager-copy loop); the cross-process fork-plan execution (`copy_one_group`,
`litebox_platform_windows_userland/src/process_fork.rs`); `Task::do_clone`'s address-space-decision
and `CLONE_VFORK` branch in full (`litebox_shim_linux/src/syscalls/process.rs:3672-3930`); and
`Process::wait_for_vfork_done`/`signal_vfork_done` (`process.rs:547-573`).

**Finding 1 (reconfirms 79th):** the child must execute real guest instructions — the fork-return
trampoline, then whatever code leads up to the `execve` syscall stub itself — before it can ever
reach a syscall-dispatch checkpoint, and those instructions need valid, populated memory (at least
code + stack) at their relocated addresses to execute at all. There is no hardware/OS page-fault
trap wired into `VmArea` on the thread-based path (confirmed again by direct reading of
`Vmem::duplicate`, unchanged since 79th) and no window in which the child can run before the copy
completes. The "whole-batch" framing does not remove this: it just changes WHEN the (still
mandatory, still full) copy has to happen relative to a checkpoint that itself cannot be reached
without the copy already having happened.

**Finding 2 (new this pass, independent of Finding 1):** even granting a hypothetical mechanism
that could reach a "first syscall dispatch" checkpoint without a completed copy (e.g. by trapping
the very first guest instruction some other way), that checkpoint is a SYSCALL event, not a
MEMORY-WRITE event. A plain `fork()`ed child is fully entitled by POSIX to WRITE its own memory —
stack locals, TLS, glibc's post-fork malloc-arena/PID-cache bookkeeping, `pthread_atfork` handlers
— with ZERO intervening syscalls before it ever reaches `execve`. Sharing the parent's live pages
until the checkpoint (the only way the deferral could actually save the copy) would let such a
write silently corrupt the PARENT's real, live memory, undetected, since there is no syscall for
the checkpoint to intercept at that moment.

This is not hypothetical inside this codebase: litebox already implements exactly this
share-instead-of-copy, materialize-only-at-`execve` pattern for real `vfork()`
(`do_clone`'s `is_process_clone`/`vforked` branch, `process.rs:3893-3925` — the child genuinely
shares the parent's `Arc<PageManager>`, gets a brand-new one only at `execve` via `ElfLoader::
load`'s vfork-detach step, and the parent is unconditionally blocked in `wait_for_vfork_done` for
the entire window). It is safe ONLY because of two properties plain `fork()` does not have: (1)
real `vfork()` carries a POSIX-mandated UB contract forbidding the child from touching any memory
but the return-value variable before `execve`/`_exit` — a contract the GUEST voluntarily accepts by
calling `vfork()` instead of `fork()`; (2) litebox additionally blocks the parent for the whole
window, removing any concurrent-access hazard on top. The 79th pass's own measurement already
established the actual workload (bash's external-command fork) uses plain `fork()`
(`CloneFlags(18874368)`, `CLONE_VM` absent) — specifically because bash does real bookkeeping in
the child (job-control state, signal-mask restoration, fd-redirection setup) that a
vfork-shared-address-space child is not allowed to do. Retrofitting vfork's sharing semantics onto
plain `fork()` would silently violate `fork()`'s own POSIX contract (an independent address space,
immediately) for any guest program that writes memory in the fork-to-exec gap, with no way to
detect the violation short of the same hardware/OS-level COW+page-fault trapping Finding 1 already
established litebox does not have.

**Conclusion:** the narrower "whole-batch, not per-page" framing does not remove the correctness
obstacle the 79th pass found — it only removes the performance argument for doing it per-page
(syscall-granularity is cheaper to check than page-granularity), while leaving the underlying
hazard (a write can happen before any syscall at all) completely unaddressed, on top of Finding 1's
already-confirmed "child needs valid memory to execute even its first instruction" obstacle. This
independently re-confirms the 79th pass's decision not to implement a defer/skip-copy fix, and adds
a second, independent, arguably more fundamental reason on top of the first. The only remaining
viable path for this whole line of investigation is genuine per-page lazy population via a real
page-fault handler (real hardware/OS-level COW), exactly as the 79th pass scoped it — no shortcut
around that requirement exists at either per-page or whole-batch granularity.

No code changed this pass. No live boot attempted (nothing to verify — this was a design-safety
investigation, not an implementation pass). Host RAM checked at session start: 6.18GB free
(`FreePhysicalMemory=6330168`/`TotalVisibleMemorySize=15987768` KB), zero litebox processes
running — clean baseline, unused this pass since no boot was attempted.

## 83rd pass (2026-09-23) — implemented the lazy reserve-then-commit-on-fault primitive; real win for fork-then-execve, a genuine unresolved bug for fork-without-execve

Full detail behind AGENTS.md's own condensed 83rd-pass bullet.

### Design implemented

New module `litebox_platform_windows_userland/src/lazy_fork_commit.rs`, wired into the real
production cross-process fork path (`litebox_platform_windows_userland::process_fork::
spawn_process_fork_child`, parent side; `litebox_runner_linux_on_windows_userland::
diag_process_fork_globalstate_probe_inner`, child side -- despite its "diag" name this is the
actual production entry point, per the existing three `LITEBOX_DIAG_PROCESS_FORK_*` gates the
parent already sets unconditionally for every real cross-process fork).

Parent side: `classify_lazy_eligible_groups(group_relocations, vma_layout)` returns, per group, a
bool -- `true` only when `LITEBOX_LAZY_FORK_COMMIT=1` is set AND no `vma_layout` range overlapping
that group carries `VM_EXEC` (CODE groups stay on the existing eager `copy_one_group` path
unconditionally, sidestepping PASS-144's exec-fixup entirely). Eligible groups get
`reserve_group_lazy(child_handle, source_group)` instead of `copy_one_group`: the same
`MEM_ADDRESS_REQUIREMENTS`-forced `VirtualAlloc2` call `copy_one_group` uses for its own step 1,
but with `MEM_RESERVE` only (no `MEM_COMMIT`, no `WriteProcessMemory` loop at all). The group's own
span is serialized (`start-end` hex pairs, comma-separated) into a new internal env var
(`FORK_CHILD_LAZY_RANGES_ENV_VAR`, plus the parent's own PID via
`FORK_CHILD_PARENT_PID_ENV_VAR`), the same environment-block bootstrap channel
`FORK_CHILD_VMA_LAYOUT_ENV_VAR`/`FORK_CHILD_GPRS_ENV_VAR` already use.

Child side: `lazy_fork_commit::install_if_configured()` parses those two env vars; if non-empty, it
`OpenProcess(PROCESS_VM_READ)`s the parent and `AddVectoredExceptionHandler(1, Some(lazy_commit_
veh))`s a new handler. `lazy_commit_veh` checks `ExceptionCode == EXCEPTION_ACCESS_VIOLATION` and
the faulting address against the registered ranges; a miss returns `EXCEPTION_CONTINUE_SEARCH`
immediately (falling through unchanged). A hit: `VirtualAlloc(page_addr, PAGE_SIZE, MEM_COMMIT,
PAGE_READWRITE)` on the CURRENT (child) process, `ReadProcessMemory(parent_handle, page_addr, ...)`
pulls the real bytes from the SAME address in the parent (identity-mapped, cross-process fork's own
existing guarantee), copies them in, returns `EXCEPTION_CONTINUE_EXECUTION` -- the CPU retries the
original faulting instruction, which now succeeds. Deliberately lock-free: no "already populated"
tracking, since a redundant commit+copy on a concurrent same-page fault from another guest thread is
harmless and idempotent.

### Platform-feasibility proof, isolated from litebox entirely (done FIRST, before touching production code)

A standalone ~350-line Rust program (`poc.rs`, compiled directly with `rustc -O`, raw kernel32 FFI
declarations, no windows-sys/litebox dependency, session scratchpad only, never committed)
validated the exact mechanism end-to-end: a "parent" process writes two known 4KB patterns into an
8MB `VirtualAlloc(MEM_COMMIT)` region, spawns itself as a "child" with the region's address+len on
its command line, the child `VirtualAlloc2`s that SAME address `MEM_RESERVE`-only, installs the
same lazy-commit VEH shape, then touches page A (read) and page B (write). Result, 5/5 clean runs:
reserve-only took 8.2-8.9us vs 2.23-2.37ms for an eager commit+copy of the same region; page A read
back the parent's real `0xab` pattern; page B read back its real pre-write `0xcd` pattern THEN the
child's own write (`0xef`) landed correctly; the parent's own copy of page B stayed `0xcd`; and,
decisively, `VirtualQueryEx` from the parent against the CHILD's still-live process (queried via a
`Sleep(2000)` in the child before exit, to avoid `ERROR_ACCESS_DENIED` on a torn-down process)
confirmed the untouched padding region 4MB into the reservation stayed genuinely `MEM_STATE_
RESERVE`, never `MEM_COMMIT` -- the actual resource-savings claim, not just a wall-clock one.
`VirtualAlloc2` is exported by `kernelbase.dll` but this toolchain's `kernel32.lib` import stub has
no forwarder for it (`LNK2019` at link time) -- resolved via `LoadLibraryA`+`GetProcAddress` instead
of a static import.

### Ordering bug found and fixed live

First integration attempt called `install_if_configured()` at the very top of
`diag_process_fork_globalstate_probe_inner`, before `Platform::new()`. Result: the subshell repro
(below) killed outright with ZERO `[lazy_fork_commit]` VEH-entry diagnostic output (a per-entry
counter was added, gated `LITEBOX_DIAG_LAZY_FORK_COMMIT=1`, capped at 40 entries -- printed
nothing). Root cause: `Platform::new()` (`WindowsUserland::new()`, `lib.rs:2966`) is what registers
this process's own main `vectored_exception_handler_entry` via `AddVectoredExceptionHandler(1,
..)` -- and that call's own doc comment (`lib.rs:3053-3068`) says "Nothing else loaded into the
process has any business seeing a guest fault first". `AddVectoredExceptionHandler(1, ..)` always
PREPENDS (last-registered runs first) -- calling `install_if_configured()` before `Platform::new()`
meant the main handler, registered afterward, became the new head, claimed every lazy-range fault
FIRST, recognized none of this mechanism's patterns, and delivered a genuine guest `SIGSEGV` (bash:
"Killed"). Fixed by moving the call to immediately after `Platform::new()` returns
(`litebox_runner_linux_on_windows_userland/src/lib.rs`,
`diag_process_fork_globalstate_probe_inner`) -- confirmed via the VEH-entry counter that
`install_if_configured` now runs and registers successfully, though (see below) this did NOT fully
fix the subshell case.

### Measured win, both builds

`bash -c 'echo hello; sleep 0.2; echo done'` (a real fork-then-immediate-execve -- the dominant real
case per the 79th pass's own 20/20 measurement), `LITEBOX_DIAG_FORK_TIMING=1`:

- Debug, eager baseline (flag unset): 6 groups, `ALL group copies done ... took 102.9946ms total`.
- Debug, lazy (flag set): same 6 groups, 2 marked lazy (`0x111140000..0x111170000` len `0x30000`,
  `0x7fefff6e0000..0x7fefffef0000` len `0x810000` -- the ~8MB guest stack region, exactly the
  canonical "mostly untouched" case this investigation predicted) -- `ALL group copies done ...
  took 33.5315ms total`, ~69% less parent-side time for this one fork. Correct output (`hello`,
  `done`), exit 0, 5/5.
- Release, lazy: `ALL group copies done, 6 group(s), took 5.6691ms total`. Correct output, exit 0.
- Default path (flag unset), both builds: unaffected, confirmed by direct rerun -- 3/3 clean,
  byte-identical output to pre-this-pass behavior.

### The unresolved bug: fork without execve

`bash -c '(echo subshell_child; x=inner_var; echo $x) ; echo parent_after'` -- a bash `(...)`
subshell forks and, since `echo`/variable assignment are bash builtins, the CHILD keeps running
bash's own already-forked, already-copied code directly rather than replacing its address space via
`execve()`. This is exactly the case the lazy mechanism's own correctness argument says should
still work, and the isolated POC proved the underlying platform primitive sound for exactly this
shape of access. In the real integration it is NOT sound yet:

- 5/5 with the flag OFF: clean, `subshell_child`/`inner_var`/`parent_after` all print, exit 0.
- 5/5 with the flag ON (post ordering-fix): `subshell_child` prints, then `/bin/bash: line 1: 2
  Killed ( echo subshell_child; x=inner_var; echo $x )` -- `inner_var` never prints. `parent_after`
  still prints (the OUTER bash's own script continues past the killed job), so a naive
  process-exit-code check reads 0 and would MISS this entirely -- the bug is only visible by
  checking actual expected STDOUT content, not exit codes.
- `LITEBOX_DIAG_FATALDUMP=1` capture: a real `EXCEPTION_ACCESS_VIOLATION` (`code=c0000005`), code
  fetch (`addr==rip`), at `rip=0x7feffffef000`. Precise arithmetic against the two registered lazy
  ranges for that same run (`0x111140000..0x111170000` and `0x7fefff6e0000..0x7fefffef0000`,
  from the `[lazy_fork_commit] install_if_configured` log line) confirms the crash address is
  OUTSIDE both -- `0xff000` bytes above the second (stack) range's own end. `[codewatch]`
  diagnostic for that same fault: `alloc_base=0x7feffffb0000 type=0x20000 (MEM_PRIVATE) protect=0x2
  (PAGE_READONLY) watched=false`. `PAGE_READONLY` does not match anything `copy_one_group` (blanket
  `PAGE_READWRITE`, `0x04`) or the PASS-144 exec-fixup (`PAGE_EXECUTE_READ`/`PAGE_EXECUTE_READWRITE`,
  `0x20`/`0x40`) would ever produce for a group on the EAGER path -- and this address's own
  `alloc_base` matches a group that in an earlier (non-crashing) timing-enabled run was confirmed to
  be on the eager `copy_one_group` path (`0x7feffffb0000..0x7fefffff0000 len=0x40000`), i.e. NOT one
  this pass's own code marked lazy. The mechanism this pass added does not appear to touch this
  memory directly -- the bug's real mechanism is not yet understood.
- Timing-race hypothesis tested directly and REFUTED: added a diagnostic-only
  `LITEBOX_DIAG_LAZY_FORK_ARTIFICIAL_DELAY_MS` env var (`process_fork.rs`, gated, no-op unless set)
  to test whether the lazy path's own dramatic speedup (103ms to 33ms, above) removed timing slack
  some OTHER startup step depended on. Reintroduced 80ms (exceeding the eager path's own real
  ~103ms elapsed time): still 5/5 killed, byte-identical failure signature. Not a simple
  "finishes too fast" race.
- Leading untested hypothesis: an interaction with `fork_verify.rs`'s own watched-code-page
  mechanism -- the crash's own `[codewatch]` diagnostic explicitly logged `watched=false` for the
  faulting page, meaning that mechanism does NOT currently recognize this page as one of its own,
  either a real gap in that recognition or a hint the true cause lies elsewhere. NOT investigated
  further this pass -- needs a live `cdb -pv` attach (debug binary) breaking on this exact
  `EXCEPTION_ACCESS_VIOLATION` class to find what sets `PAGE_READONLY` on this page and why only
  the lazy path exposes it.

### Current state left by this pass

`LITEBOX_LAZY_FORK_COMMIT` defaults OFF (unset). With it unset, this pass's entire new module is
inert -- `classify_lazy_eligible_groups` returns all-`false`, `install_if_configured` returns
immediately on an absent env var, `AddVectoredExceptionHandler` is never called by this module at
all. Confirmed live: 3/3 clean runs of the exact subshell repro above with the flag unset,
byte-identical correct output to what this whole investigation has relied on through the 82nd pass.
Both debug and release builds compile clean. Real desktop boot NOT attempted this pass with the
flag on (would need the subshell-class bug fixed first -- a real XFCE session forks many long-lived
daemons that do not immediately `execve()`, so this exact bug class would very likely recur on the
real boot path, and be far harder to isolate there than in this clean, minimal, 100%-reproducible
standalone repro).

Host RAM: ~6.2GB free at pass start, never approached exhaustion this pass (no full desktop boot
attempted). All stray `litebox_runner*` processes terminated via
`Invoke-CimMethod -MethodName Terminate` between every trial, per standing practice.

## 84th pass (2026-09-23) -- two real cross-process-fork bugs fixed, third precisely re-scoped

Investigating the 83rd pass's "unexplained `PAGE_READONLY`" on the subshell (fork-without-`execve`)
repro. Diagnostic method: the project's own rich `eprintln!`/`litebox_util_log::debug!` gates
(`LITEBOX_DIAG_FAULT_VQ`, `LITEBOX_DIAG_PROCESS_FORK_EXEC_FIXUP`, `LITEBOX_DIAG_MM`,
`LITEBOX_LOG=litebox_shim_linux=debug`), not `cdb` -- sufficient this pass, faster to iterate than
a live debugger attach for a bug whose signature was fully captured by existing structured logging.

**Bug 1 (FIXED, `litebox/src/mm/linux.rs`'s `Vmem::new_adopting_existing_memory`)**: the crashing
page (`0x7feffffef000`, one page short of the group's own end) is `copy_one_group`'s 64KiB
`VirtualAlloc2`/`MEM_ADDRESS_REQUIREMENTS` alignment padding (real Windows-committed memory,
genuinely unmapped-in-the-guest, matches `do_clone`'s own `GRANULE`-rounding, NOT
`VmArea::reserved_extra` -- that's 16MiB, three orders of magnitude too big). A fresh child's own
`sys_mmap` allocator (`get_unmmaped_area`, `litebox/src/mm/linux.rs`) had no record of this padding
at all -- it is real per-VMA guest layout (`vma_layout()`) that carries no notion of the coarser
group rounding -- so it could hand a BRAND NEW guest mmap exactly this address, silently succeeding
(the underlying Windows page was already committed, so `VirtualAlloc(MEM_COMMIT)` on it is a
harmless no-op) and corrupting host memory that, on real 4KiB-granular Linux, would simply have been
part of the SAME, single, adjacent VMA. Confirmed live via `LITEBOX_DIAG_MM=1`: `sys_mmap` returned
exactly `0x7feffffef000`, the guest then `mprotect`'d it `PROT_READ`. Fix: `new_adopting_existing_
memory` now takes the child's OWN real group-relocation spans (`do_clone`'s `groups`, plumbed via
`AddressRelocations::group_relocations()` -> a new `group_spans` parameter ->
`PageManager::new_adopting_existing_memory`) and pre-inserts a `VmFlags::VM_OWN_FORK_PADDING`
placeholder for each FULL span before adopting real per-VMA regions on top (`RangeMap::insert`
narrows/overwrites the placeholder for each real region's own sub-range, leaving it tracked only for
the genuine gaps) -- `allocate_pages`'s own free-space search (`self.vmas.gaps()`/`.overlaps()`) now
correctly treats this padding as claimed. (A parallel widening of `Vmem::duplicate`'s OWN placeholder
sizing was ALSO tried and REVERTED this pass -- `do_clone`'s doc comment establishes `Vmem::
duplicate`'s grouping feeds only a diagnostic/verification path, never real cross-process-fork
placement, and live A/B testing confirmed that change was actively harmful.)

**Bug 2 (FIXED, real but NOT sufficient alone)**: a cross-process-fork child's `SignalState` is
built via `SignalState::new_process` (`adopt_forked_process`, `litebox_shim_linux/src/lib.rs`),
which starts `sigreturn_trampoline` at `0` -- unlike `clone_for_new_task` (the thread-based path),
which correctly preserves it. `LinuxShimEntrypoints::exception`'s own x86_64 sigreturn-trampoline
recognition (`ctx.rip == self.task.sigreturn_trampoline_addr()`) can therefore never match in a
forked child that inherits a parent's already-established trampoline (real memory content correctly
copied by `copy_one_group`; the Task-level ADDRESS that lets litebox recognize a fault there is what
was missing) -- exactly the fork-without-`execve` shape. Fix: threads the forking parent's own
`Task::sigreturn_trampoline_addr()` through `spawn_cross_process_fork_child`'s new `sigreturn_
trampoline` parameter -> a new `LITEBOX_INTERNAL_FORK_CHILD_SIGRETURN_TRAMPOLINE` env var
(`process_fork.rs`) -> `adopt_forked_process`'s new parameter -> `SignalState::set_sigreturn_
trampoline_for_fork_adoption`. Confirmed live the child's own `exception()` now correctly matches
`rip == sigreturn_trampoline_addr()` for a LATER, unrelated signal event in the same run.

Both bugs are REAL, GENERAL correctness fixes, independent of `LITEBOX_LAZY_FORK_COMMIT` -- with
both landed, `LITEBOX_PROCESS_FORK=1` alone (lazy flag unset) runs the subshell repro clean 5/5
(this exact non-lazy case was ALSO already clean on pristine `a7e6a83` -- these two bugs were
latent/unobserved via this specific repro, not a new regression the 83rd pass introduced).

**Bug 3 (re-scoped, not yet fixed this pass)**: with `LITEBOX_LAZY_FORK_COMMIT=1`, the exact
subshell repro STILL crashed 5/5, byte-identical signature, even with both bugs above fixed.
Precisely characterized: the fault DOES reach `vectored_exception_handler`, which DOES redirect the
thread context to `exception_callback` (confirmed via a temporary, since-removed trace immediately
before the `context.Rip = exception_callback` assignment) -- but `LinuxShimEntrypoints::exception()`
was never observably entered afterward (confirmed via a temporary, since-removed `pid`-tagged trace:
the only `exception()` call in a full run belonged to the ORIGINAL, never-forked bootstrap process
handling an unrelated LATER signal, never the crashing forked child).

**Methodological finding, load-bearing for any future pass touching `vectored_exception_handler`**:
adding EVEN GATED, LOW-VOLUME `eprintln!`/extra local variables to that function (tried this pass: a
`pid`/`is_in_guest`/`veh_depth`-enriched `[veh-regs]` line and one new gated print) measurably
REGRESSED the NON-lazy case too (broke the now-clean 5/5 back to 5/5 killed) -- reverting those
specific diagnostic-only additions (keeping the real fixes) restored the clean non-lazy result.
Confirmed via live A/B isolation, not assumed. This function's compiled layout is sensitive enough
that any future instrumentation there needs an A/B test against the non-lazy repro before being
trusted.

`LITEBOX_LAZY_FORK_COMMIT` stayed default OFF; `DE_UP` not attempted this pass.

## 85th pass (2026-09-23) -- Bug 3 root-caused and FIXED; a fourth, deeper bug (Bug 4) found and
left OPEN; `LITEBOX_LAZY_FORK_COMMIT` still not safe for a real boot

Picked up the 84th pass's own precise pickup (a live `cdb -pv` attach on `exception_callback`/
`call_shim`). Before reaching for a debugger, re-derived the crash from first principles by reading
`vectored_exception_handler_entry`/`vectored_exception_handler`/`lazy_commit_veh` side by side and
computing exact address arithmetic against a freshly captured repro log
(`LITEBOX_DIAG_LAZY_FORK_COMMIT=1 LITEBOX_DIAG_FATALDUMP=1`) -- this alone found the real mechanism,
no debugger attach needed this pass.

**Bug 3 root cause, precisely**: the crash's own `[lazy_fork_commit] install_if_configured:
registering VEH for 2 range(s)` line gives the exact lazy ranges for that run; the crash's own `[veh]
RAWREGS`/`[veh-regs] ENTRY` lines give `rsp=0x7fefffeed200` at the moment of the (unrelated) sigreturn-
trampoline instruction-fetch fault. Arithmetic (`awk`, exact, not eyeballed) confirms `rsp` falls
STRICTLY INSIDE the second registered range (`0x7fefff6e0000..0x7fefffef0000`, the lazily-reserved
8 MiB+64 KiB stack group) -- i.e. the child's OWN live stack pointer, at the exact moment ANY
exception is delivered to it, was itself sitting on a page `lazy_commit_veh` had never yet serviced
(confirmed independently: the crash log shows ZERO `[lazy_fork_commit] fault #N serviced` lines
across the entire run, despite `LITEBOX_DIAG_LAZY_FORK_COMMIT=1` being set and confirmed reaching the
child -- the mechanism never got to service its first real page fault before the process died).
Windows delivers EVERY exception -- not only ones whose own fault address lands in a lazy range --
with `CONTEXT.Rsp` set to whatever the CPU held at fault time; for the group anchoring a fresh
child's very first instructions, that is guaranteed to still be a genuinely never-touched (`MEM_
RESERVE`-only) page under this feature, a scenario the ordinary "stack-touching instruction lazily
faults, gets serviced by `lazy_commit_veh`, retries" path (proven correct for the dominant
fork-then-`execve` case) never exercises, because there the fault address and `CONTEXT.Rsp` are
identical and get serviced before anything else can look at them.

**Fix**: `classify_lazy_eligible_groups` (`litebox_platform_windows_userland/src/lazy_fork_commit.rs`)
now takes the child's fork-time `%rsp` (threaded from `spawn_process_fork_child`'s existing
`full_gprs.rsp`, `litebox_platform_windows_userland/src/process_fork.rs`) and excludes whichever
group contains it from lazy treatment, falling back to the ordinary eager `copy_one_group` path for
THAT one group only -- every OTHER pure-data group (heap, etc.) stays lazy exactly as before.
**Verified**: the exact subshell repro's ORIGINAL crash signature (0 faults serviced, `Killed`,
`inner_var` never printed) is GONE, 5/5, both debug and release builds; the fork-then-`execve` repro
(`bash -c 'echo hello; sleep 0.2; echo done'`) remains clean 5/5, both builds, no regression; the
non-lazy default path is untouched (this parameter only affects groups when `LITEBOX_LAZY_FORK_
COMMIT=1`).

**Bug 4 (OPEN, found while re-verifying Bug 3's fix)**: with Bug 3 fixed, the SAME subshell repro
now runs further -- `lazy_commit_veh` DOES service real lazy faults now (3/3 with real parent data
via `ReadProcessMemory`, not zero-filled: `page=0x111156000`/`0x111148000`/`0x111155000`, all below
the child's own `brk=0x111169000`, i.e. genuinely heap pages, not stack) -- but the repro still fails
5/5, both builds, with a NEW, different, equally deterministic signature: `Fatal glibc error:
malloc.c:2601 (sysmalloc): assertion failed: (old_top == initial_top (av) && old_size == 0) ||
((unsigned long) (old_size) >= MINSIZE && prev_inuse (old_top) && ((unsigned long) old_end &
(pagesize - 1)) == 0)` -- SIGABRT (`bash`'s own `Aborted`, not `Killed`), and `subshell_child` now
prints but `inner_var` still never does.

This is a genuine heap-corruption assertion (glibc's `malloc_state.top`-chunk consistency check), and
the mechanism is structural, not incidental: `lazy_commit_veh` services a fault by `ReadProcessMemory`-
ing the PARENT's address space at WHATEVER MOMENT the CHILD happens to touch that page -- for a
fork-WITHOUT-`execve` child, that can be arbitrarily long after `fork()` returned control to the
PARENT's own guest thread. The eager `copy_one_group` path captures every byte SYNCHRONOUSLY, while
the parent's guest thread is still blocked inside the `fork()` syscall handler, so it is immune to
this. This module's whole point is DEFERRING that capture, which only stays correct if the parent's
memory at the same virtual address is guaranteed stable in the meantime -- true for a page the
parent never touches again, false for one it does (its own heap, mutated by its own continued
malloc/free activity while the forked-but-not-exec'd child is still running the same program). The
observed corruption shape (a torn/inconsistent read of a live, concurrently-mutating heap's own
top-chunk bookkeeping) matches this mechanism exactly.

This is a genuine TOCTOU (time-of-check to time-of-use) gap in the module's own "Correctness
argument" doc section, which only proves the CHILD's own access-based deferral is safe and never
establishes that the PARENT's memory at the same address stays stable after `fork()` RETURNS -- a
separate assumption, true for the eager path by construction (parent is blocked during the whole
copy), false in general for the lazy path (parent resumes as soon as `spawn_process_fork_child`
returns, well before any lazy fault is serviced). No fix attempted this pass -- see `lazy_fork_
commit.rs`'s own updated module doc comment (Bug B) for the two candidate real-fix directions (a
fork-time snapshot into a buffer instead of a live parent read, or genuine OS-level COW between the
two separate Windows processes) -- both are real design work, not a quick patch.

**Verification discipline**: both fixes verified 5/5 on BOTH the subshell and fork-then-`execve`
repros, both debug and release builds, with the non-lazy default path reconfirmed byte-identical
(5/5 clean, unset flag). `LITEBOX_LAZY_FORK_COMMIT` stays default OFF -- Bug 4 means it is still not
safe to flip on for `.wfgy/webtop_stack.sh` or any real boot attempt: a real desktop boot forks many
long-lived processes (shells, daemons, e.g. `dbus-daemon`) that do not immediately `execve()` AND
keep running concurrently with a parent that keeps mutating its own heap -- exactly Bug 4's trigger
shape, worse and harder to isolate than this clean, minimal, 100%-reproducible standalone repro.
`DE_UP` not attempted this pass -- the flag remains unsafe to enable for a real boot, and attempting
one anyway would very likely hit Bug 4's heap-corruption class inside `dbus-daemon` or another
long-lived forked-without-exec daemon, at real RAM/wall-clock cost, for a known-bad outcome.

Host RAM: ~6.6GB free at pass start. Full desktop boot NOT attempted (Bug 4 makes the flag unsafe).
All `litebox_runner*` processes cleaned up between trials.

## 86th pass — Bug 4's two candidate fixes both investigated and ruled out; a third found and
## scoped; nothing implemented, flag stays default OFF

Opened by re-verifying the 85th pass's own claims fresh, live, before doing any design work (debug
build, unmodified binary, host RAM ~6.3GB free at start): `LITEBOX_PROCESS_FORK=1` alone (lazy flag
unset), the subshell repro (`bash -c '(echo subshell_child; x=inner_var; echo $x); echo
parent_after'`) printed all three of `subshell_child`/`inner_var`/`parent_after` cleanly; with
`LITEBOX_LAZY_FORK_COMMIT=1` also set, the exact same repro hit the exact documented Bug 4 signature
byte-for-byte (`Fatal glibc error: malloc.c:2601 (sysmalloc): assertion failed: (old_top ==
initial_top (av) && old_size == 0) || ...`, `subshell_child` prints, `inner_var` never does,
`parent_after` still prints from the parent side). Nothing had drifted since the 85th pass.

**Candidate 2 (genuine OS-level section-object COW) — investigated via source reading across
`litebox_platform_windows_userland/src/lib.rs`'s whole `PageManagementProvider` impl
(`allocate_pages`/`deallocate_pages`/`update_permissions`/`unmap_shared_memory`), concluded this is
a disruptive full-allocator rewrite, not a scoped fix, and NOT attempted.** Every guest memory
allocation this crate ever makes is a plain private `VirtualAlloc2(MEM_RESERVE|MEM_COMMIT)` region —
never a section object anywhere in the guest VMA path. Real `MapViewOfFile3`/`PAGE_WRITECOPY` COW
needs the memory to have been section-backed from the point of ALLOCATION, not converted at fork
time — Windows has no "adopt this already-populated private `VirtualAlloc` range into a section"
primitive; the only way to move existing bytes into a section is to copy them, which is exactly the
per-byte cost this whole lazy mechanism exists to avoid, and doing it "from the start" would mean
paying it once per guest allocation rather than once per fork. Two concrete pieces of evidence this
is genuinely disruptive, not merely inconvenient:
  1. `deallocate_pages` (`lib.rs`, ~line 8469-8496) already EXPLICITLY REFUSES to
     `VirtualFree(MEM_DECOMMIT)` a `MEM_MAPPED` section view — logs an error and leaves it alone —
     because that call is only valid on privately-committed memory. Real Linux `munmap()` can unmap
     an arbitrary sub-range of a larger mapping; a Windows section view can only be unmapped WHOLE
     (a partial "unmap" needs the `MEM_RESERVE_PLACEHOLDER`/`MEM_REPLACE_PLACEHOLDER` split/coalesce
     dance instead of a one-line `VirtualFree`). Every guest `munmap`/`mprotect` sub-range call
     against a section-backed group would need this placeholder machinery, not the existing simple
     path.
  2. This exact codebase already fought this exact class of battle, for ONE fixed-size, fixed-
     address, narrowly-scoped region (`SHARED_KERNEL_HEAP_BASE`/`SHARED_KERNEL_HEAP_SIZE`, `lib.rs`
     ~line 10355-10394's own doc comment, a prior pass's own investigation) — the trail there is a
     live record of how fragile this is even in the best case: plain `SEC_RESERVE` sections,
     `MEM_RESERVE`-type views, and the documented `MEM_RESERVE_PLACEHOLDER`/`MEM_REPLACE_PLACEHOLDER`
     pair ALL failed `ERROR_INVALID_ADDRESS` at that fixed address; only a genuinely `SEC_COMMIT`
     section succeeded there, which then had to be immediately `VirtualFree(MEM_DECOMMIT)`-ed to
     avoid charging the FULL region size against system commit at creation time (a section is not
     lazily committed by default — another trap this design would reopen); a fixed placement at a
     high canonical address is also not collision-guaranteed against ASLR (confirmed live via `cdb`,
     `STATUS_CONFLICTING_ADDRESSES`, a prior pass). Reproducing this fight for a SINGLE bounded
     region already needed several iterations across multiple passes; the guest VMA allocator
     handles an open-ended number of dynamically-sized, dynamically-placed, `MAP_FIXED`-capable
     regions across a real boot's whole process tree — the same class of problem at much larger,
     much less bounded scope. This meets the "too large/risky for one pass" bar squarely.

**Candidate 1 (fork-time snapshot buffer) — investigated rigorously, concluded it cannot preserve
the dominant fork-then-`execve` case's own measured win, so it is not a net improvement over simply
leaving such groups eager, and NOT attempted.** A snapshot is only actually race-free if it is
captured SYNCHRONOUSLY while the parent's guest thread is still blocked inside the fork syscall
handler — exactly the window `reserve_group_lazy` currently does nothing in, and exactly the window
the EAGER `copy_one_group` path already exploits for its own immunity to Bug 4. The 81st pass already
proved (in the "defer the whole copy batch" framing) that there is no window to defer POPULATION
into without reopening a real fork()-without-`execve`() write race; the identical argument applies
unchanged to deferring the READ/CAPTURE side for a snapshot instead. Capturing a stable snapshot
synchronously therefore means reading every byte of every eligible group AT FORK TIME, regardless of
whether the child ever touches that memory or is about to `execve()` moments later — because nothing
distinguishes the two cases at fork time. That read is the same order of cost as the eager path's own
`ReadProcessMemory` loop this whole mechanism exists to avoid paying for the dominant case (~85% of
real forks `execve()` almost immediately, 79th pass); storing the captured bytes somewhere other than
the child's own committed guest memory (e.g. a scratch file) would avoid re-adding the CHILD's
`VirtualAlloc(MEM_COMMIT)` RAM charge for pages it never touches, but does nothing for the TIME cost,
which is what the 83rd pass's own measured win (103ms eager vs 34ms mixed, debug; proportionally
larger release-build win) was actually about. Net effect: a genuinely race-free snapshot design would
make EVERY fork pay full eager-read-equivalent cost unconditionally — strictly worse than simply
keeping such groups on the existing eager `copy_one_group` path, which pays the same read cost but
writes directly into place with no extra buffer hop or later re-fault indirection layered on top.

**Candidate 3 (new this pass, not one of the two originally proposed): software copy-on-write via
guard pages, symmetric with this module's own existing child-side VEH — sketched and scoped, found
to have its own genuine structural gap for the real multi-fork-child case, NOT implemented.** Sketch:
at fork time, instead of doing nothing (today's `reserve_group_lazy`) for an eligible group's
ALREADY-COMMITTED pages in the PARENT, `VirtualProtect` them to `PAGE_READONLY` — an O(1) call per
contiguous committed sub-range, not O(bytes), the same cheap performance class as the rest of this
mechanism. Install a SECOND VEH, on the PARENT side, that catches the parent's own next WRITE fault
to such a page: on that fault, snapshot the page's CURRENT (pre-write) bytes into a small side
buffer, restore `PAGE_READWRITE` on just that one page so the write can retry and succeed, and record
that a snapshot now exists for that address. The CHILD's existing `lazy_commit_veh` would then, on
its own first fault, prefer a recorded snapshot over a live `ReadProcessMemory` if one exists for
that address — closing Bug 4 for the single-fork-child case, since the child's data is now genuinely
stable either because the parent has not yet raced ahead of it (still `PAGE_READONLY`) or because a
stable pre-write snapshot was captured at the exact moment the parent tried to write.

This is correctness-sound for exactly ONE live fork child at a time, but a real desktop boot needs up
to `live_cross_process_fork_children`'s admission-control cap of 6 CONCURRENT long-lived children
(76th pass) forking from the same continuously-mutating parent — and a single global
protect-once/snapshot-once/unprotect cycle is provably WRONG across 2+ overlapping fork generations:
if child A forks, the parent later write-faults and captures+unprotects a page for A, and child B
THEN forks (after that write), B's own fault handler must NOT be served A's now-stale pre-write
snapshot — B needs the parent's CURRENT (post-A's-write) value as of B's own later fork moment. Fixing
this needs either re-protecting the page fresh on every new fork (cheap on its own) composed with a
real per-page GENERATION-tracked, multi-version snapshot scheme (a write must fan out a capture to
every live child generation still "behind" that write, not just one global slot), with race-free
coordination between the parent's own write-fault handler and every live child's independent
read-fault handler touching the same physical page concurrently. This is a genuine concurrent
multi-version-COW design problem — the same order of complexity as this module's OWN existing
single-generation, single-direction mechanism, which took three consecutive full passes (83rd-85th)
of live `cdb`/diagnostic-gated iteration to get right even in ITS simpler form. Attempting the full
multi-generation version without an equivalent live-debug budget in one pass risks shipping an
unverified, SILENTLY-WRONG-DATA mechanism (not merely a crash) — strictly worse than today's honest,
deterministic crash, and exactly the outcome the standing goal's "don't patch over deeper bugs"
instruction forbids. **Not implemented this pass.**

Most promising narrower first cut for a future pass to scope further: force any group touched by a
SECOND concurrent live fork child onto the existing eager `copy_one_group` path instead of sharing
one protected page across generations — i.e. only take the fast single-generation guard-page path
when at most one outstanding, not-yet-`execve()`'d/not-yet-exited child could still depend on that
parent's memory, falling back to eager the moment a second overlapping fork would otherwise need to
share state. This trades away laziness only under real multi-child contention (which 76th-pass live
evidence shows is common during a real boot's fork storm, but not universal at every instant) while
keeping the fast path for the common single-active-fork-child moment. Needs its own live-verification
pass before being trusted, exactly as this module's other three bugs each did.

**Conclusion**: `LITEBOX_LAZY_FORK_COMMIT` stays default OFF, unchanged. No runtime behavior was
modified this pass — only `lazy_fork_commit.rs`'s own module doc comment (new section) and this
archive entry. `DE_UP` not attempted (the flag remains unsafe to enable for a real boot). Host RAM:
~6.3-6.5GB free throughout: two short debug-build runs only, both cleaned up via WMI `Terminate`
immediately after.

## 83rd-87th pass full narrative (drained from AGENTS.md, 88th pass compaction)

Full detail for AGENTS.md's condensed 83rd-88th summary. `litebox_platform_windows_userland/src/
lazy_fork_commit.rs`'s own module doc comment is the OTHER canonical detailed record for this
mechanism (kept in sync with each pass that touches the file) — prefer it over this section for
anything code-level; this section is the pass-by-pass narrative trail.

- **83rd** — IMPLEMENTED the lazy (reserve-then-commit-on-first-fault) primitive
  (`litebox_platform_windows_userland/src/lazy_fork_commit.rs`, `LITEBOX_LAZY_FORK_COMMIT=1`,
  default OFF) the 79th/81st/82nd passes converged on. Real, measured win for the dominant
  fork-then-`execve` case (debug: 103ms eager vs 34ms mixed; both builds 5/5 clean). Found a
  genuine, 100%-reproducible crash for fork-WITHOUT-`execve` (a bash `(...)` subshell) — root cause
  not found this pass. Full design, ordering-bug fix, isolation-POC numbers: archive.
- **84th** — root-caused the 83rd pass's crash to TWO real, general cross-process-fork bugs, both
  FIXED and live-verified (neither is lazy-commit-specific): (1) a forked child's `sys_mmap`
  allocator had no record of `copy_one_group`'s own 64KiB alignment padding, letting a fresh mmap
  silently collide with it (`litebox/src/mm/linux.rs`'s `Vmem::new_adopting_existing_memory`, now
  pre-inserts a `VM_OWN_FORK_PADDING` placeholder per group span); (2) a cross-process-fork child's
  `SignalState` never inherited the parent's `sigreturn_trampoline` address (unlike the thread-based
  path), so `LinuxShimEntrypoints::exception`'s trampoline recognition could never match
  (`adopt_forked_process` now takes it via a new env var). With both landed, the subshell repro runs
  clean 5/5 WITHOUT the lazy flag — but STILL crashed 5/5 WITH it (Bug 3, re-scoped, not fixed this
  pass): the fault reached `vectored_exception_handler` and got redirected toward
  `exception_callback`, but `LinuxShimEntrypoints::exception()` was never observably entered.
  **Methodological finding**: gated, low-volume diagnostics added directly inside
  `vectored_exception_handler` measurably regressed the (otherwise-fixed) non-lazy case too — any
  future instrumentation there needs an A/B test against the non-lazy repro before being trusted.
  Full bug mechanics, exact diffs: archive.
- **85th** — root-caused and FIXED Bug 3 (no debugger needed — exact address arithmetic against a
  `LITEBOX_DIAG_LAZY_FORK_COMMIT=1`/`LITEBOX_DIAG_FATALDUMP=1` capture was sufficient): the child's
  own live `%rsp` at fork time landed INSIDE the same lazily-reserved (never-yet-committed) stack
  group `classify_lazy_eligible_groups` was already making lazy — confirmed via ZERO
  `[lazy_fork_commit] fault #N serviced` lines ever printing before the crash. Windows delivers
  EVERY exception with `CONTEXT.Rsp` set to whatever the CPU held at fault time, not only ones whose
  own fault address is in a lazy range; a fresh child's very first exception is guaranteed to hit
  this. Fix: `classify_lazy_eligible_groups` (`lazy_fork_commit.rs`) now takes the child's fork-time
  `%rsp` (threaded from `spawn_process_fork_child`'s `full_gprs.rsp`) and excludes whichever group
  contains it, leaving every OTHER pure-data group lazy. **Verified**: the subshell repro's ORIGINAL
  crash signature is gone, 5/5, both debug and release; fork-then-`execve` stays clean 5/5, no
  regression. **Bug 4 (OPEN, found while re-verifying the fix)**: the SAME subshell repro now runs
  further (lazy faults ARE serviced, with real, non-zero-filled parent data) but still fails 5/5,
  both builds, with a NEW signature: `Fatal glibc error: malloc.c:2601 (sysmalloc): assertion
  failed` — real heap corruption. Root cause: `lazy_commit_veh` reads the PARENT's LIVE memory
  (`ReadProcessMemory`) at WHATEVER MOMENT the child happens to touch a page, which for a
  fork-without-`execve` child can be long after `fork()` returned control to the parent's own guest
  thread — a genuine TOCTOU gap the eager `copy_one_group` path (synchronous, while the parent is
  still blocked inside its own `fork()` syscall) never had. The parent's continued heap
  mutation (malloc/free) after `fork()` returns can be read mid-mutation by a later lazy fault,
  corrupting the child's heap bookkeeping — exactly the observed assertion shape. No fix attempted
  this pass; needs either a fork-time snapshot into a buffer (not a live parent read) or genuine
  OS-level COW between the two Windows processes — real design work, not a quick patch. Full
  mechanics, exact repro logs, both candidate fix directions: archive and
  `lazy_fork_commit.rs`'s own module doc comment. `LITEBOX_LAZY_FORK_COMMIT` stays default OFF —
  Bug 4 means it is STILL not safe for a real boot (a real desktop forks many long-lived daemons
  that don't `execve()` and run concurrently with a parent still mutating its own heap — exactly
  Bug 4's trigger shape). `DE_UP` not attempted this pass.
- **86th** — investigated both of Bug 4's candidate fixes in depth (source reading, no code
  changed), concluded NEITHER is viable as a net improvement, found and scoped a THIRD candidate,
  also not implemented. Re-verified live (debug, unmodified binary) that nothing had drifted: flag
  unset stays clean, flag set still hits the exact documented `malloc.c:2601` signature.
  **Candidate 2 (real section-object COW) ruled out**: every guest allocation
  (`WindowsUserland::allocate_pages`/`deallocate_pages`/`update_permissions`) is built on private
  `VirtualAlloc2`, never a section — retrofitting COW needs section-backing from allocation time,
  not fork time (no "adopt existing private memory into a section" API exists; the only way is to
  copy the bytes, which is the cost this mechanism exists to avoid). Concrete evidence this is a
  disruptive rewrite, not a scoped fix: `deallocate_pages` (`lib.rs`:~8469) already explicitly
  refuses to decommit a `MEM_MAPPED` view (Windows can't partially unmap a section view the way
  Linux `munmap` can unmap a sub-range); this exact codebase's own `SHARED_KERNEL_HEAP_BASE` fight
  (`lib.rs`:~10355) already hit `ERROR_INVALID_ADDRESS` on every `SEC_RESERVE`/placeholder variant
  tried, ASLR placement failures, and an eager-full-commit-charge trap, for ONE fixed-size, fixed-
  address region — the guest VMA allocator handles an open-ended number of dynamically-placed,
  `MAP_FIXED`-capable regions, the same problem at much larger scope. **Candidate 1 (fork-time
  snapshot) ruled out**: a race-free snapshot must be captured synchronously while the parent is
  blocked in its own fork syscall (same window the eager path already uses) — which means reading
  every byte of every eligible group at fork time regardless of whether the child ever touches it
  or is about to `execve()`, since nothing distinguishes the two cases at fork time. That forfeits
  the 83rd pass's own measured win (103ms eager vs 34ms mixed, debug) for the dominant
  fork-then-`execve` case (~85% of forks, 79th pass) — a snapshot design would make EVERY fork pay
  full eager-read cost unconditionally, strictly worse than just keeping such groups eager.
  **Candidate 3 (new, found this pass): software COW via guard pages** — `VirtualProtect` an
  eligible group `PAGE_READONLY` in the parent at fork time (O(1), not O(bytes)); a new parent-side
  VEH catches the parent's own next write, snapshots the one page, unprotects it, lets the write
  retry; the child's existing VEH prefers a recorded snapshot over a live `ReadProcessMemory` when
  one exists. Correctness-sound for exactly ONE live fork child, but a real boot needs up to 6
  concurrent children (`live_cross_process_fork_children`'s cap, 76th pass) forking from the same
  continuously-mutating parent — a single global protect/snapshot/unprotect cycle is provably wrong
  across 2+ overlapping fork generations (a later child can be served an earlier child's stale
  snapshot instead of the parent's true value as of its own later fork). Closing this needs a real
  multi-generation/multi-version COW protocol — the same complexity class as the mechanism that
  already took three passes (83rd-85th) to get right in its simpler, single-direction form.
  **Not implemented — this pass's own conclusion is that shipping the full multi-generation version
  without an equivalent live-debug budget risks a silently-wrong-data bug, worse than today's honest
  crash.** Narrower first cut identified for a future pass: fall back to eager copy the moment a
  SECOND concurrent live fork child would otherwise need to share one protected page, keeping the
  fast path only for the common single-active-fork-child moment. `LITEBOX_LAZY_FORK_COMMIT` stays
  default OFF, unchanged; no runtime behavior modified this pass; `DE_UP` not attempted. Full
  writeup: `lazy_fork_commit.rs`'s own module doc comment (this pass's own section).
- **87th** — deliberate fork-in-the-road pass (Path A vs. Path B). **Judgment call: neither pure
  Path A (implement the full multi-generation COW protocol) nor pure Path B (abandon lazy-fork-
  commit for a different lever) — both real Path B angles investigated this pass closed with real
  evidence rather than opening a new lever, while Path A's own scope, worked through in full,
  turned out smaller and more precisely buildable than "full multi-generation" once the 86th
  pass's own single-generation Candidate 3 sketch was pushed to a real design.** Chose to spend
  this pass refining that design to genuinely buildable (closing two real gaps the 86th pass's
  sketch left open) rather than attempting to implement it, because a THIRD gap found while doing
  so (interaction with `VIRTUAL_PROTECT_LOCK`/`fork_verify` VEH machinery — real, but a solved
  precedent, not a blocker) confirmed this still needs the same live-`cdb`-verification budget
  each of the 83rd-85th passes spent a full pass on, which this pass did not have room for
  alongside the Path B evidence-gathering below. No runtime behavior changed this pass.
  - **Path B angle closed with real evidence, not speculation**: re-derived whether Windows'
    commit LIMIT (as opposed to physical RAM) is any part of the crater, live, via
    `Get-CimInstance Win32_OperatingSystem`/`Win32_PageFileUsage`/`Get-Counter '\Memory\Commit
    Limit'` at pass start — Commit Limit ~43.9 GB (15.25 GB physical + an automatically-managed
    ~25.7 GB pagefile) against only ~18.1 GB Committed Bytes in use, i.e. ~25 GB of already-unused
    commit headroom exists before any boot attempt starts. The 76th-82nd passes' own crater
    measurements (~7-8 GB additional committed at the crater) land nowhere near this limit even
    stacked on top of the pre-boot baseline — confirms the crater is genuine PHYSICAL working-set
    demand (thrashing/eviction as available RAM falls, matching AGENTS.md's own standing
    "FreePhysicalMemory falling trend" guidance), never a commit-limit rejection. **Closes the
    "grow the pagefile" idea as a lever** — there is no commit-limit problem to fix by growing it.
  - **Path B angle (admission-cap tuning) reconfirmed closed, not reopened**: read the 80th pass's
    own conclusion in full before doing anything — "the crater is driven by CUMULATIVE committed
    memory across the boot's whole fork history, not peak instantaneous concurrency" — and found
    no new evidence this pass that would change that; not re-litigated without a genuinely new
    angle, per this file's own standing instruction not to redo closed work.
  - **Design refinements landed in `lazy_fork_commit.rs`'s own module doc (this pass's own new
    section, real and substantive, not implemented as code)**: (1) the 86th pass's own Candidate 3
    sketch scoped its guard/claim state as tree-wide/global; working the argument through shows
    the actual correctness unit is PER-PARENT-PROCESS (two different parents' fork relationships
    touch disjoint memory and can never race each other), which needs no shared arena/`SharedArc`/
    new `SharedKernelStateSlot` at all — ordinary process-local statics for the claim, one new env
    var (mirroring the existing `FORK_CHILD_PARENT_PID_ENV_VAR` pattern) carrying the parent's own
    snapshot-table address for the child to `ReadProcessMemory` (protection state doesn't gate
    `ReadProcessMemory`, only committed-and-not-`PAGE_NOACCESS` does), and a non-blocking
    `WaitForSingleObject(h, 0)` liveness poll on the recorded owner pid to reclaim a stale slot —
    no new IPC primitive needed. (2) the sketch's "child prefers a recorded snapshot over a live
    read" step has an unstated TOCTOU of its own (the parent's snapshot-publish could land in the
    exact window between the child's flag check and its `ReadProcessMemory` call) — closed with an
    explicit double-checked-state protocol: read live FIRST, re-check the snapshot flag AFTER,
    prefer the snapshot if it is now set (never the reverse); sound because the flag's 0->1
    transition is one-shot and globally visible the instant it happens, so "still 0 after my read"
    is a truthful witness that no write occurred during the read window. (3) a genuinely new
    finding not in the 86th pass's writeup at all: a parent-side write-fault VEH for this mechanism
    would need to coordinate with the EXISTING `VIRTUAL_PROTECT_LOCK` (guards every `VirtualProtect`
    against guest-mapped memory process-wide, landed after a real live `labwc` SIGSEGV from two
    unlocked protection flips racing) and `fork_verify`'s own AV-path healing — checked against
    precedent (`fork_verify::write_usize_fault_tolerant`), a plain blocking `.lock()` on
    `VIRTUAL_PROTECT_LOCK` is the codebase's own already-shipped pattern even from inside VEH
    dispatch, so this is a solved problem to follow, not a new one to invent — but confirming that
    is itself part of why this needs a real verification pass, not a same-day landing.
  - **Concrete pickup, unchanged in substance from the 86th pass but now precisely scoped**:
    implement exactly the refined design above, gated behind a new, additional, default-OFF flag
    (`LITEBOX_LAZY_FORK_GUARD_COW=1`, on top of `LITEBOX_LAZY_FORK_COMMIT=1`) so it cannot affect
    the already-working fork-then-`execve` path even if buggy; verify 5/5 on both existing repros
    (regression check, flag off; fix check, flag on) PLUS a new concurrent-two-children-one-parent
    repro (this codebase does not yet have one), both debug and release, before considering `DE_UP`
    with either flag on. `LITEBOX_LAZY_FORK_COMMIT` stays default OFF; `DE_UP` not attempted this
    pass (unchanged reason: the flag remains unsafe for a real boot). Host RAM ~5.9-6.1GB free
    throughout; only `cargo check -p litebox_platform_windows_userland` was run (clean), no live
    boot attempted (no runtime behavior to verify — doc-only change).
