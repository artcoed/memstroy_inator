# scripts/package-client.ps1
#
# Windows PowerShell variant of `package-client.sh`. Builds a hardened
# release bundle of the Memstroy-inator editor for distribution to
# clients.
#
# What ships:
#   * bin\memstroy-gui.exe + bin\memstroy.exe
#   * models\u2netp.onnx (AI background removal for the canvas cutout tool)
#   * examples\*.yaml, README.md, Memstroy-inator.bat launcher
#   * catost.ico / catost.png (app icon for shortcuts & branding)
#
# What deliberately does NOT ship:
#   * Any other `assets/` tree (clips/images are fetched from the
#     operator's remote memstroy-assets-server and cached under
#     %USERPROFILE%\.memstroy\cache\ on first use).
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
#   pwsh scripts/package-client.ps1 -ServerUrl https://assets.example.com `
#                                   -Target aarch64-pc-windows-msvc
#
# Required:
#   -ServerUrl <URL>        Backend the shipped editor talks to.
#
# Optional:
#   -Out <path>             Output directory (default: .\dist).
#   -Name <name>            Bundle name (default:
#                           Memstroy-inator-windows-<arch>-<version>).
#   -Target <triple>        Optional Rust Windows target triple, e.g.
#                           aarch64-pc-windows-msvc for ARM64.
#   -Zip                    Also produce <bundle-name>.zip.
#   -AllowLoopback          Allow -ServerUrl to point at 127.* / ::1
#                           / localhost. Off by default to catch typos.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $ServerUrl,

    [string] $Out  = "",
    [string] $Name = "",
    [string] $Target = "",
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

function Get-WindowsArchName {
    param([string]$TargetTriple)

    if ([string]::IsNullOrWhiteSpace($TargetTriple)) {
        $HostArch = $env:PROCESSOR_ARCHITECTURE.ToLowerInvariant()
        switch ($HostArch) {
            "amd64" { return "amd64" }
            "x86_64" { return "amd64" }
            "arm64" { return "arm64" }
            "aarch64" { return "arm64" }
            "x86" { return "x86" }
            default { return $HostArch }
        }
    }

    switch ($TargetTriple) {
        "x86_64-pc-windows-msvc" { return "amd64" }
        "aarch64-pc-windows-msvc" { return "arm64" }
        "i686-pc-windows-msvc" { return "x86" }
        default {
            Write-Error "error: unsupported Windows target '$TargetTriple'. Supported: x86_64-pc-windows-msvc, aarch64-pc-windows-msvc, i686-pc-windows-msvc."
            exit 5
        }
    }
}

function Get-CargoReleaseDir {
    param([string]$TargetTriple)

    if ([string]::IsNullOrWhiteSpace($TargetTriple)) {
        return (Join-Path $RootDir "target\release")
    }
    return (Join-Path $RootDir "target\$TargetTriple\release")
}

function Find-VcVarsAll {
    $vswhereCandidates = @(
        "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe",
        "$env:ProgramFiles\Microsoft Visual Studio\Installer\vswhere.exe"
    ) | Where-Object { -not [string]::IsNullOrWhiteSpace($_) -and (Test-Path $_) }

    foreach ($vswhere in $vswhereCandidates) {
        $installPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null
        if (-not [string]::IsNullOrWhiteSpace($installPath)) {
            $vcvars = Join-Path $installPath "VC\Auxiliary\Build\vcvarsall.bat"
            if (Test-Path $vcvars) { return (Resolve-Path $vcvars).Path }
        }
    }

    $fallbacks = @(
        "$env:ProgramFiles\Microsoft Visual Studio\18\Community\VC\Auxiliary\Build\vcvarsall.bat",
        "${env:ProgramFiles(x86)}\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvarsall.bat",
        "$env:ProgramFiles\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvarsall.bat"
    )
    foreach ($candidate in $fallbacks) {
        if (-not [string]::IsNullOrWhiteSpace($candidate) -and (Test-Path $candidate)) {
            return (Resolve-Path $candidate).Path
        }
    }

    return $null
}

