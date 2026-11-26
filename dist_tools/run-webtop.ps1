$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$ImageRef = (Get-Content (Join-Path $ScriptDir "webtop-image.ref")).Trim()
$Exe = Join-Path $ScriptDir "litebox_runner_linux_on_windows_userland.exe"
& $Exe --oci-image $ImageRef --publish 3000:3000 --publish 3001:3001 @args
exit $LASTEXITCODE
