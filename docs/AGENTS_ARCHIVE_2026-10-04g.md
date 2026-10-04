# moved out of AGENTS.md on 2026-10-04g (byte ceiling 29,500)

Mechanism prose and the verbatim superseded clauses trimmed from `AGENTS.md` this pass. `AGENTS.md`
keeps the sha, the RULE and the numbers; what follows is the how, kept so it can be recovered.

## The chrD79-chrD85 table -- why "the shm carry kills selkies" is FALSE (full text)

The old Open 1 was read as "selkies' loopback listener dies after t=30 and the shm carry `a629714`
is the suspect". Five runs close it. The variable that tracked the death was HOST FREE RAM /
concurrent host `chrome.exe`, not the carry:

| run | carry | host browser | host state | selkies 127.0.0.1:8081 |
|---|---|---|---|---|
| chrD79 | ON | attached | starved -- run killed outright by critical host low memory | `code=200` at t=30, stdout closes right after client connect, refused from next probe on, `guest_pids` -1 |
| chrD83 | ON | attached | 16 live `chrome.exe` on the host | same death |
| chrD84 | FORCE-FAILED (`LITEBOX_DIAG_FORK_SHARED_FORCE_FAIL=1`) | attached | clean | alive -- DPI change applied, `1354x832 EncFPS: 13-14`, host `blue=42.2%/root=54.1%/grey=0/white=0` |
| chrD85 | ON | attached | clean | alive -- `code=200` t=30/60/90, zero `Exception(` |
| chrD82 | ON | none | clean | `code=200` at t=60/90/120/150/180, never fails |

The deciding cell (carry ON, browser attached, CLEAN host) is chrD85 and it PASSED. chrD84 shows
the carry is not even necessary for the desktop frame: the byte-copy fallback also renders. So the
carry is neither necessary nor sufficient for the death; host memory is.

chrD79's log had no watchdog kill and no traceback -- only the benign
`rebuilding a carried SCM_RIGHTS fd failed errno=ENOENT`, which is why the death looked like a
litebox fork bug for a whole pass.

## The client-connect DPI fork (full text)

Nothing forks in selkies until a browser client connects. At that moment:

```
INFO:ws:DPI changed from 96 to 120
```
-> `display_utils.py:1858` `_run_xfconf`
-> `await subprocess.create_subprocess_exec("xfconf-query", "-c", "xsettings",
   "-p", "/Xft/DPI", "-s", "120", "--create", "-t", "int")`
-> blocks in `_communicate_or_kill`.

Source: `/lsiopy/lib/python3.13/site-packages/selkies/display_utils.py`. selkies runs under
`/lsiopy`, NOT the system `python3` -- grepping the system site-packages finds nothing.

Under host memory pressure selkies DIES inside that await. The last line it manages to log is
always:

```
WARNING:display:Could not obtain XFCE session environment. Falling back to direct execution.
```

That fallback is permanent, not a one-off: selkies' `_pids_of("xfce4-session")` enumerates
`/proc/<pid>/cmdline`, which is EMPTY for every pid in litebox (see Open 3), so it can never find
the session and always takes the direct-execution path. The WARNING is therefore NOT evidence of a
fresh failure -- it is the last line of a healthy run too. What makes it diagnostic is that it is
the LAST line before the process vanishes with no traceback and no kill in the log.

Probes: `.wfgy/selkdpi2.sh`, `.wfgy/selkdpi3.sh`.

## chrD83 -- `Exception(14) error_code=0x15` (full registers)

Two occurrences in `chromium`, `pid=311 tid=311`:

```
Exception(14) rip=0x56120e3fe8 cr2=0x56120e3fe8 rax=0x56120e3fe8 rcx=0x56120e3fe8
error_code=0x15
```
at guest t=0.336s and t=38.199s. `error_code=0x15` = user-mode (bit 2 clear for ring 3 -> 0x4
absent, so 0x15 == 0x14|0x1) INSTRUCTION FETCH, protection violation: the fetch target is a
non-executable page. `rip == cr2 == rax == rcx` -- the same address is being jumped to, read and
faulted on, i.e. a call through a pointer that is not code.

