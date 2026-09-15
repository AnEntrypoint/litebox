# AGENTS.md archive — detail drained 2026-09-15

`AGENTS.md` crossed its 30KB compaction threshold again (32,268 bytes). This file holds what was drained
out of it in that pass: detail that is still true and still occasionally useful, but that a session does
not need in order to know the current state. Claims that a later commit *superseded or refuted* were
deleted outright in that pass rather than archived here — keeping a refuted claim anywhere is how the
"one genuinely open host crash" survived five days past its fix (`271cbb5`).

Everything below carries its proving commit sha or `file:line`. Earlier drains:
`docs/AGENTS_ARCHIVE_2026-09-10.md`, `_2026-09-05.md`, `_2026-09-03.md`.

## Trampoline-extension poisoning: the blow-by-blow (`6311f74`)

`AGENTS.md` keeps the mechanism and the fix in one paragraph; this is the live evidence behind it.

The runtime patcher's initial trampoline allocation was a flat one-page guess. When a segment needed
more stub space than one page held, the extension path tried to map at exactly one fixed adjacent
address with `MAP_FIXED_NOREPLACE` and no fallback — unlike the *initial* allocation a few lines above
it, which already had a try-fixed-then-let-the-VM-choose path. Any unrelated mapping already occupying
that one address therefore failed the extension outright, and `apply_trap_fallback` then poisoned
**every** `syscall` site in the whole segment with `ICEBP;HLT`, regardless of how many were otherwise
patchable. The guest died on the first syscall it executed after load.

Witnessed live by pulling and booting `docker.io/edgelevel/alpine-xfce-vnc:latest` fresh through
`--oci-image` (no packager step): busybox `/bin/sh` took SIGILL within 3s of exec
(`[diag-ud-entry] raw_code=0xc0000096`), with 480 sites poisoned by a single failed 4KiB extension.

The fix sizes the initial allocation from a cheap `0F 05` byte-pair count over the segment — a sound
upper bound on patchable syscall sites, the same technique the rewriter's own fast-reject scan already
relies on — capped at 4MiB. Re-run clean, zero fatal signals. The exposure generalizes: any real binary
with a few hundred `syscall` sites (ordinary, not a busybox peculiarity) was affected.

## `edgelevel/alpine-xfce-vnc`, verified against its canonical registry layer

Alpine 3.16.0. Ships `Xvfb`, `x11vnc` and `novnc_server` plus the full `xfce4-session`/`xfwm4` set — a
noVNC-over-browser pipeline. Same X-server category (Xvfb, not Xorg/DRM) as the already-verified
`alpine-mate` selkies boot, so there is no DRM/KMS+wgpu path for this image to misalign with; it lands
squarely in the already-settled Xvfb/browser pipeline, not the `--gui` one.

## The three cross-process-fork correctness bugs the perf work exposed

`AGENTS.md` names the three shas in one line. The mechanisms:

- **`060ccc3`** — no `SIGCHLD` reached a parent from a cross-process fork child, so any parent using the
  race-free mask-then-`sigsuspend` wait hung forever rather than waking on the child's exit.
- **`6e86a40`** — `sys_wait4(pid=-1)` consulted the cross-process child registry only when the
  thread-based registry was already empty, so a process with both kinds of child never reaped the
  cross-process one. This is also the fix for the curl-self-test stall that `1f30ab4` narrowed; do not
  cite `1f30ab4` as live open work.
- **`d5cc744`** — a redundant claim release deleted a coalesced `CLAIMED_RANGES` slot, and with it the
  collision coverage for a loaded library.

## Why the "one genuinely open host crash" claim outlived its fix

Recorded because the failure mode is procedural, not technical, and will recur.

`0473cc3` closed the `RtlpUnwindPrologue` crash on 2026-09-08 by fixing `VEH_FRAME_STRIDE`. The claim
that it was still open survived to 2026-09-10 because `8fc102a` recompiled `AGENTS.md` from
pre-`0473cc3` archive text without re-running the repro, and then to 2026-09-15 because each subsequent
pass carried the section forward unchallenged. `271cbb5` closed it by bisecting live rather than by
reading. The lesson is the compaction rule itself: a recompile that copies a claim forward without a
witness launders a stale claim into a fresh-looking document.

## Retired: the 2026-09-09 test-suite status note

`AGENTS.md` keeps only the standing rule ("never record a test count you did not just watch run to
completion, and never leave a suite red for an environmental reason"). The counts it was derived from
are deliberately not carried anywhere — a suite is not evidence of anything and this project does not
keep test files. The underlying findings that were *real* are already recorded elsewhere on their own
merits: the panic-becomes-host-crash handler defect (`78dda05`, in `AGENTS.md`'s crash-machinery
section), and `lstat` failing to walk an intermediate symlink on any usrmerge layout.

Full original text: memory `mem-14fccd59cddec385-2382`.

## Superseded framings deleted in this pass (do not resurrect)

- "Host-side crash machinery, and the one crash still open" as a section title, and the paragraph
  explaining why the stale claim persisted, both refuted by `271cbb5` — see above.
- The `evdev-emits-two-syn-reports-per-mouse-move` PRD row cited in the input-latency section: that row
  no longer exists, and `5683a4e` closed its validation gap with a live-counted witness. The remaining
  open row is `linux-macos-userland-presentation-still-emits-two-syn-reports-per-move`.
- The separate "Two sets of seven independent litebox defects" paragraph, folded into the browser-desktop
  section as one clause; the 2026-09-07 set is memory `mem-f17269d5777055d3-3326`, the 2026-09-08 set is
  enumerated in `docs/AGENTS_ARCHIVE_2026-09-10.md`.
