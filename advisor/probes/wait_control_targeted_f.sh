#!/bin/sh
# Is the shell's `wait` correctly waiting for the LONG-LIVED children too?
# `wait` with no args waits for ALL children -- including the sleep 300s. If so,
# the "hang" is CORRECT POSIX behaviour and my repro is simply wrong, not litebox.
# Discriminator: use `wait` on ONLY the subshell pids, leaving the sleeps alone.
echo F_START
j=0
while [ "$j" -lt 5 ]; do
    sleep 300 &
    j=$((j + 1))
done
echo F_SLEEPS_SPAWNED
pids=""
k=0
while [ "$k" -lt 20 ]; do
    ( /bin/true; /bin/echo "nested $k" >/dev/null ) &
    pids="$pids $!"
    k=$((k + 1))
done
echo F_SUBSHELLS_SPAWNED
# Wait ONLY for the subshells, by explicit pid.
for p in $pids; do
    wait "$p"
done
echo F_DONE_TARGETED_WAIT
