# litebox — current state (2026-09-03)

This file is the authoritative, up-to-date picture of what works, what's broken, and what to do
next. It replaces the previous pass-by-pass chronological log, which accumulated thousands of
lines of retracted hypotheses alongside real findings. The full prior history (every pass,
including every dead end) is preserved at `docs/AGENTS_ARCHIVE_2026-09-03.md` for anyone who
needs the detailed forensic trail — but start here, not there.

## Standing goal

Get XFCE actually rendering and staying up under litebox on a Windows host (no WSL, no
hypervisor — see `feedback_no_wsl_or_hypervisor` in project memory).

**MET — CONFIRMED, this time with real frame-content verification, not just a pixel count.**
`a63e8ca59285f5871` ran the full XFCE launch (`advisor/probes/run_xfce_xwm.sh`,
`LITEBOX_DUMP_FRAMES=1`, ~200s) with the `do_kill` cross-thread-signal fix (see the root-cause
section below) applied: `TEST_DONE` reached, `run_exit=0` (clean, no timeout, no crash). All six
standing-oracle components alive at end with ZERO exit events for any of them the whole run:
`weston`, `weston-desktop-shell`, `xfconfd`, `xfwm4`, `xfsettingsd`, `xfdesktop`, `xfce4-panel`.
27 frames dumped, `non_black_pixels` consistently non-zero (tens of thousands to millions), 64
distinct colors throughout. **Crucially, two actual frames were DECODED (not just counted)**:
frame 9 (~08:35 PM in-guest time) shows a full-width dark-blue desktop background and a live top
panel with a real, correct wall-clock reading "Thu Sep 03, 08:35 PM" plus an app-menu icon; the
LAST frame, frame 27 (~08:36 PM), shows the SAME panel with the clock genuinely advanced to
"08:36 PM" — **real time progression across the run, proving `xfce4-panel`'s clock plugin is
live and updating, not a frozen/stale render** (the desktop background went black in this frame,
a cosmetic `xfdesktop` background-state change, not a crash — the process stayed alive, nothing
exited). **This is a real, working XFCE session**: panel alive and ticking, `xfwm4`/`xfconfd`/
`xfsettingsd`/`xfdesktop` all present and non-crashed for the full run. The panel's own content is
still fairly sparse (no taskbar entries/systray icons visible in either frame, likely because no
window-managed application launched during this particular script) — a real, separate follow-on
item (launch an actual app to populate the taskbar), but **the core deadlock this entire session
was chasing is conclusively fixed and the desktop stack is genuinely alive end-to-end.** Root
cause and fix: see "ROOT CAUSE FOUND, PRECISE, CONFIRMED" further down — `do_kill` unconditionally
rejected any `tkill`/`tgkill` targeting a thread other than the caller, silently breaking a
glibc/musl-internal cross-thread signal handshake used by `SIGSETXID`/dlopen's TLS-update quiesce,
used in nearly every multithreaded program — now fixed with real cross-thread signal delivery.

**Independently confirmed by advisor-db on the combined build** (`fe4ca386` + `69ea9470`):
`xfce4-about --version` reaches `CTX_ABOUT_VERSION_RC=0`, `CTX_WAITED`, `CTX_DONE` — previously
hung at `CTX_START` and never reached `CTX_WAITED` at all. `remote tid` count in the new census:
**0**. Confirmed from two independent angles now.
**Census diff (before → after the fix)**, `advisor/probes/unsupported-census-after-dokill.txt`:
```
GONE:    sys_tkill/sys_tgkill with a remote tid   1 -> 0   (the fix)
NEW:     setsockopt(level=1, optname=59)          0 -> 2
         setsockopt(level=1, optname=31)          0 -> 2
GREW:    setitimer nonzero it_interval            26 -> 168  (6.5x, now dominant)
         fstatfs                                  20 -> 30
         membarrier                                5 -> 8
         fadvise64                                 2 -> 4
         getresuid                                 1 -> 2
         ioctl Raw{3222823994}                     2 -> 3
UNCHANGED: inotify_init1(2), inotify_init(1), pidfd_open(1), close_range(1),
           fcntl 1033/1034 family, setsockopt 16/26, ioctl Raw{1074021792}
```
**Nothing regressed — the growth is itself confirmation the fix works**: these counts rose because
the process now gets PAST the deadlock and executes code that was previously unreachable. (Run log
grew 14MB→25MB for the same reason.)
**Revised ranking for next-tier work (not blocking the now-met standing goal)**: `setitimer`
nonzero `it_interval` is now the top target, ahead of `inotify` — at 168 hits it dominates the
census 5.6x over the next entry, and it's the REPEATING timer path specifically (one-shot works,
periodic does not). Everything that redraws or polls on a schedule depends on it: the panel clock,
plugin refresh, cursor blink, animations, session-manager keepalives, GTK frame timing — directly
load-bearing for "XFCE displaying and updating," and likely behind some of the "renders once then
goes stale" behavior seen earlier this session. Should be a small fix — the one-shot machinery
already exists, it needs rearming on expiry. `inotify` (3 hits, unchanged) stays second — still
fails in the first 0.71s (`"Cannot initialize inotify: Function not implemented"`), degrading
dbus service activation under everything — degrades rather than deadlocks, whereas a dead
repeating timer stops UI updates outright. `membarrier` (8) third.
**Split for follow-on work (not urgent, goal already met)**: `a63e8ca59285f5871` — `setitimer`
rearming; advisor-db — `inotify` (starting with whether a minimal always-empty-but-valid fd
satisfies dbus, vs. needing real watch semantics).

**`inotify` RESULT: NOT LOAD-BEARING — retracted, do not build it.** advisor-db's empirical-first
approach (test whether dbus actually degrades before building anything) paid off directly: it
stopped a fix that would have bought nothing. Committed `36e7cf35`, probe
`advisor/probes/inotify_probe.sh`, evidence `advisor/probes/inotify-not-load-bearing.txt`. dbus
works fine without it — `INO_BUS_UP=yes`, `INO_LISTNAMES_RC=0`, `INO_LISTACT_RC=0` (15 activatable
services enumerated), `INO_STARTSERVICE_RC=0`. dbus's own log settles it end to end:
```
dbus-daemon: Cannot initialize inotify: Function not implemented
dbus-daemon: Activating service name='org.xfce.Xfconf' requested by ':1.2'
dbus-daemon: Successfully activated service 'org.xfce.Xfconf'
```
It complains, then enumerates its service directories by reading them directly and activates on
demand anyway — `inotify` is only used to notice LATER changes to those directories, and nothing
in XFCE startup depends on that.
**Lesson worth keeping broadly: a scary startup log message is not evidence of degradation.**
`inotify` was ranked second on the strength of the error message appearing at 0.71s under
everything — wrong. **The census counts already showed the answer and it was misread**: 3 attempts
in the first 0.71s and never again is the signature of "checked once, gave up, moved on," not of
something load-bearing — if it mattered, it would have been retried. **Applies to the rest of the
census too: attempt-count-over-time discriminates cosmetic from load-bearing before any code gets
written** — worth the same functional test before anyone builds `fstatfs`, `close_range`, or the
`fcntl` family; none look load-bearing on current evidence.
**Remaining follow-on priority, revised**: `setitimer` (168 hits, repeating timers) is the ONLY
gap currently worth doing — stays with `a63e8ca59285f5871`, non-urgent. `membarrier` (8) next
after that, purely because silent sync failures are expensive to chase (as this whole session just
demonstrated) — but there's no evidence it's currently biting anything.

**advisor-db's session summary, all committed to `main` as `lanmower`, working tree clean**:
`69ea9470` (release-build `log_unsupported!` fix + futex owner-decode, wake-census and
clone-request diagnostics), `d48bb085` (post-fix census, rising-count-as-confirmation technique
written down), `36e7cf35` (`inotify` retraction). advisor-db is at a good stopping point.

**`setitimer` FIXED AND VERIFIED — closes the last identified gap on top of the already-met
standing goal.** `a63e8ca59285f5871`, commit `a0689cb6`
(`litebox_shim_linux/src/syscalls/process.rs` + `signal/mod.rs`). Root cause matched advisor-db's
census exactly: `sys_setitimer` unconditionally returned `ENOSYS` for any nonzero `it_interval`
(a repeating timer) — exactly what GLib's main loop uses for the panel clock, plugin refresh,
cursor blink, and animation timers. **Fix**: `TimerHandle` only supports single-shot
`set_timer(duration)` (no native repeat), so periodicity is emulated — `Alarm` gained an
`interval` field, and both `SIGALRM`-firing paths (`queue_signals`, the real-platform-timer path;
`check_alarm_deadline`, the polling fallback) now re-arm for another `interval` when firing
instead of leaving the timer disarmed. Also fixed `getitimer`'s `it_interval` field, previously
always reported as zero regardless of what was actually armed.
**Verified two ways**: (1) full XFCE launch post-fix shows ZERO occurrences of `"setitimer:
nonzero it_interval not supported"` anywhere in the trace (previously 168 per advisor-db's
census) — gap fully closed; (2) no regressions — `xfce4-about` fast repro still exits `rc=0`, full
XFCE launch still reaches `TEST_DONE` with `run_exit=0`, a decoded frame shows the panel clock
still live and correctly updating (08:45 PM).
**Unrelated pre-existing issue noted in passing, NOT fixed (out of scope), flagged for whoever
picks it up next**: `cargo test -p litebox_shim_linux` currently fails to even compile —
`epoll.rs`'s `wait` signature has 5 params, several test call sites still pass 3. Confirmed via
`git stash` that this predates all of tonight's changes. May block CI or another agent's work.
**All five fix/diag commits from tonight, on `main`**: `f824eb99`, `69ea9470` (advisor-db),
`dc50f126`, `9504adbe`, `a0689cb6`.

**REOPENED — do not record the goal as fully, cleanly met yet. A real regression survives the
fix: the desktop background is lost mid-run and never recovers, and a naive pixel-count oracle
hides it.** advisor-db ran two follow-up checks after the "session complete" note above:
**(1) Headless (`--gui` omitted entirely) is independently verified working** — 33 frames dumped,
`TEST_DONE`, clean run, `decode_frame.py` shows content covering 1080/1080 rows. Confirms the
runner's dump-only DRM flip callback (registered directly when `--gui` is absent,
`lib.rs:454`) never depends on the presenter thread — frame capture doesn't require a window.
Both headed (this session's own live run) and headless are now proven.
**(2) THE REGRESSION.** Per-frame `non_black_pixels` across a full run:
```
frames ~22-26:  2,073,597   <- full desktop, the number used as "success" all session
frames  27-30:     92,036   <- drops 95.6%, and NEVER RECOVERS
frames  31-33:     92,661
```
Decoded frame 32 visually: the panel with its clock (top-right) and desktop icons (left) survive;
**the filled desktop background is gone — most of the screen goes black and stays that way for the
rest of the run.** advisor-db's clock-liveness check (`diff_frames` between 27 and 32) shows a
small localized update at x=60..133, y=109..227 — **the session is genuinely still running, not
frozen** (liveness is real) — **but liveness and correct rendering are separate claims, and only
the first is currently established.** `2,073,597` is exactly the number that's been treated as the
success figure all session; a check that samples the peak, or only counts non-black pixels without
checking the FINAL state specifically, would report success on a desktop that's actually degraded
by the end. **This may be the same blanking behavior from project memory** (`project_advisor_findings_xfce`):
"renders a COMPLETE desktop reproducibly then blanks after ~25 frames via a protocol event on a
HEALTHY connection" — frame 27 is suspiciously close to ~25. If so, this is a known-shaped bug
that PREDATES tonight's fixes and was never actually resolved — the `do_kill` fix got the desktop
to genuinely launch and stay live, which is real and major, but this specific symptom looks
untouched by it.
**Proposed next step (advisor-db offered, not yet started — assign to avoid duplication)**:
instrument the DRM flip path to record what changes between frame 26 and 27 specifically — whether
the scanout buffer swaps to a different `fb_id`, gets re-allocated, or is wiped in place. Existing
`diag-drm-flip`/`diag-drm-scanout-bytes` gating in `syscalls/drm.rs` already logs `fb_id`/`crtc_id`
per flip — a `LITEBOX_DRM_TRACE=1` run diffed around that boundary should name it directly. This
distinguishes "compositor legitimately painted black" from "scanout memory got reclaimed
underneath it" — `advisor/probes/correlate_scanout_wipe.py` already exists to cross-reference
scanout mappings against `diag-reclaim`/`diag-decommit` ranges for exactly this.
**This directly matches what the user observed live in this session's own run tonight**: a black
background with only icons/panel visible, no filled desktop — consistent with landing in the
post-blank state described above, not a config-loading problem as first suspected. Worth
re-examining the earlier "panel only has 2 of 18 configured plugins" finding through this lens too
— it may be a symptom of the same underlying blanking/reclaim issue rather than a `migrate`/config
bug in isolation.
**Standing goal status, corrected**: XFCE launches, all core components stay alive, and the
session is genuinely live (not frozen) — real, major, verified progress from tonight's `do_kill`
fix. But the desktop's OWN rendering degrades significantly partway through every observed run and
never recovers, which falls short of "renders and stays up ... as expected." **Not fully closed.**

**FOLLOW-UP TRACE (this session, reproduced independently with `LITEBOX_DRM_TRACE=1` +
`LITEBOX_DUMP_FRAMES=1` together) — reveals TWO SEPARATE PROBLEMS, not one.** New tool:
`advisor/probes/scan_frame_series.py <dir>` (scans a numbered `litebox_frame_dump_N.bmp` series
and reports `non_black_pixels` per frame with a `<<<< DROP` marker on any >50% drop from the prior
frame — used to locate the transition precisely and automatically rather than by hand).
**Problem 1, reproduced at a different frame number than advisor-db's run (frame 64 here vs. ~27
there — confirms it's real and not tied to a fixed frame count) but the SAME magnitude**:
`non_black_pixels` drops from 2,073,597 to ~92,040 (95.6%) between frames 63→64 and never
recovers. Correlated directly against `diag-drm-scanout-bytes`: the SAME `fb_id` is used before
and after (no buffer-identity swap), and the sampled `first8`/`mid8` byte offsets are IDENTICAL
before and after — but total `nonzero_bytes` drops from 6,221,900 to 2,258,788 at that exact
moment (t=42.487s → t=42.558s). **This means specific REGIONS of the buffer went to zero while
other regions (including the sampled offsets) stayed intact — consistent with a partial
reclaim/decommit of part of the framebuffer, not a full compositor repaint or full buffer swap.**
No `create_mapping`/`guest_mprotect` event was found touching the framebuffer's own address range
(`0x11b330000`-`0x11bb19000`) at the transition — the nearby memory activity found (several
`dbus-daemon` service-activation instances dying: pid=99 exit status=1, pid=98 `SIGKILL`, pid=97
exit status=0, all within ~200ms of the drop) is suggestive but not yet proven causal. **Not
resolved — needs the exact regions that zeroed correlated against what surface/client owned them.**
**Problem 2, NEW, more severe, found in this trace — a later hard crash, not just a visual
regression**: at t=115.748s, **`Xwayland` itself dies with `SIGABRT` (signal 6)**, immediately
followed by `weston` receiving `SIGPIPE` (signal 13) and dying too — the compositor connection is
severed entirely. Immediately prior: `xfce4-panel` (pid=138) is loading `/usr/lib/xfce4/panel/
plugins/libpager.so` (the pager plugin) and doing a rapid mmap/munmap churn pattern (repeated
4096-byte alloc/free, the shape of GLib/GObject allocator churn during icon-theme/pixbuf loading)
right up to t=115.730s, ~18ms before Xwayland's abort. **Plausible trigger: the pager plugin's own
X11 client work is what's crashing Xwayland** — not yet proven, needs Xwayland's own stderr/core
dump or an X protocol trace at that exact boundary to confirm which specific request (if any)
preceded the abort. This is DOWNSTREAM of and separate from Problem 1's earlier scanout drop
(t=42.5s vs t=115.7s) — fixing one will not necessarily fix the other.
**Full run's fatal-signal census, useful groundwork for whoever continues this**: 5×
`at-spi-bus-launcher` dying with `SIGTRAP`(5) (t=26.8, 27.7, 36.3, 41.8, 64.2 — a11y bus repeatedly
failing to start, consistent with the known missing `gsettings-desktop-schemas` package noted
earlier tonight, likely benign/expected); 3× `dbus-daemon` `SIGKILL`(9) (t=42.4, 80.1, 183.9 —
service-activation churn, possibly tied to Problem 1); the `Xwayland`/`weston` pair at t=115.7
(Problem 2); one `pool-2` (an xfce4-panel plugin worker thread) `SIGSEGV`(11) at t=120.1, after
the Xwayland crash, likely a downstream consequence of losing the X connection.
**This resolves the earlier "panel only has 2 of 18 plugins" observation from this same
investigation**: `libpager` (the 3rd plugin in the default 18-plugin layout) was in the middle of
loading when Xwayland crashed — the panel doesn't have only 2 plugins by design or by a
config-loading failure, it's stuck partway through loading them because the whole X session dies
mid-startup. **Not a config bug — confirmed the same root story as Problems 1/2.**

**REAL FIX LANDED (advisor-db, `20d61808`): the repeated `at-spi-bus-launcher` SIGTRAP crashes are
FIXED, and it's a genuine root cause, not cosmetic.** The layer ships all 40 `.gschema.xml`
sources but NOT `gschemas.compiled` — the binary cache GLib actually reads.
`g_settings_schema_source_get_default()` returns NULL, `at-spi-bus-launcher` calls `g_error(...)`,
and `g_error` aborts via a debug trap — that's the `SIGTRAP`(5), and it took its own `dbus-daemon`
down with it each time. `glib-compile-schemas` is already in the layer, so the fix is one line at
startup (`advisor/probes/run_xfce_gschema.sh`). **Verified: fatal SIGTRAPs 5 → 0,
GSCHEMA_COMPILED=ok.** This also retroactively accounts for the `dbus-daemon` deaths found near
the scanout drop in Problem 1's trace above — they were at-spi's own `dbus-daemon` instances dying
alongside it, **NOT the blanking cause**. Worth folding into every launcher going forward.

**Problem 1 (blanking) does NOT get fixed by the gschema patch — confirmed independently, and its
location is now proven: it's IN THE GUEST, not litebox's memory/capture path.** With SIGTRAPs at
zero, the background still drops `2,073,597` → `92,036`. advisor-db's DRM correlation matches this
session's exactly and adds the decisive piece: `guest scanout nonzero_bytes: 6,221,890 → 2,258,768`
sampled from a mapping established FRESH from the handle on that very flip, BEFORE litebox's
capture path ever touches it — the content is already gone in the guest's own buffer at the source.
Combined with this session's same-`fb_id`/same-sampled-offsets finding, **this rules out litebox's
memory/capture/coherency path entirely: something in the guest legitimately painted most of the
screen black.** Stop looking at litebox memory management for this specific symptom.

**THIRD, SEPARATE symptom found (advisor-db) — the session can also HARD HANG, distinct from both
the blanking and the Xwayland crash.** A run stopped emitting page flips entirely at t=56.2s and
never resumed: 26 live threads, CPU flat at 67.98→68.40% over seven full minutes (initially
misread as spinning; flat CPU over that long actually means genuinely hung, not busy-looping).
Last lines before the hang: a `fork_verify` lifecycle boundary and a `libLLVM.so.22.1` load — i.e.
Mesa's software renderer (`llvmpipe`) initializing. **Now THREE distinct symptoms — resist
assuming they're one bug**: (a) background blanks in the guest's own buffer (Problem 1); (b)
Xwayland `SIGABRT` + weston `SIGPIPE` at t=115.7s (Problem 2, this session's finding); (c) a hard
hang at t=56.2s (advisor-db, new). **(c) hangs EARLIER than (b) crashes — they may be alternative
outcomes of the same underlying instability rather than a fixed sequence, meaning a fix validated
against one symptom may leave another untouched.** advisor-db's next thread: check whether the
blanking coincides with `llvmpipe` initializing, and whether forcing a simpler software path
changes it — "the guest painted black" plus "we're on a software GL stack that just loaded a
100MB+ LLVM shared object" is a suggestive pairing worth testing directly.

**Process note, apply going forward**: re-run with the gschema fix applied before drawing further
conclusions from any NEW trace — 5 aborting processes per run was real noise in everything reasoned
about so far tonight (including this session's own Problem 2/Xwayland-crash trace, captured before
this fix existed). Doesn't invalidate Problem 2's finding, but any FOLLOW-UP trace on Problem 2
should use a gschema-fixed launcher to remove that confound.

**MAJOR REFRAME (advisor-db, three runs of the identical command): outcomes VARY RUN TO RUN, and
symptoms do NOT reliably co-occur.**
```
run A: full desktop -> single drop to 92,036 -> stayed
run B: full desktop -> drop -> HANG at t=56, no more flips, 26 threads, flat CPU
run C: full desktop for 20 frames -> degraded through a SEQUENCE of values (92036, 92661, 39999,
       40009, 99323, 102106, settling 92661) -> no hang, no Xwayland crash, ran fine to t=265
```
The Xwayland `SIGABRT` at t=115.7s (this session), the hard hang at t=56.2s (advisor-db), and the
blanking do NOT reliably co-occur. **A fix validated on a single run proves very little — both
sessions adopting 3+ runs before calling anything fixed, going forward.** Also retroactively
corrects the "~25 frames then blanks" framing from earlier project memory as over-fitted to one
run — this session's frame-64 observation and advisor-db's frame-27 one are the same underlying
phenomenon landing at different times, not evidence of a fixed frame count.

**NEW, MORE TRACTABLE LEAD — two DETERMINISTIC guest crashes with IDENTICAL fault addresses
across independent process instances, which is a real litebox bug signature, not flakiness.**
```
/bin/sh  SIGILL   rip=0x7feffff7fb8a   both occurrences IDENTICAL — the trampoline band, ~449KB
                  below TASK_ADDR_MAX, nothing mapped there — matches the session's known
                  trampoline #UD (advisor/probes/setx_ud_repro.sh has an old repro)
/bin/sh  SIGSEGV  cr2=0x352e30         both occurrences IDENTICAL, genuinely unmapped
xfce4-panel SIGABRT at t=86            reproduces in BOTH sessions' traces — solid, confirmed twice
```
Four `/bin/sh` deaths per run is a lot of dead launcher shells — **a launcher shell dying mid-script
silently truncates whatever it was starting**, which could plausibly produce exactly the run-to-run
variance found above. **Ranked above the blanking**: it's deterministic, it's a real litebox bug
(not guest logic), and may be upstream of the variance that's currently making everything else hard
to measure reliably.

**New tooling, with a useful negative result (advisor-db, committed `863cba19`)**:
`advisor/probes/run_xfce_stream.sh` starts a `tail -f` per component BEFORE any component launches
(files pre-created) and prefixes output `GUESTOUT[<component>]` — the old launchers only dumped
component logs at the very END, so any run that hung or crashed first destroyed exactly the
evidence needed (which is why so little direct signal has come from `xfdesktop`/panel logs so far).
**Negative result**: across a full run, the components emit essentially NO stderr at all.
`xfdesktop` and the panel are not reporting errors — they just stop painting. **The blanking will
not be explained by component logs — stop expecting that.** Combined with the earlier
fresh-mapped-buffer finding (content already gone at the source, in the guest), **the guest is
painting black deliberately and silently** — more consistent with a legitimate response to
something (an X event, a lost surface, a resize) than an outright fault.

**Task split, revised**: advisor-db switches to the deterministic `/bin/sh` `SIGILL`/`SIGSEGV`
crashes (fixed address, fast repro, likely upstream of the measurement variance); this session
stays on Xwayland/panel-`SIGABRT` as already assigned; the blanking is PARKED until the shell
crashes are understood, since they may be corrupting/truncating the very runs used to study it.
**Action item for this session's own Xwayland-crash trace specifically**: re-check whether it had
the gschema fix applied — captured before that fix existed, so 5 aborting `at-spi` processes per
run were real noise in what that trace's conclusions were based on; treat the t=115.7s finding as
needing a clean re-trace before being trusted further.

**Clean re-trace done (this session, gschema-fixed launcher, `advisor/probes/
run_xfce_crash_diag.sh`) — DID NOT REPRODUCE the Xwayland `SIGABRT`.** Confirms the run-to-run
variance directly: same command, same fix applied, different outcome from the earlier trace.
`weston.out` (now capturing real `xwm-wm-x11`/`xwayland` logger scopes) shows normal X11 window-
management traffic (`XCB_CREATE_NOTIFY`/`XCB_MAP_REQUEST`/`XCB_CONFIGURE_NOTIFY` etc. for
`xfce4-about`, `xfwm4`, `wrapper-2.0` instances) all the way through, with **no crash message, no
fatal signal, no abnormal termination logged for either Xwayland or weston anywhere in this run.**
DRM flips continued at t=203s, well past `TEST_DONE` (~t=172s) — **weston and Xwayland were
genuinely still alive and rendering; this session's own custom `/proc`-based liveness-polling
check in the diagnostic script is BROKEN** (falsely reported both dead at the very first 5s check,
contradicted directly by real flip/log activity afterward) — do not trust `SETTLE_CHECK_*` output
from `run_xfce_crash_diag.sh` as currently written; needs a fix (likely a glob/quoting issue with
`for p in /proc/[0-9]*` under this shell) before it's usable as a liveness signal.
**Frame series for this run** (`scan_frame_series.py .`): landed at a stable
`non_black_pixels=92,661` (same settled value as previous runs) via a short SEQUENCE of
transitions (frame 10→0, then partial recoveries/drops through frame 54 and 62 before settling) —
matching the SHAPE of advisor-db's "run C" (multi-step degradation, no crash, no hang, ran fine to
completion) rather than either of the other two shapes (single clean drop / hard hang). **One
`xfce4-panel` `SIGABRT`(6) at t=72.5s** did occur in this run — consistent with advisor-db's
independently-confirmed panel-`SIGABRT`-at-t=86 finding (different absolute time, same event
class) — worth folding into the "deterministic guest crashes" investigation thread.
**Net effect on Problem 2 (Xwayland `SIGABRT`)**: the original finding stands as a real, observed
event from an earlier trace, but is now understood to be one of (at least) several possible
outcomes for this launch sequence, not a reliable/reproducible failure mode on its own — consistent
with the broader "outcomes vary run to run" finding above. Not retracted, just re-scoped: rare
(1 run so far), not yet reproduced on demand.

**`xfce4-panel` `SIGABRT` ROOT-CAUSED PRECISELY, exact fix identified, real packaging gap — not a
litebox bug at all.** `panel.out`'s actual stderr, previously never captured before this session's
`run_xfce_crash_diag.sh` fixed the "dump component logs at the end after they may have already
crashed" gap:
```
(xfce4-panel:147): Gtk-WARNING **: 21:38:49.459: Invalid icon size 16
**
Gtk:ERROR:../gtk/gtkiconhelper.c:495:ensure_surface_for_gicon: assertion failed (error == NULL):
Failed to load /org/gtk/libgtk/icons/24x24/status/image-missing.png: Unrecognized image file
format (gdk-pixbuf-error-quark, 3)
Bail out! Gtk:ERROR:../gtk/gtkiconhelper.c:495:ensure_surface_for_gicon: assertion failed
(error == NULL): Failed to load .../image-missing.png: Unrecognized image file format
```
`xfce4-panel` tries to load GTK's built-in fallback "image-missing" icon (a `.png`), fails to
decode it, and `g_error()` deliberately aborts — matches the syscall trace exactly: `Write { fd:
5, ... count: 8 }` (a crash-report/log pipe write) → `RtSigaction { signum: Signal(6), ... }` →
**`Tkill { tid: 147, sig: 6 }`, the process sending itself `SIGABRT` on purpose** — this is a
deliberate GTK assertion-abort, not a memory-safety crash or a litebox bug.
**Confirmed exact cause**: `tar tf layer31_direct_fixed.tar | grep libpixbufloader` shows **only
`libpixbufloader-xpm.so` present — NO PNG loader, no `loaders.cache` file at all.**
`image-missing.png` is a PNG file; with no PNG loader registered in `gdk-pixbuf`, any icon lookup
that falls back to it fails to decode and GTK's assertion path aborts the whole process. The
underlying decoder library (`libpng16.so`/`.so.16`/`.so.16.58.0`) IS present in the layer — only
the `gdk-pixbuf` bridging plugin (`libpixbufloader-png.so`) that connects `libpng` to GTK's image
loading framework is missing. **Same family as the missing-SONAME/missing-machine-id/missing-
gschemas-compiled findings this session — a genuine layer-packaging gap, not a litebox defect.**
**This is now the clearest, most tractable fix on the table**: source or build
`libpixbufloader-png.so` for this Alpine/musl target and add it (plus a regenerated
`loaders.cache` via `gdk-pixbuf-query-loaders`, already present in the layer at
`/usr/bin/gdk-pixbuf-query-loaders`) to the layer tar. **This single missing file plausibly
explains the `xfce4-panel` `SIGABRT` seen in both sessions' traces** — any icon lookup anywhere in
the panel (or any other GTK app) that needs to decode a PNG and hits the fallback path will trip
the same abort. Investigating how to source/build the loader now.

**CORRECTED, exact mechanism refined after downloading the real Alpine package to compare —
simpler fix than first thought, same family as the `gschemas.compiled` bug.** Downloaded the
matching Alpine `gdk-pixbuf` package (`v3.20`, `x86_64`) directly from
`dl-cdn.alpinelinux.org/alpine/v3.20/main/x86_64/gdk-pixbuf-2.42.12-r0.apk` to compare — **it
ALSO has no PNG loader module**: modern `gdk-pixbuf` builds PNG support directly INTO the core
library (`libgdk_pixbuf-2.0.so`), not as a separate loadable plugin like JPEG/TIFF/etc. So the
missing `libpixbufloader-png.so` theory was wrong. **The real bug: `loaders.cache` — the file that
tells `gdk-pixbuf` which loader (including its own built-in PNG support) handles which format —
does not exist ANYWHERE in this layer at all** (confirmed: `tar tf layer31_direct_fixed.tar | grep
loaders.cache` returns nothing). Without it, `gdk-pixbuf` has no way to resolve ANY format,
including its own built-in PNG support, via the normal lookup path. **Exactly the same bug class as
the `gschemas.compiled` fix from earlier tonight — a compiled cache file the layer build never
generated.** The tool to build it, `gdk-pixbuf-query-loaders`, IS already present in the layer at
`/usr/bin/gdk-pixbuf-query-loaders`. **Fix**: run
`gdk-pixbuf-query-loaders --update-cache` (or redirect its stdout to
`/usr/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache`, matching the standard path) once at launch
time, same pattern as the `glib-compile-schemas` fix — no new files need sourcing from anywhere.

**Historical note, kept for the forensic trail below**: earlier in this session a "MET" claim was
made and retracted after a flawed pixel-count oracle mistook weston's own built-in panel for
XFCE's; that retraction was correct at the time. This entry supersedes it with a fix-verified,
frame-decoded result rather than a repeat of the earlier mistake. A first frame decode
(advisor-db, `.bmp` from `LITEBOX_DUMP_FRAMES=1`, frame 23 of 24, 1920x1080) showed a 32px-tall
top bar with an icon (x=12..31) and a clock/status area (x=1753..1904) and one flat color below
y=31 — **but timing analysis of the SAME run then showed that frame was captured at t=30.93, while
`xfdesktop` doesn't even start until t=48.67 and
`xfce4-panel` not until t=60.06 (`TEST_DONE` at t=74.06).** The decoded frame predates both
components' existence — it is weston-desktop-shell's OWN built-in panel being shown, not XFCE's.
**"xfdesktop is running and drawing nothing" is UNSUPPORTED by this evidence and is retracted; the
question is genuinely open, not answered.** `LITEBOX_DUMP_FRAMES` only captures on a weston page
flip (flip-on-damage), and there were zero flips between t=31 and t=74 — which is consistent with
either (a) XFCE genuinely produces no damage / doesn't draw (the original hypothesis, still
possible), or (b) the run simply ends too soon after the panel starts (components need 60+s to
come up, `TEST_DONE` fires only 14s after `xfce4-panel` starts) for any drawing+flip to occur in
the captured window. **Resemblance to the user's original "icon and clock" complaint is likely
coincidental** (that symptom came from a materially different, now-superseded launch path) and
should not be treated as corroboration. **Next step, in progress (advisor-db)**: rerun with a much
longer tail after the panel starts, and force periodic capture so "idle but alive" is
distinguishable from "never drew" — if frames with content below y=31 appear, XFCE is drawing; if
flips genuinely never occur while all six components are alive and settled, (a) is confirmed and
becomes a concrete question (why does a running `xfdesktop` generate zero damage). **Still OPEN,
not answered either way — but the first properly-timestamp-checked data point leans toward "not
drawing" (not conclusive, see caveats below):** a settle-window rerun captured two frames
(t=55.41, t=55.53) 10.8s after `xfdesktop` started (t=44.60) and *before* `xfce4-panel` had even
started (t=55.75) — decoded content was still only weston-desktop-shell's own 32px bar (~3.0%
coverage, background `rgb(68,34,0)` at 97% of sampled pixels), nothing from `xfdesktop` visible.
All six faults in that run were the optional `at-spi-bus-launcher` helper — no real XFCE component
died; `xfdesktop` was alive and healthy for those 11s, it simply hadn't produced visible output
yet. **Not treated as conclusive** (correctly, per advisor): 11s is short given components take
tens of seconds to initialize in this environment, `xfce4-panel` hadn't even started yet, and the
run ended at t=61.96 before reaching the intended 120s settle window — the long-tail test hasn't
actually happened yet. Component spacing is consistent across runs (~10-11s apart: `xfdesktop` at
t=44.60/t=48.67 across two runs, `xfce4-panel` at t=55.75/t=60.06), and the whole stack needs ~60s
before the last component even starts — **a longer-budget rerun that actually reaches the settle
window is in progress; treat this as leaning, not decided, until that lands.**

