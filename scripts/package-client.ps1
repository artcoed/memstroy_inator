# scripts/package-client.ps1
#
# Windows PowerShell variant of `package-client.sh`. Builds a hardened
# release bundle of the Memstroy-inator editor for distribution to
# clients.
#
# What ships:
#   * bin\memstroy-gui.exe + bin\memstroy.exe
#   * examples\*.yaml, README.md, Memstroy-inator.bat launcher
#   * catost.ico / catost.png (app icon for shortcuts & branding)
#
# What deliberately does NOT ship:
#   * Any assets\ directory. Client builds load every clip / image /
#     sound from the operator's remote memstroy-assets-server and
#     cache them under %USERPROFILE%\.memstroy\cache\ on first use.
#     Users can also add their own assets via the editor's UI.
#   * The memstroy-assets-server.exe binary itself. The server is run
#     by the operator on their backend, not by the client.
#
# Hardening notes (Level 1):
#   * The workspace [profile.release] sets `strip = "symbols"`,
#     `panic = "abort"`, no debug-info, etc. so the release artefact
#     itself does not contain symbols, panic paths or DWARF sections.
#   * MEMSTROY_DEFAULT_SERVER_URL is baked into the binary via obfstr
#     so the operator's backend address is not visible in
#     `Select-String -Pattern 'http' memstroy-gui.exe`.
#
# Usage:
#   pwsh scripts/package-client.ps1 -ServerUrl https://assets.example.com
#   pwsh scripts/package-client.ps1 -ServerUrl https://assets.example.com -Zip
#   pwsh scripts/package-client.ps1 -ServerUrl https://assets.example.com `
#                                   -Out .\build -Name memstroy-1.2.3
#
# Required:
#   -ServerUrl <URL>        Backend the shipped editor talks to.
#
# Optional:
#   -Out <path>             Output directory (default: .\dist).
#   -Name <name>            Bundle name (default:
#                           Memstroy-inator-windows-<arch>-<version>).
#   -Zip                    Also produce <bundle-name>.zip.
#   -AllowLoopback          Allow -ServerUrl to point at 127.* / ::1
#                           / localhost. Off by default to catch typos.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $ServerUrl,

    [string] $Out  = "",
    [string] $Name = "",
    [switch] $Zip,
    [switch] $AllowLoopback
)

$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RootDir   = Split-Path -Parent $ScriptDir
Set-Location $RootDir

# ── Loopback / scheme guard ─────────────────────────────────────────
# Same intent as the bash variant: catch a tired engineer shipping a
# bundle that bakes 127.0.0.1 into the obfstr'd URL.
if (-not $AllowLoopback) {
    if ($ServerUrl -match '127\.0\.0\.1|localhost|::1|0\.0\.0\.0') {
        Write-Error "error: -ServerUrl points at a loopback host ($ServerUrl). Pass -AllowLoopback if this is intentional."
        exit 3
    }
}
if (-not ($ServerUrl -match '^https?://')) {
    Write-Error "error: -ServerUrl must start with http:// or https:// (got: $ServerUrl)"
    exit 4
}

