#requires -version 5.0
# build.ps1 - assembles a clean, portable copy of BackStar into .\build, containing only what's
# needed to actually run the app (no dev/test files, no personal config/history). Run this after
# any change to app\*.ps1 so .\build always reflects the latest code - that's what you copy to
# your USB drive.

$ErrorActionPreference = 'Stop'

$root = $PSScriptRoot
$src = Join-Path $root 'app'
$dest = Join-Path $root 'build'

if (Test-Path -LiteralPath $dest) {
    Remove-Item -LiteralPath $dest -Recurse -Force
}
New-Item -ItemType Directory -Path $dest -Force | Out-Null

# Everything the app needs to run. Deliberately excludes: .test_harness.ps1 (dev-only test
# script), BackStar.config.json / BackStar.history.json (personal runtime data - the app creates
# sensible defaults on first run, so a fresh build should start clean rather than carrying over
# this machine's settings).
$filesToCopy = @(
    'BackStar.vbs'
    'BackStar.bat'
    'BackStar.ps1'
    'BackStar.Theme.ps1'
    'BackStar.Config.ps1'
    'BackStar.Engine.ps1'
    'BackStar.Tray.ps1'
    'BackStar.UI.ProjectTab.ps1'
    'BackStar.UI.SystemTab.ps1'
    'BackStar.UI.HistoryRestore.ps1'
)

foreach ($f in $filesToCopy) {
    $srcPath = Join-Path $src $f
    if (-not (Test-Path -LiteralPath $srcPath)) {
        throw "Build failed: expected file not found: $srcPath"
    }
    Copy-Item -LiteralPath $srcPath -Destination $dest
}

Copy-Item -LiteralPath (Join-Path $src 'assets') -Destination (Join-Path $dest 'assets') -Recurse

# Fail loudly if the entry script can't even parse - a build that doesn't run isn't a build.
$parseErrors = $null
[System.Management.Automation.Language.Parser]::ParseFile((Join-Path $dest 'BackStar.ps1'), [ref]$null, [ref]$parseErrors) | Out-Null
if ($parseErrors.Count -gt 0) {
    throw "Build failed: BackStar.ps1 has syntax errors:`n$($parseErrors -join "`n")"
}

Write-Host "Build complete: $dest" -ForegroundColor Green
Get-ChildItem -LiteralPath $dest -Recurse -File | ForEach-Object {
    Write-Host "  $($_.FullName.Substring($dest.Length + 1))" -ForegroundColor DarkGray
}
