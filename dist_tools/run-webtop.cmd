@echo off
setlocal
set SCRIPT_DIR=%~dp0
set /p IMAGE_REF=<"%SCRIPT_DIR%webtop-image.ref"
"%SCRIPT_DIR%litebox_runner_linux_on_windows_userland.exe" --oci-image %IMAGE_REF% --publish 3000:3000 --publish 3001:3001 %*
exit /b %ERRORLEVEL%