**RESOLVED. CONFIRMED: XFCE genuinely draws nothing.** A properly-timed run (advisor-db) settled
it: `xfdesktop` started at t=52.56, `xfce4-panel` at t=64.04, `SETTLE_START` at t=77.86, frames
captured at t=78.86/t=78.91 — **26.3s after `xfdesktop` started, 14.9s after `xfce4-panel`
started, both alive the whole time, no component died.** Decoded content: background
`rgb(68,34,0)` at 97% of sampled pixels, content in only 8 of 270 sampled rows, one band y=0..28,
bright clusters at x=12..31 and x=1753..1904 — **identical to the pre-XFCE frames.** With both
components running and SETTLED for 15-26 seconds (long enough that "we looked too early" no
longer applies), the screen still shows only weston-desktop-shell's own 32px bar. **This closes
the open question from above: it is (a), not (b) — XFCE produces no visible output, not "the run
ended before drawing could happen."** The earlier claim was directionally right the first time,
it just wasn't yet measured soundly enough to assert (see the retraction/re-confirmation trail
above — this is now the confirmed, sound version).

**Leading cause, believed likely but not yet fully confirmed**: the missing `xfce4-panel.xml` /
`xfce4-desktop.xml` finding two entries below — an unconfigured panel has no plugins to
instantiate, an unconfigured desktop has no backdrop/icons to draw, both would be exactly this:
alive, error-free, legitimately blank. **Decisive test in progress (advisor-db)**: dump the
guest's own `$HOME/.config/xfce4/xfconf/xfce-perchannel-xml/` after the settle window and query
the `xfce4-panel`/`xfce4-desktop` channels directly — configs present would mean a real drawing
bug (layout exists, chose not to render); configs absent confirms the packaging-gap theory and the
fix is shipping default configs into the layer. My dispatched agent (a63e8ca59285f5871) is
cross-checking this same question in parallel.

**ATTRIBUTION OF THE TOP BAR TO WESTON (NOT XFCE), CONFIRMED BY BYTE-LEVEL DIFF — directly
answers a user question challenging the earlier claim, and the challenge was worth raising.**
advisor-db compared two frames from the SAME run byte-for-byte (not just "same decode verdict"):
frame 26 (t=43.43, before `xfce4-panel` starts at t=54.38) vs frame 28 (t=103.48, 49s after the
panel started and fully settled). SHA256 differs — **not** byte-identical — but the actual diff is
6 changed rows (of 540 sampled) confined to y=12..22, 8 changed columns at x=1874..1881, entirely
inside the clock cluster (x=1753..1904) already identified earlier. **That's a clock digit
advancing (a minute ticking over) — nothing else on the entire 1920x1080 screen changed.** Three
independent lines of evidence now converge on the same conclusion:
1. **Timing**: the bar is present in the FIRST captured frame (t=6.60), 36s before `xfdesktop`
   starts (t=43.12) and 48s before `xfce4-panel` starts (t=54.38) — it predates both.
2. **Direct attribution**: weston loads `/usr/lib/weston/desktop-shell.so` at t=5.76 and execs
   `/usr/libexec/weston-desktop-shell` at t=6.51, immediately before the bar first appears —
   weston-desktop-shell draws its own panel with a clock by default; this is standard weston
   behavior, not anything this project configured.
3. **The diff itself**: if `xfce4-panel` were the one drawing that bar, its startup would have
   *created* it: instead the screen already had it, and `xfce4-panel` starting only nudged one
   clock digit within the existing bar.
**Conclusion: the icon+clock bar is weston-desktop-shell's own UI, not XFCE's, confirmed three
independent ways — this is solid, not just a timing inference.**

**"MISSING CONFIG" THEORY: TESTED AND DEAD — do not revisit.** The SAME run that produced the
byte-diff above had advisor-db's minimal `xfce4-panel.xml`/`xfce4-desktop.xml` already installed
AND the screen still showed only weston's bar. Direct confirmation the components actually READ
those configs: querying the live xfconf channels from inside the guest after settle shows
`xfce4-panel`'s `/panels/panel-1/{length,plugin-ids,position,...}` and `/plugins/plugin-1,-2,-3`,
and `xfce4-desktop`'s `/backdrop/.../{color-style,image-style,rgba1}` and
`/desktop-icons/{style,file-icons/...}` — exactly the values shipped. **They know they should have
three plugins and a backdrop with icons. They still draw nothing.** (Side note: the guest's own
`$HOME/.../xfce-perchannel-xml/` contains only `displays.xml`/`xfce4-keyboard-shortcuts.xml`/
`xsettings.xml` — no user copies of panel/desktop — confirming `xfconfd` is correctly serving the
shipped `/etc/xdg` defaults rather than needing a user override.)

**REAL FAILURE POINT, PRECISELY LOCATED: zero X windows are ever mapped.** weston's XWM is created
and healthy (`xfixes version: 6.0`, `created wm, root 98`) — but its log reports **NOT ONE managed
window for the entire run**, no map events, no window events at all, despite `xfwm4`/`xfdesktop`/
`xfce4-panel` all alive and configured for 30+ seconds afterward. **Everything downstream (no
surface, no composite, no pixels) follows automatically from this one fact.** This is now squarely
an X-protocol question, not a memory or compositing one. Two concrete next checks identified: (1)
confirm the components are actually connected to the right display (`--display=$DISP` resolves to
`:0`, `xfce4-about` connected successfully earlier, so probably fine but cheap to confirm from
their own X traffic); (2) decode whether their `CreateWindow`/`MapWindow` requests are being
issued at all — the raw X protocol traffic is already captured in the unix-stream logs, just needs
request-level decoding. Assigned to `a63e8ca59285f5871`, **PAUSED pending the theory directly
below**, which may make it unnecessary.

**LIKELY ROOT CAUSE OF THE ZERO-MANAGED-WINDOWS RESULT: TWO WINDOW MANAGERS RUNNING SIMULTANEOUSLY
ON THE SAME DISPLAY — strong, coherent hypothesis, verification in progress (advisor-db).**
Timeline from weston's own log against the components' own startup:
```
18:44:19.922  weston launches Xwayland
18:44:28.700  weston: "created wm, root 98"     <- weston's own XWM claims the root window
18:44:30.846  xfwm4's first message              <- xfwm4 is ALSO a window manager
18:44:45.869  xfdesktop's first message
```
X11 permits exactly ONE client to select `SubstructureRedirect` on the root window; a second
client requesting it gets `BadAccess` and either exits or runs degraded. weston's own XWM (loaded
via `xwayland=true` — the exact fix landed earlier this session) claims the root window first, and
`xfwm4` then starts and tries to become window manager too. **weston's XWM is precisely the
mechanism that maps X surfaces into the compositor's scene graph — if `xfwm4` is disrupting it,
that alone would produce exactly the observed symptom (zero managed windows, nothing composited).**
Notably coherent, not just plausible: under a Wayland compositor with rootful Xwayland, **the
compositor itself is meant to be the window manager for X11 clients** — running `xfwm4` alongside
it is architecturally redundant, not merely buggy. This also explains why the EARLIER (pre-fix, no
`xwayland=true`) runs never hit this: with no XWM at all, `xfwm4` was the only would-be window
manager and there was nothing to contest — but surfaces were then never mapped either (the
original blackout bug).

**TESTED AND WRONG — do not revisit, do not spend more time on window-manager arrangement.**
advisor-db ran the identical launcher/config with `xfwm4` simply not started (weston's XWM the
only window manager on the display). Frame at t=63.55 — 18.6s after `xfdesktop` started (t=44.91),
7.1s after `xfce4-panel` started (t=56.38) — **identical to every previous run**: content band
y=0..28 only, bright clusters at x=12..31 and x=1753..1904, 3.0% coverage. Removing the WM
conflict changed nothing. `xfwm4` was not the cause.

**Everything ruled out so far, for reference**: missing panel/desktop config (channels confirmed
populated, still nothing); two window managers contesting the root (xfwm4 removed, still nothing);
missing XWM (fixed earlier — `created wm, root 98` present in every run since); shared memory
(11/11 same-instant cross-view comparisons agree); scanout corruption (nothing decommits/unmaps
the buffers). **Components are alive, configured, with a working XWM and no WM conflict, and still
produce no windows and no pixels.**

**THE ONLY UNTESTED LINK IN THE CHAIN, now the live thread**: do the components issue
`CreateWindow`/`MapWindow` at all, and what comes back? Assigned to `a63e8ca59285f5871`
(X-protocol decode of the captured unix-stream traffic), now resumed. **One traffic-volume clue
worth building on**: in the no-`xfwm4` run, the components produced 415 socket messages after the
panel started but only 3 larger than 1KB (largest 1344 bytes) — a GTK panel that had actually
created and populated a window would be moving pixmap/image data far larger than that. This
pattern already suggests setup completes but drawing is never reached; the decode should look
specifically for the LAST successful request and the first thing that stalls or errors.

**RESULT: DECISIVE. `CreateWindow` succeeds; `MapWindow` is NEVER called by any client, and it is
not because they're stuck waiting on an X reply.** `a63e8ca59285f5871` built a proper stream-
reassembly X11 decoder (fixed an early opcode-misalignment bug from treating every `write()`
syscall boundary as a request boundary; widened trace capture from 32 to 4096 bytes since the
original prefix cut off real traffic on busy sockets — some `write()`s carry 12KB+ of batched
requests) and ran a clean full-stack launch (`run_xfce_xwm.sh`, `DBUS_UP=yes`, `TEST_DONE` at
t=70.844, exit 0) with it. 7564 unix-stream events, 14 genuine X11 client connections (verified via
protocol-major-version=11, not just the byte-order marker — that alone false-positived on D-Bus,
which shares the same convention). **9 confirmed `CreateWindow` calls, properly decoded with
correct length-field alignment. `MapWindow` (opcode 8) appears ZERO times anywhere in the entire
capture, on any of the 14 sockets, in either direction.**

Clearest single data point: `xfwm4`'s main socket (12508 bytes traffic, 538 events) issues two
`CreateWindow` calls (t=40.309, t=40.607), does normal WM-startup work (`ChangeWindowAttributes`,
`GetWindowAttributes`, `QueryTree`, two non-fatal `BadWindow`/`BadDrawable` errors that look like
ordinary "window already gone" races), does a `GetProperty` (looks like Gtk/IconSizes) that gets a
clean large reply (1168 bytes, no error) at t=40.818 — **then goes completely silent: not one more
byte in or out for the remaining ~30s of the run, connection never closed, no timeout, no error.**
Every other X11 socket (panel, desktop, display-settings) shows the identical shape: a burst of
`CreateWindow`/`ChangeProperty`/`InternAtom`/`QueryExtension` activity, a clean small reply, then
permanent silence — all clients' "last gasp" lands between roughly t=29-59s, then nothing for the
remaining 10-40s even though `TEST_DONE` doesn't fire until t=70.8. **Ruled out**: this is NOT a
request stuck waiting on a missing reply — every request found across all 14 sockets got answered.
The clients are not blocked on X11 I/O by the time they go quiet. **Points to**: something in each
client's OWN code deciding not to proceed past window creation — `CreateWindow` succeeds, some
property/theme setup happens, and the code path that would normally call `MapWindow` next is
either never reached or silently stalls without issuing another X call (GTK/glib mainloop stall, a
blocking non-X syscall, a wait on a non-X fd that never fires...). Worth checking whether it's the
SAME point in every client's own source (a shared GTK/xfce4 helper library init path).
Instrumentation not yet committed — held pending reconciliation with the timer hypothesis directly
below, which may explain this result entirely.

**PARALLEL HYPOTHESIS, running (advisor-db), tests a different link in the same chain**: does the
GTK client even believe it has a usable screen/visual to draw into? A GTK app that can't find one
initializes, sits idle, and draws nothing — with NO error — matching every observation exactly
(alive, configured, no errors, no windows, no pixels). If true, the X-protocol decode's answer
(CreateWindow never issued) follows automatically from this upstream cause. **Update: the
`GTK_DEBUG=all` plan hit a dead end** — this layer's GTK is built without `G_ENABLE_DEBUG`
(`"GTK_DEBUG set but ignored because gtk isn't built with G_ENABLE_DEBUG"`), so GTK cannot report
its own display/visual selection at all; the intended instrument doesn't exist in this build.
**Trap avoided, worth flagging generally**: running a GTK program with `--help` does NOT open a
display, so a zero exit code from `--help` proves nothing about screen usability — briefly
mistaken for evidence, caught before being asserted. **Switched to**: `GDK_SYNCHRONIZE=1` (GDK's
X error reporting is always compiled in, unlike `GTK_DEBUG`) plus actually running
`xfce4-display-settings` for real (so it genuinely queries RandR) rather than `--help`. Committed
as `advisor/probes/run_screen_probe.sh`.

**RESULT: TESTED AND REFUTED — do not revisit "client can't find a usable screen."**
`xfce4-display-settings` run for real (t=29.8 to t=138, still alive, 108+s without completing) is
**NOT blocked on a failed X call — its socket exchanges succeed**: `t=78.478` and `t=138.480` each
show a clean `send 64B → recv 64B → recv/send 32B` round trip, **60.00 seconds apart** — the same
idle heartbeat identified earlier, every exchange completing normally. Activity profile: 1248
socket events in t=20-30, 620 in t=30-40, then **nothing until 4 events at t=70-80**. The client
does its real work in ~20s, then goes completely idle, waking only on its own timer. **X
round-trips work; the screen is not unusable; GTK is not failing to find a visual.**

**What this sharpens for the decode**: every XFCE client observed is in the IDENTICAL state —
connected, exchanging successfully, alive indefinitely, never finishing startup, never drawing,
same shape as `xfce4-about --version` never exiting. **This is one behavior across every client,
not something panel- or desktop-specific.** The decode's most useful question is now: **what is
the LAST request each client sends before going idle, and is it a round-trip whose reply never
arrives?** A client blocked on a missing reply would show exactly this profile — busy, then
silent forever, connection healthy throughout.

**RETRACTED — "`/usr/bin/timeout` is missing from the layer" was WRONG, do not act on it or audit
prior results because of it.** advisor-db's own follow-up found the claim was based on
over-reading a negative: (1) no "not found" shell error appears anywhere in the actual run logs —
if `timeout` had genuinely failed to resolve, the shell would have said so; (2) the "duplicate
binaries" observation that fed the original inference was ordinary `$PATH` search trying
`/bin/xfce4-display-settings`, `/usr/bin/xfce4-display-settings`,
`/usr/local/bin/xfce4-display-settings` in turn, not evidence of anything missing, and the same
misreading pattern produced the `timeout` claim from two `ls` invocations; (3) the specific
`PROBE_REAL_RC` line that never printed simply never printed because the run ended while that
command was still executing, not because it was skipped. **No prior result relied on a
silently-bypassed timeout guard; no audit of earlier findings is needed because of this.** What
remains true and unaffected by this retraction: the layer genuinely has no `xdpyinfo`/`xrandr`/
`xwininfo`/`xprop`/`xlsclients` (checked individually and via `find`), GTK is genuinely built
without `G_ENABLE_DEBUG`, `xfce4-display-settings` run for real genuinely doesn't complete, its
X round-trips genuinely succeed 60.00s apart matching the idle heartbeat, and every XFCE client is
genuinely in that same connected-but-never-finishing state — the screen-usable conclusion and the
sharpened decode question both stand unchanged.

**STRONG UNIFYING HYPOTHESIS, testing now (advisor-db): a broken timer/alarm mechanism in the
guest.** Fell out of correcting the timeout retraction above — `timeout` IS present and DOES exec
(`argv0=/usr/bin/timeout` appears in the log) but **never actually fires**: `timeout 25
xfce4-display-settings` started at t=30.715 (pid 42) is still alive with zero exit events at
t=95.259 — 64 seconds past its 25-second deadline. `timeout(1)`'s entire job is arming a timer and
killing its child on expiry; if it never fires, the guest's timer/alarm delivery itself is broken.
**This would explain every symptom chased tonight in one shot**: GTK schedules significant startup
work on timers/idle callbacks — a broken timer means deferred work (including, plausibly, the
`MapWindow` call the X-protocol decode just found is never reached) never runs; `xfce4-about
--version` never exiting is consistent with waiting on a timer that never expires; the 60-second
"idle heartbeat" that's the ONLY thing that ever wakes any client is suspiciously close to a
socket/protocol-level keepalive — i.e. the one wakeup source that does NOT depend on guest timers
at all; `timeout(1)` itself failing is the cleanest, simplest possible confirming signal, since its
whole job is nothing but setting a timer. **Test in progress, seconds not minutes**: in a bare
guest with no display stack, run `timeout 3 sleep 30` — hangs past 3s = self-contained litebox
timer bug, trivially reproducible, fully independent of X/weston/XFCE. Then `sleep 3` alone (known
to work, since poll loops in every script this session have advanced on wall-clock time) to
separate "sleep works" from "alarm delivery works" specifically. **If this reproduces, it explains
the X-protocol decode result directly (a client blocked on a never-firing timer produces exactly
that trace) and becomes the actual root cause to fix, upstream of the window-mapping question —
hold the X-protocol instrumentation uncommitted and do not chase the client-side-stall theory
further until this is resolved either way.**

**TESTED AND WRONG — do not revisit, resume the X-protocol decode/instrumentation.** Bare alpine
rootfs, no display stack: `sleep 2` completes normally; `timeout 3 sleep 30` returns promptly;
`timeout 3 sh -c '<busy loop>'` also returns promptly. **Guest timer and alarm delivery both work
correctly.** (Note for anyone reading exit codes here: both `timeout` invocations returned `RC=0`,
not GNU `timeout`'s usual `124`-on-kill — likely busybox semantics or the child exiting via another
path; the command still returned on schedule, so the timer genuinely fired, but `timeout`'s exit
code is not a reliable "did it kill vs. did the child finish" signal in this environment — worth
knowing if anything branches on it.) The original "`timeout 25 xfce4-display-settings` never
fired" observation is now believed to be a second instance of the same measurement pattern as the
earlier `/usr/bin/timeout`-absence retraction: most likely the timer DID fire and the exit event
for that pid was simply missed by the timeline-extraction method (matches only certain exit-record
fields), not a second independent bug — not being asserted as confirmed, just no longer treated as
a live theory.

**Where this leaves the investigation — unchanged and solid, five hypotheses now refuted by direct
measurement**: X round-trips succeed, screen is usable, GTK finds a visual (screen-capability,
refuted); the WM conflict is not the cause (dual-WM, refuted); missing config is not the cause
(config-load, refuted); shared memory is fine (cross-view, confirmed correct); the scanout is not
corrupted (nothing decommits/unmaps); guest timers work (this entry, refuted). **The X-protocol
decode's question — what is the last request each client sends before going idle, and does its
reply ever arrive — remains the sharpest instrument on the table and is once again the live
thread.** `a63e8ca59285f5871`'s instrumentation and decoder should be committed and the
investigation continued from its result (client-side stall after successful `CreateWindow`, before
`MapWindow` — see above).

**PARALLEL TEST, running now (advisor-db): a minimal known-correct X11 client, bypassing GTK
entirely, to separate "X clients can't draw here" from "GTK can't draw here."** Reframe that
prompted it: **no X11 client has EVER drawn a pixel in this environment, in any run this session.**
weston-desktop-shell (the only thing seen rendering) is a Wayland client, not X11. The only X
clients observed are `xkbcomp` (draws nothing by design) and various GTK apps (none of which have
ever displayed anything) — so "X is broken here" and "GTK is broken here" have never actually been
distinguished, and the layer has no tool that could (no `xdpyinfo`, `xmessage`, `xclock`, and GTK
built without debug support). **Built**: `advisor/probes/xwire_probe.c` — a freestanding C X11
client speaking the wire protocol directly over the Unix socket, no Xlib, no headers: connects,
reads the setup reply, creates a 600x400 window with a magenta background, maps it, creates a
graphics context, fills it bright green, then drains and decodes every reply (reporting X error
codes with their major opcode, i.e. exactly which request was rejected, if any). Committed, built,
packaged, running. **Two possible outcomes, pointing in opposite directions**: a green rectangle
in a frame capture means the X path works end-to-end and the fault is in GTK or above — shrinks
the search to the toolkit; nothing appearing (or an X error) means the fault is BELOW GTK, in X or
its route to the compositor — and this becomes a ~4KB standalone reproduction instead of a full
desktop. **Explicitly complementary, not duplicating, the X-protocol decode**: the decode asks
what real clients send and whether replies arrive; this probe asks whether a minimal known-correct
client can draw AT ALL. If the probe succeeds, the decode's finding reframes to "requests are fine,
look at what GTK does with the replies." If the probe fails, the decode is measuring a path broken
beneath the clients entirely. Result pending — frame decode plus every X reply the probe observed.

**RESULT: THE WHOLE STACK WORKS. FIRST NON-WESTON CONTENT EVER RENDERED IN THIS ENVIRONMENT. THE
BUG IS ABOVE X — IN GTK/XFCE, NOT IN X11/COMPOSITOR/DRM.** The raw probe:
```
XWIRE_CONNECTED           connect to /tmp/.X11-unix/X0 succeeded
XWIRE_SETUP_STATUS=1      server ACCEPTED the connection
XWIRE_ROOT=98 size=1920x1080 visual=35    <- root=98 matches weston's "created wm, root 98" exactly
XWIRE_CREATEWINDOW_SENT   NO error returned
XWIRE_MAPWINDOW_SENT      NO error returned
```
**And it appeared on screen.** The frame captured after `MapWindow`: a new content band at
y=520..984 (468px), x=876..1520 (644px), dominant color `rgb(255,0,255)` — exactly the requested
magenta — plus `rgb(204,204,204)`/`rgb(255,255,255)` (weston's own title-bar decoration drawn
around it). Coverage jumped 3.0% → 46.3%. **A 600x400 window plus decorations, centered on a
1920x1080 screen — precisely what was requested.** (Minor, unrelated to the result: the probe's
own `CreateGC` got `BadLength` and the follow-on `PolyFillRectangle` got `BadGC` — the probe's own
request-encoding bugs, not litebox faults, and irrelevant since the window was already visible
from its background pixel alone.)

**Conclusion, proven by direct demonstration rather than inference: raw X client → Xwayland →
weston's XWM → scene graph → DRM scanout → visible pixels is a FULLY FUNCTIONAL path.** Window
creation, mapping, compositing, and display all work correctly on this exact server (confirmed
same server via matching root window id). **Every layer this session spent hours investigating
(shared memory, DRM/scanout, the XWM, the compositing path, dual-WM contention) is now proven
good by demonstration, not just by elimination.** The XFCE components are not failing because of
anything below GTK — they fail for a reason ABOVE X, in GTK initialization or the XFCE code
itself (or something those depend on).

**Sharpens the X-protocol decode's target further**: we now know a *correct* client's
`CreateWindow`/`MapWindow` succeed on this exact server. So the decode's question becomes: **do
the XFCE clients ever ISSUE `CreateWindow` at all** (already known: yes, 9 confirmed) **and, given
that a correct client's `MapWindow` call would succeed here, why do the XFCE clients never send
one?** If GTK's own internal state machine never reaches the code path that calls `MapWindow`
after `CreateWindow` succeeds, the X traffic itself is a red herring and the real bug is inside
GTK's own window-realization logic — worth tracing at that level next (glibc/pthread/syscall
tracing of what each client does between its last X write and going silent, as already planned).

**MAJOR: pid-verified syscall trace precisely locates xfwm4's actual hang point — much earlier
than previously believed, likely BEFORE any real X11/xfconf work at all.** IMPORTANT CORRECTION to
the earlier `t=40.818`/`GetProperty`-then-silence attribution: that timestamp came from the
UNVERIFIED unix-stream byte trace (busy socket inferred to be xfwm4 only by timing coincidence
with its `execve`, no actual pid-to-socket mapping). A new trace with real per-pid syscall tracing
(`a63e8ca59285f5871`) confirms via the `execve` DIAG_TIMELINE line that `xfwm4` is **pid=38**, and
shows it makes 3589 syscalls in under one second after `execve` at t=27.386, then makes its
**absolute LAST syscall ever at t=28.324931400s: a `futex` ENTRY with no matching EXIT anywhere in
the rest of the 66-second run.** Confirmed not a global logging failure — `xfce4-panel` (pid=102)
keeps logging syscalls until t=55.7+ in the same run. **The syscall sequence immediately before
the hang**: a repeated `open`/`fcntl`/`fstat`/`read`/`mmap`×N/`close`/`mprotect`×many pattern
(classic ELF shared-library `dlopen`), then `rt_sigprocmask` → `membarrier` (fails) →
`rt_sigprocmask` → `rt_sigaction` → `tkill` (fails) → `futex` (hangs forever). **This specific
sequence — `membarrier` + `tkill` + `futex` — is a known glibc pattern for dynamic thread/TLS-
registration synchronization when `dlopen` loads a library with thread-local storage.** This
strongly suggests `xfwm4` hangs extremely early, likely during its own startup `dlopen` of a
GTK/xfce shared library, **NOT after any X11/xfconf work — it may never even reach xfconf init.**
**In progress**: rebuilt instrumentation to also capture the full typed syscall request (including
file paths for `open`) to identify exactly which library triggers this, and to directly check
whether xfconf/dbus socket activity is ever reached before the hang (per the sharpened question
above). **Also being re-verified**: whether the earlier `t=40.818` GetProperty-then-silence story
is real but for a DIFFERENT client (`xfdesktop` or `xfce4-panel`, both alive and creating windows
around then) once per-pid data is available with both instrumentation flags on together — do not
treat that earlier timestamp as attributed to `xfwm4` specifically until re-confirmed.

**ROOT CAUSE FOUND, PRECISE, CONFIRMED: this is a litebox `clone()`/`fork_verify` bug, not GTK,
not xfconf, not X11 at all — a cloned pthread never resumes into guest code after its Windows
thread-healing pass completes.** `a63e8ca59285f5871`'s request-detail syscall trace (full typed
args, comm-filtered to `xfwm4`/`xfdesktop`/`xfce4-panel`) pins the exact sequence for `xfwm4`
(pid=38):
```
t=29.855976900  clone() enters -- a real pthread_create()-style thread spawn (CloneArgs)
t=29.856051900  clone() returns ok=true
t=29.910541900  fork_verify: diag-fv-lifecycle BEGIN tid=ThreadId(46) win_tid=10876 range_count=37
                 (litebox's Windows-specific post-clone memory-healing pass for the new thread)
t=29.914-30.090 several write_usize_fault_tolerant widen/restore healing ops, all ok=true
t=30.095929200  fork_verify: diag-fv-lifecycle END (cleared) tid=ThreadId(46) had_map=true
                 range_count=37 -- healing completes successfully, NO error, NO fault reported
t=30.247204-323 xfwm4's MAIN thread: RtSigprocmask x2, RtSigaction, then:
t=30.247323500  Tkill { tid: 41, sig: 34 }   -- glibc's own thread-startup sync, targets the new
                 thread (guest tid=41)
t=30.247358200  Futex { Wait { val: 0x80000000, timeout: None } }  -- waits FOREVER
```
**The newly-cloned thread (guest tid=41, Windows `ThreadId(46)`) never appears in the log again
after its healing completes at t=30.096 — no syscall, no fault, no exit, nothing, for the rest of
the 66-second run.** It was created, its memory was healed successfully, and it then silently
never executes another instruction of guest code. The parent's futex wait is a direct, mechanical
consequence — not a bug in glibc's synchronization logic, which is working exactly as designed;
the thread it's waiting on simply never runs to clear the sentinel. **This is glibc's
`pthread_create()` synchronization pattern behaving correctly against a litebox bug**: `clone()`
succeeds, `fork_verify` heals the new thread's memory successfully, and then something between
"healing marked complete" and "new thread actually resumes executing guest code" silently fails to
resume it. **Matches the shape of two already-documented bug classes in project memory** —
`project_advisor_ud_trampoline_fork_bug` and `project_advisor_forked_child_text_not_present` — both
describe a forked/cloned child that gets created and healed but never reaches its first real
instruction. **Open question**: is this literally the same root cause recurring on a different
trigger path, or a related-but-distinct issue specific to `CLONE_THREAD` (`pthread_create`) as
opposed to plain `fork()`? **This reframes the ENTIRE session's XFCE investigation**: the
GTK-internals / xfconf / dbus / X-protocol threads were following a real symptom to a real dead
end — the actual bug is in `litebox_platform_windows_userland`'s clone-resume path, squarely a
Windows-platform fork/thread-emulation bug (consistent with `feedback_fork_verify_windows_only` in
project memory — this entire bug class only exists on Windows, real Linux/macOS `fork()`/`clone()`
never needs this healing machinery at all). **Next step**: investigate exactly what happens
between `fork_verify`'s "END (cleared)" log line and the new thread's actual resume-to-guest-code
step in `litebox_platform_windows_userland/src/lib.rs` — NOT YET STARTED, that file is mid-edit by
advisor-db all session, needs coordination before anyone touches it. The syscall-timeline
instrumentation that found this (`litebox_shim_linux/src/lib.rs` + `litebox_shim_linux/src/diag.rs`,
`LITEBOX_DIAG_SYSCALL_TIMELINE=1`) is committed: `f824eb99`.

**SCOPED PRECISELY: this is NOT "clone()/thread-spawning is broken in general" — it's specific to
one particular thread-resume path.** From the same run: (1) **plain `fork()` works fine** —
`xfce4-panel`'s `fork()` at t=55.920 returns successfully at t=56.753 (833ms, slow but succeeds),
and the child (new pid=112, confirmed = `/usr/lib/xfce4/panel/migrate`) makes real, continuing
forward progress (`set_tid_address` → `rt_sigprocmask` → `rt_sigaction`×3, normal post-fork
sequence, well beyond that point) — no hang. (2) **`CLONE_THREAD` is not universally broken
either** — the SAME `xfce4-panel` process spawns two more worker threads via `clone()` moments
earlier (t=55.900915, t=55.902573, both `ok=true`), and BOTH continue executing interleaved real
work afterward (their syscalls interleave out of chronological order with each other, e.g. two
`futex` WAKE calls a few ms apart from what must be two live racing threads) — a completely
different, WORKING pattern: `futex` **WAKE** (GLib thread-pool notification style), not the
WAIT-forever pattern `xfwm4` hit. (3) `xfwm4`'s hang specifically is: `clone()` succeeds →
`fork_verify` healing completes cleanly → `tkill(tid, SIGRTMIN+2-ish)` immediately followed by
`futex(WAIT, val=0x80000000, timeout=None)` — and the new thread (guest tid=41 / win
`ThreadId(46)`) never appears again anywhere in the log, not even one further syscall. This
specific `tkill`+`futex-WAIT(no timeout)` pattern is glibc's "wait for new thread to clear its
TLS/stack-guard-page-not-ready sentinel" step — narrower than ordinary thread-pool spawning, and
looks tied to a `dlopen()`'d shared library needing TLS setup (matches the `open`/`mmap`/`mprotect`
burst immediately preceding it). **Best current read**: NOT a general clone/fork_verify failure —
most spawns of both kinds work fine in this exact run. Likely either (a) a race where the new
thread's `fork_verify` healing (t=29.910-30.096, ~185ms) doesn't finish before something else
needed for the thread's actual first-instruction resume, or (b) a bug specific to threads carrying
particular clone/stack/TLS setup — `CloneFlags(8195840)` alone doesn't distinguish the working vs.
failing cases (identical across all three spawns above), so the next narrowing step is comparing
full stack/TLS field values precisely, or instrumenting `fork_verify`'s own resume-scheduling
handoff once `litebox_platform_windows_userland` is clear to touch.

**FILE IS CLEAR — coordination resolved, fix work can proceed.** advisor-db confirmed
`litebox_platform_windows_userland/src/lib.rs` working tree is CLEAN (`git status --short` empty);
every diagnostic they'd added there is already committed (`map_shared_memory` content sampling,
the cross-view registry, fixed-address lock timing, the Debug-formatter fix). No coordination
needed, no conflict risk. advisor-db is deliberately staying out of the file entirely while this
proceeds.

