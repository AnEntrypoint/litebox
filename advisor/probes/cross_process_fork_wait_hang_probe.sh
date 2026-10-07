#!/bin/sh
# Minimal repro for a parent hang after TWO back-to-back cross-process `fork()`s
# (`LITEBOX_PROCESS_FORK=1`), discovered 2026-09-10 while chasing why the real
# `webtop_stack.sh` boot (nginx's SSL cert supervisor loop) stalls forever after the
# VirtualQuery-caching fork speedup (commit ce5648f) let it run far enough to reach that point
# for the first time.
#
# Host side:
#   LITEBOX_PROCESS_FORK=1 LITEBOX_LOG="warn,litebox_shim_linux::syscalls::process=debug" \
#     litebox_runner_linux_on_windows_userland -Z --oci-image docker.io/linuxserver/webtop:debian-xfce \
#     --resume-from <tar containing this script at /config/...> /bin/sh /config/<this script>
#   (MSYS_NO_PATHCONV=1 needed on Git Bash so `/bin/sh` is not rewritten to a host path.)
#
# Observed (30+ runs, deterministic): both backgrounded subshells run to completion --
# A_ATTEMPT=1..3/A_DONE (3 sequential `openssl req -x509` invocations, all rc=0, cert created
# every time) and B_ATTEMPT=1..5/B_DONE print correctly -- but `wait` (line ~30, waiting for
# BOTH backgrounded subshells) never returns and CONC_DONE never prints. The parent thread
# (guest tid=1) is provably not killed and not looping hot: `Get-Process`/`$p.Threads` on the
# surviving Windows process shows near-zero but nonzero CPU growth (consistent with a blocked
# wait, not a spin), and `LITEBOX_LOG=...,litebox_shim_linux::syscalls::process=debug` shows
# tid=1's LAST syscall ever is a single non-blocking `sys_wait4(pid=-1, options=WNOHANG)`
# (returns `Ok(0)`, correct -- neither child has exited yet) issued immediately after the
# SECOND `clone: try_cross_process_fork` call. No further syscall of ANY kind from tid=1 is
# ever logged again, even minutes later and even though both children log their own clean
# exit (`DIAG_TIMELINE exit_group status=0`) seconds afterward. This means the hang is not
# inside `wait_for_cross_process_exit` (never reached -- that requires a SECOND `sys_wait4`,
# which never happens) and not a missing `cross_process_children` case in `sys_waitid`/
# `sys_wait4` (also never reached) -- the parent's OWN GUEST CODE stops making syscalls
# entirely right after issuing two cross-process forks back-to-back from one thread.
#
# This is the same shape the advisor has chased for many sessions under ADVISORY-001 (section
# 3H's MAXCONCURRENT>=2 correlation, "trampoline-rw-window-race", the glibc tcache/safe-linking
# corruption class in 3N) -- but this script reproduces it in under 4 seconds with no XFCE, no
# Xvfb, no dbus, just two trivial backgrounded subshells plus `wait`. Worth retiring the
# heavier repros in favour of this one for any future pass at the concurrent-fork corruption
# class: rebuild litebox_platform_windows_userland/fork_verify's relocation/healing bookkeeping
# with `LITEBOX_VEH_TRACE=1`/`LITEBOX_DIAG_FATALDUMP=1` against THIS script first.
#
# Reading the result:
#   CONC_DONE prints          -> fixed (or not reproduced this run -- re-run a few times).
#   A_DONE and B_DONE print, CONC_DONE never does, process lingers at ~0% CPU
#                              -> reproduced. Confirm via LITEBOX_LOG debug on
#                                 litebox_shim_linux::syscalls::process that tid=1 issues no
#                                 syscall after its one post-fork WNOHANG poll.

export PATH=/lsiopy/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

(
  n=0
  while [ $n -lt 3 ]; do
    n=$((n+1))
    mkdir -p /config/ssl
    openssl req -x509 -nodes -newkey rsa:2048 -days 3650 \
      -keyout /config/ssl/cert.key -out /config/ssl/cert.pem \
      -subj "/CN=localhost" > /tmp/ssl_gen.log 2>&1
    echo "A_ATTEMPT=$n RC=$?"
    sleep 1
  done
  echo A_DONE
) &
(
  m=0
  while [ $m -lt 5 ]; do
    m=$((m+1))
    cp /etc/hostname /tmp/h_$m 2>/dev/null
    sed -i "s/x/y/g" /tmp/h_$m 2>/dev/null
    echo "B_ATTEMPT=$m"
  done
  echo B_DONE
) &
wait
echo CONC_DONE
