@echo off
rem Copyright (c) Microsoft Corporation.
rem Licensed under the MIT license.

setlocal
rem Runs the bundled Alpine (BusyBox) Linux rootfs on Windows via LiteBox.
rem Usage: run-alpine.cmd [command] [args...]
rem   No args -> /bin/sh (interactive Alpine shell)
rem   run-alpine.cmd busybox ls /            -> runs /bin/busybox with args
rem   run-alpine.cmd /bin/busybox ls /       -> an already-absolute path is used as-is
set SCRIPT_DIR=%~dp0
set CMD=%~1
if "%CMD%"=="" (
    "%SCRIPT_DIR%litebox_runner_linux_on_windows_userland.exe" --initial-files "%SCRIPT_DIR%alpine-rootfs.tar" /bin/sh
    goto :end
)
set REST=%*
call set REST=%%REST:*%CMD%=%%
if "%CMD:~0,1%"=="/" (
    "%SCRIPT_DIR%litebox_runner_linux_on_windows_userland.exe" --initial-files "%SCRIPT_DIR%alpine-rootfs.tar" %CMD%%REST%
) else (
    "%SCRIPT_DIR%litebox_runner_linux_on_windows_userland.exe" --initial-files "%SCRIPT_DIR%alpine-rootfs.tar" /bin/%CMD%%REST%
)
:end
endlocal
