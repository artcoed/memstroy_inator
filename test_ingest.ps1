$env:RUST_LOG = "info"
Write-Host "Starting test ingest..."

# Start server in background
$serverJob = Start-Job -ScriptBlock {
    param($path)
    Set-Location $path
    $env:RUST_LOG = "info"
    & "target\release\memstroy-assets-server.exe" --root "$env:USERPROFILE\.memstroy\cache"
} -ArgumentList (Get-Location).Path

Start-Sleep -Seconds 5

# Trigger ingest
Write-Host "Triggering ingest..."
$body = @{channel='mellstroy'; limit=3} | ConvertTo-Json
try {
    $result = Invoke-RestMethod -Uri 'http://localhost:8765/api/ingest/tg' -Method Post -Body $body -ContentType 'application/json'
    Write-Host "Ingest started: $($result | ConvertTo-Json)"
} catch {
    Write-Host "Error: $_"
}

# Wait and check files
Write-Host "Waiting 60 seconds for ingest to complete..."
Start-Sleep -Seconds 60

Write-Host "`nChecking files in clips directory:"
Get-ChildItem "$env:USERPROFILE\.memstroy\cache\clips" -File | Select-Object Name, Length | Format-Table

Write-Host "`nChecking txt files:"
Get-ChildItem "$env:USERPROFILE\.memstroy\cache\clips\*.txt" -ErrorAction SilentlyContinue | Select-Object Name

Write-Host "`nChecking thumbs:"
Get-ChildItem "$env:USERPROFILE\.memstroy\cache\clips\thumbs\*.jpg" -ErrorAction SilentlyContinue | Select-Object Name

# Get server output
Write-Host "`nServer output:"
Receive-Job -Job $serverJob

# Cleanup
Stop-Job -Job $serverJob
Remove-Job -Job $serverJob
