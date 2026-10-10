# FD carry degrade: multi-process chromium dumps the DOM (2026-10-10 21:20)

Config (`/tmp/lbx/probe-uz8.sh`): `/usr/lib/chromium/chromium --headless=new --disable-gpu
--no-first-run --disable-dev-shm-usage --disable-breakpad --disable-crash-reporter
--disable-extensions --no-default-browser-check --disable-features=Vulkan
--user-data-dir=/tmp/udd-$TAG --enable-logging=stderr --window-size=800,600 --hide-scrollbars
--virtual-time-budget=8000 --dump-dom file:///page.html`. **No `--no-sandbox`, no
`--single-process`.** Runner = private copy `/tmp/lbx/lbxrun`, `nice -n 19`, `timeout -s KILL 150`,
one arm at a time under `/tmp/lbx/.runner-lock`, `pkill -KILL -f '/tmp/lbx/lbxrun'` before and after.

## The change — `3291ce69`, switch `LITEBOX_FD_CARRY_DEGRADE_OFF` (ON = unset, OFF = `=1`)

`file.rs` `degraded_carried_file`, reached from `rebuild_carried_fd`'s `F` arm. Linux never fails to
install a donated fd; litebox rebuilt one by reopening a **path**, and a path valid in the donor's
view can name nothing in the receiver's (renderers are chrooted by layer-1). One unopenable fd made
the whole `sendmsg(SCM_RIGHTS)` fail -> `MSG_CTRUNC` -> Mojo folds that into a 0-length read ->
`TerminateSelfOnDisconnect` -> `exit_group(0)`, silently. Installing a private empty file in that
slot is strictly better than losing the rest of the message.

## BOTH WAYS — identical binary `md5=1221a7b51002407446c3cd1900229c44`, built 21:01:13

| arm | switch | rebuild failures | degraded installs | late `exit_group(0)` | late `exit_group(241)` | rc | `dump_dom` | guest pids | log bytes |
|---|---|---|---|---|---|---|---|---|---|
| uz12 | degrade ON (unset) | **0** | 2 | 2 (95, 50) | 3 (75, 66, 77) | 124 | **1** | 27 | 892,167 |
| uz13 | degrade OFF (`=1`) | **2** | 0 | 2 (71, 65) | 0 | 124 | **0** | 30 | 1,929,660 |

Renderers do install their seccomp filter in both arms (`diag-seccomp` tids 66/75 mode=2 at
15.39/15.45 s in uz12); the DOM is dumped at 19.67 s, i.e. after the renderers are up. Reproduced:
uz14 and uz15 (below) both degrade-ON and both `dump_dom=1` — **3 of 3 degrade-ON arms dumped the
DOM, 0 of 1 degrade-OFF arms did**.

## The remaining failure is a view gap, not a packaging gap — `0faa4bc1`

`diag-procary` now opens the carried path's PARENT and reports its errno beside the leaf's. Both
arms: leaf `/usr/share/fonts/truetype/freefont/FreeSerif.ttf` **ENOENT** and parent
`/usr/share/fonts/truetype/freefont` **ENOENT** — the tree is absent in the receiver, not
unresolvable. The file IS in the archive (3,686,360 B). Same class as `/proc/<N>/statm`
(`a5e8e289`): a name that is only valid in the donor's view. The general fix is the byte snapshot
(`T|`); the degrade is the fallback where a snapshot is not available.

## Cross-process AF_UNIX wake (`10fa53cd`) — `LITEBOX_UNIX_CROSS_WAKE_OFF`, measured, UNCHANGED

Same binary, adjacent arms, gate passed for both (load 2.41 / 2.45, MemAvailable 6.07 / 5.64 GB).

| arm | switch | rebuild failures | degraded installs | rc | `dump_dom` | guest pids | log bytes |
|---|---|---|---|---|---|---|---|
| uz14 | wake ON (unset) | 0 | 2 | 124 | 1 | 26 | 984,465 |
| uz15 | wake OFF (`=1`) | 0 | 2 | 124 | 1 | 27 | 849,077 |

**Identical, and that is the expected result right now, not a refutation.** The wake does not reach
the poll path: `epoll.rs:405-418` is a bounded 15 ms repoll that never consults the wake word, and
`has_unready_stdin_or_armed_timerfd_interest` returns `true` for every `EpollDescriptor::Unix`
(`:458`). Chromium `epoll_wait`s before it `recvmsg`s, so with the fix ON it still pays the ~15.9 ms
repoll. Recorded as **"unchanged, poll path not yet converted"**; re-run this pair after the poll
path is converted.

## What is left on this track

`rc=124` in every arm: the DOM is dumped at ~19-21 s and the browser then **never exits** — it hangs
for the remaining ~130 s in the main-thread spin (AGENTS.md: `ppoll` fd 11 + fd 13, `ready_count=0`).
Getting chromium to *render* multi-process without `--no-sandbox` is done; getting it to *exit* is a
separate defect.