**Precise pointer to the exact seam, from advisor-db's own work on this path tonight**:
`run_thread_inner` at `lib.rs:2598`, specifically the closure around `~2627`:
```rust
ThreadHandle::run_with_handle(&tls_state, || unsafe {
    // Arm fork_verify strictly AFTER run_with_handle's install_tls and strictly
    // BEFORE run_thread_arch ever resumes guest code
    if let Some(relocations) = fork_verify_relocations {
        fork_verify::begin(relocations);
    }
    run_thread_arch(&mut thread_ctx, &tls_state);
});
```
This is exactly the seam the syscall trace brackets: `fork_verify::begin` runs and logs its
lifecycle to completion, and `run_thread_arch` is what actually resumes guest code. A thread that
logs "END (cleared)" and then never executes a single guest instruction is failing between those
two lines, or inside `run_thread_arch`'s entry itself.

**Three things that may save time, from advisor-db's own prior work on this exact path**:
a) **The ordering in that closure is deliberate and load-bearing — known trap, do not "fix" it by
   reordering.** Comment at `2628-2631` and an earlier pass's note in the runner (`lib.rs:1103`)
   record that arming `fork_verify` BEFORE `run_thread` was previously tried and was a silent
   no-op, because `fork_verify::begin` only takes effect once `get_tls_ptr()` returns `Some`,
   which is only true partway through `run_thread`'s internals.
b) **Two distinct resume paths exist and are easy to conflate.**
   `run_thread_with_fork_verification` (`2589`) is used by the CROSS-PROCESS fork child via
   `adopt_forked_process` in the runner at `lib.rs:1110`. The same-process `do_clone` path (the
   `xfwm4` case — `CLONE_THREAD`) arms verification through `Task::init`'s `ThreadInitState`
   (`process.rs`, `ThreadInitState` at `924`, the `NewThread` variant at `928`) instead — **the
   runner's cross-process call site is a red herring for this specific bug.**
c) **Ties into an earlier this-session correlation that was refuted as a CAUSE but may still be a
   symptom of this same bug.** The earlier finding that concurrent `fork_verify` healing passes
   correlate with faults (41 runs, zero faults whenever only one pass was live) was refuted as
   causal — but if a healed thread can silently fail to resume, that correlation is honestly
   explained: more concurrent passes means more chances for one to not come back. Worth checking
   whether the non-resuming thread's healing pass overlapped another concurrent pass.

**CORRECTION (self-caught, code-verified): point (c) above is WRONG — `fork_verify` is NOT
involved in xfwm4's hang at all. `ThreadId(46)` was never xfwm4's cloned thread.**
`a63e8ca59285f5871` verified via code that `begin_fork_child_verification` (the only real
`fork_verify` arming path besides the explicit cross-process
`run_thread_with_fork_verification`) is called ONLY from `ThreadInitState::ForkedChild`
(`litebox_shim_linux/src/syscalls/process.rs:4633-4655`). `xfwm4`'s `clone()` flags —
`CloneFlags(8195840)` = `VM|FS|FILES|SIGHAND|THREAD|SYSVSEM|SETTLS`, no `VFORK` — make
`is_process_clone = false` (`process.rs:2251`), correctly routing to `ThreadInitState::NewThread`,
**which never touches `fork_verify` at all.** The real explanation: the same trace shows pid=42
(`dbus-daemon`) forking pid=43 (`/usr/libexec/at-spi-bus-launcher`) at t=29.957, that child exiting
by signal at t=30.069, and `dbus-daemon` itself doing `exit_group` at t=30.095929200 — matching
the earlier "`fork_verify` end (cleared)" timestamp to the microsecond. **`ThreadId(46)` was
`dbus-daemon`'s own unrelated `fork()`-based process spawn, coincidentally overlapping `xfwm4`'s
`clone()` in wall-clock time** — a timing correlation mistaken for causation, exactly the class of
error this session has repeatedly caught and corrected. **The bug is purely within the
same-process `CLONE_THREAD` path, `ThreadInitState::NewThread`'s dispatch**
(`process.rs:4555-4632`), which sets `rsp`/`rax`/`tls`/`child_tid` and already has prior
debug-level instrumentation at `4600-4630` (`"clone/NewThread: init_thread_context reached"`) that
would show directly whether `init_thread_context` is reached at all and whether the guest stack is
readable — **but this was invisible under `LITEBOX_LOG=error`, since it's a `debug!`-level line.**
**Immediate next step**: rerun with `LITEBOX_LOG=debug` (or `trace`) to actually see whether that
line fires, combined with the fast `xfce4-about --version` repro below for quick iteration.

**FULL RESUME PATH MAPPED STATICALLY (no runs, no disk cost) — two precise diagnostic checkpoints
identified for whoever reads the debug capture.** Complete call chain for the same-process
`CLONE_THREAD` path: `spawn_thread` (`litebox_platform_windows_userland/src/lib.rs:3497` — real
`std::thread::Builder::new().stack_size(32MB).spawn(...)`, error-checked; a spawn failure logs
`error!()` and returns ENOMEM, which would show as `clone() ok=false` — but `xfwm4`'s `clone()` was
`ok=true`, so the OS thread genuinely got created) → `thread_start` (`lib.rs:3423` — builds
`TlsState`, calls `ThreadHandle::run_with_handle` to install TLS, then inside that closure calls
`init_thread.init()` then `run_thread_arch`) → `run_thread_arch` (`lib.rs:2879`, naked asm — saves
host sp/bp into `TlsState`, `call init_handler`) → `init_handler` (`lib.rs:7278` — pre-commits
stack pages, then `shim.init(ctx)`) → `LinuxShimEntrypoints::init`
(`litebox_shim_linux/src/lib.rs:149` — calls `enter_shim(true, ctx, Task::handle_init_request)`) →
`enter_shim` (`lib.rs:282` — runs `handle_init_request`, which calls `init_thread_context` — the
`ThreadInitState::NewThread` dispatch setting `rsp`/`rax`/`tls`/`child_tid`, `process.rs:4555` —
then `task.prepare_to_run_guest(ctx)`; `true` → `ContinueOperation::Resume`, `false` →
`Terminate`, which would exit not hang) → `prepare_to_run_guest`
(`litebox_shim_linux/src/wait.rs:38` — delegates to the core `litebox::event::wait` crate,
processes pending signals, returns `!is_exiting()`).
**Two clean diagnostic checkpoints, in order, both previously silenced under `LITEBOX_LOG=error`**:
1. `litebox_shim_linux/src/lib.rs:150`: `warn!("drm-diag: init() entry")` — fires on EVERY
   thread/process init, WARN level. Present for `xfwm4`'s guest tid=41 → `shim.init()` was reached
   (the OS thread started and ran real litebox code). Absent → the failure is even earlier, inside
   `thread_start`/`run_with_handle`/`run_thread_arch`'s asm prologue itself, before `shim.init()`
   is ever called.
2. `process.rs:4622` `debug!()`: `"clone/NewThread: init_thread_context reached"` — fires but
   nothing after it (no syscall ever) → the failure is inside/after `prepare_to_run_guest`'s
   resume decision, or in the asm's actual jump-back-to-guest-code path.
**Whichever of these two is the LAST one present in the capture pinpoints the exact gap.**

**RESULT, MAJOR CORRECTION: the thread DOES resume and run real guest code. The bug is a LOST
FUTEX WAKEUP, not a thread that never resumes into guest code.** advisor-db's `LITEBOX_LOG=debug`
capture on the fast `xfce4-about` repro:
```
t=13.068       clone/NewThread: init_thread_context reached tid=18 host_tid=5860
               rip=0x30f4a573 rsp=0x33662128 tls=Some(862346040) stack_readable=Some(true)
t=13.07-13.46  tid=18 RUNS REAL GUEST CODE: 5,112 signal ops, 1,170 file ops, 913 memory ops
t=13.464       tid=18 -> futex WAIT enter addr=846000944 val=0 timeout=None
t=32.803       tid=17 (the MAIN thread) -> futex WAIT enter addr=821315968
neither address is EVER woken; whole-run totals: 2 futex WAITs, 44 WAKEs, zero overlap
```
**Both of the diagnostic checkpoints above fire** — `init_thread_context` is reached, and the
thread executes ~400ms of real guest work afterward. By the decision tree above that puts the
failure past both markers, and the data confirms it: **this is not a resume-path bug at all.**
**Refined diagnosis**: the cloned thread starts correctly, does real work, then blocks on a futex
with no timeout that nobody ever signals. The main thread later blocks on a DIFFERENT futex,
also never signaled. Two threads, two waits, zero matching wakes out of 44 total wakes in the
run — **a deadlock in futex wake delivery or wait/wake address pairing, i.e. a genuine litebox
lost-wakeup bug**, materially different from (and more specific than) "healed thread never
resumes." **Suspected mechanisms, in priority order**:
1. A WAKE on an address may not be reaching a waiter registered on the same address — a wait-queue
   keying mismatch would produce exactly this (wakes happening, waiters never woken).
2. **Classic lost-wakeup race**: a wake issued BEFORE the waiter registers is lost rather than
   remembered/queued — fits "44 wakes, 2 waits, no overlap" precisely. **Specific supporting
   detail**: tid=18's last actions before parking are a futex WAKE (`woken=0`, no waiter present
   yet) immediately followed by `process_signals` then its own WAIT — a wake with `woken=0`
   landing just before the partner registers is exactly this shape.
3. The `val=0` check: futex WAIT should return immediately if the word no longer equals the
   expected value at check time — a stale/racy comparison could park a thread that should never
   have slept in the first place.
**Next step**: advisor-db has the full 154MB debug log and will pull specific excerpts (not share
wholesale, will delete once extracted) — needs specific addresses or time ranges to dig into next.

**Independently triple-confirmed**: `a63e8ca59285f5871`'s own separate debug capture (disk-safe,
filtered at write time, <40 lines) shows the identical two-checkpoint pattern back-to-back
(`"drm-diag: init() entry"` at t=13.008983, `"clone/NewThread: init_thread_context reached"` 10
microseconds later at t=13.008993, `rip`/`rsp`/`tls` all plausible, `stack_readable=Some(true)`) —
matches advisor-db's finding exactly. **Both agents initially read this as "the resume path itself
is fine, so the failure must be in `prepare_to_run_guest` or the asm jump-back" — corrected**: per
advisor-db's fuller syscall-level trace, the thread genuinely DOES resume and run ~400ms of real
guest code (not an immediate hang at resume) before parking on the never-woken futex. The two
log-line checkpoints alone under-determine the failure point; the syscall-level trace is what
actually located it. **Current live focus, assigned to `a63e8ca59285f5871`**: read litebox's
actual futex WAIT/WAKE implementation (`litebox_shim_linux/src/wait.rs`, likely delegating to the
core `litebox::event::wait` crate) for a wait-queue keying mismatch, a dropped-not-queued wake
(classic lost-wakeup), or a val-check/registration race — using the specific clue that the hanging
thread's own last action before parking was ITSELF a `futex WAKE` returning `woken=0`, immediately
followed by parking on its own futex.

**RESOLVED to a specific mechanism: THIS IS A MISSING WAKE (the wake for these futexes is never
issued at all), NOT a queue-keying mismatch.** advisor-db extracted and analyzed the full 44-wake
list against the two waited addresses:
```
t=13.464  tid=18  waits addr=846000944  -- NEVER returns (zero "futex: WAIT return" lines at all)
t=32.803  tid=17  waits addr=821315968  -- NEVER returns
```
**Decisive facts across all 44 wakes**: not one wake address equals a waited address; not one is
within 100 KB of either; **every single wake reports `woken=0`** (zero waiters found, every time).
If this were a keying bug, wakes would be expected at addresses derived from the waited ones (off
by a constant, a page offset, a hash collision) — there is none of that. **The wake and wait sets
simply never intersect anywhere in the run.**
**The wake pattern itself is informative**: t=0.08-3.28 shows a steady march of one-off wakes at
~36 MB intervals (`38301584, 74542992, 110784400, ...`) — looks like per-thread/per-process
structure init, each waking its own word with no waiter yet (expected/benign). **t=13.06: tid=17
fires TWELVE wakes at `addr=841531032` in 4 milliseconds, all `woken=0`** — a tight retry burst
against one address that never has a waiter. t=13.464: tid=18 wakes `848781720`, then parks on
`846000944` microseconds later — two different words ~2.8 MB apart. **t=24.0: tid=17 fires
FOURTEEN MORE wakes at that same `841531032`, again all `woken=0`.** The repeated hammering of one
address by the main thread while `tid=18` sits on a completely different address (`846000944`,
~4.5 MB away — smells like two different thread structures, not two fields of one) is what a
broken handshake looks like: **one side signaling a word the other side is not watching.**
**Leading hypothesis for where to look**: whatever glibc uses to signal thread-start completion —
either litebox is computing the wake address from the WRONG FIELD of the thread descriptor, or a
wake that should target the waiter's word is targeting the signaler's own word instead.

