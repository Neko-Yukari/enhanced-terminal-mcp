# Installs the freshly built binary into the path both hosts launch.
#
# `cargo build --release` now writes to target-build/, so a running server no
# longer blocks the build. Windows still refuses to replace a running
# executable in place, but it does allow renaming one, so the step below moves
# the old image aside first and deletes it once the holding process exits.
#
# Usage: pwsh -File scripts/install.ps1 [-KeepOld]

[CmdletBinding()]
param(
    [switch]$KeepOld
)

$ErrorActionPreference = "Stop"

$root = Split-Path -Parent $PSScriptRoot
$built = Join-Path $root "target-build/release/enhanced-terminal-mcp.exe"
$dest = Join-Path $root "target/release/enhanced-terminal-mcp.exe"

if (-not (Test-Path $built)) {
    throw "Missing build output: $built. Run 'cargo build --release' first."
}

New-Item -ItemType Directory -Force -Path (Split-Path $dest) | Out-Null

$aside = "$dest.old"
if (Test-Path $dest) {
    Remove-Item $aside -Force -ErrorAction SilentlyContinue
    Rename-Item $dest $aside -Force
}

Copy-Item $built $dest -Force
Write-Output "installed: $dest"

if (-not $KeepOld) {
    # Still-running holders keep the old image open, so this delete may be a no-op.
    Remove-Item $aside -Force -ErrorAction SilentlyContinue
}