function Find-LlvmBinDir {
    $clang = Get-Command clang.exe -ErrorAction SilentlyContinue
    if ($clang -and $clang.Source) {
        return (Split-Path -Parent $clang.Source)
    }

    $scoopLlvm = Join-Path $env:USERPROFILE "scoop\apps\llvm\current\bin"
    if (Test-Path (Join-Path $scoopLlvm "clang.exe")) {
        return (Resolve-Path $scoopLlvm).Path
    }

    $programFilesLlvm = Join-Path $env:ProgramFiles "LLVM\bin"
    if (Test-Path (Join-Path $programFilesLlvm "clang.exe")) {
        return (Resolve-Path $programFilesLlvm).Path
    }

    return $null
}

function Assert-WindowsArm64MsvcLibs {
    $vswhereCandidates = @(
        "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe",
        "$env:ProgramFiles\Microsoft Visual Studio\Installer\vswhere.exe"
    ) | Where-Object { -not [string]::IsNullOrWhiteSpace($_) -and (Test-Path $_) }

    foreach ($vswhere in $vswhereCandidates) {
        $installPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null
        if ([string]::IsNullOrWhiteSpace($installPath)) { continue }

        $msvcRoot = Join-Path $installPath "VC\Tools\MSVC"
        if (-not (Test-Path $msvcRoot)) { continue }

        $toolset = Get-ChildItem -Path $msvcRoot -Directory -ErrorAction SilentlyContinue |
            Sort-Object Name -Descending |
            Select-Object -First 1
        if (-not $toolset) { continue }

        $libcmt = Join-Path $toolset.FullName "lib\arm64\libcmt.lib"
        if (Test-Path $libcmt) { return }

        throw "Windows ARM64 MSVC static CRT is missing: $libcmt. Install the Visual Studio C++ ARM64 build tools/component, then rerun. In Visual Studio Installer, add the ARM64/ARM64EC C++ build tools under Individual components."
    }

    throw "Visual Studio with MSVC C++ tools was not found. Install Visual Studio Build Tools/Community with Desktop C++ and ARM64 build tools."
}

$Arch = Get-WindowsArchName $Target
$CargoReleaseDir = Get-CargoReleaseDir $Target
$DefaultName = "Memstroy-inator-windows-$Arch-$Version"

if ([string]::IsNullOrEmpty($Out))  { $Out  = Join-Path $RootDir "dist" }
if ([string]::IsNullOrEmpty($Name)) { $Name = $DefaultName }

$BundleDir = Join-Path $Out $Name

Write-Host "==> Memstroy-inator client packager"
Write-Host "    workspace  : $RootDir"
Write-Host "    bundle     : $BundleDir"
Write-Host "    server URL : $ServerUrl"
Write-Host "    target     : $(if ([string]::IsNullOrWhiteSpace($Target)) { 'host' } else { $Target })"

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

# Ship a self-contained MSVC runtime so client machines do not need a
# separate Visual C++ Redistributable install. Preserve any caller-supplied
# flags (for example AVX2/FMA release builds) and append crt-static once.
$PreviousRustflags = $env:RUSTFLAGS
if ([string]::IsNullOrWhiteSpace($PreviousRustflags)) {
    $env:RUSTFLAGS = "-C target-feature=+crt-static"
} elseif ($PreviousRustflags -notmatch 'crt-static') {
    $env:RUSTFLAGS = "$PreviousRustflags -C target-feature=+crt-static"
}
Write-Host "    RUSTFLAGS  : $env:RUSTFLAGS"

