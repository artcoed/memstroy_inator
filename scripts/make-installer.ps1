# scripts/make-installer.ps1
#
# Build a Windows installer (.exe) for the memstroy-inator client
# bundle produced by `scripts/package-client.ps1`.
#
# Backend: Inno Setup. We generate an .iss script on the fly and feed
# it to ISCC.exe (the Inno Setup Compiler). Inno Setup is free,
# distributable and the de-facto standard for "single .exe installer
# with menu / desktop shortcuts and an uninstaller registered in
# Add/Remove Programs". Nothing about the build that ends up in the
# bundle changes; we just wrap it.
#
# What the produced installer does on the target machine (defaults):
#   * Installs into "%ProgramFiles%\memstroy-inator" (admin) or
#     "%LocalAppData%\Programs\memstroy-inator" (per-user).
#   * Creates a Start Menu group "memstroy-inator" with the editor
#     and the uninstaller.
#   * Creates a Desktop shortcut to the editor.
#   * Registers an entry in Settings → Apps → Installed apps so it
#     appears in the standard Windows uninstall flow.
#
# Usage:
#   pwsh scripts/make-installer.ps1 -ServerUrl https://assets.example.com
#
#   # reuse an already-staged bundle (skips cargo build)
#   pwsh scripts/make-installer.ps1 -BundleDir .\dist\memstroy-inator-windows-amd64-0.1.0
#
#   # custom output dir / installer name / Inno Setup compiler path
#   pwsh scripts/make-installer.ps1 -ServerUrl https://assets.example.com `
#       -Out .\build -Name memstroy-inator-1.2.3 `
#       -IsccPath "C:\Program Files (x86)\Inno Setup 6\ISCC.exe"
#
# Required (one of):
#   -ServerUrl <URL>       Forwarded to package-client.ps1 to build a
#                          fresh bundle. Either this or -BundleDir
#                          must be supplied.
#   -BundleDir <path>      Reuse a bundle directory already produced
#                          by package-client.ps1.
#
# Optional:
#   -Out <path>            Output directory (default: bundle's parent,
#                          which is usually .\dist).
#   -Name <name>           Installer base name (without "-Setup.exe");
#                          defaults to the bundle directory name.
#   -IsccPath <path>       Path to ISCC.exe. Auto-detected from PATH
#                          and standard install locations if omitted.
#   -AllowLoopback         Forwarded to package-client.ps1.
#
# Output: a single file "<Out>\<Name>-Setup.exe" — that's the installer.

[CmdletBinding()]
param(
    [string] $ServerUrl     = "",
    [string] $BundleDir     = "",
    [string] $Out           = "",
    [string] $Name          = "",
    [string] $IsccPath      = "",
    [switch] $AllowLoopback
)

$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RootDir   = Split-Path -Parent $ScriptDir
Set-Location $RootDir

if ([string]::IsNullOrWhiteSpace($BundleDir) -and [string]::IsNullOrWhiteSpace($ServerUrl)) {
    Write-Error "error: pass either -BundleDir <path> or -ServerUrl <URL>"
    exit 2
}

