param(
    [string]$Api = "https://web-production-00687.up.railway.app",
    [string]$Kind = "clip",
    [int]$Limit = 1
)

$ErrorActionPreference = "Continue"
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$api = $Api.TrimEnd("/")
$listUrl = "$api/api/assets?kind=$Kind&limit=$Limit"

Write-Host "== Memstroy assets download diagnostics =="
Write-Host "API:  $api"
Write-Host "Kind: $Kind"
Write-Host ""

Write-Host "== Listing =="
try {
    $list = Invoke-RestMethod $listUrl
    $list | ConvertTo-Json -Depth 8
} catch {
    Write-Host "LIST ERROR:"
    Write-Host $_
    exit 1
}

$item = $list.items | Select-Object -First 1
if ($null -eq $item -or [string]::IsNullOrWhiteSpace($item.id)) {
    Write-Host ""
    Write-Host "NO ASSET ID IN LIST RESPONSE"
    exit 1
}

$id = [string]$item.id
$encodedId = [Uri]::EscapeDataString($id)
$directUrl = "$api/api/assets/$encodedId/download"
$proxyUrl = "${directUrl}?proxy=1"

Write-Host ""
Write-Host "== Selected asset =="
Write-Host "id:         $id"
Write-Host "file_name:  $($item.file_name)"
Write-Host "size_bytes: $($item.size_bytes)"
Write-Host ""

Write-Host "== Direct HEAD (usually redirects to bucket) =="
curl.exe -sS -I --connect-timeout 10 --max-time 30 $directUrl

Write-Host ""
Write-Host "== Direct 1-byte GET with redirects =="
curl.exe -sS -L --range 0-0 --connect-timeout 10 --max-time 30 -D - -o NUL $directUrl

Write-Host ""
Write-Host "== Proxy HEAD (bytes through Railway API) =="
curl.exe -sS -I --connect-timeout 10 --max-time 30 $proxyUrl

Write-Host ""
Write-Host "== Proxy 1-byte GET =="
curl.exe -sS -L --range 0-0 --connect-timeout 10 --max-time 30 -D - -o NUL $proxyUrl
