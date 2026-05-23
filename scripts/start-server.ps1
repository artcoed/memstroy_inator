# scripts/start-server.ps1
#
# Windows PowerShell variant of `start-server.sh`. Builds (release by
# default) and runs the standalone memstroy-assets-server.
#
# Usage:
#   pwsh scripts/start-server.ps1
#   pwsh scripts/start-server.ps1 -Addr 127.0.0.1:9000
#   pwsh scripts/start-server.ps1 -Root C:\memstroy\assets
#   pwsh scripts/start-server.ps1 -Debug
#
# Env knobs:
#   $env:RUST_LOG — tracing filter, default "info,memstroy_assets_server=info".
[CmdletBinding()]
param(
    [string] $Addr = "0.0.0.0:8765",
    [string] $Root = "",
    [switch] $Debug
)

$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RootDir   = Split-Path -Parent $ScriptDir
Set-Location $RootDir

if ([string]::IsNullOrEmpty($Root)) { $Root = Join-Path $RootDir "assets" }
New-Item -ItemType Directory -Force -Path $Root | Out-Null

if (-not $env:RUST_LOG) {
    $env:RUST_LOG = "info,memstroy_assets_server=info"
}

$Profile = if ($Debug) { "debug" } else { "release" }

Write-Host "==> memstroy-assets-server"
Write-Host "    addr   : $Addr"
Write-Host "    root   : $Root"
Write-Host "    profile: $Profile"

if ($Debug) {
    & cargo run -p memstroy-assets-server -- --addr $Addr --root $Root
} else {
    & cargo run --release -p memstroy-assets-server -- --addr $Addr --root $Root
}
