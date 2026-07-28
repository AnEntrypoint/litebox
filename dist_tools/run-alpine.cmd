@echo off
setlocal
rem Runs the bundled Alpine (BusyBox) Linux rootfs on Windows via LiteBox.
rem Usage: run-alpine.cmd [command] [args...]
rem   No args -> /bin/sh (interactive Alpine shell)
rem   run-alpine.cmd busybox ls /            -> runs busybox with args
set SCRIPT_DIR=%~dp0
if "%~1"=="" (
    "%SCRIPT_DIR%litebox_runner_linux_on_windows_userland.exe" --initial-files "%SCRIPT_DIR%alpine-rootfs.tar" /bin/sh
) else (
    "%SCRIPT_DIR%litebox_runner_linux_on_windows_userland.exe" --initial-files "%SCRIPT_DIR%alpine-rootfs.tar" %*
)
endlocal
