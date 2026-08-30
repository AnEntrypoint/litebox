# AGENTS.md — handoff note (2026-08-30, sub-session 19)

## Active standing goal (session-scoped Stop hook on the originating machine)

> "go ahead and push all the way till xfce starts flawlessly, fix any bug that arises first"

This is being worked via `/goal` on litebox-main, gm session_id `litebox-xfce-1`. Full
chronological detail of every prior sub-session's investigation, ruled-out hypotheses, and
fixes lives in gm's memory store (`recall`/`codesearch` against this project) as resolved
mutables — do not re-derive from scratch; query the recall store first (e.g. search
"fork_verify AV path stale pointer", "DRM PRIME handle", "wlroots shm keymap",
"D-Bus session bus export", "step bound exhaustion").

## Current state (as of sub-session 19)

**Sub-session 19 root-caused (but did NOT fix — proven unsafe naive fixes, see below) the
dbus-daemon real-daemon fork-child (tid=10, Windows `ThreadId(14)`) SIGSEGV at
`rip=fault_addr=0x8b7f024` that sub-session 13 left needing binary disassembly.** No
disassembly was actually needed — this is NOT a new decode-pattern gap in `fork_verify`'s AV-path
healing. Fresh `LITEBOX_DIAG_FATALDUMP=1 LITEBOX_VEH_TRACE=1` capture (exact AGENTS.md repro)
shows the crash is caused by `fork_verify.rs`'s own deliberate
`MAX_THREAD_VERIFICATION_STEPS = 16384` proactive cutoff (line ~171) firing mid-flight WHILE
genuine stale-pointer healing was still actively occurring: the literal log line immediately
preceding the fatal RAWREGS crash is
`[fork_verify] tid=ThreadId(14) on_single_step: step bound 16384 exceeded at rip=0x931c23f,
ending verification early`, and the very next VEH event on that thread shows `is_verifying=false`
taking a raw `c0000005` AV at `rip=0x8b7f024` completely unverified — both AV-path healers
(`translate_stale_source_rip`/`translate_stale_source_memory_operand_registers`, fixes #8/#9,
already landed and correct) are gated on `is_verifying(tls)`, which is now false, so neither ever
gets a chance to fire on this exact fault. This directly contradicts the constant's own doc
comment claiming post-fork staleness is "front-loaded": stale-pointer WARN healing lines are
still firing right up to a fraction of a second (~t=25.92s) before the bound trips and the crash
happens (~t=26.30s) — verification was doing real necessary work when cut off, not idling through
an unrelated hang.

**TRIED AND REVERTED — proven unsafe, do not retry a bare bound increase.** Raised
`MAX_THREAD_VERIFICATION_STEPS` to 262144 (16x): the tid=10 crash was avoided, but a DIFFERENT
and WORSE **host-level** crash appeared elsewhere in the same run (real Windows segfault, process
exit 139, `rip=0x2`, a `.meta=0`-shaped null-deref pattern:
`[codewatch] crash page rip=0x2 ... alloc_base=0x0`). Retried at a much more conservative 32768
(2x) — the SAME worse host-crash still reproduced, with byte-identical stackwalk magic values
(`0xc0000100` / `0x826429fee6489d13`) both times, proving this is a real, deterministic
consequence of extending verification duration, not a fluke. This is the exact same "broad
blanket-extend of fork_verify coverage/duration causes a worse regression" class sub-session 13
already proved twice for other dimensions (the reverted register-to-register-mov-propagation
patch, the six-syscall-ABI-register attempt) — now proven a THIRD time, for the step-bound
dimension specifically. Reverted cleanly to 16384 (`git status --porcelain` clean, confirmed).

**Two real, targeted (not blanket) follow-up mechanisms identified, neither attempted yet** (see
gm mutable `dbus-daemon-tid10-crash-is-step-bound-exhaustion-not-decode-gap` for full detail):
(a) an adaptive/decaying bound — extend the cutoff only while stale-pointer WARN hits keep
occurring within a short recent step window, cut off once they go quiet for N steps, matching
what "front-loaded staleness" actually requires instead of a fixed count; or (b) keep the AV-path
healers callable for a bounded grace window of instructions past the point `is_verifying` flips
false, without re-arming full single-step `TF` tracing for that window (the expensive per-
instruction trap is what the bound is actually guarding against — not the healing check itself).
Needs its own focused prototyping-and-test session (raise well past this exact failure point,
confirm the worse host-crash reproduces or does not, same method this session used) — do not
attempt a third bare bound-value guess.

## Prior state (as of sub-session 18)