**REFINEMENT (self-caught by a63e8ca59285f5871, independently reproduced the same addresses):
the "wrong field" hypothesis is WEAKENED — the address deltas are too large for it.** Own separate
run confirms the exact same addresses (`tid=18` parks at `846000944`; `tid=17` hammers
`841531032` 26× with `requested=INT_MAX` "wake all", always `woken=0`; `tid=18` itself wakes a
third address `848781720` just before parking; `tid=17` later parks at `821315968`
`val=0x80000000`, the same glibc thread-creation-sync sentinel from the very first trace last
night). All four addresses are properly 8-byte aligned, but pairwise deltas are multi-megabyte
(4.4MB, 7.2MB, 2.7MB, 20MB, 24MB) — **not a small constant offset (4/8/16 bytes)** that "reading
the wrong field of the same nearby struct" would produce. This looks more like genuinely separate
memory regions (different threads' stacks/TLS blocks, typically MB apart from fresh mmaps) —
arguing against a simple wrong-field bug and toward either (a) legitimate, unrelated
synchronization points in normal musl/glibc/glib startup, or (b) a subtler bug (a stale per-thread
pointer, or referencing the wrong thread's control block entirely, not just a field within it).

**Important reframe on `woken=0`, worth internalizing broadly**: `requested=INT_MAX` + `woken=0`,
repeated many times on one address by the same thread, is **normal behavior on real Linux too** —
futex-based mutex/condvar implementations routinely call `FUTEX_WAKE` speculatively on unlock even
when nobody is waiting (glibc's `pthread_mutex_unlock`/GLib's `GMutex` both do this — fast path,
wake unconditionally, let the kernel say 0 waiters). **`tid=17`'s repeated `woken=0` wakes may not
themselves be a bug** — could just be a lock being pulsed with nobody waiting at that instant. **The
one unambiguous, definitely-broken fact remains**: `tid=18` parks at `846000944` `val=0`, no
timeout, and is NEVER woken by anything for the rest of the run (10+ seconds observed).

**Sharper next question, replacing "which wake call was supposed to match"**: what host-visible
EVENT was `tid=18` waiting for, and did whatever's responsible for producing it actually run? A
10+ second gap with zero logged activity from `tid=17` (t=13.19-24.2s in the independent trace)
raises a new possibility — `tid=17` may ALSO be effectively wedged (busy-spinning, blocked on I/O,
or genuinely working very slowly) rather than genuinely making forward progress; its own
wake-spam at `841531032` could itself be a symptom of a stuck retry loop rather than healthy
operation. **Decisive test proposed and approved**: add memory-content dumps to the trace — read
what's actually stored at `846000944` at park time and periodically afterward. If something
writes a non-zero value there but the corresponding wake message never fires, that's a genuine
lost-wakeup in litebox's futex implementation. If nothing ever writes there at all, `tid=18` is
waiting on an event that never occurs upstream (a different, non-futex bug entirely) — directly
distinguishing "nobody ever unlocks this" from "someone unlocks it but the wake is lost."

**PRECISE CODE-PATH IDENTIFIED: this is fontconfig cache work, not glibc thread-start signaling —
retracts the earlier "look at thread-start handshake" guidance.** `tid=18`'s exact syscalls right
before parking (advisor-db):
```
13.4633  sys_read fd=4 len=120 -> Ok(120)
13.4635  sys_openat /root/.cache/fontconfig/5ca8086aeacc9c68e81a71e7ef846b3b-le64.cache-9
13.4637  sys_openat /usr/share/fonts/encodings/large/.uuid
13.4638  sys_openat /root/.fontconfig/5ca8086aeacc9c68e81a71e7ef846b3b-le64.cache-9
13.4640  sys_openat /usr/share/fonts/encodings/large/.uuid
13.4641  futex WAKE addr=848781720 requested=2147483647 woken=0
13.4642  futex WAIT addr=846000944 timeout=None
```
The thread is deep in fontconfig cache scanning when it blocks — **the earlier suggestion to look
at glibc's thread-start signaling was the wrong target; redirect to whatever synchronization
fontconfig/glib uses around cache init.** `requested=2147483647` (`INT_MAX`) is the signature of a
lock RELEASE or condition BROADCAST ("wake everyone"), not a targeted signal to one specific
waiter — so the sequence is: finish a fontconfig operation, broadcast-release one word, immediately
park on a DIFFERENT word. **This is a condition-variable or once-initialization handshake, exactly
the pattern where a lost wakeup deadlocks.**
**Fontconfig itself is confirmed NOT broken**: standalone in a bare guest, `fc-cache -f -v` returns
`rc=0`, `fc-list` returns `rc=0` and enumerates 46 fonts, no hang. **The layer ships no prebuilt
`/var/cache/fontconfig`**, so every client rebuilds the cache on first run — explaining why this
path is hot at startup for every GTK app (another layer-packaging gap, same family as the missing
SONAMEs/machine-id/X-query-tools, though not necessarily the root cause here since fontconfig
itself works fine standalone).
**The pattern across everything tested tonight is now fully consistent — nothing is broken in
isolation, everything fails only inside the multi-process/multi-threaded display-stack context**:
```
dlopen + TLS   fine bare   hangs in-stack
fontconfig     fine bare   parks in-stack
xfce4-about    exits 1.7s  hangs in-stack
guest timers   fine bare   n/a
raw X path     -           renders a window correctly (works even in-stack)
```
This is precisely the profile of a futex wake-delivery bug that only manifests under real
concurrency — consistent with, not contradicting, the ongoing code investigation.
**Instrumentation addition proposed and approved**: log the wake's target address alongside the
full waiter list AT WAKE TIME (not just after the fact), to distinguish "waiter was registered but
unmatched" from "waiter was genuinely absent when the wake fired" — the one thing current logging
cannot yet tell apart, and the fact that would separate "wake arrives too early" (classic
lost-wakeup race) from "wake never targets that word at all" (address-computation bug).

**DECISIVE: this is NOT a lost-wakeup / futex-manager bug at all — it's upstream of the futex
mechanism entirely.** `a63e8ca59285f5871` added memory-value dumps to both WAIT and WAKE (reads
the actual u32 at the futex address alongside every trace line):
```
t=13.809-13.8xx  tid=17 WAKE addr=841531032, current_value INCREMENTING 1,2,3...12, requested=INT_MAX, all woken=0
t=14.490594      tid=18 WAKE addr=848781720, current_value=Some(1), woken=0
t=14.490647      tid=18 WAIT enter addr=846000944 val=0 current_value=Some(0)  -- CORRECT park, val matches exactly
                 ...nothing touches 846000944 EVER AGAIN for the remaining 23+ seconds...
t=29.919466      tid=17 WAKE addr=841531032, current_value continues 13...26, still woken=0
t=37.996072      tid=17 WAIT enter addr=821315968 val=0x80000000, current_value matches -- tid=17 ALSO correctly parks
```
**Key findings**: (1) `841531032`'s value is a monotonically incrementing counter on every wake
from the same thread (`tid=17`) — this exactly matches GLib's `g_once_impl` pattern
(`g_once_init_leave`: store a new generation value, then unconditional `FUTEX_WAKE(INT_MAX)`,
whether or not anyone's waiting) — `woken=0` here is genuinely normal, not a bug, confirming the
earlier reframe. (2) `846000944`'s wait is **CORRECTLY parked** — `val=0` matches `current_value`
exactly at park time, no race, no stale check, the futex mechanism itself did the right thing.
**But nothing in the rest of the run ever touches that address again — no WAKE, and (implicitly)
no write either.** This is NOT "a wake happened but missed the waiter" — **it's "the write+wake
that was supposed to eventually happen here never happens at all, from any thread, for the rest of
the run."** **This retracts the futex-manager-bug framing**: the bug is not in litebox's futex
WAIT/WAKE correctness — it's that whatever OTHER thread is supposed to (a) finish producing the
value `tid=18` is waiting for and (b) call `FUTEX_WAKE(846000944)` once it does, **never gets
there at all.** Consistent with the fontconfig-cache-init-condvar hypothesis IF the thread
responsible for that init work is itself stuck, never spawned, or never reaches its own completion
code — **which loops the investigation back to a resume-path or thread-spawn question for THAT
specific (different) thread, not a futex-manager correctness bug.** Only 4 threads visible active
near t=13-14.5s, no distinct "initializer" thread visible completing near then — two live
possibilities: (a) `tid=17` itself was supposed to do the cache-init work and write+wake
`846000944`, but is off doing something else (the `g_once` dance, then its own unrelated park at
t=37.9) — i.e. `tid=17` may ALSO be stalled/deprioritized rather than genuinely progressing; or
(b) a dedicated worker thread for this was supposed to spawn and simply never did — the SAME class
of bug as the very first finding in this whole investigation (a thread that should exist but
doesn't). **Approved next step**: add a "log every distinct tid ever seen" + "log thread exit"
trace across the whole window to definitively answer whether an expected initializer thread is
simply missing.

**Complementary instrumentation built in parallel (advisor-db), compiled and verified present in
the binary (built 22:18)** — deliberately not a duplicate of the tid-lifecycle trace above:
1. `"clone: request registered"` — `litebox_shim_linux/src/syscalls/process.rs`, at the
   `thread.init_state.set(init_state)` seam BOTH the `ForkedChild` and `NewThread` arms fall
   through to. Logs `child_tid`+`parent_pid` at the moment a clone is ACCEPTED, before the new
   thread has any chance to run.
2. `"futex: WAKE matched nothing"` — `litebox/src/sync/futex.rs`, in `wake()` when `woken == 0`
   (already existed; kept as corroboration — expected to be quiet per the current read, and
   quietness there would itself support the finding above).
**Why this is the discriminating complement, not a duplicate**: the tid-lifecycle trace enumerates
tids that were SEEN (executed). This one enumerates tids that were ASKED FOR (`clone()` accepted).
The set difference between them settles the two live possibilities cleanly:
- **Clone request logged, no corresponding start in the lifecycle trace** → the spawn path itself
  drops it — bug is in thread creation/scheduling, HOST-side (litebox).
- **No clone request logged at all for the missing thread** → the guest never even called
  `clone()` — bug is upstream in the GUEST's own logic (a library deciding not to spawn its
  worker, e.g. because an earlier probe/init call returned something unexpected) — and the whole
  thread-spawn machinery is exonerated.
**Note**: fires on the fork path too (shared seam) — expect volume on a heavy-forking run; grep
for the specific tid rather than reading linearly.
**Practical blocker, needs resolving to actually run this**: advisor-db's ~100x-faster repro
(`advisor/probes/run_ctx_test.sh`) is not packaged inside their current layer tar, and rebuilding a
fresh 2.5GB layer risks repeating the earlier disk-fill incident (124GB free right now — real
headroom, but advisor-db is rightly cautious about burning it on a full tar copy per iteration,
per the standing disk-hygiene lesson). If whoever has a layer with a working script-injection step
already wired can run this repro against the newly-built binary, it settles both questions in one
~2 minute pass.

**RESOLVED: this is ONE bug, not two — collapses the whole investigation to a single precise
question.** `a63e8ca59285f5871`'s full thread census (clone/execve/exit_group + futex trace
together, on `xfce4-about --version`):
```
t=12.232717  pid=17 execve /usr/bin/xfce4-about
t=13.023028  tid=17 starts its own g_once-shaped counter at addr=841662104 (Some(1)..Some(12), woken=0)
t=13.025455  clone: parent_tid=17 child_tid=18 flags=CloneFlags(4001536) is_process_clone=false
             -- the ONLY thread xfce4-about ever spawns
t=13.362118  tid=18 WAKE addr=848912792 current_value=Some(1) woken=0
t=13.362172  tid=18 WAIT enter addr=846132016 val=0 current_value=Some(0)  -- PARKS HERE, correctly
             ...tid=18 NEVER appears again anywhere in the rest of the 33-second run...
t=24.810392  tid=17 resumes its g_once hammer (Some(13)..Some(26), still woken=0)
t=32.970194  tid=17 WAIT enter addr=821447040 val=0x80000000 (the sentinel)  -- tid=17 ALSO parks
run ends (timeout)
```
**Only TWO threads ever exist in this process — `tid=17` (main) and `tid=18` (its one worker).
There is no "missing initializer thread"** — `tid=18` IS the thread supposed to do the init work
(fontconfig cache scan, per the earlier syscall evidence) and then signal completion or exit.
**The `val=0x80000000` sentinel `tid=17` waits on at the end is glibc's classic "wait for this
specific thread to finish" pattern** (`pthread_join`-equivalent / `__libc_start_main`'s exit-wait)
— NOT a separate bug, but the direct mechanical consequence of `tid=18` never finishing:
`tid=17` is waiting for `tid=18`'s `clear_child_tid` futex wake (`litebox_shim_linux/src/syscalls/
process.rs:1772-1785`, `prepare_for_exit`'s wake-on-thread-exit code, which only fires if the
thread actually reaches `prepare_for_exit`), and `tid=18` never reaches its own exit because it's
stuck at `846132016` first. **Everything downstream (`tid=17`'s own park) is just this one hang
propagating up — one bug, not two.**
**The real, now-narrower open question**: what is SUPPOSED to write a non-zero value to
`846132016` and wake it? `tid=17` is still actively running (its own `g_once` dance) when `tid=18`
parks at t=13.36 — not itself stuck yet — so it COULD in principle be a producer/consumer handoff
where `tid=17` is meant to eventually signal `tid=18`. **But the trace shows `tid=17` never
touches `846132016` (or anything near it) at any point in the whole run** — only `841662104` (its
`g_once` counter) and finally `821447040` (its own join-wait). **`tid=17` genuinely never attempts
to wake `tid=18`'s futex either.** Narrows to exactly two possibilities: (a) this is a
signal/timerfd/epoll-driven wake, not another thread — e.g. fontconfig's cache scan waiting on an
inotify or timer event that litebox's emulation never delivers; or (b) `tid=17` was SUPPOSED to
call futex-wake on `846132016` as part of some code path it takes between t=13.36 and t=32.97, but
instead takes a different path (the `g_once` retry loop) that never reaches the real wake.
**Approved next step**: full syscall-type histogram for `tid=17` specifically across the
t=13.36-32.97s window (not just futex calls) — what is it actually spending 19+ seconds doing
instead of servicing `tid=18`.

**Discriminator question (host-side spawn bug vs. guest-side never-called-clone) already answered
from existing data — no rebuild/rerun needed.** `a63e8ca59285f5871` had both signals already: (1)
its own `"clone: spawned new task"` log (`process.rs:3162`, fires only AFTER `spawn_thread`
returns `Ok` — i.e. the real OS thread already succeeded) shows exactly one clone from
`xfce4-about`'s main thread, `parent_tid=17 child_tid=18` at t=13.025455700, a real
`pthread_create`; (2) `tid=18` then makes genuine, unambiguous guest syscalls afterward (the
`WAKE`/`WAIT` pair above), only reachable after `init_thread_context`/`prepare_to_run_guest`/
`run_thread_arch`'s asm resume have all already succeeded. **The set difference is EMPTY**: the
one thread that was clone-requested is the same one seen executing real guest code. **Confirms
definitively: NOT a host-side spawn-scheduling bug** (clone accepted, thread never starts) — `tid=18`
genuinely starts, runs real code, and only then gets stuck on its own internal futex wait, exactly
as already concluded above. No missing thread, no silently-dropped clone. advisor-db's binary/
repro is not needed to re-answer this specific question; effort redirects to the syscall
histogram for `tid=17` above.

**HISTOGRAM RESULT: `tid=17`'s later park (at t~31-33) is CONFIRMED a downstream cascade of
`tid=18`'s original hang, not an independent second bug — collapses the investigation to ONE
precisely-scoped open question.** `a63e8ca59285f5871`'s full request-detail trace: `tid=17` is
genuinely BUSY, not idle, across t=13.36-32.97 — 494 real syscalls, dominated by `recvmsg` (143,
X11/DBus event loop) and `mmap`/`open`/`mprotect` (dlopen-shaped library loading, consistent with
GTK module/theme-engine loading). Real forward progress. The window ends with:
```
t=31.076831800  tkill enter: Tkill { tid: 18, sig: 34 }   -- targets tid=18 BY NAME, explicitly
t=31.076868900  futex enter: Wait { addr: 810305920, val: 2147483648, timeout: None }  -- tid=17 parks
```
This is glibc's dynamic linker doing a `dlopen()`-triggered TLS/module-load quiesce
(`membarrier`+per-thread `tkill` handshake — same SHAPE as thread-creation sync but with no
preceding `clone()` in this run, so it's dlopen's "quiesce all existing threads to update TLS"
path, not a spawn). **It explicitly signals `tid=18` — the SAME thread parked at `846132016`
since t=13.362 — and then waits for it to acknowledge.** So `tid=17`'s hang is the direct,
mechanical consequence of needing to synchronize with `tid=18` as a normal part of loading another
shared library, and being unable to because `tid=18` has been unresponsive the whole time. **Fixing
`tid=18`'s original park should resolve this second hang as a side effect — no separate fix
needed for it.**

**ENTIRE BUG NOW NARROWED TO ONE PRECISE QUESTION: why does nothing ever write a non-zero value to
`addr=846132016` and wake `tid=18` from its `futex Wait(val=0, no timeout)` at t=13.362172?** Both
threads in this process are fully accounted for — `tid=17` never touches this address at any point
before its own later, unrelated (now-explained) hang. So whatever `tid=18` is waiting for is
either (a) meant to come from OUTSIDE this process entirely — e.g. a response from Xwayland/dbus/
the X server over a socket, if `tid=18`'s blocked operation is itself gated behind a prior
`recvmsg`/`poll` that never completes — worth checking `tid=18`'s own syscalls in the moments
just before it parks, not just at park time; or (b) a signal/timer-driven wake litebox's
timer/signal emulation never delivers.

**COMPLEMENTARY, POSSIBLY-CONVERGING theory (advisor-db, independent repro, `advisor/probes/
ctx-futex-evidence.txt`) — a signal delivered while the guest still holds a lock it needs back.**
Their own independent run shows the SAME two-wait shape (`tid=18` at `846656304` val=0; `tid=17`
at `821971328` val=`0x80000000`=`FUTEX_WAITERS`, both correctly parked, both never touched again)
— **and crucially, `tid=17`'s val being exactly `FUTEX_WAITERS` means it's blocking on a lock
whose contended bit is ALREADY set: waiting for an owner to hand it off, not merely quiescing.**
Immediately before that park: `WARN signal: process_signals entry with ctx tid=17 ... orig_rax=200`
— `orig_rax=200` is `tgkill`, appearing EXACTLY ONCE in the entire run, at this exact moment
(every other `orig_rax` for `tid=17` is ordinary: 9 `mmap`, 2 `open`, 202 `futex`, ...).
Immediately prior to THAT: bulk sequential reads of one file to EOF, then four `mmap`s and a
`munmap` — the dynamic-loader-mapping-a-shared-object signature (same dlopen shape both agents are
seeing). **advisor-db's read**: a library finishing `dlopen` sends a signal to a sibling thread
(`tgkill`), and the handler needs a lock the caller is holding. On real Linux this is safe if the
lock is released before the signal fires, or the handler doesn't take it. **Under litebox, if
signal delivery is injected at a point where the guest still holds that lock — or delivery runs on
the wrong thread's context — this exact shape results: a correctly-parked waiter on a
correctly-contended lock, with the owner parked elsewhere.** This would also explain the
session-wide context-dependence (bare `xfce4-about` is fine, in-stack hangs) — it needs a SECOND
live thread to be signaled at the wrong instant, which only exists once real concurrency is
present. **Predicts the bug is in WHERE `process_signals` is allowed to run relative to guest lock
ownership — consistent with, and possibly the same underlying issue as, the `tid=18`-origin
question above** (a signal-during-dlopen-lock-hold could equally explain why `tid=18`'s own
completion signal to `846132016` never arrives, if the same class of mistiming affects the very
first dlopen in the chain). **Corroboration from both agents independently**: advisor-db's
`"futex: WAKE matched nothing"` instrument fired 44× with waiters absent (matches the earlier
read that it's quiet/uninformative); their clone-request instrument independently matched the
empty-set-difference result above (20 clone requests, zero orphans) — same conclusion via a
second, separately-built tool.
**Reusable, disk-safe repro invocation for anyone continuing this** (advisor-db's `--resume-from`
trick avoids any full-tar rebuild — injects a 10KB script tar over the existing layer):
```
MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1 LITEBOX_LOG=debug \
./target/release/litebox_runner_linux_on_windows_userland.exe \
  --initial-files layer_timeline3.tar --resume-from <win-path>/ctx_inject.tar \
  --env HOME=/root -- /bin/sh /run_ctx_test.sh
```
Two traps already hit and worth avoiding: MSYS mangles `/bin/sh` into a Windows path (use
`MSYS2_ARG_CONV_EXCL='*' MSYS_NO_PATHCONV=1`); the runner is a Windows binary so an MSYS-style
`/tmp` path passed to `--resume-from` fails — always pass a real Windows path. Also: a leading
slash combined with a MISSING `--resume-from` file panics into the known stack-overflow bug at
`lib.rs:347` (pass 336) — still live, watch for it.
**Suggested next probe (advisor-db offered, awaiting a decision on who runs it to avoid
duplication)**: log the guest `rip` and the OWNING tid at every `futex WAIT` with
`val==0x80000000`, plus whether `process_signals` is currently on the stack — names the lock
owner directly and confirms or kills the signal-during-lock-hold theory in one run.

**MAJOR: this is NON-DETERMINISTIC — the exact failure point moves run to run, strongly favoring a
race condition over a single deterministic bad line/address.** Own-tooling bug found and fixed
first (`a63e8ca59285f5871`): the syscall-detail trace had been keying on `self.pid` — the
THREAD-GROUP id, shared by all `CLONE_THREAD` siblings including `tid=18` — without also including
`tid`, so `tid=17` and `tid=18`'s syscalls had been silently MERGED under "pid=17" the whole
session. Fixed by adding `tid` to the trace; rebuilt, reran. **With correct per-tid attribution,
this run's `tid=18` hangs at a DIFFERENT, MUCH EARLIER point than the previous run**:
```
t=13.252535600  tid=18 rt_sigprocmask (SIG_SETMASK) -- ok=true
t=13.252801900  tid=18 prctl(SetName(...)) -- ok=true (pthread_setname_np during thread startup)
...tid=18 makes ZERO further syscalls for the entire rest of the run (timed out, run_exit=124)...
```
vs. the PREVIOUS run's `tid=18`, which got much further — a `WAKE`, then a genuine `futex WAIT`
with `val=0`. **This run doesn't even reach its first futex call**; it goes silent immediately
after the first two post-clone setup syscalls. **This run-to-run variability in exactly WHERE
`tid=18` stops is a strong signal of genuine non-determinism** — consistent with advisor-db's
independent finding of a `tgkill` landing during a dlopen-shaped `mmap` burst: a RACE between
thread-startup/dlopen's own internal synchronization and asynchronous signal delivery, not one
fixed line of guest code always failing identically. **Also reframes the session's very first
finding**: "thread parks essentially immediately post-clone, no further syscalls ever" (last
night) and "thread does real work first, then parks in a specific futex" (tonight) may be TWO
OBSERVATIONS OF THE SAME underlying race, not two different bugs — the race just resolves at a
different point depending on scheduling luck each run. **Raises the likelihood this is a genuine
litebox-side bug in how a signal (plausibly the TLS/thread-start synchronization signal itself,
given `tid=17` later `tkill`'s exactly `tid=18`) gets delivered to a newly-`clone()`'d thread
relative to that thread's own startup sequence** — e.g. a signal landing before the thread has
even finished its early setup (`prctl`, etc.), then never being processed/acknowledged correctly,
permanently wedging it regardless of where in startup it happened to be.
**Approved follow-up**: check whether `tid=18`'s sudden silence right after `prctl` means it's
spinning in pure guest userspace (a busy-wait, no host round-trip) vs. genuinely blocked in the
platform layer below the syscall level (an unhandled/swallowed exception) — via VEH/exception
trace activity for `tid=18`'s underlying `win_tid` around t=13.25-13.3 in this specific run.

**BREAKTHROUGH — LIKELY ROOT CAUSE, PRECISE AND STRUCTURAL: litebox's `futex WAIT` is probably not
signal-interruptible, a real Linux-semantics gap.** advisor-db's follow-up probe (log guest `rip` +
owning tid at every `FUTEX_WAITERS` wait) resolves the whole chain end to end. **Headline: `tid=17`
is waiting on a contended lock with `owner_tid=0` — nobody owns it.** Not a deadlock over a held
lock; a signal that never gets delivered to a futex-parked thread. Decisive sequence, one run:
```
13.519402  tid=18  futex WAKE addr=849502616 requested=INT_MAX woken=0
13.519489  tid=18  futex WAIT enter addr=846721840 val=0 current_value=Some(0)
           ...tid=18 emits ZERO further log lines for the rest of the run (verified, count 0 after t=13.6)

36.870760  tid=17  RtSigprocmask { how: SIG_BLOCK, ... }
36.870807  tid=17  RtSigaction  { signum: Signal(34), act: Some(...) }
36.871030  tid=17  Tkill { tid: 18, sig: 34 }
36.871089  tid=17  futex WAIT enter addr=822036864 val=0x80000000
36.871102  tid=17  futex WAIT on CONTENDED lock owner_tid=0 raw_val=0x80000000
```
**Mechanism**: that `sigprocmask` → `sigaction(34)` → `tkill(18, 34)` → `futex-wait` sequence is
glibc/musl's **SIGSETXID / thread-list broadcast** — a thread makes a change every other thread
must acknowledge, signals each sibling, then blocks until they all check in. `tid=17` does exactly
that, then waits for `tid=18`'s acknowledgement. **But `tid=18` has been parked in `futex WAIT`
since t=13.52 — 23 seconds earlier — and never runs again.** The signal is posted to a thread
that's blocked inside a futex wait and is never woken to run its handler. Nobody acknowledges, so
`tid=17` waits forever. **`owner_tid=0` is the proof this isn't a lock-ordering deadlock**: the
word is exactly `0x80000000`, `FUTEX_WAITERS` set with a ZERO tid field — a real held lock would
carry the owner's tid; an unowned-but-contended word is what a handoff protocol produces when it
never completes.
**What this means for the fix — a Linux-semantics gap, not a lock-specific bug**: on real Linux, a
signal sent to a thread blocked in `futex(FUTEX_WAIT)` INTERRUPTS the wait — the thread returns
`EINTR`, runs the handler, and (for restartable cases) re-enters the wait. This is precisely how
SIGSETXID acknowledgement works at all — every sibling is usually parked somewhere when the
broadcast arrives. **If litebox's futex wait is not interruptible by signal delivery, this
deadlock is STRUCTURAL and will hit any multithreaded glibc/musl program that triggers a
setxid/thread-list broadcast** — exactly why it reproduces across every GTK app and never in
single-threaded bare runs. **The one question that decides the fix**: does `futex_manager.wait()`
have any path that returns `EINTR` on a pending signal, and does signal delivery to a parked
thread wake its waker? Predicted answer: no — the wait likely blocks on a host primitive with only
`done` and timeout as wake conditions, no signal-pending check. **If confirmed, the fix is to make
the futex wait signal-interruptible** (register the parked thread so signal delivery wakes it,
return `EINTR`, let the guest's restart logic re-enter) — not anything futex-keying-specific.
**Subsumes the earlier cascade framing cleanly**: `tid=17`'s park IS downstream of `tid=18`'s (as
already established), but the coupling is the undeliverable signal, not a shared lock.
**Explicit caveat, not papered over**: WHY `tid=18` parked at t=13.52 in the first place (its own
wait at `846721840` val=0 is a plain condition wait, not contended) is still unexplained — it was
doing fontconfig cache work (`/root/.cache/fontconfig/...cache-9`,
`/usr/share/fonts/encodings/large/.uuid`) immediately prior. **These are two separate defects**:
even if `tid=18`'s original park turns out legitimate and short-lived on real Linux, the
signal-interruptibility gap alone would still convert it into a permanent hang here — worth fixing
on its own merits regardless of how the first question resolves.
**Assigned**: `a63e8ca59285f5871` — verify directly in code whether `futex_manager.wait()` (or
equivalent) checks/returns on pending signals at all; advisor-db is available to instrument the
signal-delivery path (does anything attempt to wake a parked thread on `tkill`?) if that's not
already covered, to avoid duplication.

**ROOT CAUSE CONFIRMED, EXACT LOCATION, FOUND VIA DIRECT CODE READING — no run needed.**
`litebox_shim_linux/src/syscalls/signal/mod.rs`, `do_kill` (called by both `sys_tkill` and
`sys_tgkill`), lines 791-794:
```rust
if tid.is_some_and(|tid| tid != self.tid) {
    log_unsupported!("sys_tkill/sys_tgkill with a remote tid");
    return Err(Errno::ESRCH);
}
```
**Explicit and unconditional: ANY `tkill`/`tgkill` targeting a DIFFERENT thread than the caller is
rejected outright with `ESRCH`.** But `tkill(tid, sig)`'s entire POSIX purpose is signaling an
ARBITRARY OTHER thread — that's the only reason it takes a `tid` argument instead of just being
`raise()`. **Self-signaling-only support means every real cross-thread `tkill` use is silently
broken.** Directly confirmed live in the trace: `tid=17`'s exact call `Tkill { tid: 18, sig: 34 }`
shows `syscall=tkill ok=false` at t=28.7426s in one run; the very first trace from the start of
this investigation showed the identical shape (`tkill` immediately followed by a `futex Wait` that
never returns). Signal 34 = `SIGRTMIN` (real-time signal base), which glibc/musl's NPTL
implementation uses internally for exactly this cross-thread pthread synchronization (dlopen's
TLS-update quiesce handshake). **glibc's internal `__nptl_setxid`/TLS-update signal-and-wait path
does NOT check `tkill`'s return value** — it's an internal implementation detail, not something
application code is expected to see fail — it fires the `tkill`, then unconditionally
futex-waits for the target to acknowledge via a shared counter/flag. Since litebox's `tkill`
silently no-ops, **the target thread NEVER receives the signal, NEVER runs whatever
handler/acknowledgment code was supposed to bump the futex word, and the waiter blocks forever.**
**This single `ESRCH` guard plausibly explains BOTH observed hang shapes** (early, right after
`prctl SetName`; and later, after real work, in a specific `futex Wait`) — the SAME handshake
pattern (`tkill` + `futex-wait`) recurs at multiple points during thread startup/dlopen (matches
glibc's NPTL design — this pattern fires for every dlopen involving TLS-using modules, not just
once), so WHICH occurrence "wins the race" and hangs first varies run to run depending on
scheduling — **the non-determinism observed above, now explained without needing an actual race
condition**: it's a deterministic missing feature (cross-thread `tkill`) that different runs
happen to trip over at different call sites depending on thread interleaving.
**Proposed fix**: implement real cross-thread signal delivery in `do_kill` for the `tid.is_some()`
case instead of rejecting it — (1) look up the target `Task` by `tid` (likely via the Process's
thread table, similar to how the existing `deliver_to_child` closure looks up child processes),
(2) enqueue the signal on that target `Task`'s own pending-signal queue (the same mechanism
self-signaling and process-directed signals already use), (3) interrupt the target thread if
currently blocked in a wait — litebox already has `interrupt_thread`/
`ThreadProvider::interrupt_thread` (`litebox_platform_windows_userland/src/lib.rs:3586-3590`) — so
a thread parked in an unrelated futex wait gets woken to process the new signal, exactly like real
Linux's signal delivery. **Ready to implement.**

**Independently confirmed by advisor-db, exact same location and mechanism, arrived at
separately.** Confirms `owner_tid=0` matches exactly: the lock is contended with no owner because
the handoff was never started (`tid=18` was never told to produce the acknowledgment). advisor-db
retracts their own earlier "futex wait is not signal-interruptible" theory — the interrupt
machinery is fine and `WaitError::Interrupted` exists; the signal simply never gets sent in the
first place, so interruptibility was never the bottleneck. **The fix is smaller than either agent
initially proposed.**

**SECOND, INDEPENDENT DEFECT FOUND — WHY THIS WAS INVISIBLE FOR 300+ PASSES ALL SESSION, worth
fixing on its own merits regardless of the `tkill` fix.** `log_unsupported!` expands to:
```rust
fn log_unsupported_fmt(args: core::fmt::Arguments<'_>) {
    if cfg!(debug_assertions) {
        litebox_util_log::warn!(feature:% = args; "unsupported");
    }
}
```
**In a release build, that body compiles to nothing.** Every run in this entire investigation has
been `--release`. So the single most load-bearing event in the whole hang — "I was asked to do
something I do not implement, and I silently lied about it" — was invisible by construction, at
ANY `LITEBOX_LOG` level, all session. This is why the night went to futex keying, lost wakeups,
thread spawn, and lock ordering: **the actual failure never appeared in any log captured, because
the log line reporting it doesn't exist in release builds.** Every `log_unsupported!` site in the
tree is currently a silent behavioral divergence from Linux in exactly the build configuration
used for testing. **Fixing this (log unconditionally at warn, or gate on an env var instead of
`cfg!(debug_assertions)`) is probably the single highest-leverage observability change available
right now** — likely to surface several more silently-unimplemented syscalls/features immediately.

**Task split, confirmed**: `a63e8ca59285f5871` takes the `do_kill` remote-thread-delivery fix
(already assigned, in progress); advisor-db takes the release-build `log_unsupported!` logging
fix; both re-run the fast repro against the combined result once ready. **Both halves of the
`do_kill` fix already exist elsewhere in the codebase and just need wiring together, not new
machinery**: `interrupt()` is already called on siblings in `exit_group`/`kill_other_threads`
(`process.rs:831`/`895`) — the wake-a-parked-sibling path is proven to work.

**Why this unblocks XFCE broadly, not just one test client**: signal 34 with that
`sigprocmask`/`sigaction`/`tkill`/`futex-wait` preamble is glibc/musl's SIGSETXID / thread-list
broadcast — routine in any multithreaded program touching setuid/setgid/locale/nsswitch. This is
exactly why it reproduces across every GTK client and never in a single-threaded bare run —
"works bare, hangs in the stack" has been the signature all along, and any threaded XFCE
component can hit it.

**Standing caveat, still open**: `tid=18`'s ORIGINAL park at t=13.52 (`846721840`, `val=0`, plain
condition wait, immediately after fontconfig cache work) is still unexplained. Priority order
changes though — fix `tkill` first, then re-check: with remote delivery working, does `tid=18`'s
park turn out to have been a real problem, or was it idling correctly the whole time, simply
waiting for a broadcast that (pre-fix) could never arrive?

**LOGGING FIX LANDED, ALREADY PAID FOR ITSELF: 19 distinct unsupported features exposed that were
invisible in every release run this session has ever done.** advisor-db's change:
`litebox_shim_linux/src/lib.rs:121`, `log_unsupported_fmt` no longer wraps its `warn!` in
`if cfg!(debug_assertions)` (comment records why — `warn!` is already level-filtered at runtime,
there was never a reason to also strip it at compile time). **Cost note**: at `LITEBOX_LOG=warn`
this run is 14MB vs 165MB at `debug` — cheap enough to be the default for a first-look run,
greppable in one pass. **Full census, one `xfce4-about`-in-stack run** (count × feature):
```
26  setitimer: nonzero it_interval not supported
20  unsupported syscall fstatfs
 6  fcntl(cmd = 1033, arg = 2)
 5  unsupported syscall membarrier
 4  fcntl(cmd = 1034, arg = 48)
 4  fcntl(cmd = 1034, arg = 0)
 2  unsupported syscall inotify_init1
 2  unsupported syscall fadvise64
 2  setsockopt(level = 1, optname = 26)
 2  setsockopt(level = 1, optname = 16)
 2  ioctl Raw { cmd: 3222823994 }
 1  unsupported syscall pidfd_open
 1  unsupported syscall inotify_init
 1  unsupported syscall getresuid
 1  unsupported syscall close_range
 1  sys_tkill/sys_tgkill with a remote tid       <- the root cause above, appears EXACTLY ONCE
 1  ioctl Raw { cmd: 1074021792 }
 1  fcntl(cmd = 1033, arg = 3)
 1  fcntl(cmd = 1033, arg = 14)
```
Saved to `advisor/probes/unsupported-features-census.txt`. **Two things to take from this list**:
1. **`remote tid` appears exactly once, at the moment predicted** — independent confirmation of
   the root cause from a completely different logging path than the futex instrumentation. **Clean
   pass/fail signal for the `do_kill` fix**: this line should disappear entirely once it lands.
2. **`inotify` is the next escalation target after `do_kill`.** All three `inotify` calls fail in
   the first 0.71s, with dbus's own reaction logged inline: `[session uid=0 pid=8] Cannot
   initialize inotify: Function not implemented`. `dbus-daemon` uses `inotify` to watch its
   service directories — without it, service activation and config reload are degraded from the
   very start of EVERY run, upstream of essentially every XFCE component (they all activate over
   the session bus). **May be behind failures attributed elsewhere all session.** Second pick:
   `setitimer` with a nonzero interval (26 hits) — the repeating-timer path, i.e. anything doing
   periodic work (clocks, blinking cursors, autosave, panel plugin refresh). Third: `membarrier`
   (5 hits) — glibc uses this for RCU-ish synchronization; a silent failure there can produce
   exactly the "correct-looking but never progresses" stall class this whole investigation chased.
**Sequencing recommendation (agreed)**: land `do_kill` first, re-run at `LITEBOX_LOG=warn`, diff
the census. Any feature that disappears was a downstream consequence of the `do_kill` bug; whatever
remains is a genuine independent gap, ranked by the counts above.
**Scope confirmation**: advisor-db has NOT touched `litebox_platform_windows_userland` (still
`a63e8ca59285f5871`'s half) — their changes are in `litebox_shim_linux/src/lib.rs` and
`litebox/src/sync/futex.rs` plus `process.rs` instrumentation, all additive logging except the
one-line `cfg` gate removal.

**FIX LANDED AND VERIFIED — THE HANG THAT GATED THIS ENTIRE INVESTIGATION IS FIXED.**
`a63e8ca59285f5871` implemented and verified the `do_kill` fix. Diff:
```rust
// litebox_shim_linux/src/syscalls/process.rs -- new method on Process
pub(crate) fn interrupt_thread(&self, tid: i32) -> bool {
    let remote = self.inner.lock().threads.get(&tid).cloned();
    let found = remote.is_some();
    if let Some(thread) = remote {
        thread.interrupt();
    }
    found
}

// litebox_shim_linux/src/syscalls/signal/mod.rs -- do_kill, replacing the old ESRCH-always guard
if let Some(target_tid) = tid
    && target_tid != self.tid
{
    if let Some(signal) = signal
        && !self.is_signal_ignored(signal)
    {
        self.signals
            .shared_pending
            .lock()
            .push(&self.process().limits, signal, siginfo_kill(signal));
    }
    return if self.process().interrupt_thread(target_tid) {
        Ok(0)
    } else {
        Err(Errno::ESRCH)
    };
}
```
**Rationale**: `ThreadRemote` only exposes `interrupt()`, not a handle to the target `Task`'s own
per-thread pending-signal queue, so there's no truly per-sibling signal queue reachable from
another thread yet. Delivers via `shared_pending` (process-wide, the same mechanism
`deliver_to_child`'s process-directed delivery already uses), then interrupts ONLY the intended
target thread specifically via the new tid-keyed `interrupt_thread`, using the existing
`threads: BTreeMap<i32, Arc<ThreadRemote<Platform>>>` registry (already present for
`interrupt_all_threads`). Not perfectly POSIX-accurate (a different unblocked thread could
theoretically steal the signal first) but correct in the overwhelmingly common real case this
fixes: the target is the only thread actually blocked waiting on this specific notification. Also
fixes real-ESRCH-for-genuinely-missing-tid (previously any remote tid was rejected identically
whether it existed or not).
**VERIFIED with the fast repro, clean release build, no debug logging needed**:
```
CTX_START
CTX_ABOUT_VERSION_RC=0      <- was: hangs forever (run_exit=124 timeout) before this fix
CTX_ABOUT_SPAWNED
```
**`xfce4-about --version` now exits with code 0 instead of hanging indefinitely — this is the
exact symptom that gated the entire investigation.** One unrelated, pre-existing issue noted in
the run tail (not blocking): `org.a11y.Bus` dbus service fails to activate — `Cannot get the
default GSettingsSchemaSource - is the gsettings-desktop-schemas package installed?` — a missing
guest package (layer gap, not a litebox bug), affects only accessibility-bus activation. A
`run_exit=127` on the outer script may be the wait-loop being affected by something separate
(backgrounding `xfce4-about` without `--version` opens a real window) — flagged for a follow-up
run, does not affect the confirmed core fix.
**Next**: commit this fix, then run the full XFCE launch (`run_xfce_xwm.sh`) to confirm it also
resolves the original `xfwm4`/panel hangs from the very start of the session — the actual standing
goal.

**In progress in parallel**: advisor-db is running a context test (a GTK binary inside the full
display stack, expected ~2s reproduction if display-stack context is what triggers this) to give a
fast verification target for whatever fix lands here.

**RESULT: INDEPENDENT CONFIRMATION FROM A COMPLETELY DIFFERENT METHOD, PLUS A ~100x FASTER
REPRODUCTION FOR VERIFYING ANY FIX.** Same binary, same command line, only the surrounding context
differs:
```
xfce4-about --version, BARE guest (no weston/Xwayland/XFCE): EXITS in 1.7s
xfce4-about --version, FULL display stack:                   pid 17, still hung at t=128.6
```
**Why this matters beyond corroboration**: (1) two entirely different methods — a syscall trace
finding a thread that never resumes, and a black-box exit/hang measurement — converge on the same
conclusion, from opposite directions. (2) `--version` runs almost no application logic (no window,
no xfconf work, no rendering) — so the hang is in STARTUP, exactly where GTK/glib spawns its first
thread, ruling out everything downstream and matching `pthread_create` → `clone` → thread-never-
resumes precisely. (3) **This is a ~100x faster reproduction**: the full XFCE stack takes 60+
seconds to reach the failure and needs a frame decode to verify; this reaches it in the time it
takes weston+Xwayland to come up (a few seconds), and the verdict is a single exit-or-hang check.
**Verification recipe, committed**: `advisor/probes/run_ctx_test.sh` (bring up seatd + weston with
`xwayland=true`, wait for the X socket, run `DISPLAY=:0 GDK_BACKEND=x11 xfce4-about --version` —
PASS = exits, FAIL = hangs) paired with a control, `advisor/probes/tls_dlopen_test.sh` (the same
binary in a bare guest, must keep exiting in ~2s so a regression there is distinguishable from
this bug). **Clean before/after for whatever fix lands**: today it hangs in the stack and exits
bare; after a real fix it should exit in both. advisor-db is staying out of
`litebox_platform_windows_userland/src/lib.rs` entirely as agreed, and is ready to run this
verification against any fix attempt immediately (harness warm, tars already in place).
**Bonus explanation**: this also accounts for the earlier-puzzling "60-second idle heartbeat" —
a process hung in `pthread_create` is not idle by choice; the only thing still waking it is an
unrelated timer elsewhere in the process. Consistent with this bug, not a separate mystery.
**Reusable tool**: `advisor/probes/xwire_probe.c` (4KB, freestanding, no Xlib, decodes X error
codes with major opcode) is now a standing known-good baseline for "is X itself working right
now" — use it first on any future X-related question in this project rather than re-deriving from
inference. **Reproduced independently on a second, fresh weston/Xwayland run**: identical result
(`XWIRE_SETUP_STATUS=1`, `root=98 size=1920x1080 visual=35`, `CreateWindow`/`MapWindow` both
error-free, frame coverage 46.3% with content bands at both y=0..28 and y=520..984) — "a correct
X client can create, map and display a window here" is now confirmed twice, not a one-off; the
stack below GTK is genuinely, repeatably good.

**Bug found and fixed in the probe itself (`744009f0`) — worth knowing before trusting any of its
output before this fix.** An intermediate run reported `XWIRE_SETUP_STATUS=1 bytes=8` followed by
nonsense event types and zeroed error codes, briefly looking like a real X fault. It wasn't: the
probe read the connection setup reply with a single `read()` call, and a socket read is not
guaranteed to return a whole message — when only 8 bytes arrived, it parsed root window/visual out
of an unfilled buffer and produced garbage ids, cascading into garbage results. **Fixed**: now
reads the 8-byte header, takes the declared remaining length from bytes 6-7, and loops until that
much has arrived, reporting `XWIRE_SETUP_INCOMPLETE` instead of silently continuing on partial
data — verified working (the reproduction run above parsed `root=98` correctly). **General lesson,
joins the session's other measurement pitfalls: never assume one `read()` returns a whole protocol
message** — this is the second short-read-style assumption to bite an investigation tonight. Since
this probe is now a shared cross-session baseline, make sure any copy in use is post-`744009f0` —
a diagnostic that can silently produce plausible nonsense is worse than no diagnostic at all.

**INVALID A/B RUN, correctly self-caught before being asserted, but surfaced a real layer gap.**
advisor-db's GTK-vs-XFCE A/B test (raw known-good probe + `thunar` in the same run) did NOT
validly test the intended question: the script never ran `dbus-uuidgen`/started a session bus, so
`thunar` failed immediately with `Failed to initialize Xfconf: Cannot spawn a message bus without
a machine-id: Unable to load /var/lib/dbus/machine-id or /etc/machine-id: No such file or
directory`. The XFCE launchers DO run `dbus-uuidgen` already, so their own panel/desktop failures
are unaffected and unrelated to this — but the A/B comparison itself is invalid as run, since it
denied `thunar` a prerequisite the XFCE components had. **Fixed, rerunning with a machine-id and
session bus.**

**The real finding, independent of the script mistake: THE LAYER SHIPS NO MACHINE-ID AT ALL.**
Neither `/var/lib/dbus/machine-id` nor `/etc/machine-id` exists. Any GTK/dbus client launched
without `dbus-uuidgen` run first fails immediately and hard — with a clear error message, but no
window, which looks exactly like every other "silent" GTK failure investigated tonight unless you
happen to be capturing that specific stderr line. **Same family as the missing SONAME links and
the absent X query tools — worth fixing in the layer itself** (ship a machine-id, or generate one
at image build time) rather than every launcher having to remember `dbus-uuidgen` individually;
removes a whole class of "why did this GTK app do nothing" failures for good.

**Sharp follow-up question for the GTK/syscall-timeline investigation**: do the XFCE components
get far enough that their OWN xfconf initialization actually succeeds? `thunar`'s failure was at
exactly that step. The earlier `XFCONF_PROBE_RC=0` finding only confirmed `xfconfd` was reachable
externally — it does NOT confirm each client's own internal xfconf init succeeds. If the panel and
desktop are stalling at this same step for a different (litebox-relevant, not machine-id-related,
since their launcher already provides one) reason, that would be a much more specific, actionable
lead than "GTK stalls somewhere."

**Layer gap, worth fixing regardless of how this investigation lands — has quietly shaped the
whole session's guesswork problem**: the guest layer contains **zero X query tools** — no
`xdpyinfo`, `xrandr`, `xwininfo`, `xprop`, `xlsclients` — confirmed absent. This is why every
question about X server state all session had to be answered by inference from litebox's own
logs rather than a direct one-line query, exactly the guesswork the standing directives ask to
eliminate. **Concrete suggestion**: add `xdpyinfo` and `xrandr` to the layer — tiny, no unusual
dependencies, and between them answer screen geometry/depth/visuals/extensions/RandR outputs
directly. Given how much time tonight went into inferring what `xdpyinfo` would print in
milliseconds, this is probably the single highest-leverage layer addition available.

**POSSIBLE GAME-CHANGER, NEEDS IMMEDIATE RE-VERIFICATION: the slow-startup investigation (my
dispatched agent, `a23992b80c8de9190`) found the layer tar's `weston.ini` had REGRESSED and lost
the `xwayland=true` fix entirely.** `.wfgy/xfce-build/layer31_direct_fixed.tar`'s baked-in
`weston.ini` no longer had `xwayland=true` — meaning **Xwayland was never actually starting** in
whatever runs used this tar copy, and every "60-98s startup" measurement from earlier passes was
just a shell poll loop burning its full timeout waiting for an X11 socket that could never appear.
Fixed (restored `xwayland=true`, old version kept as `.bak_no_xwayland_fix`): X11 socket now ready
on the first 0.2s poll, `xfce4-about --version` exits cleanly `rc=0` in ~17s total instead of never
exiting in 85-98s. **This directly threatens the "zero mapped windows" chain of investigation
above (dual-WM test, missing-config test, the X-protocol decode) — if THOSE runs also used a copy
of this same regressed tar, Xwayland may not have been running at all during them, which would
trivially explain zero mapped windows without needing any X-protocol bug.** advisor-db's zero-
managed-windows finding explicitly showed weston's XWM log lines (`xfixes version 6.0`, `created
wm, root 98`), which only happen if Xwayland DID start — so advisor-db's specific runs are likely
unaffected by this particular regression. But **anyone re-running or re-verifying anything above
should first confirm their own layer tar actually has `xwayland=true` in its baked-in `weston.ini`
before trusting any result**, since this tar can silently regress (it's a gitignored artifact, not
tracked, and has apparently reverted at least once already this session).

**CONFIRMED: advisor-db's zero-managed-windows chain is UNAFFECTED by the weston.ini regression
above — verified two independent ways.** Static: every tar advisor-db used (`noxfwm.tar`,
`probe.tar`, `wm.tar`, `withcfg.tar`) has `xwayland=true` present in its baked `weston.ini`.
Dynamic (the stronger proof): the run logs themselves show weston logging `"launching
'/usr/bin/Xwayland'"` and `"created wm"` exactly once per run (`noxfwm2`, `wm1`) — these lines only
fire when weston actually loads `xwayland.so` and spawns Xwayland itself, so the setting wasn't
just present in the tar, it was read and acted on. **The dual-WM test, missing-config test, and
frame decodes all genuinely ran with Xwayland up under weston's management — stand as recorded.**

**METHODOLOGY GOTCHA WORTH KNOWING for anyone auditing these tars**: a tar can contain multiple
entries for the same path (e.g. `./etc/xdg/weston/weston.ini` AND `etc/xdg/weston/weston.ini` as
two separate members, from appending overlays onto a base tar) — **the LAST member wins at
extraction time**, not the first. Grepping/extracting by only one path-prefix form can find a
STALE earlier copy and report a false regression (this nearly happened to advisor-db just now — a
`./`-prefixed extraction reported `xwayland=true` absent, while the unprefixed extraction, which
is what the runner actually uses, showed it present). **Any tar audit must extract the same way
the runner does, not just grep the first match.** Durable fix identified but not yet done: rebuild
these tars from a single clean tree instead of appending overlays, so there's exactly one copy of
every path and no extraction-order ambiguity — removes this whole class of silent drift, which
traces back to the layer tar being a gitignored, untracked, silently-mutable artifact in the first
place.

Also fixed by the same agent, both real: `import_writable_layer` panicked with
`PathError(MissingComponent)` on tar entries whose parent dirs weren't separate members — now
creates parent dirs on the fly; the leading-slash `load_program(...).unwrap()` bug from pass 336
now prints the actual error and exits cleanly instead of stack-overflowing (verified:
`/bin/does_not_exist` → clean `ENOENT` message); ~11,000 unconditional `error!`-level diagnostic
log lines firing in 8 seconds of guest execution (no env-var gate at all, unlike every other
diagnostic in the file) now gated behind `LITEBOX_DIAG_MM=1` (default off). New repro script:
`advisor/probes/startup_timing_repro.sh`. All committed `1cc6d9f0`.

**STRONG CANDIDATE EXPLANATION FOUND (advisor-db), plausible and cheap to confirm/refute — NOT
YET ASSERTED, verification pending**: **the layer may simply have no desktop/panel configuration
to draw.** `/etc/xdg/xfce4/xfconf/xfce-perchannel-xml/` in the layer contains only
`xfce4-keyboard-shortcuts.xml`, `xfce4-session.xml`, `xsettings.xml` — **`xfce4-desktop.xml` and
`xfce4-panel.xml` are both MISSING** (a `find` across the whole layer returns nothing for either).
Panel plugin binaries ARE present (`/usr/share/xfce4/panel/plugins/`: actions, applicationsmenu,
clock, directorymenu, launcher, pager) but nothing tells the panel which plugins to instantiate or
where, and nothing tells `xfdesktop` what backdrop to draw or whether to show icons. **An
unconfigured `xfce4-panel` has no plugins to show; an unconfigured `xfdesktop` may legitimately
draw nothing — both alive, healthy, zero errors, empty screen. This matches every observation so
far exactly**, and if true would put the remaining gap in the same family as the missing-SONAME
packaging bug from earlier this session (a layer-content gap, not a litebox defect). **Not yet
confirmed**: `xfce4-panel`'s own `/usr/lib/xfce4/panel/migrate` step DID run in an earlier
observed run, and migrate normally creates a default layout when none exists — so either it wrote
a config that hasn't been found yet, or it failed silently.
**Two cheap tests to settle it, advisor-db running (1) next**:
1. After a run, list `$HOME/.config/xfce4/xfconf/xfce-perchannel-xml/` from inside the guest. Files
   present = panel/desktop had a layout and chose to draw nothing (a real bug). Absent = nothing to
   draw, packaging gap (not a litebox bug).
2. Ship a minimal `xfce4-panel.xml` (one or two plugins) and `xfce4-desktop.xml` (a backdrop) into
   the layer, rerun. Content appearing confirms the diagnosis; fix is layer content, not code.

**What IS genuinely fixed and verified (real, durable progress, not undersold)**: all six
components (`weston`/`xfconfd`/`xfwm4`/`xfsettingsd`/`xfdesktop`/`xfce4-panel`) now start and stay
alive for the full run, where they previously exited with failures; the D-Bus session bus and
`xfconfd` settings daemon work (`DBUS_UP=yes`, `XFCONF_PROBE_RC=0`, `XFCE_DISPLAY=:0`, the whole
"Connection refused" failure class is gone); the compositing blackout is fixed and understood; the
layer's 921 broken ELFs (missing SONAME symlinks) are fixed; two real litebox bugs were found and
fixed this session (`unmap_shared_memory` host-crash race, fork claim-ownership race) and verified
independently by both sessions. 24 frames captured, none go to zero after the fix (previously every
run wiped to black and stayed there) — the specific mechanism that was destroying the framebuffer
is genuinely gone, it's just that what remains after that fix is a mostly-undrawn desktop, not a
fully-rendered one. Working launcher (process/compositing fix only, does not fix the under-drawing
gap): `advisor/probes/run_xfce_xwm.sh`, committed `4e6fc556`.

**Root cause of the entire session-long blocker, and the fix — both non-litebox, zero litebox
code changes required:**
1. **Missing XWM.** Launch scripts spawned `Xwayland` as a bare separate process. Rootful Xwayland
   needs the launching compositor to attach an X Window Manager over a `-wm <fd>` connection —
   that's what maps an X11 window's surface into the compositor's scene graph. weston's
   `desktop-shell.so` has no XWM logic of its own; that lives exclusively in weston's own
   `xwayland` module, loaded via `[core] xwayland=true` in `weston.ini`, which spawns AND manages
   Xwayland itself (including the `-wm` handshake). Without it, X11 client surfaces got real pixel
   content written into their buffers (independently verified byte-identical via same-instant
   cross-process comparison — litebox's shared-memory path was never at fault) but were never
   mapped into weston's scene graph, so nothing ever composited — the "renders fine, then goes
   black and never recovers" symptom that dominated this entire session.
2. **`set -x` in the launch script.** Shell tracing deterministically triggers a real, separate,
   still-open litebox bug (a trampoline `#UD` at `rip=0x7feffff7fb8a`, deterministic 30s repro at
   `advisor/probes/setx_ud_repro.sh`) that kills the first backgrounded child before it reaches
   `execve()` — this is what was taking out `dbus-daemon` specifically, cascading into
   `xfconfd`/`xfsettingsd`/`xfce4-panel` all failing with "Connection refused". `set -x` was
   reintroduced by copying an older script mid-session and cost real additional time before being
   caught a second time — treat as a standing hazard, not a one-off.

**Durable launch-script configuration (6 items — apply to every XFCE launch script, not just the
one already fixed)**:
1. `weston.ini`: `[core] xwayland=true`.
2. No manual `Xwayland` launch — let weston manage it.
3. Discover the display weston chooses (currently `:0`) rather than hardcoding `:1`.
4. **No `set -x` anywhere** in the script or anything it sources — use explicit `echo` markers at
   stage boundaries instead. Grep for this explicitly when touching any launch script; it is easy
   to reintroduce by copying. **Full repo-wide audit done (advisor-db, `f3248418`)**: 7 files
   mention `set -x`, only 3 as an active directive — `setx_ud_repro.sh` (intentional, it IS the
   `#UD` repro), and two stale real launchers that DID still have it live
   (`xfce_on_weston.sh`, `xfce_diag_launch.sh`) — fixed, each now carries a comment explaining the
   mechanism instead of a bare deletion (so it isn't silently re-added for debugging later). The
   three current launchers (`run_xfce_xwm.sh`, `run_xfce_staged.sh`, `run_xfce_noxfwm.sh`) are
   clean. Two scripts inside the merged guest layer tar (`run_xfce.sh`, `xfce_on_weston.sh`) still
   carry tracing but are confirmed NOT executed by the tar/script combination actually run
   (`xfce_launch.sh` is what's inside the tar, has no `set -x`, and run logs confirm it's never
   invoked) — nothing dormant is interfering with current results. **Closed, no longer a live
   loose end.**
5. Single dbus spawn, no retry — retrying a backgrounded spawn after losing one child to the `#UD`
   kills the launcher shell itself, not just the child. If dbus is lost, rerun the whole script.
6. Capture backgrounded services' stderr AND print/tee it, so a fast fatal crash never presents as
   a silent, misleading readiness-timeout.

**What's still open, but no longer blocking process launch or compositing**: the trampoline `#UD`
itself (root-caused to `set -x`, but the underlying litebox bug that fires ANY time a backgrounded
child races that specific instruction sequence is real and unfixed — just no longer triggered now
that `set -x` is banned from launch scripts). Worth closing eventually per the standing "always
build/fix, don't just work around" discipline. See `advisor/probes/setx_ud_repro.sh` for the repro.

**Remaining follow-on work, re-prioritized after the frame-decode correction above (this is now
the real state of the standing goal, not a nice-to-have polish list)**:
1. **TOP PRIORITY: does `xfdesktop`/`xfce4-panel` render its own content at all? OPEN QUESTION,
   not yet answered.** The earlier claim that they "render almost nothing" was retracted (see
   above) — the frame that seemed to show that predates both components even starting, so it
   actually showed weston-desktop-shell's own built-in panel, not XFCE's. The real test (rerun
   with a longer post-panel-start tail + forced periodic capture, so idle-but-alive is
   distinguishable from never-drew) is in progress, advisor-db. This may share a root cause with
   item 2 below (client startup stalling) if the answer turns out to be "components are alive but
   still mid-startup/never finish initializing enough to draw" — but that's speculative until the
   rerun lands.
2. **Client startup is extremely slow — unexplained, and likely the user's original "pretty long
   wait" complaint, and possibly directly related to item 1.** `xfce4-about --version` never
   exited in an 85s run; `xfce4-appfinder` never finished startup in 98s; even in the "successful"
   run, components take ~60s to come up. The existing `ppoll`/wait-duration instrumentation from
   earlier this session is already pointed at the right area; a dispatched agent is investigating
   this now.
3. **Close the trampoline `#UD` itself** (`advisor/probes/setx_ud_repro.sh`, 30s deterministic
   repro, `rip=0x7feffff7fb8a`). Real bug, understood, workaround (never `set -x`) is free and now
   standing policy — lower priority than 1/2 since it's fully mitigated already and doesn't affect
   visual output.

See "Rendering/scanout blocker" below for the full forensic trail (kept for anyone who needs the
detailed history of how this was diagnosed — memory-corruption theories all refuted, compositing
theory confirmed via same-instant cross-process comparison, root cause found via targeted web
research on weston/Xwayland internals).

## Standing directives (do not relitigate these)

- **Never conserve token/context budget.** No instruction anywhere says to throttle effort or
  stop at a token threshold. Work at full effort until the actual goal is met. If genuinely near
  a context ceiling, let auto-summarization handle it or checkpoint via a commit — never
  preemptively cut work short and frame it as budget conservation.
- **Always build debug/observability tooling proactively while investigating**, not just enough
  to explain the current bug. Several real bugs this session were only found because someone
  built a general tool (dependency auditor, preflight checker, per-process stderr capture)
  instead of continuing to guess from interleaved logs.
- **Use `/gm` to drive non-trivial coding tasks.** Zero branches or worktrees — work on `main`.
  Commit only as `lanmower`, never attribute anything to Claude/AI.
- **The success oracle for a launch run is `non_black_pixels > 0` in the FINAL frames of a run**
  (via `LITEBOX_DUMP_FRAMES=1`), not "any frame anywhere in the run." A run can render a correct
  desktop for 20+ seconds and then go black — that is not success. This distinction cost real
  time this session (a claimed "XFCE working, sustained 30+ seconds" turned out to be weston's
  own render, with the screen already black by the time XFCE's own components started).
- **Exit code is not a valid success oracle either.** A run can exit 0 while several forked
  children were silently killed by fatal signals. Count "fatal signal" lines in
  `LITEBOX_LOG=error` output directly.

## What's confirmed working (do not re-investigate)

- **litebox's DRM/display emulation is sound.** Mode enumeration, dumb-buffer allocation, the
  pixman renderer, and swapchain creation all work correctly against the real virtual DRM output
  ("Virtual-1"). Verified via targeted DRM-ioctl logging and direct frame inspection.
- **weston renders a complete desktop reproducibly** under litebox (`--use-pixman
  --shell=desktop-shell.so`), independently confirmed across many runs this session
  (`non_black_pixels=2073597`, the canonical full-1920x1080-desktop signature).
- **labwc is NOT the right compositor to keep pursuing.** It unconditionally creates a transient
  0x0-dimension headless output on startup as a documented upstream workaround (`src/server.c`,
  `wlr_headless_add_output(backend, 0, 0)` immediately destroyed) — normal on real hardware where
  the create-destroy completes before anything renders to it. Under litebox's timing, something
  renders/modesets it inside that window, hitting wlroots' `assert(width > 0 && height > 0)` in
  `wlr_swapchain_create` and aborting. This is real upstream behavior interacting with litebox's
  timing, not a litebox display bug — use weston instead; it's already proven working.
- **Windows fork-emulation machinery (`fork_verify.rs`) is Windows-only by architecture.** Real
  Linux/macOS `fork()` gives the child identical virtual addresses for free and never needs this
  machinery — never assume a `fork_verify`-attributed crash needs investigating on those
  platforms, and never port a `fork_verify.rs` fix there.

## Real bugs found and fixed this session (all committed on `main`)

1. **Layer packaging: `tar` drops SONAME symlinks.** Extracted layer tars kept only the versioned
   filename (`libX11.so.6.4.0`) for hundreds of shared libraries, never the plain SONAME
   (`libX11.so.6`) the dynamic loader actually looks up — because `tar` doesn't reliably recreate
   a symlink before its target exists on extraction. Silently broke `xfwm4`, `xfdesktop`,
   `xfce4-panel`, and hundreds of other binaries (946 of 1696 ELFs in one measured layer). New
   tools: `advisor/probes/audit_layer_deps.py <extracted-root>` (scans, reports every unresolved
   `DT_NEEDED`, zero exit code = clean) and `advisor/probes/fix_layer_sonames.py <extracted-root>`
   (creates the missing links from each `.so`'s real `DT_SONAME`, idempotent, `--dry-run`
   available). Also `advisor/probes/preflight_layer.sh` — a fast, fail-loud check of a layer
   before spending a full run on it (shell/loader present, `ld-musl` search path present, zero
   unresolved deps, at least one X client, session XML present).
2. **Host-crash race: `unmap_shared_memory` unsynchronized against `update_permissions`.**
   `litebox_platform_windows_userland/src/lib.rs`'s `unmap_shared_memory` (`UnmapViewOfFileEx`)
   was the one Windows VAD-tree mutator in the file not serialized under `VIRTUAL_PROTECT_LOCK`,
   unlike every other allocate/deallocate/protect path. A guest process's real `munmap()`/exit
   teardown of a shared mapping could race a different thread's concurrent `mprotect()` on the
   same region: the region gets freed between `update_permissions`'s `MEM_COMMIT` query and its
   actual `VirtualProtect` call, which then observes `MEM_FREE`, fails with a spurious
   `ERROR_SUCCESS`, and trips an `assert!` that panics the whole host process. Fixed by adding the
   missing lock guard (18 lines). Verified: previously guaranteed a host panic within ~46s of an
   XFCE launch; post-fix, a full 100s run produced zero panics. Commit `984927b0`.
3. **`sys_mprotect` bitmask gap, `remove_mapping` clamp scoping, `VmArea`/`rangemap` coalescing
   bug, `deallocate_pages` missing MEM_MAPPED guard, `memfd` mmap-time wipe bug.** Multiple real
   memory-management correctness fixes landed earlier this session — see git log
   (`3a0755e4`, `140c1711`) for full detail. None were the cause of the crashes described below,
   but all are real, verified fixes worth keeping.
4. **`protect_mapping` caller attribution.** Every call site of `Vmem::protect_mapping` now tags
   its caller (`guest_mprotect` / `make_pages_*` / `create_mapping` / `fork_duplicate`), so a
   future protection-related crash can be attributed to its actual origin in one log read instead
   of hours of inference. This tooling investment paid for itself directly this session.
5. **Unlocked-write gap in proactive fork stale-pointer fixup.** The proactive
   `fixup_stale_stack_pointers`/`fixup_stale_elf_data_pointers` pass (runs on the parent's own
   thread right after `PageManager::duplicate()`) rewrote a freshly-forked child's memory with no
   locking at all — not against `fork_verify`'s existing reactive healing lock, nor against a
   second concurrently-forking parent thread's own proactive pass. Fixed by adding
   `ForkChildVerificationProvider::lock_fork_verify_heal()` and wrapping both calls with it.
   Commit `bcc6a3e7`. **Real and worth keeping, but does NOT by itself fix the open blocker
   below** — direct measurement showed no change in fault rate; see "Open blockers" for the
   still-open mechanism.

## Open blockers (the real remaining gap)

**MAJOR UPDATE (this session, latest): a complete XFCE desktop now comes up and stays alive.**
weston, Xwayland, xfconfd, xfwm4, xfsettingsd, xfdesktop, and xfce4-panel all start successfully
and remain alive to the end of a run (confirmed: `DBUS_UP=yes`, `XFCONF_PROBE_RC=0`, no component
exits with a failure status, only cosmetic warnings in their stderr — AT-SPI accessibility bus
address errors, missing GSettings schema, no SESSION_MANAGER var, none fatal). This required BOTH
of the concurrent-fork fixes below AND removing `set -x` from the launch script (see the #UD
bisection further down — `set -x` itself was triggering a real, separate litebox bug that broke
the launch chain). **The remaining gap is now narrow and purely a rendering/scanout issue, not a
process-launch issue**: weston renders a full desktop correctly at t=7.4s
(`non_black_pixels=2073597`), then the framebuffer goes black at t=20.3s and never recovers —
this happens the moment Xwayland forks `xkbcomp` after a large pointer-healing pass, BEFORE any
XFCE component even starts (all XFCE components start from t=29.9s onward, well after the
blackout — neither XFCE nor `xfwm4` causes it). See "Rendering/scanout blocker" below for the
precise next diagnostic. **Two process-level bugs are fully fixed** (see "FIX LANDED" further
down for both); **one process-level bug remains open but no longer blocks the launch chain**
(the `set -x`-triggered #UD — has a fast deterministic repro, still needs a real fix, see below).

**Historical framing (both now fixed, kept for context): forked children died (SIGSEGV/SIGILL,
`rip==cr2`) before they could `execve()`, under concurrent forking only.** The earlier
"MAXCONCURRENT fork_verify healing passes" theory below is REFUTED as of a later measurement —
read the correction further down before acting on it.

- Reproduces on a **bare alpine rootfs with zero display components** — no weston/Xwayland/XFCE
  needed. A background/concurrent-fork shell pattern alone triggers it. Sequential forking is
  rock-solid (0 faults across repeated runs); only concurrent forking triggers it.
- Concretely, this kills `dbus-daemon`'s forked child before it reaches `execve()` in a typical
  XFCE launch, which cascades: no D-Bus session bus → `xfconfd`/`xfsettingsd`/`xfce4-panel` all
  fail with "Connection refused" → `xfce4-session` launches zero children. This is why the
  desktop currently goes black by the end of a run even with the DRM/weston/host-panic layers all
  working correctly — everything downstream of dbus never gets its settings/session
  infrastructure.
- Regression oracle (sub-second to a few seconds, litebox host + bare rootfs, no display stack
  needed):
  ```
  FAIL case (must go to 0 faults after a real fix):
    i=1; while [ $i -le 30 ]; do /bin/true & i=$((i+1)); done; sleep 2
    (currently: 3-6 "fatal signal" log lines per run, unchanged by the fix below)

  PASS control (must STAY at 0 — don't break this while fixing the above):
    i=1; while [ $i -le 10 ]; do sleep 5 & sleep 0.3; i=$((i+1)); done; sleep 6
    (was 0,0,0; noted as occasionally noisy on a loaded host in the most recent session — treat
    a single nonzero reading here with suspicion and rerun before trusting it as signal)
  ```
- **A real, genuine locking gap WAS found and fixed** (commit `bcc6a3e7`): the proactive
  `fixup_stale_stack_pointers`/`fixup_stale_elf_data_pointers` pass
  (`litebox_shim_linux/src/syscalls/process.rs`'s `do_clone`, runs on the parent's own thread
  right after `PageManager::duplicate()`) rewrote a freshly-forked child's memory with **zero
  locking** — not serialized against `fork_verify`'s reactive AV-path/single-step healing lock
  (`FORK_VERIFY_HEAL_LOCK`, which already existed but was never wired into this path), nor
  against a second concurrently-forking parent thread's own proactive pass. Fixed by adding
  `ForkChildVerificationProvider::lock_fork_verify_heal()` and wrapping both proactive fixup
  calls with it. This is a correct, worthwhile fix on its own merits — but **direct measurement
  shows it does NOT reduce the fault rate of the bug described here.** Landed and kept regardless.
- **CORRECTION — the "MAXCONCURRENT fork_verify healing passes" correlation's CAUSAL EXPLANATION
  was wrong; the correlation itself was real.** Setting `LITEBOX_FORKVERIFY_OFF=1` (disables
  fork_verify's reactive single-step/AV-path healing entirely, proactive fixup left on)
  reproduces the SAME fault rate as normal — proving reactive healing machinery is NOT itself the
  mechanism. But the underlying 41-run measurement (0 faults in 8/8 at MAXCONCURRENT==1) was not
  spurious: concurrent fork_verify healing passes were a PROXY for concurrent
  `PageManager::duplicate()` calls — the actual root cause, fixed below (`166b5a90`) — since both
  scale together with concurrent forking. Record this as "correct correlation, wrong causal
  attribution," not "the correlation was noise": it remains a useful detector for this class of
  concurrency bug even though the fix landed elsewhere. Conversely, disabling the *proactive*
  fixup pass instead spikes faults to ~31/run (nearly every child) — confirming that pass does
  real, necessary work and is not itself spurious corruption.
- **Crash signature re-examined and clarified**: `rip==cr2`, offset `0x1464b`/`0x464b` low bits,
  confirmed via `objdump` disassembly to be a **real, valid busybox instruction**
  (`lea 0x148(%rbx),%rax`) at the CORRECT offset relative to the child's own load base — this is
  not a corrupted/wrong jump target. Windows is genuinely reporting that page as not-present at
  the moment of the fault. No address-range collisions were found across extensive
  `fork_duplicate`/`create_mapping`/`guest_mprotect` log cross-referencing between concurrently
  forking children.
- **`VirtualQuery`-at-fault-time measurement DONE this session — corrects the "not-present"
  characterization above.** Added `LITEBOX_DIAG_FAULT_VQ=1` to `vectored_exception_handler` in
  `litebox_platform_windows_userland/src/lib.rs` (gated on `rip == cr2`, the documented crash
  signature, to avoid the 268,000+ line flood an ungated version produces from `fork_verify`'s own
  expected healing faults — confirmed live). Real captures from the regression oracle (3 real
  crashes in one run, all 3 captured cleanly):
  ```
  cr2=rip=0x153e464b  state=0x1000 (MEM_COMMIT)  type=0x20000 (MEM_PRIVATE)  protect=0x2 (PAGE_READONLY)
  cr2=rip=0x1fb4464b  state=0x1000 (MEM_COMMIT)  type=0x20000 (MEM_PRIVATE)  protect=0x2 (PAGE_READONLY)
  cr2=rip=0x3b13464b  state=0x1000 (MEM_COMMIT)  type=0x20000 (MEM_PRIVATE)  protect=0x2 (PAGE_READONLY)
  ```
  All three: the page IS committed and present (contradicting the earlier "genuinely not-present"
  read) — it is **`PAGE_READONLY` where `PAGE_EXECUTE_READ` is expected** for a code page about to
  execute an instruction-fetch. This is a real permissions bug, not a missing-mapping bug. Given
  `prot_flags()` (same file) correctly maps `VmFlags::VM_EXEC` to `PAGE_EXECUTE_READ` and
  `Vmem::duplicate`'s eager-copy path (`litebox/src/mm/linux.rs`) correctly calls
  `protect_mapping(dest_range, vma.flags.into(), "fork_duplicate")` with the source region's real
  flags, the most likely explanation is that the exec-narrowing `protect_mapping` call for this
  specific region either never ran, or ran and was then overwritten back to `PAGE_READONLY` by a
  DIFFERENT thread's own operation on the same or an adjacent real address before the child ever
  got to execute there. Not yet root-caused to an exact line.
- **Tried and REVERTED**: wrapping the entire `PageManager::duplicate()` call in `do_clone`
  (`litebox_shim_linux/src/syscalls/process.rs`) with `lock_fork_verify_heal()` (the same guard
  already used for the proactive stale-pointer fixup passes) was a natural next attempt given the
  above evidence, but **measured to make the oracle worse**, not better (30-concurrent-`/bin/true`:
  baseline 3-6 faults rose to 9-11 across 5 reruns with this lock held). Reverted; a comment is
  left at the call site so this specific change is not retried without new evidence. The real fix
  needs to narrow down WHERE inside `duplicate()`'s per-region loop the wrong protection value
  reaches `update_permissions`/`VirtualProtect`, not just serialize the whole call more broadly.
- **Host resource exhaustion recurred this session**, matching a pattern this project's own memory
  already documents (`pass 314`/`pass 315`'s "Heisenbug-shaped timing race" vs. genuine host
  degradation distinction): free physical memory dropped to ~1.1-1.3 GiB out of 16 GiB after
  several build+run cycles and did not recover after several seconds idle, with fault counts on
  BOTH the FAIL oracle (rising to 8-12) and the PASS control (rising to 5-8, which must stay 0)
  becoming unreliable at the same time. Every number from this investigation from that point
  forward should be treated as suspect until re-measured on a fresh host/session state — this is
  the same known confound, not new evidence of anything code-related.
- **Next concrete step**: re-run the `LITEBOX_DIAG_FAULT_VQ=1` capture on a FRESH host session
  (low memory pressure) across several of the 3-6 baseline faults, this time also logging, from
  `Vmem::duplicate`'s own per-region loop, the exact `(source_range, dest_range, vma.flags)` for
  every region as it is placed — then cross-reference by dest-address against the `cr2` values
  captured here to identify definitively whether the faulting address's OWN `protect_mapping` call
  ran at all, and with what flags, versus was silently skipped or overwritten afterward.

- **FIX LANDED (this session): root mechanism found and fixed, regression oracle verified clean.**
  Confirmed by direct code reading: `PageManager::duplicate()` (`litebox/src/mm/linux.rs`) runs
  entirely on the PARENT's own thread — the new child's real OS thread does not exist yet
  (`spawn_thread`/`std::thread::Builder::new()` in `litebox_platform_windows_userland/src/lib.rs`
  runs strictly AFTER `duplicate()` returns, see `do_clone` in `litebox_shim_linux/src/syscalls/
  process.rs`). Every `allocate_pages`/`protect_mapping` call `duplicate()` makes therefore runs
  under `current_claim_owner()` == the PARENT's own `ClaimOwner` (`CURRENT_GUEST_PID` is only
  repointed at the CHILD's pid later, inside the new OS thread's own closure, via
  `reclaim_ranges_for_fork_child` — long after `duplicate()` already ran). Windows' own
  `CLAIMED_RANGES` foreign-claim defense (`claim_range`, `find_foreign_claim`, same file) exists
  specifically to stop one guest process's allocation from silently decommitting/recommitting over
  a DIFFERENT, still-live guest process's memory — but it only recognizes a claim as foreign when
  its owner differs. Two SIBLINGS forking CONCURRENTLY from the same parent both get their entire
  eager address-space copy attributed to that SAME parent owner, so `claim_range`'s own
  same-owner-coalescing fast path (deletes and merges any prior claim from the "same" owner that
  overlaps or touches the new one — by design, this is what keeps ordinary sequential heap growth
  on one thread cheap) can merge/absorb one sibling's just-placed destination region into the
  other sibling's own claim, making the `Replace`-mode per-region placement blind to a genuine
  cross-sibling collision it would otherwise have caught and relocated away from. This produces
  exactly the observed `PAGE_READONLY`-where-`PAGE_EXECUTE_READ`-expected signature: one child's
  freshly-narrowed R+X code page gets silently decommitted/recommitted (back to the eager-copy's
  initial R+W, before ITS OWN later narrowing step runs) by a concurrently-copying sibling that
  was never flagged as foreign.

  **Fix**: added `ThreadProvider::with_fork_duplicate_claim_owner(child_pid, f)` (default no-op,
  `litebox/src/platform/mod.rs`), implemented on Windows (`litebox_platform_windows_userland/src/
  lib.rs`) as a save/restore of the CALLING (parent) thread's own `CURRENT_GUEST_PID`
  thread-local around `f`. `do_clone` (`litebox_shim_linux/src/syscalls/process.rs`) now wraps
  the `PageManager::duplicate()` call with `self.global.platform.with_fork_duplicate_claim_owner
  (child_tid, || ...)` — `child_tid` is already allocated before this point and, for a real
  process-clone (`fork()`), IS the child's real future `pid` (matches what
  `set_next_spawned_thread_guest_pid` assigns later at spawn time). This makes every claim the
  eager copy registers belong to the CHILD's own future identity instead of the parent's, so two
  concurrently-duplicating siblings are correctly mutually foreign for the whole vulnerable
  window — restoring the exact collision defense that already existed for "two unrelated guest
  processes" to this "two sibling children of the same still-forking parent" case too. Narrowly
  scoped to claim attribution only, per this section's own "narrow it down further" directive —
  does not touch the previously-reverted broader `lock_fork_verify_heal()`-around-the-whole-call
  approach (still correctly reverted, still a worse fix).

  **Verified against the regression oracle**, with real memory headroom explicitly checked before
  trusting the numbers (free physical memory ~1.9–3.7 GiB out of ~16 GiB across the runs below —
  BELOW the ~4 GiB caution threshold this project's own memory already flags as a known confound;
  treat these as probably-real but not iron-clad, and re-verify on a fresh host if revisited):
  FAIL case (30-concurrent-`/bin/true`, baseline 3-6 faults/run): **8/8 consecutive runs at 0
  faults** post-fix. PASS control (was 0/0/0): **3/3 runs still at 0/0/0**, no regression.

  **Independently reverified on a separate host session with healthy memory (~5.7 GiB free,
  above the caution threshold)**: FAIL case **0 faults in 10 CONSECUTIVE runs** (deliberately run
  longer than the original 8 given the earlier memory-pressure caveat), PASS control **0 faults,
  2/2**. Two independent methods (litebox's own VMA-flags table, logged as
  `VM_READ|VM_MAYREAD|VM_MAYWRITE|VM_MAYEXEC` with `VM_EXEC` absent on the crashing range, plus a
  separate count of 108 `PAGE_READONLY` vs. 59 `PAGE_EXECUTE_READ` `VirtualProtect` calls in one
  run; and this session's own `VirtualQuery`-at-fault-time capture,
  `MEM_COMMIT`/`MEM_PRIVATE`/`PAGE_READONLY`) independently agree on the same permissions-mismatch
  symptom this fix addresses. This is a solid, cross-verified fix — confidence is high.

- **NEW: second, distinct crash signature found blocking the standing goal under the real XFCE
  launch load — STILL OPEN, not fixed by the above.** Running the full reproduction command
  (below) against `layer31_direct_fixed.tar` with the fix above in place: weston (`--use-pixman
  --shell=desktop-shell.so`) still renders correctly and `non_black_pixels=2073597` (the full
  1920x1080 desktop signature) is sustained through the FINAL frames of a 100s run — but this is
  STILL the weston-only false-positive this doc's own "standing directives" section warns about:
  XFCE itself never comes up. Direct cause, found by reading the log's own shell `-x` trace
  end-to-end: `xfce_direct.sh` line 11, `dbus-daemon --nofork --nopidfile --nosyslog
  --address="$DBUS_SESSION_BUS_ADDRESS" --session &`, backgrounds a subshell that is killed by a
  fatal signal BEFORE `dbus-daemon` itself ever reaches `execve()` — confirmed by grepping the
  entire run log for any trace of `dbus-daemon` (execve, DIAG_TIMELINE, or otherwise): it never
  appears ANYWHERE. The killed process's own `comm` is still `sh` (the backgrounding subshell),
  `Signal(4)` (SIGILL), `cr2=0x0`/`error_code=0x0` (genuinely-unmapped `#UD`, not a page-fault —
  see this doc's own "Exception(N)/error_code decoding" technique below), and critically **`rip`
  is the SAME fixed value both times this was observed in one run** (`0x7feffff7fb8a`) —
  `pid=7` at 0.68s (the dbus-daemon backgrounding attempt) and again `pid=73` at 35.1s (a later,
  unidentified `sh` subshell during the xfwm4/xfdesktop/xfce4-panel launch sequence). A fixed,
  repeated `rip` across two unrelated fork instances initially looked like a fault-tolerant
  healing helper (`fork_verify`'s own protection widen/restore lines appear right beside both
  crashes in the log) — **checked directly and refuted**: `rip=0x7feffff7fb8a` decimal is
  `140668768353162`, which falls squarely inside the SAME run's own logged `diag-exec-mmap` range
  for `/lib/ld-musl-x86_64.so.1` (`140668768194560`..`140668768559104`, offset `+158602` into the
  mapping) — this is a fault on REAL, in-range musl libc code (very likely `sh`'s own fork/thread
  startup path inside musl, given the two occurrences are both `sh` forking), not a `fork_verify`
  internal at all. The concrete next step is therefore the same class of investigation as the
  now-fixed bug above, but for this different path: capture `LITEBOX_DIAG_FAULT_VQ=1` for THIS
  signature specifically (it won't be caught by the existing `rip==cr2` gate, since here
  `cr2=0x0` while `rip` is the real fault address — the gate condition itself may need widening
  to `rip != 0 && (rip == cr2 || cr2 == 0)` for this class) to see the actual committed/protect
  state of the `ld-musl` page at fault time, then check whether THIS shared library's own
  concurrent-fork placement path (likely still going through `Vmem::duplicate`'s per-region loop,
  same as before, but possibly a DIFFERENT coalescing/grouping edge case than the one just fixed
  — e.g. `ld-musl` may land in its OWN relocation group separate from `/bin/sh`'s main image,
  worth checking `MAX_INTRA_GROUP_GAP`-driven grouping specifically) is similarly vulnerable to a
  same-owner-coalescing collision the just-landed fix does not fully cover. Because `dbus-daemon` never starts, `xfconfd`/`xfwm4`/`xfdesktop`/`xfce4-panel` all still
  launch with no session bus — the log shows zero "Connection refused" lines this run (an
  improvement over the previously-documented full cascade) but also zero evidence any of those
  components did real session-bus-dependent work, since none of their execve/DIAG_TIMELINE lines
  appear either (only weston, weston-desktop-shell, and dbus-uuidgen ever reach execve in this
  run's full log). **The standing goal is NOT met.** This is a DIFFERENT crash signature (fixed,
  non-`cr2` `rip`; `cr2=0` not a real code-page address) from the `rip==cr2`/`PAGE_READONLY` bug
  fixed above — do not assume the same root cause or the same fix applies; investigate
  independently, likely starting inside `fork_verify.rs`'s own fault-tolerant-write healing path
  rather than `Vmem::duplicate`.

  **DETERMINISTIC 30-SECOND REPRO FOUND for this #UD (bisected, do not use the full XFCE stack to
  chase this — use this instead):** the trigger is `set -x` in the launch script, NOT anything
  about dbus-daemon or XFCE specifically. Bisection: starting from a script where `dbus-daemon`
  spawns fine (3/3), adding back only `set -x` (no `LD_LIBRARY_PATH`) reproduces the #UD 2/2;
  adding back only `LD_LIBRARY_PATH` (no `set -x`) stays clean 2/2. `set -x` makes the shell
  write a trace line to stderr before every command, including right around a backgrounded job's
  fork — extra `write()` syscalls interleaved with fork, each running through a patched
  trampoline stub. Working theory: this extra concurrent trampoline traffic during fork is what
  makes a stub fail to decode. Minimal repro going forward: `set -x` + a single backgrounded
  command, no XFCE/weston/display stack needed at all.

  **Methodological warning this bisection surfaces**: `set -x` is present in most of this
  project's own debug/launch scripts written throughout tonight's investigation (including
  `advisor/probes/run_xfce_staged.sh` and probably `xfce_launch.sh` variants). Some portion of
  earlier "XFCE is broken" findings in this session's history may be an observer effect — the
  tracing instrumentation itself crashing the very thing being traced — rather than a genuine
  XFCE/display-path bug. Treat any earlier finding that used a `set -x`-instrumented script with
  appropriate skepticism until reproduced without it. **Action taken**: `set -x` should be
  dropped from launch/debug scripts going forward (replace with explicit `echo` stage markers) —
  but the #UD itself is still a real, worth-fixing litebox bug now that it has a fast deterministic
  repro, not something to just work around by removing tracing.

  **Next measurement, given the fault is now reliably reproducible in ~30s**: dump the trampoline
  stub bytes from BOTH the parent (which survives) and the child (via
  `LITEBOX_DIAG_FAULT_VQ=1`/its rip==cr2-widened variant) and diff them directly. Differing bytes
  = fork is corrupting trampoline stubs during copy. Identical bytes = the child jumped into a
  valid stub at a non-instruction boundary (a different bug class — a jump-target computation
  issue, not memory corruption).

  **Keep this on the list even after it stops blocking the launch chain** (see the major update
  at the top of this section — removing `set -x` unblocked the full XFCE launch without fixing
  this bug). It's a real litebox bug with a fast, deterministic repro, and it silently breaks any
  traced (`set -x`) script — worth fixing properly, not just avoiding.

  **UPDATE (this pass): exact exception decoded, root cause partially found and partially fixed,
  NOT fully closed — the #UD is NOT a plain `#UD` at all.** `LITEBOX_DIAG_FAULT_VQ=1`'s gate
  (`vectored_exception_handler`, `litebox_platform_windows_userland/src/lib.rs`) was `rip == cr2`
  only, which never fires for this signature (`cr2=0` always, for either sub-case below) —
  widened to also catch raw Windows exception code `0xc0000096`
  (`STATUS_PRIVILEGED_INSTRUCTION`), not just `EXCEPTION_ILLEGAL_INSTRUCTION`. Live capture on
  both real crashes this pass: `raw_code=0xc0000096`, `region_base=0x7feffff7f000`,
  `state=MEM_COMMIT`, `protect=PAGE_EXECUTE_READ` — a **real, present, executable page inside the
  trampoline-stub band** (`maybe_patch_exec_segment`'s allocation range, `litebox_shim_linux/src/
  syscalls/mm.rs`), not a plain `#UD`/invalid-byte-decode. `0xc0000096` is Windows' name for an
  unprivileged `hlt` — the EXACT trap musl's mallocng `a_crash()` deliberately executes on a
  detected heap-integrity violation (see this file's own `is_private_data_range` doc comment in
  `litebox/src/mm/linux.rs` for a PRIOR, already-fixed instance of this identical
  `STATUS_PRIVILEGED_INSTRUCTION`/`hlt` signature, caused THAT time by a stale untranslated
  post-fork heap pointer reaching `free()`). This crash is very likely the SAME class of signal
  (mallocng correctly self-detecting real corruption), not litebox generating a bad instruction
  directly — but the corruption source this time is different from that prior fix.

  **A real, confirmed synchronization gap was found and fixed** (uncommitted as of this pass —
  see below): `PageManager::duplicate()`'s eager per-region byte-copy (`litebox/src/mm/linux.rs`)
  reads a forking process's OWN LIVE trampoline-stub memory region with **no lock at all** — not
  even the Windows allocation/protect locks its own writes use. `maybe_patch_exec_segment`
  (`litebox_shim_linux/src/syscalls/mm.rs`) WRITES new stubs into that exact region while holding
  `elf_patch_cache.lock()`, but `do_clone` (`litebox_shim_linux/src/syscalls/process.rs`) never
  took that same lock before calling `duplicate()` — a genuine, unsynchronized data race between
  one thread extending the trampoline (triggered by `set -x`'s extra `write()` syscalls, each one
  executing through a trampoline stub) and a different thread's concurrent `fork()` reading that
  same memory. **Fix applied**: `do_clone` now holds `self.global.elf_patch_cache.lock()` for the
  duration of the `duplicate()` call (dropped immediately after, before relocation-map
  merging/fd-table duplication, which don't need it).

  **Verification result — fix is real but INCOMPLETE, do not claim this bug is closed:**
  - The isolated fast repro (advisor's bisected `set -x` + single backgrounded `dbus-daemon`,
    `alpine-pinned2.tar`): **6/6 clean runs post-fix** (was reliably 2/2 FAIL pre-fix).
  - The 30-concurrent-fork regression oracle (the OTHER, earlier-fixed bug's own oracle): **3/3
    clean**, no regression.
  - The FULL `xfce_direct.sh` launch (still has `set -x`, `layer31_direct_fixed.tar`): **still
    crashes, bit-for-bit identical signature** (`rip=0x7feffff7fb8a`, `pid=7`/`73`, same two
    timestamps ~0.5s/~36s) even with this fix applied. An isolated repro built from the SAME tar
    (`layer31_direct_fixed.tar` instead of `alpine-pinned2.tar`) but only running the dbus-daemon
    lines from `xfce_direct.sh` (not the full script) stayed clean, meaning the full script's
    heavier concurrency (more background services, more forked children, more trampoline
    extension traffic) still finds a window this specific lock does not close — likely a second
    reader of the trampoline region that also bypasses `elf_patch_cache` (candidate: `fork_verify`'s
    own single-step/AV-path healing reads code bytes via `read_code_bytes`,
    `litebox_platform_windows_userland/src/fork_verify.rs`, also with no `elf_patch_cache`
    coordination) or a race window inside `duplicate()`'s multi-step
    allocate-then-copy-then-protect sequence that a single outer lock around the whole call does
    not fully serialize against a writer that also needs to allocate more trampoline pages
    mid-race (`maybe_patch_exec_segment`'s `do_mmap_anonymous` growth path, `litebox_shim_linux/
    src/syscalls/mm.rs` ~line 1530).
  - **Next step if resumed**: per the diff-the-stub-bytes suggestion already in this section,
    capture the PARENT's copy of the same trampoline region (via `LITEBOX_DIAG_FAULT_VQ`'s
    already-added, now `0xc0000096`-aware capture, extended to dump N bytes at `rip` not just
    `VirtualQuery` metadata) and diff against the CHILD's corrupted copy on a repro that still
    fails post-fix, to confirm whether the corruption is still a torn trampoline write (this fix's
    own hypothesis, apparently still not fully closed) or something else the evidence has not yet
    distinguished.

  **Priority note**: superseded as the launch-blocking issue by "Rendering/scanout blocker" below
  (removing `set -x` from the launch script sidesteps this bug entirely and the desktop now comes
  up) — this bug is no longer standing between the session and the standing goal, but is still a
  real, reproducible litebox bug silently breaking any `set -x`-traced script, worth closing
  properly if picked up again.

## Rendering/scanout blocker (the current single remaining gap)

With both concurrent-fork process bugs fixed and `set -x` removed from the launch script, a full
XFCE desktop now starts and stays alive (weston, Xwayland, xfconfd, xfwm4, xfsettingsd,
xfdesktop, xfce4-panel — see the major update at the top of this section for exact confirmation).
**The only remaining problem is that the framebuffer goes black and never recovers, independent
of XFCE or `xfwm4` entirely:**

- t=7.4s: `non_black_pixels=2073597`, `colors=64` — weston renders a full desktop correctly.
- t=20.3s: `non_black_pixels=0`, `colors=1` — goes black, never recovers for the rest of the run.
- The blackout coincides with Xwayland (not yet running any XFCE component) forking `xkbcomp`
  after a large (46,327-pointer) fork_verify healing pass.
- Every XFCE component starts from t=29.9s onward — well AFTER the blackout. Neither XFCE nor
  `xfwm4` causes this; it's already black before any of them exist.

This is now a compositing/scanout question, not a process-launch question: does Xwayland's output
ever reach weston's scanout buffer, or does weston stop flipping once Xwayland becomes the top
surface?

**MEASUREMENT DONE: `LITEBOX_DRM_TRACE=1` (commit `a6d6ba55`, corrected call path in `37cf16fb`)
answers the "did weston stop flipping" question decisively — it did NOT.** 67 DRM ioctls
captured on a full XFCE run, 27 `DrmModePageFlip`. Critical correlation:
- Frames go BLACK at t=19.63.
- Page flips CONTINUE at t=19.74, 19.99, 20.10 — AFTER the blackout.
- 27 page flips == 27 captured `LITEBOX_DUMP_FRAMES` frames exactly — no missed-frame/capture
  artifact; every flip is faithfully observed.

**The guest IS flipping buffers — the buffer CONTENTS are empty.** This rules out "weston
stopped presenting" entirely; the mechanism is a buffer-content problem, not a flip-scheduling
problem.

**Sharper timing pattern**: flips are not evenly spaced.
- t=7.78–8.15: nine flips in ~0.4s (weston-desktop-shell's own render, `2,073,597` px each — the
  already-confirmed-working weston path).
- t=8.15 → t=19.10: **an 11-SECOND GAP with ZERO flips at all.**
- t=19.10, 19.63, 19.74, 19.99, 20.10: flips RESUME, now BLACK, never recovers.

The gap begins the moment Xwayland `exec`s (pid 17 at t=9.78) and ends around when Xwayland forks
its `xkbcomp` helper (pid 21 at t=18.80). Sequence: weston renders its own shell fine → Xwayland
starts and weston stops flipping ENTIRELY for 11s → flipping resumes with an EMPTY buffer and
never recovers. Reads as Xwayland taking over the output and never producing real content — not
anything XFCE does (every XFCE component starts after t=29, well past this whole sequence).

**MEASUREMENT DONE (commit `9c4dd998`): fb-id logging gives a definitive answer — content loss
on EXISTING buffers, not a surface-ownership swap.** weston double-buffers between `fb_id=1` and
`fb_id=2` for the whole run. Correlating each flip's fb id against that frame's pixel count:
```
t=7.07 - 7.81   fb 1,2,1,2,...   px=2,073,597   good
t=18.34         fb_id=1          px=2,073,597   still good
t=18.89         fb_id=2          px=0           BLACK
t=19.28         fb_id=2          px=0           BLACK
t=19.42         fb_id=1          px=0           BLACK
```
`fb_id=1` renders `2,073,597` pixels at t=18.34 and `0` pixels at t=19.42 — the SAME framebuffer,
contents gone 1.1 seconds later. **No third framebuffer ever appears — rules out
surface-ownership handoff entirely.** Combined with flips continuing throughout (established
above), the mechanism is: **the existing dumb buffers' contents are being zeroed or their mapping
lost, while the DRM bookkeeping stays perfectly valid.** weston keeps flipping the same two fbs;
they simply no longer contain what weston drew.

**Matches an already-fixed bug class in this project**: same shape as the `memfd` mmap-time wipe
bug fixed earlier this session (commit `3a0755e4`) — a shared object's contents overwritten with
zeros behind a live mapping — just on the DRM dumb-buffer path instead of `wl_shm`. Timing fits:
the zeroing happens right as Xwayland starts up and forks, exactly when the memory subsystem is
busiest.

**Concrete suspects, in priority order**:
1. Anything that re-creates/re-commits the dumb buffer's backing memory while a mapping is still
   live — check for a DRM-dumb-buffer equivalent of the memfd "copy the Vec over the shared
   object" bug (see the fixed memfd bug for the exact pattern to look for).
2. CoW/fork interaction with `MAP_SHARED` dumb buffers: weston maps the scanout buffer, a fork
   happens nearby (Xwayland/xkbcomp). If a shared scanout mapping gets treated as private and
   copied during fork, the compositor keeps writing into a copy while scanout reads the
   original — exactly matches "flips continue, content frozen then lost." Check whether
   `PageManager::duplicate()` (already touched twice tonight for unrelated fork races) handles a
   `MAP_SHARED` dumb-buffer mapping correctly during fork.
3. Decommit-then-recommit on the buffer range — a recommitted page comes back zeroed, matching
   `px=0` exactly (not garbage) far better than a corruption theory would.

**MEASUREMENT DONE (commit `80590825`): the buffer is genuinely wiped at the SOURCE — airtight,
eliminates every alternative.** Sampled the scanout buffer's real shared backing store directly
at each flip (fresh `map_shared_memory(handle)` every time, never a stale view):
```
t=7.42-8.16  fb 1,2,1,2...  nonzero=6,221,884  first8=[23,11,0,255,...]  real content
t=19.68      fb=1           nonzero=6,221,884  first8=[23,11,0,255,...]  still good
t=20.27      fb=2           nonzero=0          first8=[0,0,0,0,0,0,0,0]  WIPED
t=21.07      fb=1           nonzero=0          first8=[0,0,0,0,0,0,0,0]  WIPED
```
`fb=1` holds `6,221,884` non-zero bytes at t=19.68, exactly ZERO at t=21.07. **This rules out**:
capture-path bug (fresh mapping each read), CoW/private-mapping divergence (reading the shared
object itself), surface ownership (no third fb ever appears), compositor-stopped-presenting
(flips continue throughout).

**The signature is decisive**: buffers read EXACTLY zero, not garbage. Freshly-committed pages
read as zero; corrupted/reused memory reads as garbage. This means the buffer's pages are being
DECOMMITTED AND RECOMMITTED, or the shared section is being replaced/recreated, while DRM
bookkeeping (fb ids, handles, flip path) stays perfectly valid — exactly why everything
downstream still looks healthy. Same class as the already-fixed `memfd` mmap-time wipe bug, now
on the DRM dumb-buffer path.

**Critical narrowing**: the wipe window (t=19.68 to t=20.27) contains NO DRM ioctl at all — only
205 `diag-vprotect` entries. Nothing in the DRM path itself wipes it; the memory subsystem does,
during heavy protection churn while Xwayland is starting/forking.

**Where to look, in priority order (not yet instrumented)**:
1. Any path that recreates or resizes a shared-memory object for an EXISTING handle — the memfd
   fix's analogue (whatever `resize_memfd_shared_backing`-equivalent may exist for DRM dumb
   buffers, for the exact same shape as the already-fixed bug).
2. Whether a decommit/recommit ever touches the dumb buffer's address range — a recommit produces
   exactly-zero content, matching the signature precisely.
3. `Vmem::duplicate`'s shared-mapping branch — read directly and appears correct (re-maps the
   same handle rather than copying), but not yet instrumented/proven; verify rather than trust.

**Next measurement, not yet done**: log create/resize/destroy of shared-memory objects with their
handle, then grep for the dumb buffer's specific handle in the t=19.7-20.3 window. If that
handle's underlying object gets recreated there, that's the bug, found directly.

**FURTHER NARROWED (not yet confirmed — needs instrumentation, do not trust a code read here)**:
measured facts: (1) the scanout buffers are created ONCE at t=5.98 (2×`DrmModeCreateDumb` +
2×`DrmModeMapDumb`), no `DestroyDumb`/`RmFB`/re-create for the whole run — kills the
"object recreated" theory outright. (2) The wipe window (t=19.68→20.27) contains ZERO DRM
ioctls, only 205 memory ops, 197 of them `caller=fork_duplicate` — the fork is Xwayland forking
`xkbcomp` at t=19.37. (3) Some `fork_duplicate` ranges are framebuffer-shaped (82,944,000 bytes =
exactly 10×1920×1080×4). (4) **Across the ENTIRE run, 9,304 `diag-protect-mapping` entries report
`vma_shared=false` — not one `vma_shared=true` anywhere** (`vma_shared` =
`vma.shared_handle.is_some()`).

**Lead**: if weston's DRM-dumb-buffer VMA has no `shared_handle` attached, `Vmem::duplicate` at
fork takes the non-shared branch and EAGERLY COPIES the region into fresh pages instead of
re-mapping the same handle — fresh pages read as exactly zero, matching the measured signature
precisely, and explains the timing (wipe only ever happens at a fork).

**Caveat, stated honestly, do not skip verifying this**: the forking thread at the wipe moment is
Xwayland's (weston is pid 13, Xwayland's fork is a different thread), so the absent
`vma_shared=true` might just mean the diagnostic never fires on WESTON's own mapping at all — not
that the mapping genuinely lacks a `shared_handle`. Zero logging currently exists on
`map_shared_memory`/shared-handle attachment to settle this either way.

**CHECKED AND REFUTED**: logged the VMA at creation —
`diag-shared-vma-created flags=123 has_handle=true is_shared=true`,
`diag-drm-dumb-mmap len=8294400 offset=4096`. `flags=123` =
`VM_READ|VM_WRITE|VM_SHARED|VM_MAYREAD|VM_MAYWRITE|VM_MAYEXEC`, handle present, size exactly
`1920*1080*4`. **The mapping IS correctly shared.** Fork takes the shared branch as expected — the
"eager copy into fresh zeroed pages" theory does NOT apply. Do not re-investigate this. (The
`vma_shared=false`-everywhere observation was a red herring: that diagnostic simply never fires
on this VMA at all — an instrumentation gap, not evidence of a missing handle.)

Also checked and refuted: two new shared objects DO get created right at the blackout (handles
576/580 at t=20.62/20.77), but they are 82,944,000 and 36,864 bytes — NOT framebuffers. The
capture reads `bytes_len=8,294,400` on every single flip (always the original 8.29MB objects), so
no buffer-swap is happening either.

**What's now solid, reproduced across three runs**: the SAME 8.29MB buffer objects are read
throughout the whole run (`bytes_len` constant, never changes); buffers created once at t=5.94,
never destroyed, never re-created; no DRM ioctl in the wipe window (197 of 205 ops there are
`caller=fork_duplicate`); **weston forks exactly once, at t=6.52, LONG before the wipe — weston's
own fork is innocent** (the wipe happens during Xwayland forking `xkbcomp`, a COMPLETELY
DIFFERENT process from the one owning the mapping).

**MEASUREMENT RETRACTED then CORRECTED AND RE-CONFIRMED (see below) — the reclaim/decommit path
IS excluded, this time on solid footing.** The original overlap test
(`advisor/probes/correlate_scanout_wipe.py`, commit `dcae67c8`) correlated destroy ranges against
the address where the CAPTURE mapped the buffer — but `notify_flip_callback` calls
`map_shared_memory` FRESH on every flip, reads, and unmaps immediately, so that mapping exists
for microseconds. Finding nothing destroys THAT is nearly tautological; it never tested weston's
own actual, long-lived mapping. Tell in the data: `fb_id=1` was logged at four DIFFERENT
addresses across different flips (`1620770816`, `2126577664`, `4470276096`, `743571456`) — a
stable framebuffer does not move; those were all transient capture mappings, not weston's real
one. **weston's actual persistent mapping address was never logged and the correct overlap test
has not been run.**

**FIXED AND RE-RUN — this time a real, valid result: the reclaim/decommit path is genuinely
excluded.** Logged the GUEST's real persistent scanout mapping addresses and re-ran the overlap
test against them (not the transient capture mapping):
```
GUEST persistent scanout mappings:
  0x1ef70000-0x1f759000 (8294400 bytes)
  0x1fad0000-0x202b9000 (8294400 bytes)
19,063 reclaim/decommit events
-> no destroy event overlaps EITHER guest scanout mapping
```
This is a valid negative this time, not the near-tautology from before. **The reclaim and
decommit paths genuinely do NOT touch the scanout buffers.**

**What still stands, unaffected by the retraction above**: same two shared objects (508, 512) for
the whole run, created once, never destroyed (no `DestroyDumb`/`RmFB`); VMA correctly shared
(`flags=123`, `has_handle=true`); fb→handle mapping stable (`fb_id=1`→handle 508 set once at
t=7.55, `fb_id=2`→handle 512 set once at t=6.74, neither ever changes, verified by address
matching); weston (pid 13) and weston-desktop-shell (pid 15) both alive to the end, zero exits;
system stays busy afterward (7,297 socket ops after t=25). **And yet contents still go from
6,221,880 non-zero bytes to EXACTLY zero.** Whether an explicit destroy/decommit call is
responsible is UNRESOLVED (see retraction above) — not excluded, not confirmed.

**"Not a memory bug" REFRAMING ABOVE IS ITSELF REFUTED — confirmed genuine memory bug, decisive
detail found.** Per-flip content digest + non-zero byte count (commit `71cd2ee8`):
```
t=6.67-7.50  nonzero=2,073,600   buffer at creation
t=7.63-8.36  nonzero=6,221,895   weston actively drawing (digest changes every flip)
t=18.93      nonzero=6,221,895   last real content
t=19.53+     nonzero=0           BLACK
```
**Decisive detail**: `2,073,600` = exactly `1920*1080` = one non-zero byte per pixel — an OPAQUE
CLEARED framebuffer (`alpha=255`, RGB zero), the buffer's state at creation. `6,221,895` ≈ 3
bytes/pixel = real color content. `0` = not even the alpha channel. **The final state is
STRICTLY EMPTIER than the buffer's own initial state.** weston never writes an all-zero buffer —
even a fully black desktop keeps alpha set (t=6.67's state). A compositor that merely stopped
drawing would leave the CLEARED state behind (`nonzero=2,073,600`), not reach `nonzero=0`. **The
memory really is being zeroed.** (Caveat: the digest samples every 4096th byte, so an identical
digest between states is a hash collision, not proof of identical state — the non-zero BYTE COUNT
is the real distinguishing evidence, not the digest.)

**Where this leaves us, all explicit destroy paths now excluded**: something zeroes the 8.29MB
shared section — never destroyed, correctly shared (`flags=123`, `has_handle=true`, stable
fb→handle mapping), not touched by any decommit/unmap (0 overlaps in 19,133 destroy events) —
while another process forks. The remaining candidates are IMPLICIT (don't go through an explicit
destroy call at all): a fresh `VirtualAlloc2` with `MEM_COMMIT` over the SAME address, a
`MEM_RESET`, or a section view being re-established/re-mapped.

**MEASUREMENT DONE (this session's own instrumentation, independently converged with advisor's
identical fix): `diag-commit` added at all `VirtualAlloc2(MEM_COMMIT)` call sites
(`litebox_platform_windows_userland/src/lib.rs`, both inside the `reserve_and_commit` closure and
the fixed-address-loop's direct commit-over-`MEM_RESERVE` branch), same field format as
`diag-reclaim`/`diag-decommit` (`start=`/`end=`/`len=`). Also independently added
`diag-drm-fb-addr` logging at the GUEST's own `try_dri_dumb_buffer_mmap` success point
(`litebox_shim_linux/src/syscalls/mm.rs`, ~line 646) — confirmed the same persistent guest
addresses advisor found (`519503872`/`0x1EF00000`..`527798272` and
`531431424`/`0x1FA00000`..`539725824`), logged exactly once per run at ~t=6s and never again for
the buffer's whole lifetime.**

**Result, re-verified across 2 complete runs that both instrumented ALL THREE paths (`diag-commit`,
`diag-decommit`, `diag-reclaim`) AND reached the wipe window (one wipe at t=19.30→19.82, another at
t=18.34→18.84): ZERO overlaps with either guest scanout address range, for ANY of the three memory
APIs, across the ENTIRE run** (not just the wipe window — checked in full, ~2,000-7,700 events per
path per run). This independently confirms and extends advisor's `correlate_scanout_wipe.py`
result: it is not merely that decommit/reclaim don't touch the buffer, `VirtualAlloc2(MEM_COMMIT)`
doesn't either. **Every Windows-level memory-management API this project can instrument
(decommit, unmap, reclaim, and now commit) is now excluded as the wipe mechanism.**

**Also checked and dead-ended**: (1) `fork_verify`'s `write_usize_fault_tolerant` (the single-step
stale-pointer healing write path) only ever writes one `usize` (8 bytes) per fault — structurally
cannot explain a ~6MB content loss even under a hypothesized systematic mistranslation, and its
target addresses are individually fault-driven, not a bulk range write; not instrumented further
because the mechanism itself is the wrong shape for this signature. (2) `close_shared_memory` is
never called on the buffer's handle in any run (zero `diag-shm: close_shared_memory` log lines) —
rules out handle-value reuse/collision via an errant close. (3) Confirmed via a dedicated subagent
that ALL guest "processes" (weston, Xwayland, xkbcomp, etc.) run as `std::thread`s inside ONE real
Windows host process — `fork()`/`clone()` goes through `Vmem::duplicate` (`litebox/src/mm/linux.rs`),
never real `CreateProcessW` (that path, `litebox_platform_windows_userland/src/process_fork.rs`, is
explicitly diagnostic-only, gated behind `LITEBOX_DIAG_PROCESS_FORK_SPAWN=1`, and its spawned child
is `TerminateProcess`'d immediately without running real guest code). This rules out a
cross-real-process HANDLE-value collision (no second real handle table exists), but leaves open
whether two DIFFERENT guest processes' own *logical* `Vmem`s (weston's and Xwayland's, each
believing it owns its own guest-virtual address space) could pick the SAME real underlying Windows
host address for unrelated allocations without either one's bookkeeping ever knowing — this was
dispatched to a subagent for investigation (see `CLAIMED_RANGES`,
`litebox_platform_windows_userland/src/lib.rs` ~line 4018-4370) but not yet resolved as of this
writing; the next session should read that subagent's report (or re-run the investigation if it
did not complete) before opening new avenues.

**Operational note for future sessions**: host memory pressure is a real, live confound.
`Get-CimInstance Win32_OperatingSystem` showed ~4.9GB free out of 16GB total during this session
with two parallel investigating sessions plus their subagents/background runs all launching XFCE
concurrently; at least one run in this session and one in advisor's died early
(`memory allocation of ... bytes failed`, or simply stalled with no further output) well before
ever reaching the t≈19s wipe window. A run that dies/stalls early looks identical to "no wipe
occurred" to a naive oracle — always check that a "clean" run's own log actually reaches
`diag-drm-flip-source-bytes` entries past t≈20s before trusting a negative/absent-overlap result
from it. Check free memory before launching another full XFCE run if multiple sessions are active.

**FAST REPRO FOUND (~16 seconds, not the full ~500s XFCE launch) — use this for iteration going
forward**: three-way isolation, each run under 40 seconds:
```
weston alone + 20 backgrounded forks     -> NO wipe. 25 frames, ends at 2,073,597 px.
weston + Xwayland, nothing connects to X -> NO wipe. 24 frames, ends at 2,073,597 px.
weston + Xwayland + ONE X client         -> WIPE at t=16.4, nonzero 6,221,890 -> 0
```
**Forks alone do NOT trigger it. Xwayland merely running does NOT trigger it. It needs an X
CLIENT to actually connect.** That connection is what makes Xwayland fork `xkbcomp` (2 execs in
the failing repro, 0 in the no-client passing run), and `xkbcomp`'s fork carries a ~46,000-pointer
heal versus a few hundred for an ordinary shell fork. Repro recipe: start seatd, start weston
(drm-backend, pixman, desktop-shell), start Xwayland `:1` fullscreen, wait for
`/tmp/.X11-unix/X1`, then run any X client (`DISPLAY=:1 xfce4-about --version` works) — no dbus,
no xfconfd, no XFCE session, no window manager, no panel needed at all.

**Every explicit memory operation now instrumented, NONE of them touch the scanout buffers**:
scanned every logged operation carrying a start/end range against the two guest scanout mappings
in the fast repro — `diag-commit` (`VirtualAlloc2`/`MEM_COMMIT`): 0 overlaps; `diag-reclaim`/
`diag-decommit`: 0 overlaps; `diag-vprotect`: 2 overlaps, both at t=0.97 (creation only);
`DrmModeMapDumb`: 2 total, both at startup; guest re-mmap of the buffer: none after t=0.97.
**Nothing we currently log touches the scanout buffers between creation and the wipe, yet their
contents go to exactly zero.** This is itself a strong clue: whatever zeroes it is not going
through any currently-instrumented mapping-level path — it must be a WRITE, not a
map/commit/decommit/protect operation.

**Candidates, in priority order, none yet confirmed**:
1. The fork/duplicate path writing INTO the child at addresses that alias the parent's shared
   section. All guest processes share one real Windows address space, so a relocation computing
   a wrong destination could land on the framebuffer without any decommit/commit ever being
   logged — it would just look like an ordinary memcpy. Ties directly into the still-open
   cross-`Vmem` real-address-collision question two paragraphs above.
2. `fixup_stale_elf_data_pointers`/fork_verify's own healing writing through a stale pointer. The
   ~46,000-pointer heal during `xkbcomp`'s fork is the largest such operation in the whole run,
   and it's exactly what distinguishes the failing case from the two passing ones.
3. Anything that zero-fills a BSS or new mapping using a length or base computed from the wrong
   VMA.

**`LITEBOX_FORKVERIFY_OFF=1` bisection TRIED — INCONCLUSIVE, do not repeat.** Disabling reactive
healing breaks the guest well before the point of interest: run dies at t=3.2 (`exit=11`) having
only reached seatd startup — weston never starts, Xwayland never starts, no X client, no frames
captured at all. Two guest faults on the way down, the second (`rip=0x7feffff80233`) in the
trampoline band again. This cannot distinguish the heal-path candidate from the other two; the
test itself is broken, not the hypothesis.

**Decisive next measurement, not yet done — use one of these, not the FORKVERIFY_OFF bisection**:
1. **`GetWriteWatch`** (Windows API, exactly for this): allocate the scanout buffers with
   `MEM_WRITE_WATCH`, poll `GetWriteWatch` at each page flip, log which pages were dirtied since
   the previous flip. During normal rendering this should show weston's own drawing; at the wipe
   it will show whoever actually wrote there — the dirtied page addresses alone will usually
   identify the writer directly.
2. **Cheaper, deliberately destructive trap**: after the last known-good flip, `VirtualProtect`
   the scanout range to `PAGE_READONLY` and let the offending write raise an access violation —
   the existing VEH captures the faulting `rip` directly, naming the writer in one run. Not a
   fix, a probe, but names the exact call site fast.

**Caution for whoever runs this**: correlate against the GUEST's own PERSISTENT mapping, not any
per-flip capture mapping — the capture maps the section fresh and drops it within microseconds,
and the same fb has been observed at four different addresses across different flips. Correlating
against the capture mapping gives a false negative that looks convincing (this exact mistake was
made and retracted earlier in this investigation).

Standing facts for whoever picks this up: repro is seatd, weston (drm/pixman/desktop-shell),
Xwayland `:1` fullscreen, then ONE X client (`advisor/probes/scanout_wipe_repro.sh`); wipe at
t~16, `nonzero 6,221,890 -> exactly 0`; zero overlaps against the scanout range across
`diag-commit`/`diag-reclaim`/`diag-decommit`; only two `diag-vprotect` hits on that range in the
whole run, both at creation (t=0.97) — nothing instrumented touches the buffer between creation
and the wipe.

**Independent second-session confirmation (this session), fully converged with everything above,
plus new findings, dead ends, and a corrected fast-repro fact**:

- Re-ran the full three-metric overlap check (`diag-commit`, `diag-decommit`, `diag-reclaim`)
  against the GUEST's own persistent scanout addresses (independently re-derived and logged at
  `litebox_shim_linux/src/syscalls/mm.rs`'s `try_dri_dumb_buffer_mmap` success point — confirmed
  identical addresses to advisor's own: `0x1EF00000`/`519503872` and `0x1FA00000`/`531431424`,
  logged exactly once per run at ~t=6s and stable for the buffer's whole lifetime) across two full
  XFCE runs that both reached the wipe (t=19.30→19.82 and t=18.34→18.84). **Zero overlaps for all
  three APIs, for the entire run, not just the wipe window** — same result as advisor's, reached
  independently. This is now confirmed by two separate sessions using two separately-added,
  independently-verified logging sites.
- **Corrected fast-repro fact**: built a minimal fast-repro script (seatd → weston → Xwayland `:1`,
  no X client at all) and reproduced the wipe in ~13.6s, WITHOUT ever running an X client —
  contradicting advisor's "needs an X client to connect" finding above. Xwayland merely running
  long enough (past `XWAYLAND_READY`) is sufficient to trigger it in this session's runs; whether
  advisor's original 3-way isolation result was itself timing-sensitive (i.e. the "no wipe" cases
  simply hadn't run long enough yet) is unresolved — the isolation experiment should be re-run with
  a longer timeout before trusting "needs a client" as a real precondition.
- **`LITEBOX_FORKVERIFY_OFF=1` bisection independently re-attempted and independently reached the
  same inconclusive result** as advisor's own attempt above (both sessions tried this without
  seeing each other's result first): with fork_verify's reactive healing disabled, the run dies
  with a genuine host-side `#PF` (`Exception(14)`, `cr2` genuinely unmapped) partway through
  startup — this run never even got weston running, let alone Xwayland or a client. **Two
  independent attempts, two independent confirmations that this bisection is unusable** — do not
  attempt it a third time; fork_verify's healing is load-bearing for basic process-launch
  stability, and disabling it does not isolate the wipe question, it just substitutes a different,
  earlier, already-documented crash.
- **Traced but did NOT find evidence of exploitation**: `CLAIMED_RANGES`
  (`litebox_platform_windows_userland/src/lib.rs` ~line 4018-4370), the registry that prevents two
  different guest processes' `Replace`-mode (fixed-address) allocations from colliding on the same
  real Windows address, is populated ONLY from `allocate_pages` calls (`claim_range`, called at
  lines ~4332/5729/5797) — it is NEVER populated by `map_shared_memory` (confirmed via `grep`: zero
  `claim_range` call sites in `map_shared_memory`/`create_shared_memory`). This means weston's DRM
  scanout buffer's real address is structurally INVISIBLE to `CLAIMED_RANGES` — a genuinely
  existing gap, not a hypothetical one. However: `Replace`-mode's OTHER collision guard
  (`has_committed_page`, a direct `VirtualQuery` against Windows' own real VAD state, checked
  before `find_foreign_claim` even runs) WOULD still see the buffer's real `MEM_COMMIT`/`MEM_MAPPED`
  state correctly regardless of `CLAIMED_RANGES` — so this gap is not immediately exploitable by
  itself. Whether some code path could still race past `has_committed_page`'s check (a TOCTOU
  window, or a `Hint`-mode allocation that never queries commit state for a NULL-hint request) was
  not fully resolved; this is real remaining uncertainty, not a dead end, but no live evidence of
  it firing was found in any instrumented run (would show up as a `diag-reclaim`/`diag-commit`
  overlap, and none were found).
- **Two weston-upstream-source hypotheses investigated via subagents against real weston source
  (gitlab.freedesktop.org/wayland/weston), both DEAD-ENDED**:
  1. `drm_rb_discarded_cb()`/`pixman_renderer_resize_output()` creating a fresh (genuinely
     all-zero-including-alpha) dumb buffer on an output resize/mode-change (`backend-drm/drm.c`,
     `pixman-renderer.c`) — directly refuted against this session's own logs: the wipe window in a
     confirmed-wiped run contains **exactly one `DrmModeSetCrtc` ioctl in the ENTIRE run, at t=6.09s
     (initial setup)**, none anywhere near the wipe (t=18.3-18.8s), and the SAME `fb_id`/buffer
     `handle` values (4556/4560) are used both immediately before and immediately after the wipe —
     no new buffer was ever created, ruling out this mechanism for the observed data.
  2. weston's pixman renderer or damage-tracking legitimately/buggily zero-filling the WHOLE output
     (as opposed to real content or a solid background color) via some client-buffer-attach-failure
     or damage-computation edge case — refuted by direct source reading:
     `pixman_renderer_repaint_output()` scopes both `repaint_surfaces()` and `copy_to_hw_buffer()`
     strictly to `output_damage`, never the whole buffer; `draw_view()` SKIPS compositing entirely
     (a no-op, leaving existing content untouched) when a view has no buffer attached, rather than
     clearing that region to zero. No `PIXMAN_OP_CLEAR`/memset-to-zero path exists in the renderer
     for a stalled/hung client. **weston's own real compositing code structurally cannot produce a
     whole-output, all-channel-zero frame while continuing to flip real fb ids** — this is a strong
     negative result, not merely an unconfirmed one.
- **Bottom line after two independent full sessions' worth of instrumentation**: every mechanism
  either session could name AND instrument has been checked and excluded (Windows memory
  management in full; weston's own real compositing/resize logic in full; fork_verify's own write
  path, wrong shape for the data volume; handle-value reuse, never closed; cross-real-process
  handle collision, structurally impossible in this architecture). The `CLAIMED_RANGES` gap above
  is the one item that is genuinely still open rather than excluded, but has zero supporting
  evidence from any run. **The honest state is: the wipe is real, reproducible in ~14-20s via the
  fast repro, and its mechanism is not visible to any currently-instrumented logging path** — it is
  a WRITE (not a map/commit/decommit/protect operation), it originates during Xwayland's presence
  (not necessarily its fork specifically — see the corrected fast-repro fact above), and finding it
  now requires either `GetWriteWatch`-style live write observation (does not work here — `MEM_WRITE_
  WATCH` is incompatible with `MapViewOfFile3`-backed section views, only works on private
  `VirtualAlloc`-committed memory, so this specific tool is NOT usable for a shared-section-backed
  buffer like this one, a correction to option 1 below) or the `VirtualProtect(PAGE_READONLY)` +
  existing-VEH write-trap approach (option 2 below), which was not attempted this session due to
  the risk of destabilizing the existing, delicate VEH/fork_verify interaction without enough
  remaining session budget to verify it doesn't regress anything — this is the precise, concrete
  next step for whoever picks this up next, not a vague "needs more investigation."

**PROXIMITY check tightens this further, AND opens a new possibility this whole section had not
seriously considered — read before committing to GetWriteWatch/trap work.** Checked every logged
range operation during the tight 450ms wipe window for PROXIMITY, not just overlap: not one range
comes within 16MB of either buffer (436 commits, 404 reclaims, 280 protect-mappings, 197
`fork_duplicate` ops in that window, none anywhere near the framebuffer). The window itself:
```
t=15.48  Xwayland forks, pid 16 execs xkbcomp
t=15.78  xkbcomp exits status=0
t=15.97  LAST GOOD flip, nonzero=6,221,890
t=16.42  FIRST BLACK flip, nonzero=0
t=16.60  Xwayland forks AGAIN, pid 18 execs xkbcomp
t=16.67  second xkbcomp exits status=0
```
Confirms the write does not come from address-space bookkeeping — it comes through a mapping,
which range logs structurally cannot see.

**Possibility this may not be a memory bug at all, reconsidering the earlier "exactly zero"
argument**: two `xkbcomp` forks within 1.2s means Xwayland set up its keymap twice — in a Wayland
compositor, a client appearing and resulting surface/output changes routinely cause a repaint,
and a repaint of a scene with no visible content legitimately clears the framebuffer to zero. The
earlier argument ("weston would leave alpha set on a real clear, so exact-zero proves corruption")
is weaker than it looked: pixman clearing to TRANSPARENT BLACK writes all-zero INCLUDING alpha —
only the DRM dumb-buffer ALLOCATION path produces the opaque-black initial state seen at t=6.67.
So the differing states (opaque-black at creation vs. fully-zero at wipe) do NOT actually rule out
weston legitimately clearing the buffer during a normal repaint.

**MAJOR REFRAME: the flip cadence itself was misread, and this changes the whole shape of the
investigation.** Full flip timeline (not just the few flips immediately after the wipe) across
three independent runs shows weston is NOT continuously rendering — it flips in short bursts when
something changes, then goes completely idle for 10+ seconds, and eventually stops flipping
altogether entirely. That is CORRECT compositor behavior (no damage, no repaint), not evidence of
anything broken:
```
xt1: 29 flips, first t=1.51, LAST t=25.45 -- run continues to t=85 with NO further flips
     gaps: t=2.8->15.3 (12.5s), t=16.8->25.4 (8.5s)
xc1: 30 flips, last t=28.1, gaps of 13.0s and 10.6s
xfM: 25 flips, last t=48.8, gaps of 10.6s and 28.6s
```
The real sequence: t=1.5-2.8 weston paints its shell, flips actively (6,221,880 non-zero bytes).
t=2.8-15.3: IDLE, no flips at all — nothing happening to the framebuffer during this whole
window. t=15.3: Xwayland/xkbcomp activity causes a repaint. t=15.97: last flip, STILL showing old
content. t=16.42: next flip, ZERO. **This is consistent with "weston repaints a scene that now has
nothing visible in it, and paints it to zero" (candidate (c), legitimate clearing) rather than
"content is drawn, then something corrupts it mid-life."** The earlier "weston would leave alpha
set on a real clear" argument against (c) does not hold (see the prior reconsideration above) —
this reframe makes (c) the LEADING hypothesis, not an outsider.

**A more concrete, likely more directly user-facing bug found in the same investigation**: a
trivial X client (`xfce4-about --version`, which should print a version string and exit in
milliseconds) instead runs for 60+ SECONDS without exiting — still alive at the end of a
repeated-client test, actively allocating shared memory and exchanging Wayland protocol at t=25.
**This would fully explain the user's ORIGINAL reported symptom** ("we saw a blue bar, clock and
icon display... then went black" after "a pretty long wait") — the X clients are alive but
pathologically slow, so almost nothing gets drawn in reasonable time, independent of any
memory/scanout question at all.

**Revised priority order, redirect here first**:
1. **Why does a trivial X client take 60+ seconds of wall time instead of milliseconds?** Profile
   where it spends its time. This is likely the actual "XFCE is slow and mostly blank" cause and
   is more directly tied to the original user-reported symptom than the scanout-zero question.
2. Only after (1) is answered: whether the zero-buffer is weston correctly painting an empty
   scene. Cheap test: run an X client that actually MAPS A WINDOW (not just connects and exits)
   and see whether content appears. If a real window renders, there is likely no memory bug here
   at all.

**Hold `GetWriteWatch`/trap plumbing until (1) and (2) are answered** — that work presumes memory
corruption that may not exist.

**(1) PARTIALLY ANSWERED: the client is NOT round-trip-amplification-slow — it's blocking on
genuine multi-second dead stalls.** Directly measured: extracted every `diag-unix-stream`
timestamp after the client execve's (503 messages over ~37s). The gap distribution is NOT "many
round-trips each slightly slow" — most consecutive gaps are ~90 MICROSECONDS (fast, normal
socket traffic), interrupted by a handful of MULTI-SECOND dead gaps with ZERO logged activity of
ANY kind (no socket traffic, no epoll activity, no memory ops, no fork_verify activity) during
them:
```
21.92 -> 23.76  (1.83s)
24.44 -> 29.38  (4.94s)
30.00 -> 39.74  (9.74s)   <- checked directly, genuinely nothing logged in this window
39.94 -> 43.39  (3.45s)
43.51 -> 47.11  (3.60s)
48.30 -> 54.45  (6.15s)
54.59 -> 56.86  (2.28s)
```
This refutes the round-trip-amplification theory: if thousands of round-trips each cost ms
instead of µs, spacing would be roughly even throughout, not fast bursts separated by
multi-second silence. **The process is genuinely blocking on something** (a wait/poll/timeout, a
lock, a resource) during these gaps, not doing slow-but-steady protocol work.

**CONFIRMED FROM TWO INDEPENDENT ANGLES: the stall is a GLOBAL FREEZE, not the X client blocking
on something specific.** advisor-db checked the ENTIRE log (all processes, all subsystems) for
gaps and found the same multi-second dead windows with NOTHING logged by ANY process —
weston, Xwayland, the shell, no memory ops, no fork activity, all silent simultaneously
(`t=3.71` gap 4.23s, `t=8.54` gap 1.08s, `t=10.28` gap 6.81s, `t=21.31` gap 5.16s — 17.3s of dead
time in a 98s run). Independently, this session's own `sys_ppoll`-scoped debug capture confirms
it from a different subsystem: Xwayland's own event loop (`tid=12`, which normally spins at
~5-7ms poll intervals continuously) ALSO goes completely silent for the exact same window
(t≈29.44 to t=39.36 in that run) — Xwayland is not doing anything either, not just the client.
**A single guest thread waiting on a timer would not silence weston, Xwayland, and the shell all
at once — every guest thread stops together.**

**Leading theory: lock contention, likely `VIRTUAL_PROTECT_LOCK`/`ALLOCATE_PAGES_FIXED_ADDR_LOCK`
(the same lock, two names) held across a large `fork_duplicate` copy.** This session already
landed a fix (`984927b0`) making `unmap_shared_memory` take this same shared lock, and it's
already known to be shared across allocate/deallocate/protect paths. If `PageManager::duplicate()`
holds this lock for the DURATION of copying a large region (confirmed elsewhere in this
investigation: `fork_duplicate` copies up to 110,206,976 bytes, and 197 `fork_duplicate`
operations were observed inside one single 450ms window), every OTHER guest thread that touches
memory during that copy blocks behind it — producing exactly the observed "everything freezes at
once" signature. Fits the earlier (now-recontextualized) dose-response finding: more concurrent
forking correlates with more/longer freezes.

**CAUTION before acting on this**: a harness's own `sleep 0.5` polling loop in a launch script
produces regular ~0.5s "gaps" that are NOT real stalls — exclude those; only the irregular
multi-second gaps are the real signal. Also: **host memory pressure is a live, real confound
right now** (multiple concurrent sessions/agents running heavy launches) — before concluding this
is a genuine litebox lock-contention bug, re-run the fast repro with real memory headroom and
check whether the stalls shrink or vanish, to separate "real litebox bug" from "tonight's
memory-pressure-induced host scheduling noise." This distinction should be settled BEFORE
changing any locking code.

**CONTROL TEST DONE: memory pressure is EXCLUDED, and the result confirms the lock-contention
theory decisively.** Same repro, same binary, same script, two host memory states:
```
2.6 GB free:  4 stalls, 17.3s stalled of a 98s run   (18% stalled)
9.3 GB free:  5 stalls, 106.1s stalled of a 117s run (91% stalled)   -- includes a single
                                                                          30.8s stall AND a
                                                                          single 59.8s stall
```
**With nearly 4x the free memory, stalls got dramatically WORSE, not better.** This directly
excludes "tonight's host memory pressure/concurrent-session noise" as the explanation — the
stalls are reproducible and severe regardless of host state, confirmed on the SAME code both
times. This also confirms the lock-contention mechanism predicts EVERY observed property:
- all guest threads silent simultaneously → a global lock, not a per-thread wait
- duration varies wildly (1s to 60s) → scales with the size of whatever holds the lock
- **worse with MORE free memory** → larger copies SUCCEED and run to completion (holding the
  lock the whole time) instead of failing/bailing early when memory is tight — this is the
  counterintuitive result that most sharply confirms the theory over any host-noise explanation
- correlates with fork activity → `fork_duplicate`'s eager copy is the big lock-holder
- dose-response with concurrency (measured much earlier this session) → more concurrent forks,
  more contention

**LOCK-CONTENTION THEORY REFUTED BY DIRECT MEASUREMENT — do NOT touch `VIRTUAL_PROTECT_LOCK`/
`ALLOCATE_PAGES_FIXED_ADDR_LOCK`'s scope, that was a false lead.** The reasoning above (memory
headroom correlating with hold duration) was plausible but wrong -- exactly the class of error
this investigation has repeatedly had to catch via measurement rather than inference. Decisive
test: instrumented the lock at `lib.rs:5425` with both WAIT-to-acquire and HOLD duration timing,
logging any acquisition where either exceeded 50ms. Same 16s repro (weston + Xwayland + one X
client), 3 stalls observed (4.27s, 5.38s, 5.22s):
```
lock acquisitions held >= 50ms:        ZERO
lock acquisitions that waited >= 50ms: ZERO
```
**Not one acquisition of this lock even reached 50 milliseconds** — neither a long hold nor a
long wait, anywhere in the run. If this lock were the mechanism, the holder would show a
multi-second HOLD and blocked threads would show multi-second WAITs; neither appears. The
structural code-reading analysis of what the lock covers was correct — it just isn't the cause of
these stalls. Narrowing or restructuring it would reintroduce the real TOCTOU race it exists to
prevent, for zero benefit.

**What still stands, measured not inferred**: stalls are global (every process/subsystem silent
at once, confirmed two independent ways); NOT host memory pressure (9.3GB free made it WORSE than
2.6GB); NOT lock contention (zero slow acquisitions, just refuted above); stalls recur even in
the minimal repro with very little forking (t=3.5, 9.8, 19.1 observed in one run).

**New leading candidates, ranked**:
1. **Something that deliberately suspends ALL guest threads simultaneously BY DESIGN** — `fork`'s
   `kill_other_threads` path, or any `fork_verify` single-step pass that suspends threads. A
   suspend-all that then waits on one thread which is itself slow to reach a safe point would
   produce exactly this signature (global freeze, no single lock implicated). **Top suspect.**
2. A host-side GC/allocator pause inside the runner process itself (the Rust host allocator, not
   litebox's guest-facing page management).
3. Waiting on a Windows synchronization object with a long/infinite timeout satisfied late.

**Next step, not yet done**: search for `kill_other_threads` and any thread-suspension code in
`litebox_shim_linux`/`litebox_platform_windows_userland` (likely in the fork/clone path and
possibly `fork_verify.rs`'s single-step machinery). Log entry/exit of any all-thread-suspend
operation with duration — if a suspend-all call's own duration spans a stall window, that names
the mechanism directly and points at a far more tractable fix than anything involving locking.

**A "60-second timeout" theory was proposed and then self-corrected within the same
investigation — recorded here so it isn't re-derived.** A striking measurement (successive
stall-end timestamps across 4 independent runs differing by exactly ~60.000s, sub-10ms alignment)
initially looked like a missed-wakeup-rescued-by-timeout bug. Follow-up showed this was a
misreading: at each 60-second boundary, the SAME epoll entry fires (`entry_id` fixed,
`events_bits=1` then `=0` ~90µs later), then 60s of total silence — a periodic HEARTBEAT the
guest itself set (in the ~208s run, only SIX log events total occur after t=30), not a rescued
waiter. Static grep for `60_000`/`60000`/`Duration::from_secs(60)` across the tree also found
nothing, consistent with this being a guest-side timer, not a litebox one. **Do not chase a
missed-wakeup-on-a-60s-timeout theory** — it's refuted.

**CURRENT BEST UNDERSTANDING, replacing the timeout theory: a client permanently stalls, not
periodically.** `xfce4-about --version` never exits across a 208-second run — does real work for
~25 seconds, then goes PERMANENTLY quiet (not throttled, not periodically slow — simply stops
making progress at all, forever, except for its own unrelated heartbeat timer described above).
This is the signature of a client **waiting for a reply that never arrives** — most likely a
protocol response from Xwayland that's owed but never sent.

**FOUND — the full coherent picture, likely the actual root cause.** At t=28.03, in one 100ms
window, everything observed together:
```
client creates shared memory handle=592, size=245,760, maps it, VirtualProtects it
client sends 112 bytes to the compositor (a surface commit)
weston page-flips fb_id=2, handle=516, size=8,294,400
that scanout reads nonzero_bytes=0
```
`245,760 = 320*192*4` is a small CLIENT SURFACE buffer. `8,294,400 = 1920*1080*4` is the SCANOUT
buffer. **The client IS allocating a buffer, IS drawing, and IS committing it to the compositor.
weston IS receiving the commit and IS page-flipping. But the client's surface never appears in
the scanout, which stays at exactly zero.**

**This makes it a COMPOSITING problem, not memory corruption.** Nothing wipes the scanout buffer
— weston composites an EMPTY SCENE into it and flips that (correctly, mechanically). This
retroactively explains every observation this whole investigation collected: no decommit/unmap/
commit ever touches the scanout (0 overlaps in 19,133 events) because nothing does; the buffer
ends "strictly emptier than initial state" because weston clears to transparent black (alpha
included) vs. the dumb-buffer allocation's opaque-black initial state; fb→handle mapping stable
because it was never the problem; weston stops flipping afterward because an unchanging empty
scene generates no damage; the client never exits because it's waiting for a frame callback that
never comes, since its surface isn't being composited.

**Where to investigate now — a genuinely different area from everything dug into so far — why
weston does not include the client's surface in its scene**, in priority order:
1. The surface is never "mapped" — the commit arrives but weston doesn't treat it as ready to
   show (missing/mis-handled `wl_surface.attach`/`commit` sequence, or weston rejects the buffer).
2. **The shm pool import fails silently on weston's side** — weston has a surface with no usable
   buffer content. Closest to litebox's own code (weston maps the client's 245,760-byte pool
   through the shim's shared-memory path) — and this project already found ONE shm bug this
   session (the memfd mmap-time wipe, already fixed). If weston's mapping of the CLIENT buffer
   reads as zero, the client draws into one view while weston reads a different one — a real
   litebox bug, on the client-buffer path instead of the scanout path.
3. Xwayland's rootful window isn't being given a shell surface role at all, so weston has nothing
   positioned to draw.

**Cheap decisive test, not yet done — same instrumentation already built, pointed at a different
handle**: sample the CLIENT's buffer (handle 592, 245,760 bytes) the same way the scanout buffer
is already sampled. If it's non-zero, the client drew successfully and weston is failing to
composite it (candidate 1 or 3). If it's zero, the client's own drawing isn't landing at all
(candidate 2, a shared-memory bug in the client-buffer path).

**CLIENT-BUFFER SAMPLING INSTRUMENTATION ALREADY EXISTS — confirmed by code reading, this session
(`litebox_platform_windows_userland/src/lib.rs`, `map_shared_memory`, ~line 6139-6161): every
`map_shared_memory` call already logs `nonzero_in_sample` (first 4KiB of any buffer <=4MiB),
gated the same as the scanout digest. The doc note above ("not yet done") is stale relative to
current code; some peer session already landed this. Re-run and grep `nonzero_in_sample` rather
than adding new instrumentation.**

**One re-run this session (`fast_repro.sh` inside `layer31_direct_fixed.tar`, plain
`xfce4-about --version`, `LITEBOX_DRM_TRACE=1`) did NOT reproduce advisor's t=28 compositing
picture at all — a different, earlier divergence, underscoring the session's already-documented
non-determinism:**
```
t=3.2-4.7   client creates several shm handles (4904/4908/4940, sizes up to 8,294,400)
t=4.50      map_shared_memory FAILED handle=4980 win32_err=1132 (ERROR_MAPPED_ALIGNMENT) x3,
            correctly retried/handled per the existing NoReplace/AddressInUse fallback -- not a bug
t=4.51-4.68 create_shared_memory handle=4988 size=245760 (matches the 320x192x4 client-surface
            shape advisor described) and handle=4992 size=4096 -- but NEITHER is ever mapped via
            map_shared_memory anywhere later in this run (zero nonzero_in_sample events at all)
t=5.06-5.79 scanout genuinely has real content, nonzero_bytes=6,221,881 (weston's own shell paint)
t=16.4-17.4 Xwayland starts; scanout wipes to nonzero_bytes=0 and stays there through TEST_DONE
t=21.1      execve xfce4-about; prints "xfce4-about 4.20.1 (Xfce 4.20)" and exits promptly;
            Xwayland logs "failed to read client connection (pid 20)"
t=38.8      a NEW create_shared_memory handle=4724 size=245760 appears (some other client/process)
final frames (LITEBOX_DUMP_FRAMES): non_black_pixels=0 -- standing goal NOT met in this run
```
This run's `xfce4-about --version` behaved like a normal short-lived CLI probe (prints version,
exits), not like advisor's characterized long-lived GTK client that draws a 320x192 surface and
stalls at t=25-28 waiting on a compositor reply. Both shapes are real and reproducible on
different runs -- **whether `xfce4-about --version` builds a real GTK window (and thus a wl_shm
surface) at all may itself be non-deterministic or environment-dependent** (frozen locale/DISPLAY
race, GTK falling back to a no-display code path, etc.) and is itself worth checking directly
(`ldd`/`strace`-equivalent on what `xfce4-about --version` does on real Linux) before spending
more time chasing the compositing theory on a client invocation that may not even reach the
drawing code path every time.

**Next step for whoever picks this up**: (1) confirm whether `xfce4-about --version` is expected
to create a GTK window on real Linux at all (if not, swap the repro's client for one that
definitely does, e.g. plain `xfce4-terminal` or a minimal wayland/X11 test client that always
maps a surface) so the repro reliably reaches the code path the compositing theory is about; (2)
once a client reliably reaches `map_shared_memory` for its own surface buffer, re-run with
`LITEBOX_DRM_TRACE=1` and read `nonzero_in_sample` directly off the existing instrumentation
(no new code needed) to settle candidates 1/2/3 above.

**(2) IS NOW DONE, on a run where the client DID reach the drawing path — DECISIVE, COMPOSITING
THEORY CONFIRMED. STOP ALL MEMORY-CORRUPTION WORK.** Sampling every shared mapping under 4MiB at
map time in the 16s repro:
```
handle=540, size=245,760: nonzero_in_sample=0 at map (t=2.51), then 1024 at t=2.64
handle=600, size=245,760: nonzero_in_sample=0 at map (t=25.65), then 1024 at t=31.65
```
245,760 bytes = 320*192*4, a client surface buffer. It starts empty then HAS CONTENT. Other
client buffers show the same pattern (36864→4096 nonzero, 20480→1664, 40960→1664). **The client
draws successfully, and litebox delivers that content faithfully through the shared-memory path
— the shared-memory path WORKS.** Meanwhile the 8,294,400-byte scanout stays at exactly zero
throughout the same run. **The client has pixels; weston is not compositing them into the
scanout. This is confirmed as a compositing problem, not memory corruption.**

**STOP, effective immediately, do not resume without strong new contrary evidence**:
`GetWriteWatch` on the scanout, any `PAGE_READONLY` write-trap, any further lock-scope work, any
further hunt for what "zeroes" the framebuffer. Nothing zeroes it — weston composites an empty
scene, and an empty scene reads as zero.

**Caveat on the above (advisor, precise on purpose — do not overread this)**: each client handle
above was seen mapped at TWO distinct addresses (e.g. handle=540 at addr=482,082,816 reading 0,
and addr=928,317,440 reading 1024). It is tempting to read this as "the client sees content,
weston's own mapping reads zero" — **it does not show that.** Both samples were taken at MAP
time, so the zero reading is just a mapping established before the client had drawn, and the
non-zero one is later — same object, two different moments, not necessarily two different
processes' views. (Also: every mapping logs host pid 25532 for all of them, because all guest
"processes" are threads in one shared host process, so pid cannot be used to distinguish
client-side vs weston-side mappings here.) The still-open, still-decisive test is: sample BOTH
the client's mapping and weston's own mapping of the SAME handle at the SAME instant. If weston's
reads zero while the client's reads non-zero at that instant, that's a genuine litebox
cross-process shared-mapping bug (new territory, never instrumented this session). If they agree,
litebox is delivering correctly and the bug is entirely inside weston's own scene graph (surface
role / damage / repaint scheduling) — likely not a litebox bug at all. Cheapest next discriminator
per advisor: weston's own debug flags (surface role, damage, repaint-scheduling logging) may name
the reason directly, without guessing from memory contents.

**Methodology finding (advisor, applies broadly — audit other launch scripts for this)**: a
background service (e.g. `weston ... > file 2>&1 &`) that dies with a clear fatal error prints
that error ONLY into the redirected file, never into the main log. The launcher then just times
out waiting for the socket/marker that service was supposed to create, which looks exactly like a
stall or a litebox bug rather than what it is (a bad CLI arg / fast crash). Confirmed directly: a
stray `n` typo on weston's command line caused `fatal: unhandled option: n` + immediate exit,
invisible until the redirect file was read by hand; the launcher reported `WESTON=60` (full
timeout) with zero indication in the main log of why. **Any script backgrounding a service with
`> file 2>&1` should either tee to the console too, or `cat` the file automatically on a
readiness-timeout path — silent redirects turned real, fast, self-explanatory crashes into
mysterious multi-minute "hangs" for a meaningful fraction of this session's wasted investigation
time.** `advisor/probes/run_xfce_staged.sh` and this session's own probes (`fast_repro.sh`,
`scanout_wipe_repro.sh`, `scanout_wipe_discriminator.sh`) all redirect weston/Xwayland/xfwm4/
xfsettingsd/xfdesktop/xfce4-panel this same way — treat as a liability, not a feature, and fix
before further debugging sessions burn time on phantom "timeouts."

**AUDITED AND FIXED (this pass) for every probe script that has real source on disk**:
`run_xfce_staged.sh` (added `xfconfd.out` to the existing unconditional end-of-run `cat` loop --
every other redirected service there was already covered), `scanout_wipe_repro.sh` (now
unconditionally `cat`s `xc.out`, the client's own output, alongside the already-fixed
`weston.out`), and `scanout_wipe_discriminator.sh` (previously had NO file redirects at all for
seatd/weston/Xwayland — fine for visibility but meant nothing to `cat` on a hang either; now
redirects all of them plus every client (`xc1`-`xc4`) to files and unconditionally `cat`s all
seven at the end). **`fast_repro.sh` could NOT be fixed the same way** — it exists only packed
inside `.wfgy/xfce-build/layer31_direct_fixed.tar` (confirmed via `tar tf`), with no source file
anywhere in this repo; whoever packed it did so ad hoc. If it's still in active use, extract it
from the tar, apply the same unconditional-`cat`-at-end fix, and repack — or replace it with
`run_xfce_staged.sh`/`scanout_wipe_repro.sh`, which are equivalent and now fixed.

With the weston-arg typo fixed, weston's own log confirms the DRM/wgpu emulation path is fully
healthy — no errors/warnings anywhere in weston's own startup: `weston 14.0.2`, OS reports as
`LiteBox, 5.11.0, x86_64`, `drm-backend` loads, libseat/seatd session granted, `/dev/dri/card0`
in use, `Using Pixman renderer, shadow framebuffer`, head `Virtual-1` connected at
`virtual-1920x1080@60.0`, `desktop-shell.so` loaded, input device associated with the output. This
is a genuinely good, previously-unconfirmed result for the DRM/wgpu work: weston itself considers
litebox's virtual display device fully functional. The corrected run reproduces the same frame
pattern (19 real frames, then zero) — the scanout-blackout timing/behavior is unchanged by this
fix, so it does not explain the blocker, but it does rule out "weston doesn't like the DRM device"
as a contributing theory.

**Where the actual bug is now, in priority order**:
1. **weston's own import of the client's `wl_shm` pool — closest to litebox, cheapest to test
   with existing instrumentation.** weston receives the commit and must map the client's
   245,760-byte pool on ITS OWN side. Compare: does the SAME handle (540 or 600 above) get mapped
   a SECOND time by weston's own process, and does THAT mapping's `nonzero_in_sample` agree with
   the client's? If weston's own view of the identical handle reads zero/empty while the client's
   view is non-zero, that is a cross-process shared-mapping consistency bug — genuinely litebox's,
   but on a completely different code path than the scanout (never investigated this session).
   If weston's view agrees (non-zero), litebox is faithfully delivering the content and the bug
   is entirely inside weston's own scene-graph handling — likely NOT a litebox bug at all.
2. Surface role and mapping — a `wl_surface` with a buffer attached but no assigned role, or
   never properly mapped, is legitimately not composited by a correct compositor. Xwayland's
   rootful window needs a shell-surface role from weston's desktop-shell.
3. Damage/frame-callback handling — the client waiting forever for a frame callback is consistent
   with weston never scheduling a repaint that includes it.

**If (1) comes back "weston's own view agrees, non-zero"**: this is very likely NOT a litebox bug
at all, and the standing goal may need reframing around a weston/Xwayland-side workaround (e.g. a
different shell/compositor configuration, or an upstream weston fix) rather than a litebox code
change — worth surfacing to the user as a real possible outcome, not assumed to always be
litebox's fault to fix.

**LIKELY ROOT CAUSE FOUND (web research), cheap to test, try BEFORE any more shared-memory
forensics: no XWM (X Window Manager) is running.** Our setup launches weston with
`--shell=desktop-shell.so` and spawns `Xwayland :1 ...` as a bare separate process. Rootful
Xwayland launched this way needs the launching compositor to also attach an X Window Manager over
a separate `-wm <fd>` connection — that XWM is what maps an X11 window's Wayland surface into the
compositor's scene graph on `MapNotify`. weston's `desktop-shell.so` implements `wl_shell`/
`xdg-shell` roles for NATIVE Wayland clients ONLY — it has no XWM logic. XWM support lives
exclusively in weston's OWN `xwayland` module (`xwayland.so`), which is loaded via
`[core] xwayland=true` in `weston.ini` and which spawns AND manages Xwayland itself (including
the `-wm` fd handshake). By manually spawning `Xwayland` as an unrelated separate process, we
bypass this entirely: **the client's wl_shm buffer gets written with real content (matches our
own instrumentation exactly) but the surface never receives a role/gets mapped into weston's
scene graph, because nothing ever performed the XWM's map-on-MapNotify step.** This is a known,
documented pattern (Arch Wiki Weston page, weston.ini man page both describe `xwayland=true` as
the supported mechanism; the separate "Xweston" project exists specifically to swap out
desktop-shell for an external WM, confirming XWM duties and the shell are coupled, not
independent).
**Fix to try next**: stop spawning `Xwayland` manually. Instead set `xwayland=true` under
`[core]` in a `weston.ini` weston can find, ensure `xwayland.so` is present/loadable in the layer
tar, let weston launch Xwayland itself, and point clients at the `$DISPLAY` weston exports (rather
than hardcoding `:1` and manually waiting for `/tmp/.X11-unix/X1`).
**Cheap diagnostic if the fix doesn't immediately work**: weston's `scene-graph` debug scope
(`--debug` + `--logger-scopes=scene-graph`, or live via the `weston-debug` protocol client)
dumps every layer/view/surface + buffer info on demand, without requiring the client to exit —
this would show directly whether the X11 client's surface has ANY view/layer entry in the scene
graph at all, confirming or refuting this theory in one shot.

**MEMORY PATH FULLY EXONERATED — litebox's shared-memory implementation is CORRECT.** advisor's
same-instant cross-view comparison now covers the actual surface pools (the 245,760-byte buffers
that carry window pixels, not just protocol/cursor-sized objects), and ALL 11 comparisons agree:
```
handle=540 views=[(482082816, 1024), (928317440, 1024)] agree=true
handle=108 views=[(929169408, 1024), (929562624, 1024)] agree=true
```
plus 4096/12288/20480/36864/40960-byte handles, all agree=true. Two genuinely different mappings
of the same surface pool, read at the same instant, contain byte-identical content. **litebox's
cross-process shared memory is correct end-to-end, including for the exact buffers that carry
window pixels — there is no memory-path bug anywhere in this story.** Combined with the earlier
XWM research this fully explains every measurement taken all session: client buffer has content
(measured) -> both processes see identical content (measured, 11/11 agree) -> scanout is exactly
zero (measured) -> weston's own log is clean/happy with the DRM path (measured) -> client never
finishes startup, waits forever (measured, consistent with a surface that's never mapped so its
frame callback never fires). **A surface with a valid buffer but no role, never entered into
weston's scene graph because no XWM ever ran, explains all of it at once and requires zero litebox
code changes.**

**FIX CONFIRMED. ROOT CAUSE WAS OUR LAUNCH CONFIGURATION, NOT LITEBOX. THE BLACKOUT IS GONE.**
advisor-db tested it directly: added `xwayland=true` under `[core]` in the layer's
`/etc/xdg/weston/weston.ini` (`xwayland.so` was already present at
`/usr/lib/libweston-14/xwayland.so`), stopped spawning `Xwayland` manually, let weston start and
manage it itself, and discovered whichever display socket weston actually created (it picked `:0`
on its own — our old hardcoded `:1` was ALSO wrong) instead of hardcoding one. weston's own log
now shows the piece that was always missing:
```
[18:10:38.764] Loading module '/usr/lib/libweston-14/xwayland.so'
[18:10:39.080] Registered plugin API 'weston_xwayland_v3' of size 32
[18:10:39.080] Registered plugin API 'weston_xwayland_surface_v2' of size 24
[18:10:50.253] launching '/usr/bin/Xwayland'
[18:11:05.429] created wm, root 98
```
`created wm, root 98` is the XWM attach that was never happening before. Result:
```
BEFORE (manual Xwayland spawn): 19-23 frames, then px=0 permanently, wipe at t=16-23
NOW (weston-managed Xwayland):  24 frames, run ENDS on px=2,073,597, NO WIPE AT ALL
```
First time all session the framebuffer still has real content at the end of a run. Combined with
the memory-path exoneration above (11/11 cross-view agree=true, including the surface pools),
this is a complete, closed explanation requiring **zero litebox code changes**: spawning Xwayland
as a bare separate process gives an X server with no window manager attached, so X11 client
surfaces get buffers with real pixels written into them but are never mapped into weston's scene
graph, so they never composite.

**NEXT STEP (in progress)**: run the full XFCE stack (not just one test client) this way — take
`advisor/probes/run_xfce_staged.sh`, remove its manual Xwayland launch, set `xwayland=true`, point
all XFCE components (xfconfd/xfwm4/xfsettingsd/xfdesktop/xfce4-panel) at weston's own display
instead of a hardcoded `:1`. Since every XFCE component already launches and stays alive (per
earlier findings in this doc) and the compositing path is now confirmed working, this is expected
to be the run that finally satisfies the standing success oracle (`non_black_pixels > 0` in the
FINAL frames of a full XFCE session, not just one test client).

**Confound found mid-verification, correctly NOT conflated with the XWM fix**: the first
full-stack attempt with `xwayland=true` progressed cleanly (DBUS/SEATD/WESTON/XWAYLAND/XFCONFD/
XCHECK stages, 18 frames at 2,073,597 non-black, no wipe at t=36) but hit `DBUS_UP=no` — the
long-known intermittent trampoline `#UD` (`Exception(6) rip=0x7feffff7fb8a`, same bit-identical
address as every prior occurrence this session) fired again and killed the backgrounded
`dbus-daemon` before it could exec (`dbus-daemon execs: 0`), so `xfconfd`/`xfsettingsd`/
`xfdesktop`/`xfce4-panel` can't start (would fail with "Connection refused" as in every earlier
run). **This is a SEPARATE, already-known-intermittent bug (partially mitigated by b4330590,
still not fully closed) — it does NOT retroactively implicate or exonerate the XWM fix either
way.** A run where an unrelated component fails to start is not a fair test of the compositing
fix in either direction: non-black final frames from such a run wouldn't prove the XWM fix handles
a full desktop, and black final frames wouldn't disprove it either, since half the desktop never
launched. Correct discipline (applied): rerun until a clean `DBUS_UP=yes` run is obtained, and
only then read the final-frame oracle. **The isolated (non-full-stack) result is unaffected by
this confound and stands on its own regardless of how the full-stack run lands**: weston managing
Xwayland itself (`xwayland=true`) ends a run at 2,073,597 non-black pixels with no wipe, vs.
permanent blackout before — reproducible, doesn't touch dbus at all. Anyone hitting `DBUS_UP=no`
in a future full-stack run should treat it as this known trampoline `#UD` flakiness, retry, and
not read anything into the frame result of that specific run.