function Invoke-ClientCargoBuild {
    param([Parameter(Mandatory=$true)][string]$Package)

    $Args = @("build", "--release")
    if (-not [string]::IsNullOrWhiteSpace($Target)) {
        $Args += @("--target", $Target)
    }
    $Args += @("-p", $Package, "--no-default-features")

    if ($Target -eq "aarch64-pc-windows-msvc") {
        Assert-WindowsArm64MsvcLibs

        $vcvars = Find-VcVarsAll
        if (-not $vcvars) {
            throw "Visual Studio vcvarsall.bat not found. Install MSVC Build Tools with C++ support before building Windows ARM64."
        }

        $llvmBin = Find-LlvmBinDir
        if (-not $llvmBin) {
            throw "clang.exe not found. Windows ARM64 builds need LLVM for ring; install it (for example: scoop install llvm) and rerun."
        }

        $argLine = ($Args | ForEach-Object { '"' + ($_ -replace '"', '\"') + '"' }) -join ' '
        $cmdLine = "call `"$vcvars`" amd64_arm64 && set `"PATH=$llvmBin;!PATH!`" && cargo $argLine"
        & cmd /d /s /v:on /c $cmdLine
        return
    }

    & cargo @Args
}

# Build GUI without local-server feature
Invoke-ClientCargoBuild "memstroy-gui"
if ($LASTEXITCODE -ne 0) { throw "cargo build failed (memstroy-gui)" }

# Build CLI without telegram feature
Invoke-ClientCargoBuild "memstroy-cli"
if ($LASTEXITCODE -ne 0) { throw "cargo build failed (memstroy-cli)" }

function Assert-NoDynamicVcruntime {
    param([Parameter(Mandatory=$true)][string]$ExePath)

    if (-not (Test-Path $ExePath)) { throw "missing binary for CRT check: $ExePath" }

    function Get-PeImportedDllNames {
        param([Parameter(Mandatory=$true)][string]$Path)

        $bytes = [System.IO.File]::ReadAllBytes($Path)
        if ($bytes.Length -lt 0x100) { return @() }

        $u16 = { param($o) [BitConverter]::ToUInt16($bytes, $o) }
        $u32 = { param($o) [BitConverter]::ToUInt32($bytes, $o) }

        if (& $u16 0 -ne 0x5A4D) { return @() }
        $pe = [int](& $u32 0x3C)
        if ($pe -lt 0 -or $pe + 0x18 -ge $bytes.Length) { return @() }
        if (& $u32 $pe -ne 0x00004550) { return @() }

        $coff = $pe + 4
        $section_count = [int](& $u16 ($coff + 2))
        $optional_size = [int](& $u16 ($coff + 16))
        $optional = $coff + 20
        if ($optional + $optional_size -gt $bytes.Length) { return @() }

        $magic = & $u16 $optional
        $data_dir = if ($magic -eq 0x20B) { $optional + 112 } elseif ($magic -eq 0x10B) { $optional + 96 } else { return @() }
        if ($data_dir + 8 -gt $bytes.Length) { return @() }
        $import_rva = & $u32 ($data_dir + 8)
        if ($import_rva -eq 0) { return @() }

        $sections = @()
        $section_table = $optional + $optional_size
        for ($i = 0; $i -lt $section_count; $i++) {
            $s = $section_table + $i * 40
            if ($s + 40 -gt $bytes.Length) { break }
            $sections += [pscustomobject]@{
                VirtualAddress = & $u32 ($s + 12)
                VirtualSize = & $u32 ($s + 8)
                RawPointer = & $u32 ($s + 20)
                RawSize = & $u32 ($s + 16)
            }
        }

        function Rva-ToOffset {
            param([uint32]$Rva)
            foreach ($sec in $sections) {
                $size = [Math]::Max([uint32]$sec.VirtualSize, [uint32]$sec.RawSize)
                if ($Rva -ge $sec.VirtualAddress -and $Rva -lt ($sec.VirtualAddress + $size)) {
                    return [int]($sec.RawPointer + ($Rva - $sec.VirtualAddress))
                }
            }
            return $null
        }

        function Read-AsciiZ {
            param([int]$Offset)
            if ($Offset -lt 0 -or $Offset -ge $bytes.Length) { return $null }
            $end = $Offset
            while ($end -lt $bytes.Length -and $bytes[$end] -ne 0) { $end++ }
            if ($end -le $Offset) { return $null }
            [Text.Encoding]::ASCII.GetString($bytes, $Offset, $end - $Offset)
        }

        $import_off = Rva-ToOffset $import_rva
        if ($null -eq $import_off) { return @() }

        $names = @()
        for ($desc = $import_off; $desc + 20 -le $bytes.Length; $desc += 20) {
            $name_rva = & $u32 ($desc + 12)
            $first_thunk = & $u32 ($desc + 16)
            $orig_thunk = & $u32 $desc
            if ($name_rva -eq 0 -and $first_thunk -eq 0 -and $orig_thunk -eq 0) { break }
            $name_off = Rva-ToOffset $name_rva
            if ($null -ne $name_off) {
                $name = Read-AsciiZ $name_off
                if ($name) { $names += $name }
            }
        }
        $names
    }

    $peImports = Get-PeImportedDllNames $ExePath
    if ($peImports -match '^(VCRUNTIME140(?:_[0-9]+)?|MSVCP140(?:_[0-9]+)?|CONCRT140)\.dll$') {
        throw "$ExePath imports Microsoft VC runtime DLLs ($($peImports -join ', ')). Static CRT linking failed; check RUSTFLAGS."
    }

    $dumpbin = Get-Command dumpbin.exe -ErrorAction SilentlyContinue
    if ($dumpbin) {
        $imports = & $dumpbin.Source /dependents $ExePath 2>$null | Out-String
        if ($imports -match '(VCRUNTIME140(?:_[0-9]+)?|MSVCP140(?:_[0-9]+)?|CONCRT140)\.dll') {
            throw "$ExePath imports Microsoft VC runtime DLLs. Static CRT linking failed; check RUSTFLAGS."
        }
        return
    }

    $objdump = Get-Command llvm-objdump.exe -ErrorAction SilentlyContinue
    if (-not $objdump) { $objdump = Get-Command objdump.exe -ErrorAction SilentlyContinue }
    if ($objdump) {
        $imports = & $objdump.Source -p $ExePath 2>$null | Out-String
        if ($imports -match '(VCRUNTIME140(?:_[0-9]+)?|MSVCP140(?:_[0-9]+)?|CONCRT140)\.dll') {
            throw "$ExePath imports Microsoft VC runtime DLLs. Static CRT linking failed; check RUSTFLAGS."
        }
        return
    }

    Write-Host "    CRT check : PE imports parsed without dumpbin/objdump"
}

