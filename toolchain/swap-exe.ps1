param([string]$Dir = 'C:\Program Files\cloud-stream')

$ErrorActionPreference = 'Stop'
$exe = Join-Path $Dir 'stream-server.exe'
$new = $exe + '.new'
$prev = $exe + '.prev'

Get-Process stream-server -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 700

if (-not (Test-Path $new)) { throw "no staged binary at $new" }
if (Test-Path $exe) { Copy-Item $exe $prev -Force }
Move-Item $new $exe -Force

$item = Get-Item $exe
Write-Output ("exe bytes = " + $item.Length)
Write-Output ("exe mtime = " + $item.LastWriteTime.ToString('yyyy-MM-dd HH:mm:ss'))
Write-Output ("prev kept = " + (Test-Path $prev))