# Pull workspace version out of Cargo.toml's first `version = "X.Y.Z"`.
$Version = (Select-String -Path "Cargo.toml" -Pattern '^version\s*=\s*"([^"]+)"' `
    | Select-Object -First 1).Matches[0].Groups[1].Value
if ([string]::IsNullOrWhiteSpace($Version)) { $Version = "dev" }

$Arch = $env:PROCESSOR_ARCHITECTURE.ToLower()
$DefaultName = "Memstroy-inator-windows-$Arch-$Version"

if ([string]::IsNullOrEmpty($Out))  { $Out  = Join-Path $RootDir "dist" }
if ([string]::IsNullOrEmpty($Name)) { $Name = $DefaultName }

$BundleDir = Join-Path $Out $Name

Write-Host "==> Memstroy-inator client packager"
Write-Host "    workspace  : $RootDir"
Write-Host "    bundle     : $BundleDir"
Write-Host "    server URL : $ServerUrl"

# ── Build release binaries (client mode) ────────────────────────────
# Pass the build-time signals through env vars so build.rs bakes the
# obfstr-wrapped URL and the IS_CLIENT_BUILD flag into the artefact.
# Explicitly disable heavy optional features:
#   - GUI: `local-server` (excludes memstroy-assets-server + axum/tower-http)
#   - CLI: `telegram` (excludes memstroy-tg + scraper/reqwest)
# This cuts build time roughly in half and reduces binary size significantly.
Write-Host "==> cargo build --release (client mode)"
$env:MEMSTROY_CLIENT_BUILD        = "1"
$env:MEMSTROY_DEFAULT_SERVER_URL  = $ServerUrl

# Build GUI without local-server feature
& cargo build --release -p memstroy-gui --no-default-features
if ($LASTEXITCODE -ne 0) { throw "cargo build failed (memstroy-gui)" }

# Build CLI without telegram feature
& cargo build --release -p memstroy-cli --no-default-features
if ($LASTEXITCODE -ne 0) { throw "cargo build failed (memstroy-cli)" }

# ── Stage the bundle ────────────────────────────────────────────────
Write-Host "==> staging bundle"
if (Test-Path $BundleDir) { Remove-Item -Recurse -Force $BundleDir }
New-Item -ItemType Directory -Path (Join-Path $BundleDir "bin") | Out-Null

# Note: memstroy-assets-server.exe is intentionally NOT shipped — the
# server lives on the operator's backend, not on the client.
$BinNames = @("memstroy-gui.exe", "memstroy.exe")
foreach ($bin in $BinNames) {
    $src = Join-Path $RootDir "target\release\$bin"
    if (-not (Test-Path $src)) { throw "missing release binary: $src" }
    Copy-Item -Path $src -Destination (Join-Path $BundleDir "bin")
}

# ── Examples + docs ─────────────────────────────────────────────────
New-Item -ItemType Directory -Force -Path (Join-Path $BundleDir "examples") | Out-Null
Get-ChildItem -Path (Join-Path $RootDir "examples") -Filter "*.yaml" -ErrorAction SilentlyContinue |
    ForEach-Object { Copy-Item -Path $_.FullName -Destination (Join-Path $BundleDir "examples") -Force }
$ReadmeSrc = Join-Path $RootDir "README.md"
if (Test-Path $ReadmeSrc) { Copy-Item -Path $ReadmeSrc -Destination $BundleDir -Force }

# ── App icon ────────────────────────────────────────────────────────
# The Windows installer (scripts/make-installer.ps1) uses this .ico
# for `SetupIconFile=` (icon shown for Setup.exe in Explorer) and for
# `UninstallDisplayIcon=` (icon shown in Settings -> Apps). We also
# carry the source PNG alongside the .ico so any future cross-platform
# tooling can find the same logo without reaching back into the repo.
$IconIcoSrc = Join-Path $RootDir "assets\internal_images\catost.ico"
$IconPngSrc = Join-Path $RootDir "assets\internal_images\catost.png"
if (Test-Path $IconIcoSrc) {
    Copy-Item -Path $IconIcoSrc -Destination (Join-Path $BundleDir "catost.ico") -Force
} else {
    Write-Warning "app icon not found at $IconIcoSrc; the installer will fall back to the default Inno Setup icon"
}
if (Test-Path $IconPngSrc) {
    Copy-Item -Path $IconPngSrc -Destination (Join-Path $BundleDir "catost.png") -Force
}

# ── Top-level launcher .bat ─────────────────────────────────────────
# The launcher no longer cd's into the bundle to find a local assets\
# directory — there isn't one in client mode. The editor reads its
# cache from %USERPROFILE%\.memstroy\cache\ regardless of where the
# launcher was invoked from.
@'
@echo off
REM Launch the Memstroy-inator editor.
REM
REM All assets are fetched from the configured assets-server on demand
REM and cached under %USERPROFILE%\.memstroy\cache\.
"%~dp0bin\memstroy-gui.exe" %*
'@ | Set-Content -Path (Join-Path $BundleDir "Memstroy-inator.bat") -Encoding ASCII

# ── Optional .zip ───────────────────────────────────────────────────
if ($Zip) {
    Write-Host "==> zipping bundle"
    $ZipPath = Join-Path $Out "$Name.zip"
    if (Test-Path $ZipPath) { Remove-Item $ZipPath -Force }
    Compress-Archive -Path $BundleDir -DestinationPath $ZipPath
    Write-Host "    archive : $ZipPath"
}

Write-Host "==> done"
Write-Host "    run with: $BundleDir\Memstroy-inator.bat"