Assert-NoDynamicVcruntime (Join-Path $CargoReleaseDir "memstroy-gui.exe")
Assert-NoDynamicVcruntime (Join-Path $CargoReleaseDir "memstroy.exe")

# ── Stage the bundle ────────────────────────────────────────────────
Write-Host "==> staging bundle"
if (Test-Path $BundleDir) { Remove-Item -Recurse -Force $BundleDir }
New-Item -ItemType Directory -Path (Join-Path $BundleDir "bin") | Out-Null

# Note: memstroy-assets-server.exe is intentionally NOT shipped - the
# server lives on the operator's backend, not on the client.
$BinNames = @("memstroy-gui.exe", "memstroy.exe")
foreach ($bin in $BinNames) {
    $src = Join-Path $CargoReleaseDir $bin
    if (-not (Test-Path $src)) { throw "missing release binary: $src" }
    Copy-Item -Path $src -Destination (Join-Path $BundleDir "bin")
}

# ── Microsoft VC++ runtime side-by-side DLLs ─────────────────────────
# Some build hosts produce MSVC-linked binaries that import
# vcruntime140.dll / vcruntime140_1.dll. Do not rely on the end user's
# machine having the VC++ redistributable installed: stage the runtime
# DLLs next to the executables so the app starts out of the box.
$VcRedistArch = switch ($Arch) {
    "amd64" { "x64" }
    "arm64" { "arm64" }
    "x86" { "x86" }
    default { $Arch }
}
$HostVcRedistArch = switch ($env:PROCESSOR_ARCHITECTURE.ToLowerInvariant()) {
    "amd64" { "x64" }
    "x86_64" { "x64" }
    "arm64" { "arm64" }
    "aarch64" { "arm64" }
    "x86" { "x86" }
    default { $env:PROCESSOR_ARCHITECTURE.ToLowerInvariant() }
}
function Resolve-VcRuntimeDll {
    param([Parameter(Mandatory=$true)][string]$DllName)

    $candidates = New-Object System.Collections.Generic.List[string]
    if (-not [string]::IsNullOrWhiteSpace($env:MEMSTROY_VC_REDIST_DIR)) {
        $candidates.Add((Join-Path $env:MEMSTROY_VC_REDIST_DIR $DllName))
    }
    if (-not [string]::IsNullOrWhiteSpace($env:VCToolsRedistDir)) {
        $candidates.Add((Join-Path $env:VCToolsRedistDir "$VcRedistArch\Microsoft.VC143.CRT\$DllName"))
        $candidates.Add((Join-Path $env:VCToolsRedistDir "$VcRedistArch\Microsoft.VC142.CRT\$DllName"))
    }

    $vsRedistRoots = @(
        "$env:ProgramFiles\Microsoft Visual Studio",
        "${env:ProgramFiles(x86)}\Microsoft Visual Studio"
    ) | Where-Object { -not [string]::IsNullOrWhiteSpace($_) -and (Test-Path $_) }
    $redistArchPattern = [regex]::Escape($VcRedistArch)
    foreach ($root in $vsRedistRoots) {
        Get-ChildItem -Path $root -Recurse -Filter $DllName -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -match "\\VC\\Redist\\MSVC\\[^\\]+\\$redistArchPattern\\Microsoft\.VC\d+\.CRT\\" } |
            Sort-Object LastWriteTime -Descending |
            ForEach-Object { $candidates.Add($_.FullName) }
    }

    if ($VcRedistArch -eq $HostVcRedistArch) {
        $system32 = Join-Path $env:WINDIR "System32\$DllName"
        $candidates.Add($system32)
    }

    foreach ($candidate in $candidates) {
        if (-not [string]::IsNullOrWhiteSpace($candidate) -and (Test-Path $candidate)) {
            return (Resolve-Path $candidate).Path
        }
    }
    return $null
}

