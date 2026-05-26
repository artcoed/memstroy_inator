# scripts/build-ico.ps1
#
# Generate a multi-resolution .ico from the source catost.png.
# Requires ffmpeg on PATH. Produces catost.ico with 16/32/48/64/128/256 px.
#
# Usage:
#   pwsh scripts/build-ico.ps1

$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RootDir   = Split-Path -Parent $ScriptDir
Set-Location $RootDir

$SrcPng  = "assets\internal_images\catost.png"
$DstIco  = "assets\internal_images\catost.ico"
$TmpDir  = Join-Path $env:TEMP "memstroy-ico-build"

if (-not (Test-Path $SrcPng)) {
    Write-Error "Source PNG not found: $SrcPng"
    exit 1
}

# Create temp dir for intermediate PNGs
if (Test-Path $TmpDir) { Remove-Item -Recurse -Force $TmpDir }
New-Item -ItemType Directory -Path $TmpDir | Out-Null

$Sizes = @(16, 32, 48, 64, 128, 256)

# Generate resized PNGs via ffmpeg
foreach ($s in $Sizes) {
    $out = Join-Path $TmpDir "${s}.png"
    & ffmpeg -hide_banner -loglevel error -i $SrcPng -vf "scale=${s}:${s}" -y $out
    if ($LASTEXITCODE -ne 0) { throw "ffmpeg failed for size $s" }
}

# Build ICO binary manually (ICO format spec)
$pngDataList = @()
foreach ($s in $Sizes) {
    $pngPath = Join-Path $TmpDir "${s}.png"
    $pngDataList += ,[System.IO.File]::ReadAllBytes($pngPath)
}

$stream = New-Object System.IO.MemoryStream
$writer = New-Object System.IO.BinaryWriter($stream)

# ICO Header
$writer.Write([UInt16]0)              # Reserved
$writer.Write([UInt16]1)              # Type: 1 = ICO
$writer.Write([UInt16]$Sizes.Count)   # Number of images

# Calculate initial data offset: header(6) + directory(16 * count)
$dataOffset = 6 + 16 * $Sizes.Count

# Write directory entries
for ($i = 0; $i -lt $Sizes.Count; $i++) {
    $s = $Sizes[$i]
    $pngBytes = $pngDataList[$i]
    $dim = if ($s -ge 256) { [byte]0 } else { [byte]$s }

    $writer.Write([byte]$dim)          # Width (0 = 256)
    $writer.Write([byte]$dim)          # Height (0 = 256)
    $writer.Write([byte]0)             # Color palette
    $writer.Write([byte]0)             # Reserved
    $writer.Write([UInt16]1)           # Color planes
    $writer.Write([UInt16]32)          # Bits per pixel
    $writer.Write([UInt32]$pngBytes.Length)  # Image data size
    $writer.Write([UInt32]$dataOffset)      # Offset to image data

    $dataOffset += $pngBytes.Length
}

# Write image data
for ($i = 0; $i -lt $Sizes.Count; $i++) {
    $writer.Write($pngDataList[$i])
}

$writer.Flush()
[System.IO.File]::WriteAllBytes((Join-Path $RootDir $DstIco), $stream.ToArray())
$writer.Close()
$stream.Close()

# Cleanup temp files
Remove-Item -Recurse -Force $TmpDir

$fileSize = (Get-Item (Join-Path $RootDir $DstIco)).Length
Write-Host "Generated $DstIco ($($Sizes.Count) sizes: $($Sizes -join ', ') px) — $fileSize bytes"
