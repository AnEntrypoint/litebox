#!/bin/sh
# RETRACTED -- THIS IS NOT A BUG. Kept only as the control for the real answer.
#
# This script was reported as a "5-second, 3/3 deterministic hang in sh wait".
# That was WRONG, and the error was mine, not litebox's.
#
# Bare `wait` (no arguments) waits for ALL of the shell's children. This script
# backgrounds five `sleep 300 &` and then calls bare `wait`, so blocking for 300
# seconds is CORRECT POSIX behaviour. The bisect that appeared to incriminate
# "long-lived siblings + subshell forking" was really just detecting which
# variants happened to contain a `sleep 300`.
#
# Two controls settle it, both in this directory:
#   wait_control_targeted_f.sh   -- same shape, but `wait "$p"` on the subshell
#                                   pids only: completes cleanly.
#   wait_control_shortlived_g.sh -- bare `wait`, but children are `sleep 2`:
#                                   completes in 3.7s.
#
# Lesson worth keeping: "the process stopped producing output" is not evidence of
# a hang. Check what the program was correctly waiting for before calling it a bug.
# Phase 2 (long-lived background children) THEN phase 3.
# Discriminates "live sleeping siblings present" from "fork/execve churn".
echo E_START
j=0
while [ "$j" -lt 5 ]; do
    sleep 300 &
    j=$((j + 1))
done
echo E_PHASE2_OK
k=0
while [ "$k" -lt 20 ]; do
    ( /bin/true; /bin/echo "nested $k" >/dev/null ) &
    k=$((k + 1))
done
wait
echo E_DONE