**Sub-session 18 fixed and verified live the nested-fork (fork-of-a-fork) grandparent-generation
staleness gap sub-session 17 root-caused but deliberately did not fix.** Landed (commit to
follow, `litebox`/`litebox_shim_linux`/`litebox_platform_windows_userland`, all three crates
this gap spans):

1. `litebox::mm::AddressRelocations::merge_ancestor_ranges` (`litebox/src/mm/mod.rs`) — folds an
   ancestor generation's own `(range, dest_base)` pairs into `self`, re-translating each
   `dest_base` through `self`'s own map wherever this fork's own `duplicate()` call relocated it
   again, appending as-is otherwise; carries all parallel per-range metadata
   (`executable`/`private_data`/`is_file_backed`/`flags`) index-aligned.
2. `litebox::platform::ForkChildVerificationProvider::current_thread_fork_relocations` (new trait
   method, default `None`) — lets `do_clone` (running on the PARENT's own thread) ask the
   platform for the calling thread's own currently-active relocation map, if the parent is
   itself a fork descendant still under verification. Implemented in
   `litebox_platform_windows_userland/src/lib.rs` by reading `tls.fork_verify` on the current
   thread.
3. Wired into `litebox_shim_linux/src/syscalls/process.rs`'s `do_clone`, immediately after
   `PageManager::duplicate()`: if the parent has ancestor relocations, merge them into this
   fork's fresh map before it is handed to the new child's `begin_fork_child_verification` — so a
   grandchild (and any further descendant) transitively inherits full ancestor coverage, not just
   its immediate parent's single generation.

**Verified live** against the exact repro (`LITEBOX_LOG=debug`, 150s window,
`.wfgy/xfce-build/xfce-layer17.tar` `--resume-from`): the dbus-launch nested-fork chain
(`tid=6` execve's `dbus-launch`, forks to `tid=7`, `tid=7` forks AGAIN to `tid=8`/`tid=9` — the
exact grandchild pattern sub-session 17 root-caused) now completes with **zero fatal signals on
either grandchild**: `tid=8` (`dbus-daemon`, execve'd from `tid=7`'s first fork) exits cleanly
(`Exit(0)` at 2.067s); `tid=9` (`tid=7`'s second, nested fork — the grandchild) also exits
cleanly (`Exit(1)`, not a crash, at 5.202s). Full log:
`grep -n "fatal signal" .gm/scratch-repro-sub18.clean.log` finds exactly ONE fatal signal in the
whole 150s run, on `tid=10` at `2.225551200s` — a DIFFERENT, single-generation clone
(`parent_tid=8 child_tid=10`, not nested), confirmed pre-existing and unrelated to this fix (see
AGENTS.md's own fork_verify caution: "not every crash is fork_verify's fault"). Regression
suite: `cargo test -p litebox_shim_linux --lib -- --skip test_mremap` (177/177) and
`cargo test -p litebox_platform_windows_userland` (4/4), both clean, matching baseline.

**Current blocker (pre-existing, unaffected by this fix, tracked separately as
`xfsettingsd-exits-1-and-panel-desktop-never-launch`)**: `xfsettingsd` still fails to connect to
D-Bus (`Could not connect: Connection refused`) and exits with status 1 before `xfce4-panel`/
`xfdesktop` ever execve — the same gap sub-session 8 already registered. This is NOT the nested-
fork gap (which is now fixed and verified); it is a separate, still-open D-Bus-session-
readiness issue. No `xfce4-panel`/`xfdesktop` `sys_execve` line appears anywhere in this run's
log.

## Prior state (as of sub-session 17)

**Sub-session 17 classified the mixed crash population sub-session 16 found (no code fix landed
— this is a genuine new architectural gap, not a narrow decode-pattern extension safe to force
this session).** Re-added the temporary `[veh-diag]` AV-path eprintln (removed again before this
commit — see git history if needed), rebuilt, and captured 3 fresh runs with
`LITEBOX_LOG=debug LITEBOX_VEH_TRACE=1`:

- Run 1: `rip=fault_addr=0x59b0801`, `rbp=0x1`, `rdi=0x2` (tiny-integer register shape).
- Run 2 and Run 3: **byte-identical** `rip=fault_addr=0x59b98f3 rax=0x4 rbx=0x7866b28
  rcx=0x59fa5a0 rdx=0x4 rsi=0x77a7008 rdi=0x8 rbp=0x0 rsp=0x806f868 r8=0x9=0x0 r10=0x0 r11=0x246
  r12=0x0 r13=0x1 r14=0x0 r15=0x0` — fully deterministic across runs, not the non-determinism
  sub-session 16 hypothesized (it's deterministic per-instance; the mix comes from which of two
  known code sites the run happens to reach first).

**Root-caused run 2/3's crash precisely** by correlating the crashing thread's own
`[fork_verify] tid=ThreadId(13) begin: ranges=...` dump against the guest's `sys_mmap`/
`sys_execve`/`clone` log lines around it: the crashing thread is guest `tid=9`
(`fatal signal: ... pid=9 tid=9`), forked from guest `tid=7` (`dbus-launch`'s own **second**
`fork()` — `tid=6` execve's `/usr/bin/dbus-launch`, forks once to `tid=7` at `t=7.227s`, and
`tid=7` forks *again* to `tid=8`/`tid=9` at `t=7.297s`/`t=7.888s` — i.e. this is a **fork-of-a-
fork**, a grandchild, not a direct child). `ThreadId(13)`'s (tid=9's) own tracked source ranges
start at `102760448` and go up to `114073600` — these are `tid=7`'s own **destination** (already-
translated) addresses from `tid=7`'s own earlier fork, confirmed by cross-referencing
`ThreadId(11)`'s (tid=7's) begin-ranges dump, which shows source `93732864..94097408` (dbus-
launch's `.text`, confirmed via `tid=6`'s `sys_mmap: returned tid=6 ... addr=93732864
len=364544`) translating to destination `102973440`. The crashing `rip=0x59b98f3` (94083315)
falls squarely inside `93732864..94097408` — i.e. it is **`tid=6`'s (the grandparent's) original,
pre-translation address**, not `tid=7`'s (the immediate parent's) translated one. `is_in_source`
against `tid=9`'s own relocation map (which only knows about `tid=7`'s single most-recent
generation) correctly returns `false` for this value, because it belongs to a generation the
child's relocation map has no visibility into at all.