$VcRuntimeDlls = @(
    "vcruntime140.dll",
    "vcruntime140_1.dll",
    "msvcp140.dll",
    "concrt140.dll"
)
foreach ($dll in $VcRuntimeDlls) {
    $runtimeSrc = Resolve-VcRuntimeDll $dll
    if ($runtimeSrc) {
        Copy-Item -Path $runtimeSrc -Destination (Join-Path $BundleDir "bin\$dll") -Force
        $runtimeSizeKb = [math]::Round((Get-Item $runtimeSrc).Length / 1KB, 1)
        Write-Host "    bundled   : bin\$dll (${runtimeSizeKb} KB from $runtimeSrc)"
    } else {
        Write-Warning "VC runtime DLL not found on this build host: $dll"
    }
}

# ── Bundled FFmpeg ──────────────────────────────────────────────────
function Resolve-RequiredToolPath {
    param(
        [Parameter(Mandatory=$true)][string]$ToolName,
        [string]$EnvPath
    )

    if (-not [string]::IsNullOrEmpty($EnvPath) -and (Test-Path $EnvPath)) {
        return Resolve-RealToolPath (Resolve-Path $EnvPath).Path $ToolName
    }

    $repoCandidate = Join-Path $RootDir "tools\ffmpeg\bin\$ToolName"
    if (Test-Path $repoCandidate) {
        return Resolve-RealToolPath (Resolve-Path $repoCandidate).Path $ToolName
    }

    $cmd = Get-Command $ToolName -ErrorAction SilentlyContinue
    if ($cmd -and $cmd.Source -and (Test-Path $cmd.Source)) {
        return Resolve-RealToolPath $cmd.Source $ToolName
    }

    throw "missing $ToolName; install FFmpeg on the build machine, set MEMSTROY_FFMPEG/MEMSTROY_FFPROBE, or place binaries in tools\ffmpeg\bin"
}

