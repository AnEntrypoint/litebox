#! /bin/bash

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

# usage: t.sh 'shell commands' [timeout]
R=/home/user/litebox/target/release/litebox_runner_linux_userland
cd /home/user/wt
timeout -s KILL ${2:-60} $R -Z --initial-files rootfs.tar --rewrite-syscalls --uid 0 --gid 0 --env PATH=/lsiopy/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin --env HOME=/root /home/user/wt/rootfs/bin/sh -c "$1" 2>&1
pkill -9 -x litebox_runner_ 2>/dev/null