**FULL-STACK RESULT: COMPOSITING FIX VERIFIED END-TO-END, ORACLE PASSES — with one honest,
correctly-flagged caveat.** advisor-db's full-stack run with weston managing Xwayland:
```
23 frames captured; last four (t=24.40, 24.44, 37.49, 37.53) ALL non_black_pixels=2,073,597
no zero frame anywhere in the run -- the first full-stack run all session that never blacks out
alive at end (t=75.5): seatd, weston, xfwm4, xfdesktop
```
Every previous full-stack run went to zero and stayed there; this one holds real content to the
end. **The compositing/blackout blocker that dominated this entire session is fixed and verified.**

**Caveat, stated precisely and NOT to be glossed over**: `dbus` was lost to the same trampoline
`#UD` again in this run (`DBUS_UP=no`, `Exception(6)` at `rip=0x7feffff7fb8a`, `dbus-daemon`
never execs), so `xfconfd` never started and `xfsettingsd`/`xfce4-panel` never came up. **This is
a PARTIAL desktop — window manager (xfwm4) and desktop (xfdesktop) running and rendering; panel
and settings daemon missing for the unrelated, already-known dbus `#UD` reason.** Do not call this
"XFCE working" until a run has both the compositing fix AND a clean dbus start (all of xfconfd/
xfwm4/xfsettingsd/xfdesktop/xfce4-panel alive) with non-black final frames — that is the actual
remaining bar for the standing goal, now narrowed to exactly one known bug.