**This is a real, previously-unknown class of gap: `AddressRelocations::duplicate` composes a
relocation map only from the immediate parent's CURRENT (already possibly-once-translated)
vmem state (`litebox/src/mm/mod.rs`'s `PageManager::duplicate`, confirmed by reading it) — it has
no mechanism to chain back through an ancestor generation's own relocation map.** A value that
was already stale *before* the immediate parent's own fork_verify pass ever healed it (e.g. it
sat untouched in memory or a register the whole time, exactly the shape case (1)/(2)/(2b)/(2c)/
(2d) exist to catch for ONE generation) and survives verbatim into a grandchild fork is invisible
to every one of those cases, because they all gate on `is_in_source` against the child's own
single-generation map. Confirmed via `git log -p` reading of `litebox/src/mm/mod.rs`'s
`duplicate()` doc comment and body — this is architecture, not a decode gap in
`fork_verify.rs`'s instruction-pattern matching.

**Deliberately NOT fixed this session.** This is not a narrow, well-understood single-register/
single-transition-point extension of the kind fixes #6-9 were (this module's own doc comments and
`AGENTS.md`'s "fork_verify caution" section both call out that two prior broad-fix attempts at
this general bug family caused real regressions) — it requires an actual design decision about
how (or whether) to chain relocation maps across nested fork generations, which needs its own
focused session: e.g. (a) have `duplicate()` accept and fold in the parent's own inherited
relocation map (if any) so a grandchild's map covers every ancestor generation transitively, or
(b) something narrower scoped only to the AV-path healing functions. Registered as gm mutable/PRD
row `fork-verify-nested-fork-grandparent-generation-staleness-gap` (session
`litebox-xfce-1-sub17`) for a dedicated follow-up session — do not attempt a quick patch without
re-reading this section and `litebox/src/mm/mod.rs`'s `duplicate()`/`AddressRelocations` in full
first.

**Run 1's `rbp=0x1`/`rdi=0x2`/`rip=fault_addr=0x59b0801` capture is the SAME class** (a different
call site inside the same grandparent-generation dbus-launch `.text` region, `93732864..
94097408` — `0x59b0801` = 94046209 also falls inside it), not a separate root cause. Sub-session
16's "mixed/non-deterministic" framing was itself imprecise: each individual instance is fully
deterministic (run 2 and run 3 are byte-identical); the appearance of a "mix" was two different
call sites within the same one architectural gap, reached in different visitation order run to
run, not two different bug classes.

**Regression suite verified clean after the diagnostic was added, used, and fully removed again**
(worktree diff on `lib.rs` is empty relative to HEAD): `cargo test -p litebox_shim_linux --lib --
--skip test_mremap` (177/177) and `cargo test -p litebox_platform_windows_userland` (4/4), both
passing, matching the pre-existing baseline exactly.

## Prior state (as of sub-session 16)

