# Frees disk space in the Windows temp dir consumed by stale litebox debugging scratch state.
# This project's own debugging workflow routinely writes multi-GB scratch tars (`combined.tar`,
# `repro-rootfs.tar`, layer snapshots, etc.) into %TEMP% during live repros -- across many sessions
# these accumulate silently and %TEMP% fills to 100%, which then presents as confusing, unrelated
# failures elsewhere (a litebox runner segfault reading its own `--resume-from` archive because it
# ran out of space mid-read, `tail: write error`, etc.) rather than an obvious "disk full" message.
# This session alone recovered >70GB of stale scratch this way, three separate times.
#
# Usage:
#   powershell -File dev_tools\gui_debug\clean-scratch.ps1            # dry run, lists what WOULD be removed
#   powershell -File dev_tools\gui_debug\clean-scratch.ps1 -Apply     # actually removes it
#
# Only removes directories matching known scratch-naming patterns from this project's own
# established conventions (`litebox-*`, `litebox-e2e-*`, `litebox-plane-*`, `litebox-drm-*`) --
# never touches anything else in %TEMP%, and never touches this session's own live scratchpad
# (`claude\<repo-hash>\<session-id>\scratchpad`).

param(
    [switch]$Apply
)

$patterns = @('^litebox-')
$dirs = Get-ChildItem $env:TEMP -Directory -Force -ErrorAction SilentlyContinue |
    Where-Object {
        $name = $_.Name
        ($patterns | Where-Object { $name -match $_ }).Count -gt 0
    }

if (-not $dirs) {
    Write-Output "No stale litebox-* scratch directories found in $env:TEMP."
    Get-PSDrive C | Select-Object @{N='FreeGB';E={[math]::Round($_.Free/1GB,2)}}
    exit 0
}

$total = 0
foreach ($d in $dirs) {
    $size = (Get-ChildItem $d.FullName -Recurse -Force -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum
    $total += $size
    $sizeGB = [math]::Round($size / 1GB, 2)
    Write-Output "$($d.Name): ${sizeGB} GB"
}
Write-Output "---"
Write-Output "TOTAL: $([math]::Round($total/1GB, 2)) GB across $($dirs.Count) directories"

if ($Apply) {
    $dirs | Remove-Item -Recurse -Force -ErrorAction SilentlyContinue
    Write-Output "Removed."
    Get-PSDrive C | Select-Object @{N='FreeGB';E={[math]::Round($_.Free/1GB,2)}}
} else {
    Write-Output "(dry run -- pass -Apply to actually remove)"
}