**Durable fix, to be landed permanently in the launch scripts** (three fix + two hard-won
operational lessons):
1. `weston.ini` needs `xwayland=true` under `[core]`.
2. Do NOT spawn `Xwayland` manually — let weston launch/manage it.
3. Discover weston's own display rather than hardcoding `:1` (it has chosen `:0` in every run).
4. Capture backgrounded services' stderr AND print/tee it — silent redirects turn fast, clear
   crashes into mysterious multi-minute "timeouts" (see the earlier weston-typo methodology
   finding above).
5. The dbus spawn needs a bounded retry structured as a shell **function called N times**, NOT a
   `while`-loop body backgrounding inside the loop — advisor found that backgrounding from inside
   a `while` loop converts what should be a probabilistic single-child death into a deterministic
   death of the launcher shell itself (bit-identical registers across runs); a function call
   avoids this shape. advisor is testing this retry now; report pending.

**CORRECTION — item 5 above is WRONG, do not implement it. RETRY MAKES THIS WORSE, NOT BETTER.**
advisor tested the bounded dbus retry two ways (while-loop body, and a shell function called
three times) and BOTH kill the launcher shell itself, not just the dbus child:
```
attempt 1 (while-loop retry):        dbus child dies (Exception 6, rip=0x7feffff7fb8a),
                                      then the LAUNCHER SHELL dies at rip=0x7feffff6fb11
attempt 2 (function called 3x):      identical outcome, same two addresses
```
So the trigger is NOT loop-vs-function syntax (that was a reasonable but incorrect earlier
hypothesis) — it's **retrying a backgrounded spawn at all, after one child has already been
lost to this bug, that takes down the shell issuing the retry.** Consequence: **single-spawn is
the only currently-safe pattern.** If dbus is lost to the `#UD`, the correct response is to
**rerun the whole script, not retry in-script** — an in-script retry converts an intermittent
*partial* failure (missing panel/settings this run) into a reliable *total* failure (dead
launcher, nothing comes up at all). **Do not add a dbus retry loop to `run_xfce_staged.sh` or any
other durable script.** The trampoline `#UD` at `rip=0x7feffff7fb8a` (partially mitigated by
b4330590, still not fully closed) is now the single remaining bug standing between this session
and a reproducible complete desktop — worth prioritizing over any further cosmetic script work.