**Distinguish it from the classic `Exception(14) error_code=0x7`** (write to a user page with no
mapping), which is the relocating-fallthrough fork signature. Different error_code, different
address pattern, different site. chrD79 and chrD85 -- the two runs on the same combined tree with a
clean host -- have ZERO of either. chrD83 ran in the starved-host window alongside 16 host
`chrome.exe`, so it is unproven as a litebox defect until reproduced on a clean host.

## Two probes that DISPROVE two plausible mechanisms (record so nobody re-derives them)

- `.wfgy/inetfork2.sh` 8/8 PASS. A fork child that inherits a carried INET fd and then EXITS, EXECS
  or CLOSES it does NOT destroy the parent's listener or an accepted connection. This kills the
  hypothesis that the `borrowed` contract (carried connection/bound UDP is borrowed: no smoltcp
  close/abort/port free) leaks a teardown back into the parent. The three exit paths all hold.
- `.wfgy/shmexec1.sh`. A parent holding an anonymous `MAP_SHARED` mapping survives
  `os.fork()+execv`, `subprocess.run`, `os.posix_spawn`, the real `xfconf-query`, and the exact
  `asyncio.create_subprocess_exec` path -- mapping intact, listener alive. This kills the
  hypothesis that an exec-after-fork (which every selkies `create_subprocess_exec` does) corrupts a
  carried shared mapping.
  **The probe's own "CORRUPT" verdict is BACKWARDS**: after a child writes to the shared mapping the
  parent SHOULD see the child's bytes. That is sharing working. Do not "fix" it.

## chrS6 -- the headless gate is INCONCLUSIVE, not failed

chrS6 ran the headless baseline on the combined tree and was truncated by the 300s harness cap
mid-arm: `HEADLESS_SHOT_RC=124` with no PNG. `124` is `timeout(1)`'s own "timed out" -- and
`timeout(1)` HANGS in-guest (harness lessons), so an arm that reaches its `timeout` never returns
and the cap kills the run. Nothing about the compositor was measured. Re-run with
`.wfgy/guest2.ps1 -Secs 600` before any verdict; the arms print `raf ok` vs `RAF_NEVER_FIRED`.

## Superseded clause: "branch state / land gate" (verbatim from 2026-10-04f)

> **Branch state: `main` is deliberately still `0cfe406`; the whole set (inet carry + shm carry)
> sits on `inetfix` (`4792fc5`).** Held back ON PURPOSE: landing the shm carry and then finding
> selkies broken means reverting again -- the churn `f2d67f8` already cost. **Land only after Open
> 1's cell is measured.**

VOID as of `c39a050`: `main` is fast-forwarded to `c39a050`, identical to `inetfix`. The cell that
gated the landing (carry ON + host browser attached + clean host) is chrD85 and it PASSED.

## Superseded clause: "You CANNOT cherry-pick e41cafa9+bfc33f7 straight onto main" (verbatim)

> **You CANNOT cherry-pick `e41cafa9`+`bfc33f7` straight onto main.** `git merge-base main shmfork2`
> = `f2d67f8`, an ancestor of main that `112cae4` REVERTED -- the diffs were written against code
> main no longer has, so the pick leaves `litebox/src/mm/linux.rs`, `mm/mod.rs`,
> `litebox/src/platform/page_mgmt.rs` reverted while the carry code calling into them returns (MIXED
> TREE). Correct order: `git revert -Xignore-all-space 112cae4` FIRST, then the two picks (naive dry
> run: 28 conflict hunks in `platform/lib.rs`).

Kept for history only -- with `main` == `inetfix` == `c39a050` there is nothing left to pick. The
trap itself (a pick whose base was later reverted produces a silently MIXED TREE) is still true of
any future pick off these branches.

## `chrdesk52.sh:69` -- the PIPESTATUS trap (verbatim)

```sh
( selkies --debug 2>&1 | sed 's/^/[selk] /' ) &
...
echo "[s] SELKIES_EXIT rc=$?"
```

`$?` after a pipeline is the status of the LAST command -- here `sed`, which exits 0 whenever it is
not killed. Every `SELKIES_EXIT rc=0` reported by chrD79/80/82/83 therefore proved only that
selkies' STDOUT CLOSED, which is exactly what a dead selkies and a healthy selkies both do.
`chrdesk53.sh` replaces it with `${PIPESTATUS[0]}` (plus a wall-clock stamp, so a death can be
correlated against host `Available MBytes`).
