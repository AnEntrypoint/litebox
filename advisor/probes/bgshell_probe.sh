#!/bin/sh
# Minimal repro for the guest-shell fragility that blocks every XFCE launch path.
#
# Observed (2026-09-03): launch scripts die partway through, with FOUR different fatal signals
# across runs -- SIGILL(4), SIGSEGV(11), SIGTRAP(5), SIGABRT(6) -- always in a /bin/sh that is
# backgrounding long-lived services. In pass_weston5.log the shell took SIGILL at t=0.997s
# (Exception 6, #UD, rip=0x7feffff7fb8a, ~449 KB below TASK_ADDR_MAX, nothing mapped there)
# immediately after `dbus-daemon ... &`, so dbus never started and every downstream wait timed
# out. Because every multi-service launch script needs exactly this, it sits underneath all the
# XFCE work regardless of compositor.
#
# This script isolates that ONE behaviour: background N long-lived processes from a shell, then
# have the shell keep running. No dbus, no weston, no X, no XFCE.
#
# Run it as the runner's TOP-LEVEL program from inside a tar layer (never via `sh -c "..."`).
# Host side: export LITEBOX_LOG=error
#
# Reading the result:
#   ALL_ALIVE + BG_SHELL_OK      -> backgrounding is fine; the launch failures are elsewhere.
#   the script dies before OK    -> confirmed: the shell cannot survive backgrounding.
#                                   Grep the run log for "fatal signal" to get the signal and rip.
#   MISSING=n                    -> children died rather than the shell; a different bug.
#
# Run it 20 times. The failures are intermittent (2 of 4 recent runs showed no fatal signal at
# all), so a single clean pass proves nothing.

echo "BG_PROBE_START"

# 1. Background several long-lived children, the way every real launch script does.
i=1
while [ "$i" -le 5 ]; do
    sleep 120 &
    echo "SPAWNED=$i pid=$!"
    i=$((i + 1))
done

# 2. Keep the shell doing ordinary work afterwards. The observed crashes happen in the SHELL,
#    shortly after the background spawn, not in the children.
i=1
while [ "$i" -le 20 ]; do
    # Cheap builtins plus one fork/exec, mirroring what a launch script does between spawns.
    x=$(echo "iteration $i")
    /bin/true
    i=$((i + 1))
done
echo "SHELL_SURVIVED_WORK_LOOP"

# 3. Confirm the children are still there. `ps` may be limited under litebox; jobs is a builtin
#    fallback and does not depend on /proc.
alive=0
for p in $(jobs -p 2>/dev/null); do
    alive=$((alive + 1))
done
echo "JOBS_ALIVE=$alive"
if [ "$alive" -eq 5 ]; then
    echo "ALL_ALIVE"
else
    echo "MISSING=$((5 - alive))"
fi

# 4. A second round, since the first backgrounding may prime whatever fails.
i=1
while [ "$i" -le 5 ]; do
    sleep 120 &
    i=$((i + 1))
done
echo "SECOND_ROUND_SPAWNED"

# 5. Wait a little, the way a launch script waits on a readiness condition.
i=0
while [ "$i" -lt 10 ]; do
    i=$((i + 1))
    sleep 0.5
done
echo "POLL_LOOP_SURVIVED"

echo "BG_SHELL_OK"
