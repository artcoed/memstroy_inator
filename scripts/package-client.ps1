# scripts/package-client.ps1
#
# Windows PowerShell variant of `package-client.sh`. Builds release
# binaries, mirrors the runtime asset skeleton, drops a launcher .bat
# and (optionally) zips the result.
#
# Usage:
#   pwsh scripts/package-client.ps1
#   pwsh scripts/package-client.ps1 -Out .\build -Name memstroy-1.2.3
#   pwsh scripts/package-client.ps1 -Zip
[CmdletBinding()]
param(
    [string] $Out  = "",
    [string] $Name = "",
    [switch] $Zip
)

$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RootDir   = Split-Path -Parent $ScriptDir
Set-Location $RootDir

# Pull workspace version out of Cargo.toml's first `version = "X.Y.Z"`.
$Version = (Select-String -Path "Cargo.toml" -Pattern '^version\s*=\s*"([^"]+)"' `
    | Select-Object -First 1).Matches[0].Groups[1].Value
if ([string]::IsNullOrWhiteSpace($Version)) { $Version = "dev" }

$Arch = $env:PROCESSOR_ARCHITECTURE.ToLower()
$DefaultName = "memstroy-inator-windows-$Arch-$Version"

if ([string]::IsNullOrEmpty($Out))  { $Out  = Join-Path $RootDir "dist" }
if ([string]::IsNullOrEmpty($Name)) { $Name = $DefaultName }

$BundleDir = Join-Path $Out $Name

Write-Host "==> memstroy-inator client packager"
Write-Host "    workspace : $RootDir"
Write-Host "    bundle    : $BundleDir"

# ── Build release binaries ──────────────────────────────────────────
Write-Host "==> cargo build --release"
& cargo build --release `
    -p memstroy-gui `
    -p memstroy-assets-server `
    -p memstroy-cli
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

# ── Stage the bundle ────────────────────────────────────────────────
Write-Host "==> staging bundle"
if (Test-Path $BundleDir) { Remove-Item -Recurse -Force $BundleDir }
New-Item -ItemType Directory -Path (Join-Path $BundleDir "bin") | Out-Null

$BinNames = @("memstroy-gui.exe", "memstroy-assets-server.exe", "memstroy.exe")
foreach ($bin in $BinNames) {
    $src = Join-Path $RootDir "target\release\$bin"
    if (-not (Test-Path $src)) { throw "missing release binary: $src" }
    Copy-Item -Path $src -Destination (Join-Path $BundleDir "bin")
}

# ── Asset skeleton ──────────────────────────────────────────────────
Write-Host "==> mirroring asset skeleton"
$AssetSubs = @("clips", "videos", "images", "sounds", "particles", "text", "mellstroy")
foreach ($sub in $AssetSubs) {
    New-Item -ItemType Directory -Force -Path (Join-Path $BundleDir "assets\$sub") | Out-Null
}
if (Test-Path (Join-Path $RootDir "assets")) {
    Get-ChildItem -Path (Join-Path $RootDir "assets") -Filter "README.md" -Recurse -Depth 1 |
        ForEach-Object {
            $rel = $_.FullName.Substring($RootDir.Length + 1)
            $target = Join-Path $BundleDir $rel
            New-Item -ItemType Directory -Force -Path (Split-Path -Parent $target) | Out-Null
            Copy-Item -Path $_.FullName -Destination $target -Force
        }
}

# ── Examples + docs ─────────────────────────────────────────────────
New-Item -ItemType Directory -Force -Path (Join-Path $BundleDir "examples") | Out-Null
Get-ChildItem -Path (Join-Path $RootDir "examples") -Filter "*.yaml" -ErrorAction SilentlyContinue |
    ForEach-Object { Copy-Item -Path $_.FullName -Destination (Join-Path $BundleDir "examples") -Force }
foreach ($doc in @("README.md", "AI_MEME_INSTRUCTIONS.md")) {
    $src = Join-Path $RootDir $doc
    if (Test-Path $src) { Copy-Item -Path $src -Destination $BundleDir -Force }
}

# ── Top-level launcher .bat ─────────────────────────────────────────
@'
@echo off
REM Launch the editor from the bundle root so assets\ is auto-discovered.
cd /d "%~dp0"
"%~dp0bin\memstroy-gui.exe" %*
'@ | Set-Content -Path (Join-Path $BundleDir "memstroy-inator.bat") -Encoding ASCII

# ── Optional .zip ───────────────────────────────────────────────────
if ($Zip) {
    Write-Host "==> zipping bundle"
    $ZipPath = Join-Path $Out "$Name.zip"
    if (Test-Path $ZipPath) { Remove-Item $ZipPath -Force }
    Compress-Archive -Path $BundleDir -DestinationPath $ZipPath
    Write-Host "    archive : $ZipPath"
}

Write-Host "==> done"
Write-Host "    run with: $BundleDir\memstroy-inator.bat"
