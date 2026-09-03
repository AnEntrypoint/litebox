# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.

# Runs the bundled Alpine (BusyBox) Linux rootfs on Windows via LiteBox.
# Usage: .\run-alpine.ps1 [command] [args...]
#   No args -> /bin/sh (interactive Alpine shell)
#   .\run-alpine.ps1 busybox ls /   -> runs /bin/busybox with args
#   .\run-alpine.ps1 /bin/busybox ls /   -> an already-absolute path is used as-is
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$Exe = Join-Path $ScriptDir "litebox_runner_linux_on_windows_userland.exe"
$RootfsTar = Join-Path $ScriptDir "alpine-rootfs.tar"

if ($args.Count -eq 0) {
    & $Exe --initial-files $RootfsTar /bin/sh
} else {
    $Program = if ($args[0].StartsWith("/")) { $args[0] } else { "/bin/$($args[0])" }
    & $Exe --initial-files $RootfsTar $Program @($args | Select-Object -Skip 1)
}
