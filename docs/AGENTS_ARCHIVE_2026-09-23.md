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