**SELF-CORRECTION (advisor) to the two entries directly above — read this before acting on
either.** The repeated dbus failures across tonight's full-stack runs were advisor's own fault,
not an intermittent litebox bug: the script that added the XWM fix carried over `set -x` from an
older script, and `set -x` is the EXACT trigger this session bisected hours earlier as
deterministically killing the first backgrounded child via the trampoline `#UD` (see the `set -x`
entry under "Useful techniques"/observer-effect notes elsewhere in this doc). Evidence: dbus-daemon
started perfectly in isolation on the current build, 3/3 runs, zero faults; it failed 4/4 inside
the full script that had `set -x`. **Correction to what stands from the two entries above:**
1. "Retrying kills the launcher shell" is still literally true as a measurement (bit-identical
   crash addresses, loop and function form alike) — but the retry was never actually needed. The
   correct fix was removing `set -x`, not working around its consequences. Do not read "retries
   are unsafe" in isolation without this context.
2. "The `#UD` intermittently kills dbus, full-stack verification stays flaky" is **overstated**.
   On the current build, with no shell tracing anywhere in the launch path, dbus is reliable. The
   `#UD` is real (still worth closing eventually) but is **not currently blocking full-stack
   verification** — remove `set -x` and it goes away for this purpose.

**Durable guidance that actually stands**: **never use `set -x` in any of these launch scripts —
use explicit `echo` markers at stage boundaries instead.** `set -x` is easy to reintroduce by
copying an older script (exactly what happened here) — anyone touching a launch script (including
the agent landing the durable fix) must grep for and remove `set -x` from every script they touch.
advisor is rerunning the full stack now with tracing removed — this should finally be a fair,
unconfounded test of the complete desktop; result pending. The compositing fix and its
verification are unaffected either way: both the isolated repro and the earlier (traced, dbus-
broken) full-stack run ended on 2,073,597 non-black pixels, and neither depended on dbus.