# ── Build the bundle if not pre-supplied ────────────────────────────
if ([string]::IsNullOrWhiteSpace($BundleDir)) {
    Write-Host "==> building client bundle via scripts/package-client.ps1"
    # Use hashtable splatting so named parameters (-ServerUrl, -AllowLoopback)
    # bind by name regardless of the receiving script's positional rules.
    # Array splatting can mis-bind "-ServerUrl" as the *value* of $ServerUrl
    # when the target parameter has an implicit position.
    $pkgArgs = @{ ServerUrl = $ServerUrl }
    if ($AllowLoopback) { $pkgArgs.AllowLoopback = $true }
    & (Join-Path $ScriptDir "package-client.ps1") @pkgArgs
    if ($LASTEXITCODE -ne 0) { throw "package-client.ps1 failed" }

    # Mirror package-client.ps1's naming convention to recover the
    # path it just produced.
    $Version = (Select-String -Path "Cargo.toml" -Pattern '^version\s*=\s*"([^"]+)"' `
        | Select-Object -First 1).Matches[0].Groups[1].Value
    if ([string]::IsNullOrWhiteSpace($Version)) { $Version = "dev" }
    $Arch       = $env:PROCESSOR_ARCHITECTURE.ToLower()
    $BundleName = "memstroy-inator-windows-$Arch-$Version"
    $BundleDir  = Join-Path $RootDir "dist\$BundleName"
}

if (-not (Test-Path $BundleDir -PathType Container)) {
    Write-Error "error: bundle directory does not exist: $BundleDir"
    exit 3
}
$BundleDirAbs   = (Resolve-Path $BundleDir).Path
$BundleBaseName = Split-Path $BundleDirAbs -Leaf
$GuiExe         = Join-Path $BundleDirAbs "bin\memstroy-gui.exe"
if (-not (Test-Path $GuiExe)) {
    Write-Error "error: $GuiExe not found"
    exit 3
}

# ── Locate branded app icon inside the bundle ───────────────────────
# package-client.ps1 stages catost.ico at the bundle root. We use it
# for Setup.exe's own Explorer icon (SetupIconFile), the Add/Remove
# Programs entry (UninstallDisplayIcon) and the Start Menu / Desktop
# shortcuts (IconFilename). When the file is missing we fall back to
# Inno Setup's default and the .exe's embedded icon, so an older
# bundle without the .ico still installs cleanly.
$BundleIconIco = Join-Path $BundleDirAbs "catost.ico"
$HasBundleIcon = Test-Path $BundleIconIco

if ([string]::IsNullOrWhiteSpace($Out)) {
    $Out = Split-Path $BundleDirAbs -Parent
}
New-Item -ItemType Directory -Force -Path $Out | Out-Null
$Out = (Resolve-Path $Out).Path

if ([string]::IsNullOrWhiteSpace($Name)) {
    $Name = $BundleBaseName
}

# ── Locate ISCC.exe (Inno Setup compiler) ───────────────────────────
function Find-Iscc {
    param([string] $Hint)
    if (-not [string]::IsNullOrWhiteSpace($Hint) -and (Test-Path $Hint)) {
        return (Resolve-Path $Hint).Path
    }
    $cmd = Get-Command iscc.exe -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $candidates = @(
        "$env:ProgramFiles\Inno Setup 6\ISCC.exe",
        "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
        "$env:ProgramFiles\Inno Setup 5\ISCC.exe",
        "${env:ProgramFiles(x86)}\Inno Setup 5\ISCC.exe"
    )
    foreach ($c in $candidates) {
        if (-not [string]::IsNullOrWhiteSpace($c) -and (Test-Path $c)) { return $c }
    }
    return $null
}

$IsccExe = Find-Iscc -Hint $IsccPath
if (-not $IsccExe) {
    Write-Error @"
error: Inno Setup Compiler (ISCC.exe) was not found.

Install Inno Setup 6 from https://jrsoftware.org/isinfo.php
(it is free for any use, including commercial), then either:

  * make sure ISCC.exe is on PATH, or
  * pass the full path via -IsccPath.

Example default install location:
    "C:\Program Files (x86)\Inno Setup 6\ISCC.exe"
"@
    exit 4
}
Write-Host "==> using Inno Setup compiler: $IsccExe"

# ── Read workspace version (used for Inno's AppVersion field) ───────
$Version = (Select-String -Path "Cargo.toml" -Pattern '^version\s*=\s*"([^"]+)"' `
    | Select-Object -First 1).Matches[0].Groups[1].Value
if ([string]::IsNullOrWhiteSpace($Version)) { $Version = "0.0.0" }

# Inno Setup's `VersionInfoVersion` / `VersionInfoProductVersion`
# directives expect a 4-part dotted version (Win32 VS_VERSIONINFO
# format: "major.minor.patch.build"). The workspace version is
# usually 3-part SemVer ("0.1.0"); pad missing components with "0".
$VersionParts = @($Version -split '\.')
while ($VersionParts.Count -lt 4) { $VersionParts += '0' }
$VersionInfoVersion = ($VersionParts[0..3]) -join '.'

$Year            = (Get-Date).Year
$AppId           = "{{B5A4F3A2-9A01-4E69-9E2E-MEMSTROYINATOR}}"  # stable GUID for upgrades
$AppName         = "memstroy-inator"
$AppPublisher    = "memstroy-inator contributors"
$AppExeName      = "memstroy-gui.exe"
$InstallerStem   = "$Name-Setup"
$InstallerPath   = Join-Path $Out "$InstallerStem.exe"

# ── Generate a temporary .iss script ────────────────────────────────
# Notes on the .iss directives we use:
#   * `Source: "<bundle>\*"; DestDir: "{app}"; Flags: recursesubdirs`
#     copies the entire staged bundle as-is.
#   * `PrivilegesRequiredOverridesAllowed=dialog commandline` lets the
#     user pick admin (Program Files) vs. per-user (LocalAppData)
#     install at runtime — Windows-default UX, no special config.
#   * Start Menu entry, Desktop shortcut and uninstaller are added by
#     Inno Setup's stock `[Icons]` block; the uninstaller registration
#     in Settings → Apps is automatic.
$IssTemp = New-Item -ItemType Directory -Force `
    -Path (Join-Path $env:TEMP "memstroy-inator-iss-$([System.Guid]::NewGuid().ToString('N'))")
$IssPath = Join-Path $IssTemp "installer.iss"

$IssLines = @(
    '; Auto-generated by scripts/make-installer.ps1 — do not edit.'
    '[Setup]'
    "AppId=$AppId"
    "AppName=$AppName"
    "AppVersion=$Version"
    "AppPublisher=$AppPublisher"
    "DefaultDirName={autopf}\$AppName"
    "DefaultGroupName=$AppName"
    "OutputDir=$Out"
    "OutputBaseFilename=$InstallerStem"
    # ── Installer compression ───────────────────────────────────────
    # We deliberately do NOT use `lzma2/ultra`. The combination of an
    # ultra-LZMA2 self-extracting payload + an unsigned PE looks, to
    # several AV heuristics (notably Microsoft Defender's Wacatac.B!ml
    # and Skyhigh's `BehavesLike.ObfuscatedPoly`), exactly like a
    # crypter / packer artefact, and reliably triggers false
    # positives on VirusTotal even though the contents are a stock
    # Rust release binary. `lzma2/normal` keeps installer size within
    # ~10–15% of ultra while substantially reducing payload entropy.
    'Compression=lzma2/normal'
    'SolidCompression=yes'
    'WizardStyle=modern'
    'PrivilegesRequired=lowest'
    'PrivilegesRequiredOverridesAllowed=dialog commandline'
    'ArchitecturesInstallIn64BitMode=x64'
    'UninstallDisplayName=memstroy-inator'
    $(if ($HasBundleIcon) {
        # Branded icon shipped in the bundle: point both the Setup.exe
        # itself (icon shown for the .exe in Explorer) and the
        # uninstaller entry in Settings -> Apps at the catost logo.
        # `SetupIconFile` is a path to a .ico relative to the
        # OutputDir / SourceDir Inno Setup uses; we already pass an
        # absolute path so resolution is unambiguous.
        "SetupIconFile=$BundleIconIco"
    } else {
        '; SetupIconFile= (no bundled icon found, using Inno default)'
    })
    $(if ($HasBundleIcon) {
        # Once installed, point the Add/Remove Programs entry at the
        # copy of the .ico the installer drops next to the binary.
        # Falling back to the .exe is fine because build.rs now
        # embeds the same icon as a PE resource, but referencing the
        # .ico explicitly is one less surface that depends on the
        # resource compiler having run on the build host.
        'UninstallDisplayIcon={app}\catost.ico'
    } else {
        "UninstallDisplayIcon={app}\bin\$AppExeName"
    })
    # ── PE VersionInfo block ─────────────────────────────────────────
    # Populates the Properties → Details tab of the generated
    # Setup.exe. A blank VersionInfo block is itself a heuristic
    # signal for AV ML models ("unsigned PE with no metadata"), so
    # filling these fields is one of the cheapest ways to cut the
    # false-positive rate without paying for code-signing.
    "VersionInfoVersion=$VersionInfoVersion"
    "VersionInfoProductVersion=$VersionInfoVersion"
    "VersionInfoCompany=$AppPublisher"
    "VersionInfoProductName=$AppName"
    "VersionInfoDescription=$AppName installer"
    "VersionInfoOriginalFileName=$InstallerStem.exe"
    "VersionInfoCopyright=Copyright (c) $Year $AppPublisher"
    ''
    '[Languages]'
    'Name: "en"; MessagesFile: "compiler:Default.isl"'
    'Name: "ru"; MessagesFile: "compiler:Languages\Russian.isl"'
    ''
    '[Tasks]'
    'Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: checkedonce'
    ''
    '[Files]'
    "Source: `"$BundleDirAbs\*`"; DestDir: `"{app}`"; Flags: ignoreversion recursesubdirs createallsubdirs"
    ''
    '[Icons]'
    $(if ($HasBundleIcon) {
        # Anchor every shortcut to {app}\catost.ico so the Start Menu
        # entry and Desktop shortcut survive a future change in how
        # the .exe carries (or doesn't carry) its embedded icon.
        "Name: `"{group}\memstroy-inator`"; Filename: `"{app}\bin\$AppExeName`"; IconFilename: `"{app}\catost.ico`""
    } else {
        "Name: `"{group}\memstroy-inator`"; Filename: `"{app}\bin\$AppExeName`""
    })
    "Name: `"{group}\{cm:UninstallProgram,memstroy-inator}`"; Filename: `"{uninstallexe}`""
    $(if ($HasBundleIcon) {
        "Name: `"{autodesktop}\memstroy-inator`"; Filename: `"{app}\bin\$AppExeName`"; IconFilename: `"{app}\catost.ico`"; Tasks: desktopicon"
    } else {
        "Name: `"{autodesktop}\memstroy-inator`"; Filename: `"{app}\bin\$AppExeName`"; Tasks: desktopicon"
    })
    ''
    '[Run]'
    "Filename: `"{app}\bin\$AppExeName`"; Description: `"{cm:LaunchProgram,memstroy-inator}`"; Flags: nowait postinstall skipifsilent"
)
[System.IO.File]::WriteAllLines($IssPath, $IssLines, (New-Object System.Text.UTF8Encoding $false))

Write-Host "==> compiling installer with ISCC"
Write-Host "    bundle    : $BundleDirAbs"
Write-Host "    output    : $InstallerPath"
if ($HasBundleIcon) {
    Write-Host "    app icon  : $BundleIconIco"
} else {
    Write-Host "    app icon  : (none — install will use Inno default)"
}

& $IsccExe "/Q" $IssPath
if ($LASTEXITCODE -ne 0) { throw "ISCC.exe failed (exit $LASTEXITCODE)" }

# Best-effort cleanup of the temp .iss directory.
try { Remove-Item -Recurse -Force $IssTemp } catch { }

if (-not (Test-Path $InstallerPath)) {
    throw "expected installer at $InstallerPath but ISCC produced nothing"
}

$SizeBytes = (Get-Item $InstallerPath).Length
$SizeMB    = "{0:N1}" -f ($SizeBytes / 1MB)

Write-Host "==> done"
Write-Host "    installer : $InstallerPath"
Write-Host "    size      : $SizeMB MiB ($SizeBytes bytes)"
Write-Host ""
Write-Host "Distribute the file above; users double-click it to install."
