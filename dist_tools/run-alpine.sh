#! /bin/bash

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

# Runs the bundled Alpine (BusyBox) Linux rootfs via LiteBox.
# Usage: ./run-alpine.sh [command] [args...]
#   No args -> /bin/sh (interactive Alpine shell)
#   ./run-alpine.sh busybox ls /       -> runs /bin/busybox with args
#   ./run-alpine.sh /bin/busybox ls /  -> an already-absolute path is used as-is
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
exe="$script_dir/litebox_runner_linux_userland"
rootfs_tar="$script_dir/alpine-rootfs.tar"

if [ "$#" -eq 0 ]; then
    exec "$exe" --unstable --initial-files "$rootfs_tar" --program-from-tar /bin/sh
fi

case "$1" in
    /*) program="$1" ;;
    *) program="/bin/$1" ;;
esac
shift
exec "$exe" --unstable --initial-files "$rootfs_tar" --program-from-tar "$program" "$@"
