# Build ash_renderer and copy the exe into a publish folder, so a frozen stable build
# can be played while development builds and automated tests keep running.
# The copied exe detects it runs outside target/ and serves BRP on 15710 instead of 15702.
# Usage: powershell -ExecutionPolicy Bypass -File publish.ps1 [-BuildProfile release] [-Dest <dir>] [-SkipBuild]
# Kept ASCII-only: Windows PowerShell 5.1 reads BOM-less UTF-8 scripts as ANSI.
param(
    [string]$BuildProfile = "release",
    [string]$Dest = "",
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"

$repo = (Resolve-Path (Join-Path $PSScriptRoot "..\..\..\..")).Path
if (-not $Dest) { $Dest = Join-Path $repo "publish" }
$Dest = [System.IO.Path]::GetFullPath($Dest)

$running = Get-Process ash_renderer -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -and $_.Path.StartsWith($Dest, [StringComparison]::OrdinalIgnoreCase) }
if ($running) { throw "Published build is running (PID $($running.Id -join ',')); close it before publishing" }

if (-not $SkipBuild) {
    Push-Location $repo
    try {
        cargo build -p ash_renderer --bin ash_renderer --profile $BuildProfile
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed (exit $LASTEXITCODE)" }
    } finally { Pop-Location }
}

# Ask cargo for the real target dir: worktrees redirect it via .cargo/config.toml
$meta = cargo metadata --format-version 1 --no-deps --manifest-path (Join-Path $repo "Cargo.toml") | ConvertFrom-Json
$targetDir = [System.IO.Path]::GetFullPath($meta.target_directory)
if ($Dest.StartsWith($targetDir, [StringComparison]::OrdinalIgnoreCase)) {
    throw "Dest must be outside $targetDir, otherwise the exe still counts as a dev instance"
}
$profileDir = if ($BuildProfile -eq "dev") { "debug" } else { $BuildProfile }
$exe = Join-Path $targetDir "$profileDir\ash_renderer.exe"
if (-not (Test-Path $exe)) { throw "Build output not found: $exe" }

New-Item -ItemType Directory -Force $Dest | Out-Null
Copy-Item $exe (Join-Path $Dest "ash_renderer.exe") -Force

$commit = (git -C $repo rev-parse --short HEAD).Trim()
$dirty = if (git -C $repo status --porcelain) { " (with uncommitted changes)" } else { "" }
$info = "commit: $commit$dirty`r`nprofile: $BuildProfile`r`nbrp: http://127.0.0.1:15710 (override with ASH_BRP_PORT)`r`npublished: $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')`r`n"
[System.IO.File]::WriteAllText((Join-Path $Dest "VERSION.txt"), $info, (New-Object System.Text.UTF8Encoding $false))

Write-Output "Published: $(Join-Path $Dest 'ash_renderer.exe')"
Write-Output $info
