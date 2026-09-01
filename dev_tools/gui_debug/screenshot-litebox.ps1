# Screenshots the litebox `--gui` window reliably, working around a real, repeatedly-hit issue in
# this dev environment: `SetForegroundWindow`/`AttachThreadInput` frequently fail to bring the
# litebox window to front (Windows' own foreground-lock protection rejects the request when this
# session isn't the OS's currently-focused process, which is normal on a shared/remote-controlled
# machine) -- every earlier debugging pass in this project re-discovered this by trial and error,
# burning several tool calls each time on a wrong-window capture before finding the fix below.
#
# The fix: `SetWindowPos(HWND_TOPMOST)` does NOT require foreground-focus permission the way
# `SetForegroundWindow` does -- it only reorders z-order, which any process can request for its
# own window. This reliably surfaces the litebox window even when this session cannot "steal
# focus" the normal way.
#
# Usage:
#   powershell -File dev_tools\gui_debug\screenshot-litebox.ps1 -OutFile scratchpad\shot.png
#   powershell -File dev_tools\gui_debug\screenshot-litebox.ps1 -OutFile shot.png -TitleMatch "litebox virtual display"
#
# Exits with a clear error (not a silent wrong-window capture) if no matching window is found, so
# a caller can tell "process not up yet" apart from "captured the wrong thing".

param(
    [Parameter(Mandatory = $true)]
    [string]$OutFile,

    [string]$TitleMatch = "litebox virtual display",

    # How long to wait for the window to appear before giving up, in case this is called right
    # after launching the runner and the window hasn't been created yet.
    [int]$WaitSeconds = 10
)

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class LiteboxScreenshotWin32 {
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT lpRect);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hWnd, IntPtr hWndInsertAfter, int X, int Y, int cx, int cy, uint uFlags);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);
    [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr hWnd);
    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
}
"@

$HWND_TOPMOST = [IntPtr]-1
$SWP_NOMOVE = 0x2
$SWP_NOSIZE = 0x1
$SW_RESTORE = 9

$deadline = (Get-Date).AddSeconds($WaitSeconds)
$proc = $null
while ((Get-Date) -lt $deadline) {
    $proc = Get-Process | Where-Object { $_.MainWindowTitle -eq $TitleMatch } | Select-Object -First 1
    if ($proc -and $proc.MainWindowHandle -ne [IntPtr]::Zero) { break }
    Start-Sleep -Milliseconds 500
}

if (-not $proc -or $proc.MainWindowHandle -eq [IntPtr]::Zero) {
    Write-Error "No window with title '$TitleMatch' found within ${WaitSeconds}s. Is the runner up with --gui?"
    exit 1
}

$hwnd = $proc.MainWindowHandle

if ([LiteboxScreenshotWin32]::IsIconic($hwnd)) {
    [LiteboxScreenshotWin32]::ShowWindow($hwnd, $SW_RESTORE) | Out-Null
}
[LiteboxScreenshotWin32]::SetWindowPos($hwnd, $HWND_TOPMOST, 0, 0, 0, 0, ($SWP_NOMOVE -bor $SWP_NOSIZE)) | Out-Null
Start-Sleep -Milliseconds 300

$rect = New-Object LiteboxScreenshotWin32+RECT
[LiteboxScreenshotWin32]::GetWindowRect($hwnd, [ref]$rect) | Out-Null
$w = $rect.Right - $rect.Left
$h = $rect.Bottom - $rect.Top

if ($w -le 0 -or $h -le 0) {
    Write-Error "Window rect is degenerate ($w x $h) -- window may be minimized or off-screen."
    exit 1
}

$bmp = New-Object System.Drawing.Bitmap $w, $h
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($rect.Left, $rect.Top, 0, 0, (New-Object System.Drawing.Size $w, $h))
$bmp.Save($OutFile)
$g.Dispose()
$bmp.Dispose()

Write-Output "saved: $OutFile ($w x $h), pid=$($proc.Id)"
