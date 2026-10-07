#! /bin/bash

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

# usage: runwt.sh WAIT  -> boots webtop /init under the Linux runner (env: LOGSPEC, TL)
WAITS=${1:-120}
ENVS=(--env PATH=/lsiopy/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin --env HOME=/config --env LANG=en_US.UTF-8 --env TERM=xterm --env S6_CMD_WAIT_FOR_SERVICES_MAXTIME=0 --env S6_VERBOSITY=1 --env S6_STAGE2_HOOK=/docker-mods --env VIRTUAL_ENV=/lsiopy --env DISPLAY=:1 --env PERL5LIB=/usr/local/bin --env START_DOCKER=false --env PULSE_RUNTIME_PATH=/defaults --env SELKIES_INTERPOSER=/usr/lib/selkies_input_interposer.so --env DISABLE_DRI3=true --env SELKIES_ENABLE_BASIC_AUTH=false --env "SELKIES_ALLOWED_ORIGINS=*" --env "TITLE=Debian XFCE" --env LSIO_FIRST_PARTY=true --env PUID=1000 --env PGID=1000 --env TZ=UTC --env WAIT=$WAITS)
R=/home/user/litebox/target/release/litebox_runner_linux_userland
cd /home/user/wt
(python3 /home/user/wt/netcfg.py tun0 > /home/user/wt/netcfg.log 2>&1 &)
env ${POISON:+LITEBOX_SHARED_HEAP_POISON=1} LITEBOX_DIAG_SYSCALL_TIMELINE=${TL:-} LITEBOX_DIAG_FAULT=1 LITEBOX_DIAG_BIGALLOC=1 LITEBOX_LOG=${LOGSPEC:-warn} LITEBOX_PRINT_EXE_BASE=1 timeout -s KILL $((WAITS+200)) $R -Z --initial-files rootfs.tar --rewrite-syscalls --uid 0 --gid 0 --pid1 --tun-device-name tun0 --export-writable-layer layer.tar "${ENVS[@]}" /home/user/wt/rootfs/bin/sh /init > wt.out 2>&1 < /dev/null
echo "runner rc=$?"
pkill -9 -x litebox_runner_
