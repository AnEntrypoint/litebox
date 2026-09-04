#!/bin/sh
# Definitive control: bare `wait` with SHORT-lived background children only.
# If POSIX `wait`-waits-for-everything explains the earlier "hang", this must
# complete quickly, since there is nothing long-lived to wait for.
echo G_START
j=0
while [ "$j" -lt 5 ]; do
    sleep 2 &
    j=$((j + 1))
done
k=0
while [ "$k" -lt 20 ]; do
    ( /bin/true; /bin/echo "nested $k" >/dev/null ) &
    k=$((k + 1))
done
wait
echo G_DONE_BARE_WAIT