**Fixed and pushed, verified live, in order** (every one of this chain that initially looked
like it might be an "upstream" bug turned out to be litebox's own gap — keep defaulting to
that hypothesis for anything new):
1. mallocng `.meta=0` crash — commit `b4a40e3d`.
2. libinput evdev rejection, missing `fallocate`, `migrate_file_up` panic — commit `5458d74c`.
3. Full DRM sysfs subtree, `DRM_CAP_*`, `DRM_IOCTL_GET_MAGIC`/`AUTH_MAGIC` — commits `1f51bf4a`,
   `024d704f`. labwc's wlroots DRM backend creates successfully.
4. `fchmod`-on-unlinked-fd + `mmap(MAP_SHARED)` on unlink-based shm files — commit `61c97e9f`.
5. `DRM_IOCTL_PRIME_HANDLE_TO_FD`/`FD_TO_HANDLE`/`GEM_CLOSE` — commit `17312da4`. Got
   `xfsettingsd` to genuinely `sys_execve` for the first time.
6. fork_verify: stale `rcx` (return address) at the syscall-trampoline disarm boundary —
   commit `8ec32c4b`.
7. fork_verify: stale `rdi` (first-arg register) at case-(1)'s indirect-call-landing boundary
   — commit `c3182da7`.
8. fork_verify: stale CODE `rip` reaching a raw `EXCEPTION_ACCESS_VIOLATION` instead of the
   `EXCEPTION_SINGLE_STEP` trap case (1) depends on — commit `4bf0acac`.
9. fork_verify: stale DATA-pointer memory-operand registers, same AV-bypass problem as #8 but
   for case (2)/(2b)'s data-pointer healing — commit `a9895bec`.

**Non-code fix, real and necessary**: the repro command must use `dbus-launch --sh-syntax
--exit-with-session` + `eval` + explicit `export DBUS_SESSION_BUS_ADDRESS` — `dbus-daemon
--print-address` alone discards the address. See repro command below.

