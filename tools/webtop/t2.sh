#!/bin/bash
# usage: t2.sh 'guest shell commands' ; runs with tun0 configured on host (10.0.0.1), guest 10.0.0.2
R=/home/user/litebox/target/release/litebox_runner_linux_userland
cd /home/user/wt
(python3 /home/user/wt/netcfg.py tun0 > /home/user/wt/netcfg.log 2>&1 &)
timeout -s KILL ${2:-60} $R -Z --initial-files rootfs.tar --rewrite-syscalls --uid 0 --gid 0 --tun-device-name tun0 --env PATH=/lsiopy/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin --env HOME=/root /home/user/wt/rootfs/bin/sh -c "$1" 2>&1
pkill -9 -x litebox_runner_ 2>/dev/null