**IN PROGRESS, LOOKS LIKE THE FIRST FULLY UNCONFOUNDED RUN — NOT YET CONFIRMED CLEAN, result
pending.** With `set -x` removed AND the XWM fix in place simultaneously (first time both
conditions held at once):
```
DBUS_UP=yes
XFCONF_PROBE_RC=0        -- xfconf-query reached the daemon, settings available
XFCE_DISPLAY=:0          -- weston's own Xwayland, discovered not hardcoded
frames: 19 x 2,073,597, no wipe
running so far: xfconfd, xfwm4, xfsettingsd, xfdesktop
```
Every component that previously failed with "Connection refused" now has a working bus and
settings daemon — that whole failure class appears gone. **Do not treat this as confirmed
success yet**: advisor explicitly flagged two other faults in this same run (not dbus-related,
not yet identified) and will not call the run clean until those are checked. Final frame data and
surviving-component list pending.

**Durable configuration for `run_xfce_staged.sh`, now believed complete (6 items)**:
1. `weston.ini`: `[core] xwayland=true`
2. No manual `Xwayland` launch — weston manages it.
3. Discover the display weston chooses (currently `:0`) rather than hardcoding.
4. **No `set -x` anywhere** in the script or anything it sources — explicit `echo` markers at
   stage boundaries instead. (This one has bitten the project twice now — once in original
   bisection, once when advisor reintroduced it by copying an old script an hour ago — deserves an
   explicit check/grep in whatever lands, not just a comment.)
5. Single dbus spawn, no retry (retry-after-loss kills the launcher shell itself).
6. Capture backgrounded services' stderr AND print/tee it, so failures never present as silent
   timeouts.

## Reproduction commands

**CRITICAL, READ FIRST: the program-path argument after `--` MUST be relative (no leading `/`),
or every run below stack-overflows with no useful error.** Confirmed pre-existing bug (not a
regression, predates all of this session's other work): `load_program(...).unwrap()` in
`litebox_runner_linux_on_windows_userland/src/lib.rs:688` gets `ENOENT` for any leading-slash
program path (`/bin/echo`, `/bin/sh`, ...) and the panic then **overflows the runner's own stack**,
so the real ENOENT error is invisible — you just see `thread '<unknown>' has overflowed its
stack`, which gives zero clue what's actually wrong. The SAME path with the leading slash dropped
(`bin/echo`, `bin/sh`) works fine — confirmed on both a release build and an independently-built
debug binary from earlier in this session, so it is not build-state-specific. **Every reproduction
command below has been corrected to the working relative form** — do not add a leading slash back
in. Real fix, not yet landed: normalize a leading slash in `load_program`'s path handling, and
return the error there instead of `.unwrap()`ing it (so a future occurrence of this class of bug
fails loud instead of as an unrelated-looking stack overflow).

Full XFCE launch — **use `advisor/probes/run_xfce_xwm.sh` as the launch script** (the proven
working launcher, commit `4e6fc556`; do NOT use any `set -x`-instrumented script — it will trigger
the still-open #UD bug above and derail the run before it ever reaches the rendering blocker):
```
cd C:\dev\litebox-main
cargo build --release -p litebox_runner_linux_on_windows_userland --target x86_64-pc-windows-gnu
export LITEBOX_LOG=error
export MSYS2_ARG_CONV_EXCL="*"
export LITEBOX_DUMP_FRAMES=1
timeout 100 target/x86_64-pc-windows-gnu/release/litebox_runner_linux_on_windows_userland.exe \
  --initial-files .wfgy/xfce-build/layer31_direct_fixed.tar \
  --gui \
  -- bin/sh advisor/probes/run_xfce_xwm.sh \
  > /tmp/pass_repro.log 2>&1
```
(`layer31_direct_fixed.tar` = `alpine-pinned2.tar` + `layer31_direct.tar` merged, soname-repaired.
If missing, rebuild: extract both into one directory, run
`python advisor/probes/fix_layer_sonames.py <dir>`, then `tar cf layer31_direct_fixed.tar -C <dir> .`)

Bare-rootfs fork-bug repro (fast, no display stack): see the regression oracle above.

## Useful techniques discovered this session

- **CRITICAL, applies to every run this file recommends: the Bash tool caps a command at roughly
  two minutes EVEN WITH `run_in_background`.** Any litebox run needing longer (all full XFCE
  launches) can get silently cut off mid-run — confirmed: two attempts truncated at t=18 and t=21
  with ZERO faults and no final marker, which is indistinguishable at a glance from a genuine
  hang/failure. `timeout N` INSIDE the command does not help — the cap is on the tool invocation,
  not the command. **Diagnostic rule: a run that stops mid-log with zero faults and missing stage
  markers was CUT OFF, not broken — check the last log timestamp against the script's expected
  duration before investigating it as a bug.** **The form that actually survives**: a fully
  detached subshell —
  ```
  (LITEBOX_LOG=error LITEBOX_DUMP_FRAMES=1 ./target/release/litebox_runner_...exe \
      --initial-files <tar> bin/sh ./script.sh > /path/run.log 2>&1 &) ; sleep 5
  ```
  — then poll the log file for an end marker separately. The parenthesized subshell with the
  trailing `&` survives the tool's timeout; a bare `run_in_background` call on the runner directly
  does not. **Retroactive concern**: several of this session's "the component never got there"
  observations may have this as their actual cause rather than a real stall — treat any earlier
  finding that ended abruptly with no explicit end-of-run marker as suspect until re-verified with
  the detached-subshell form.
- **A stuck/hung litebox run holds the runner binary's exe file open, and `cargo build` then
  fails with "Access is denied" on that exe.** This looks like a toolchain problem but isn't —
  find and kill the leftover `litebox_runner*` process (Task Manager or
  `Get-Process litebox_runner* | Stop-Process -Force`) and the build unblocks immediately. Check
  for this FIRST before investigating any other cause of a build failing only with a Windows
  file-locking error.
- **Guest stdout is interleaved character-wise with litebox's own log lines** in the runner's
  captured output, making a crashing guest program's own diagnostic messages unreadable via
  normal line-based grep. Recovery: `sed 's/\x1b\[[0-9;]*m//g' run.log | tr -d '\n' | grep -oE
  ".{N}PATTERN.{M}"`. A proper fix (`LITEBOX_GUEST_STDOUT_FILE`, routing guest pty output to its
  own file) exists but currently only activates under `--pty-mode`, which itself has a bug — it
  breaks the top-level shell with SIGPIPE in a non-interactive harness context. Worth fixing
  properly if picked up again; in the meantime, redirect each guest process's own stderr to a
  file inside the launch script (`cmd > /tmp/cmd.log 2>&1 &`) and `cat` it — this is what
  actually recovered real error messages (e.g. "xfsettingsd: Could not connect: Connection
  refused") this session.
- **`Exception(N)`/`error_code` decoding**: `cr2=0x0` and `error_code=0x0` together rule out a
  page fault (both are always set for `#PF`) — that combination means `#UD` (invalid opcode), an
  instruction that decoded to invalid bytes, not a jump to unmapped memory. `rip==cr2` with a
  nonzero `error_code` whose low bit is set (e.g. `0x6`) means an instruction-FETCH fault on a
  not-present page — a genuine missing/unbacked mapping.
- **A stale non-socket file at a well-known path blocks the real daemon from starting.** A
  resumed writable layer can leave e.g. `/run/seatd.sock` as a regular file instead of the real
  socket from a prior run, causing `seatd -l debug &` to silently refuse to start
  ("Non-socket file found at socket path... refusing to start"). Always `rm -f` well-known socket
  paths at the top of a launch script when resuming from a prior writable layer.
- **A missing `--resume-from`/`--initial-files` file panics the runner with a stack overflow**
  instead of a clean error (`lib.rs:338` and `lib.rs:679`, both `.unwrap()` on a file-open
  result) — if you see "thread 'main' has overflowed its stack" right after an "os error 2" (file
  not found) message, the actual bug is your invocation, not litebox itself.
- **FIXED (this session, "client startup" investigation): the `load_program(...).unwrap()` on the
  spawned initial-guest-thread (`litebox_runner_linux_on_windows_userland/src/lib.rs`, was line
  688) no longer panics-then-stack-overflows on a bad program path.** Root-caused precisely: this
  is NOT a leading-slash issue in litebox's own path handling (leading `/` is litebox's correct
  internal convention, confirmed by reading `import_writable_layer`'s
  `alloc::format!("/{header_path}")`) — it was **Git-Bash/MSYS2 silently rewriting a bare
  `/bin/sh`-style argument into a Windows path before the runner ever saw it**, which then
  legitimately got `ENOENT` from `load_program`, and panicking on that specific spawned thread
  overflows the stack during unwind (a real, separate, now also-fixed issue below). Confirmed via
  `MSYS2_ARG_CONV_EXCL="*"`: with that set, a plain `/bin/sh` argument reaches the runner correctly
  and works fine — **always set `MSYS2_ARG_CONV_EXCL="*"` before invoking the runner from Git
  Bash/MSYS2**, or the exact same false "litebox path bug" will reappear. Independently also fixed
  the panic-shape itself: the spawned thread's `load_program(...).unwrap()` now matches on the
  `Result` and does `eprintln!` + `std::process::exit(1)` instead of unwinding, so any FUTURE
  genuine `load_program` failure (wrong path, corrupted binary, whatever) fails with a clean,
  readable one-line error instead of the opaque "thread '\<unknown\>' has overflowed its stack".
  Verified directly: `-- /bin/does_not_exist` now prints
  `failed to load program "/bin/does_not_exist": OpenError(Errno(2 = ENOENT: ...))` and exits 1,
  no stack overflow.

## Host memory hygiene — check before trusting any run's result

When multiple sessions/agents run heavy full-XFCE launches concurrently, host free memory can
drop low enough (confirmed: ~5 GiB free out of 16 GiB, one run outright died mid-launch with
`memory allocation of 1342177280 bytes failed` at t=11.8s, well before the run's real content —
e.g. the scanout blackout at t~19-20s — was ever reached) to silently corrupt oracle results.

**The dangerous failure mode**: a run that dies early looks like "no bug occurred" to any oracle
that only checks final frames or exit status — a FALSE PASS. A run genuinely ending on non-black
frames because it crashed at t=8s, before ever reaching a bug that only manifests at t=19s, is
indistinguishable from a real fix without checking the run actually completed its full intended
duration.

**Before trusting any launch-run result** (a regression-oracle count, a "the bug is fixed" claim,
a clean/passing frame capture): check host memory first
(`powershell -Command "Get-CimInstance Win32_OperatingSystem | Select-Object
FreePhysicalMemory,TotalVisibleMemorySize"`), and verify the run's own log shows it actually ran
for its full intended duration rather than dying early (check for the expected final log lines —
`TEST_DONE`, the expected number of frames, no unexpected `thread panicked`/allocation-failure
lines). **Prefer serializing heavy full-XFCE-launch runs across concurrent sessions rather than
running several in parallel** — wall-clock is the thing being optimized, and a false result from
memory pressure costs more total time than the parallelism saves.

## Disk hygiene — read before generating any new layer tar or crash dump

`.wfgy/xfce-build/` and `.wfgy/crash-dumps/` accumulated to **73 GiB** and **19 GiB**
respectively by 2026-09-03, mostly near-duplicate incremental layer tars from iterative
debugging (`layer77-idletest7.tar` through `layer84-wterm.tar` alone: ~5.5 GiB of snapshots that
were never cleaned up) and old crash `.dmp` files (~3.8 GiB each) from a session whose findings
were already fully captured as text in `AGENTS.md`/project memory. Total repo directory size hit
85+ GiB before cleanup. Cleaned up same day: `.wfgy/xfce-build/` now holds only the 4 tars
actually referenced by this file's "Reproduction commands" section
(`alpine-pinned2.tar`, `xfce-layer31-nopanel.tar`, `layer31_direct.tar`,
`layer31_direct_fixed.tar`, ~4.2 GiB total); `.wfgy/crash-dumps/` and `.wfgy/gdb-session/` were
deleted entirely (their findings are already written up as text — the dumps themselves added no
further value once analyzed).

**To prevent this recurring:**
- **A layer tar you build for one debugging iteration is disposable once you've extracted what
  you needed from the run.** Do not accumulate `layerNN-<description>.tar` snapshots — if you
  need to preserve a specific known-good state, name it something durable (e.g.
  `layer31_direct_fixed.tar`, matching what's actually referenced in this file) and overwrite it
  in place rather than incrementing a number and keeping every prior version.
- **A `.dmp` crash dump is disposable once you've extracted the fault address/module/stack you
  needed via `VirtualQuery`/`objdump`/gdb and written the finding into `AGENTS.md` or project
  memory as text.** Delete it after use — a full-process minidump is typically 3-4 GiB on this
  project, and the actual signal you need from it is a few lines of text.
- **Only tars actually referenced in this file's "Reproduction commands" section (or an active,
  in-progress investigation) belong in `.wfgy/xfce-build/`.** Before adding a new one, check
  whether an existing tar can be reused/overwritten instead of creating another numbered variant.
- **Periodically (or before ending a long debugging session), run `du -h --max-depth=1 .wfgy`**
  and clean up anything not currently referenced — this is now a known failure mode for this
  project specifically, not a one-off.
- **Second recurrence, different location: a per-session scratchpad grew to 115 GiB** by copying a
  full 2.4-3.5 GiB layer tar for every A/B/probe variant (~40 variants × ~3 GiB) instead of reusing
  one working tar and swapping only the small script inside it. Drove the whole `C:` drive to 1.5
  GiB free mid-investigation, crashing an unrelated concurrent process ("not enough space on
  disk"). Cleaned to 3.5 GiB, freeing 127 GiB total. **Same underlying mistake as the
  `.wfgy/xfce-build/` incident above, just in a different directory — the general rule applies
  everywhere, not just `.wfgy`: never copy a multi-gigabyte tar per iteration; build one layer and
  swap only what changed, or write variants into a single tar.**
- **`rm -rf` on a directory another process (a running litebox instance, another session) has
  open fails silently with "Device or resource busy" on Windows, and a subsequent `mv <src>
  <same-name>` then lands INSIDE the still-existing target instead of replacing it** (confirmed
  live: a cleanup pass this session moved 4 essential tars into
  `.wfgy/xfce-build/xfce-build-keep/*.tar` instead of `.wfgy/xfce-build/*.tar`, one level deeper
  than intended, when the original `xfce-build/` directory couldn't be removed because a
  concurrent litebox process still had it open). Nothing was lost, but a peer session relying on
  the expected path got a false "file is gone" read. **After any `rm -rf`/`mv` cleanup pass on a
  shared directory, verify with `ls`/`find` that the result actually landed where you expect** —
  don't assume a `mv` to a name that used to be occupied succeeded as a plain rename.

## Pass 340 — "client startup is slow" ROOT-CAUSED AND FIXED: it was never litebox at all

**Assigned task**: priority-2 follow-on, "client startup under litebox is extremely slow,
unexplained" (`xfce4-about --version` never exited in an 85s run; `xfce4-appfinder` never finished
in 98s). Used the existing `LITEBOX_DIAG_WAIT_DUR=1` instrumentation and built a minimal timed
repro (`advisor/probes/startup_timing_repro.sh`, new, committed) that stamps `date +%s.%N` at every
launch-script stage boundary (dbus/seatd/weston/Xwayland-socket/xfconfd/client) instead of relying
on frame captures or full-desktop launches.

**FOUND, CONFIRMED, AND FIXED: the layer tar's baked-in `weston.ini` never actually had the
`xwayland=true` fix that this file's pass 322/328 (2026-09-03, earlier the same day) describe as
landed and verified.** Direct extraction proved it:
```
$ tar xf .wfgy/xfce-build/layer31_direct_fixed.tar -O ./etc/xdg/weston/weston.ini
[core]
shell=desktop-shell.so
[shell]
...
```
No `xwayland=true` line anywhere. `run_xfce_xwm.sh` itself never sets it either (relies entirely on
the layer's own config) — so despite the earlier passes' clean measurements, **the actual
`layer31_direct_fixed.tar` on disk regressed back to the pre-fix config at some point** (most
likely overwritten by a later `--export-writable-layer` snapshot from a run that didn't have the
ini fix applied, given how many sessions have written to this same filename per the disk-hygiene
section above). Direct consequence, confirmed by isolating just the seatd+weston+Xwayland-wait
stages with `--logger-scopes=log,xwm,xwayland`: weston's own log shows **no
`Loading module '.../xwayland.so'` line at all**, and the launch script's own X11-socket poll loop
(`while [ "$i" -lt 200 ]; do ... sleep 0.2; done`, a 40-second budget) runs to its FULL bound with
`disp=` staying empty the entire time. **This is the "extremely slow, unexplained" client startup**
— not a litebox lock, not a missed wakeup, not thousands of small syscall overheads: a fixed 40s
(or up to ~56s measured, depending on which script's own poll bounds are summed) shell-level
timeout burning down while waiting for an X11 socket that will never appear, because Xwayland was
never told to start. When `xfce4-about --version` then runs against an empty `DISPLAY=""`, it hits
a **separate, already-extensively-documented (30+ archived passes), still-unresolved crash class**
(`[diag-unrecov-av] ... addr=0x2 ... is_in_guest=false`, a near-null host-side fault with no
exception-table entry — see `docs/AGENTS_ARCHIVE_2026-09-03.md`'s 20th/21st/26th passes) rather
than exiting cleanly — this crash is NOT new, was not investigated further here (already has 30+
passes of prior investigation with no root cause found; not this session's job to re-open), and is
a completely separate bug from the weston.ini regression.

**Fix applied and verified**: rebuilt `.wfgy/xfce-build/layer31_direct_fixed.tar`'s
`etc/xdg/weston/weston.ini` with `xwayland=true` restored under `[core]` (old broken tar kept as
`.wfgy/xfce-build/layer31_direct_fixed.tar.bak_no_xwayland_fix` for reference, not committed —
`.wfgy/` is gitignored). Direct before/after measurement, isolated repro (seatd+weston+Xwayland
socket wait only, no full desktop):
```
WITHOUT xwayland=true:  XWAYLAND_SOCKET_UP disp=            iters=200  (full 40s timeout burned)
WITH xwayland=true:     XWAYLAND_SOCKET_UP disp=:0 iters=0  (~1s, first poll succeeds)
weston's own log, WITH the fix: 18:42:10.538 (weston starts) -> 18:42:10.923 ("xserver listening
  on display :0") -- under 400ms for the entire seatd+DRM+Xwayland-launch sequence
```
Full end-to-end repro through an actual `xfce4-about --version` call, WITH the fix:
```
STAGE_DBUS_START -> CLIENT_DONE rc=0 in 17 seconds total wall time (was: never finished in 85-98s)
xfce4-about prints its version banner and exits cleanly, rc=0 -- no hang, no crash
```
Final full-desktop verification against the fixed tar via the documented repro command
(`advisor/probes/run_xfce_xwm.sh`, `--gui`, `LITEBOX_DUMP_FRAMES=1`): `DBUS_UP=yes`,
`SEATD_READY=1`, `WESTON_READY=1`, `XFCE_DISPLAY=:0`, `XWAYLAND_READY=0` (ready on the very first
poll), `XFCONF_PROBE_RC=0`, **`XCHECK_RC=0`** (the embedded `xfce4-about --version` probe — the
exact command this whole investigation was assigned to explain — now exits 0 immediately, not
"never exits in 85-98s"), all six components reach their `_WAITED` stage, `TEST_DONE` reached.

**Answering the direct question this pass was asked to settle**: with the fix, startup
**completes**, does not hang/stall indefinitely, and does not take 60-98 seconds for a trivial
client — it was never a real per-component slowness at all. The ~60s figures measured in earlier
passes were shell polling-loop timeout budgets being fully consumed while waiting on a socket that
could never appear; they say nothing about litebox's own syscall-emulation performance, and nothing
about whether XFCE draws correctly once it does start (that remains pass 339's open question, now
on solid footing since components genuinely do come up promptly).

**Also fixed, found investigating this (both in
`litebox_platform_windows_userland`/`litebox_runner_linux_on_windows_userland`, tracked-source
commits)**:
1. **`import_writable_layer` (`--resume-from` archive import) panicked with `PathError(
   MissingComponent)` on any archive entry whose parent directories weren't themselves present as
   separate tar members** (e.g. one built by appending individual files rather than a full
   directory-recursive `tar -c` — confirmed reproducible on `advisor/probes/run_xfce_staged.sh`'s
   own entry in `layer31_direct_fixed.tar`). Fixed by creating parent directories on the fly before
   `fs.open`, mirroring the existing `Directory` arm's `AlreadyExists`-tolerant `mkdir`.
2. **The spawned initial-guest-thread's `load_program(...).unwrap()` panicked-then-stack-
   overflowed on any real `load_program` failure** (confirmed: a genuinely missing binary path),
   producing an opaque "thread '\<unknown\>' has overflowed its stack" with zero indication of the
   real underlying error. Changed to a `match` that prints the real error and `std::process::exit
   (1)` cleanly instead of unwinding — verified: `-- /bin/does_not_exist` now prints
   `failed to load program "/bin/does_not_exist": OpenError(Errno(2 = ENOENT: ...))` and exits 1.
3. **~11,000 unconditional `error!`-level log lines fired in the first 8 seconds of a single guest
   run** from `litebox_platform_windows_userland/src/lib.rs`'s hot memory-management path
   (`VirtualAlloc2`/`VirtualProtect`/`VirtualFree`/shared-memory create/map/close) — these calls
   had NO env-var gate at all (unlike every other diagnostic in this file, including the existing
   `LITEBOX_DIAG_WAIT_DUR`), fired regardless of `LITEBOX_LOG` level (they're `error!` calls), and
   were pure overhead: a full desktop launch performs tens of thousands of such operations. Not the
   root cause of the 60s startup symptom (that was the weston.ini regression, above), but a real,
   previously-unidentified source of avoidable per-syscall overhead and log-volume noise that made
   this investigation itself much harder (11,405 lines to read through for 8 seconds of guest
   time). Gated all of them behind a new `LITEBOX_DIAG_MM=1` env var (default OFF, same
   thread-local-cached pattern as `diag_wait_dur_enabled`) — `nonzero_in_sample`/
   `diag-shm-crossview` (the compositing-bug diagnostics pass 322 relied on) are preserved under
   the same flag for any future investigation that needs them, just no longer always-on.

**Files changed**: `litebox_platform_windows_userland/src/lib.rs` (import-layer parent-dir fix,
`LITEBOX_DIAG_MM` gating), `litebox_runner_linux_on_windows_userland/src/lib.rs` (clean
`load_program` error path), `advisor/probes/startup_timing_repro.sh` (new, the minimal timed
repro used throughout this pass), `.wfgy/xfce-build/layer31_direct_fixed.tar` (weston.ini
`xwayland=true` restored — gitignored, not part of the commit, but the canonical filename other
scripts/sessions reference, fixed in place per this file's own disk-hygiene convention; the
pre-fix tar is kept as `.tar.bak_no_xwayland_fix` alongside it for reference).