**Current blocker**: `xfsettingsd` genuinely attempts its D-Bus connection but still fails
(`Could not connect: Connection refused`) because a dbus-daemon-adjacent forked thread still
occasionally SIGSEGVs at guest level (litebox handles these cleanly — no host crash, all nine
fixes above verified zero-regression). **Fixes #6-9 form a genuinely reusable AV-path-healing
mechanism now (`fork_verify::translate_stale_source_rip` /
`translate_stale_source_memory_operand_registers`, both callable from
`vectored_exception_handler`'s new AV branch in `lib.rs`) — but the specific remaining crash
signature has proven durable across ALL of them**, and sub-session 16's direct diagnostic
capture (temporary instrumentation, not committed) showed the crash signature is actually
NON-DETERMINISTIC across runs (one capture showed `rip=fault_addr` with `rbp=0x23`, a clearly
unrelated/different fault shape from the earlier `rbp=rcx=0x62c4080` signature that recurred
identically across several EARLIER runs before fixes #8/#9 landed) — meaning the crash
population is a MIX of distinct root causes, not one single remaining gap. Confirmed this
thread genuinely IS under `fork_verify` (`is_verifying(tls)` was true) when the diagnostic
fired, so the gap is a decode/coverage miss in the healing logic itself (case (b) from
sub-session 15's hypothesis), not a step-bound exhaustion (case (a), ruled out this session).

**Recommended next step**: add BACK the temporary diagnostic (see the exact `eprintln!` block
removed at the end of sub-session 16 — full register + fault-address dump, right after both
AV-path healing attempts fail in `vectored_exception_handler`, `litebox_platform_windows_
userland/src/lib.rs`) and capture SEVERAL fresh instances (the crash population is mixed, so
one capture is not enough) to determine whether each instance is: (a) a genuinely NEW register-
propagation pattern the existing `translate_memory_operand_registers` decode doesn't recognize
(e.g. a `lea`, an indexed addressing mode, a stale value reaching a register via something
other than a direct `mov`), or (b) an unrelated, real guest-level bug with no connection to
fork() staleness at all (plausible for the `rbp=0x23`/`rax=0`/`rdx=0` signature, which looks
like ordinary small-integer/null-pointer guest state, not a stale-source-range value). Do NOT
assume every remaining dbus-daemon-adjacent crash is fork_verify's fault — verify each capture
independently against `is_in_source` before extending the healing logic further.

**Also landed, safe and independently useful**: `mesa-dri-gallium` (software rasterizer)
installed into `.wfgy/xfce-build/xfce-layer17.tar` (a full resumable overlay on
`xfce-layer16.tar`). Use `xfce-layer17.tar` as `--resume-from`.

## Repro command (current known-good)

```
target/release/litebox_runner_linux_on_windows_userland.exe --initial-files .wfgy/xfce-build/alpine-pinned2.tar --resume-from .wfgy/xfce-build/xfce-layer17.tar -- /bin/sh -c "mkdir -p /run/user/1000 /dev/shm /var/lib/dbus; chmod 700 /run/user/1000; chmod 1777 /dev/shm; export XDG_RUNTIME_DIR=/run/user/1000; export XKB_CONFIG_ROOT=/usr/share/X11/xkb; export WLR_RENDERER=pixman; dbus-uuidgen --ensure=/var/lib/dbus/machine-id 2>&1 || true; eval \$(dbus-launch --sh-syntax --exit-with-session) 2>&1; export DBUS_SESSION_BUS_ADDRESS; seatd -l debug & for i in 1 2 3 4 5 6 7 8 9 10; do [ -S /run/seatd.sock ] && break; sleep 1; done; labwc -s \"xfsettingsd & xfce4-panel & xfdesktop &\""
```
with `LITEBOX_LOG=debug` (add `LITEBOX_DIAG_FATALDUMP=1` for crash register/instruction-byte
capture), `MSYS_NO_PATHCONV=1` in Git Bash. Rebuild
`cargo build --locked --release -p litebox_runner_linux_on_windows_userland` first. Strip ANSI
color codes before grepping for `tid=`/`pid=` (`sed 's/\x1b\[[0-9;]*m//g' logfile > clean.log`)
or plain grep silently misses matches. Regression suite:
`cargo test -p litebox_shim_linux --lib -- --skip test_mremap` (177/177) and
`cargo test -p litebox_platform_windows_userland` (4/4).

## Completion criterion (unchanged, NOT YET MET)

labwc's own `-s "xfsettingsd & xfce4-panel & xfdesktop &"` session targets launch (real
`sys_execve` log lines) and survive a 90-150+ second window with no `fatal signal:`/
`sys_exit_group` (Signal) in a `LITEBOX_LOG=debug` capture — log-based evidence only, never
`busybox kill -0` (confirmed unreliable in this rootfs).

## Hard constraints (non-negotiable, apply on any machine)

- Never use WSL2/WSL1/Hyper-V/any hypervisor — real litebox guest process on bare Windows via
  `litebox_runner_linux_on_windows_userland.exe` only.
- Never take a full-screen screenshot — crop-capture via `GetWindowRect`, or log-only evidence.
- Never recompile, binary-patch, or otherwise modify any guest package/binary — fixes go in
  litebox's own source, or use official unmodified Alpine packages/config/env-vars as-is.
- Commits authored **only** as `lanmower <657315+lanmower@users.noreply.github.com>` — never
  attribute Claude anywhere.
- Zero branches/worktrees — work directly on `main`.
- **Evidentiary discipline**: every claim must be backed by real, quoted tool output. Never
  invent a fix, a passing test, or a "confirmed running" claim. Report honest negative results.
- `busybox kill -0 $PID` is confirmed unreliable in this rootfs — use log-based liveness
  evidence instead.
- **Push safety**: stage ONLY the specific files you changed (never `git add -A`/`.`).
  `.gitignore` already covers `target-myfork/`, `alpine-fresh-test.tar`, `.agentplug/`,
  `.wfgyxfce-*.ps1`.
- **fork_verify caution**: deep, carefully-reasoned platform-layer code. A broad/blanket fix
  (e.g. translating every register-to-register mov unconditionally) has been tried twice and
  found unsafe both times. Every safe fix landed so far (#6-9) was narrow: one specific,
  well-understood register or decode case, at one specific, well-understood transition point,
  using the same proven `is_in_source`+`translate()` pattern case (1) already established. Not
  every crash is fork_verify's fault, either — verify `is_in_source` on the actual fault value
  before assuming a new healing case is needed (sub-session 16's diagnostic found the crash
  population is a MIX of real fork-staleness and possibly-unrelated guest bugs). Read the FULL
  module doc comment before attempting anything; test every change in isolation.

## Rootfs/artifact locations

- `.wfgy/xfce-build/xfce-layer17.tar` — current furthest-progressed rootfs (layer16 + mesa DRI).
  Use as `--resume-from`.
- `.wfgy/xfce-build/xfce-layer16.tar` — prior layer, has real `usr/bin/labwc`, no mesa DRI.
- `.wfgy/xfce-build/alpine-pinned2.tar` — base tar, paired with a layer-N overlay via `--resume-from`.
- Large scratch artifacts (`target-myfork/`, `alpine-fresh-test.tar`, `.agentplug/`) are local
  build/test byproducts, gitignored, safe to ignore or regenerate.