function Resolve-RealToolPath {
    param(
        [Parameter(Mandatory=$true)][string]$Candidate,
        [Parameter(Mandatory=$true)][string]$ToolName
    )

    $Resolved = (Resolve-Path $Candidate).Path
    $ShimSidecar = [System.IO.Path]::ChangeExtension($Resolved, ".shim")
    if (Test-Path $ShimSidecar) {
        $Line = Get-Content $ShimSidecar -ErrorAction SilentlyContinue |
            Where-Object { $_ -match '^\s*path\s*=' } |
            Select-Object -First 1
        if ($Line -and $Line -match '^\s*path\s*=\s*"([^"]+)"\s*$') {
            $Target = $Matches[1]
            if (Test-Path $Target) {
                $Resolved = (Resolve-Path $Target).Path
            }
        }
    }

    $Size = (Get-Item $Resolved).Length
    if ($Size -lt 1MB) {
        throw "$ToolName resolved to a tiny launcher/shim ($Size bytes): $Resolved. Install/copy a real static FFmpeg binary or set MEMSTROY_FFMPEG/MEMSTROY_FFPROBE."
    }

    $VersionOk = $false
    try {
        & $Resolved -version *> $null
        $VersionOk = ($LASTEXITCODE -eq 0)
    } catch {
        $VersionOk = $false
    }
    if (-not $VersionOk) {
        throw "$ToolName failed '-version' check: $Resolved"
    }

    return $Resolved
}

$FfmpegSrc = Resolve-RequiredToolPath "ffmpeg.exe" $env:MEMSTROY_FFMPEG
if (-not [string]::IsNullOrEmpty($env:MEMSTROY_FFPROBE)) {
    $FfprobeSrc = Resolve-RequiredToolPath "ffprobe.exe" $env:MEMSTROY_FFPROBE
} else {
    $SiblingProbe = Join-Path (Split-Path -Parent $FfmpegSrc) "ffprobe.exe"
    if (Test-Path $SiblingProbe) {
        $FfprobeSrc = (Resolve-Path $SiblingProbe).Path
    } else {
        $FfprobeSrc = Resolve-RequiredToolPath "ffprobe.exe" $null
    }
}
Copy-Item -Path $FfmpegSrc -Destination (Join-Path $BundleDir "bin\ffmpeg.exe") -Force
Copy-Item -Path $FfprobeSrc -Destination (Join-Path $BundleDir "bin\ffprobe.exe") -Force
$FfmpegSizeMb = [math]::Round((Get-Item $FfmpegSrc).Length / 1MB, 2)
$FfprobeSizeMb = [math]::Round((Get-Item $FfprobeSrc).Length / 1MB, 2)
Write-Host "    bundled   : bin\ffmpeg.exe  (${FfmpegSizeMb} MB from $FfmpegSrc)"
Write-Host "    bundled   : bin\ffprobe.exe (${FfprobeSizeMb} MB from $FfprobeSrc)"

# ── AI background-removal model (U²-Netp) ─────────────────────────────
$ModelSrc = Join-Path $RootDir "assets\models\u2netp.onnx"
if (-not (Test-Path $ModelSrc)) {
    throw "missing AI model: $ModelSrc - place u2netp.onnx there before packaging"
}
$ModelDestDir = Join-Path $BundleDir "models"
New-Item -ItemType Directory -Force -Path $ModelDestDir | Out-Null
Copy-Item -Path $ModelSrc -Destination (Join-Path $ModelDestDir "u2netp.onnx") -Force
Write-Host "    bundled   : models\u2netp.onnx"

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
# directory - there isn't one in client mode. The editor reads its
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

@'
@echo off
REM Launch the Memstroy-inator editor through the WGPU graphics path.
REM Use this on machines where the normal OpenGL window opens black.
"%~dp0bin\memstroy-gui.exe" --graphics=safe %*
'@ | Set-Content -Path (Join-Path $BundleDir "Memstroy-inator-safe-graphics.bat") -Encoding ASCII

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
