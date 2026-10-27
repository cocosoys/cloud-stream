# Remote log triage: fallback / gdi / cursor / resolution events (latest 200 lines filtered)
$log = 'C:\dev\stream\logs\stream-server.log'
if (-not (Test-Path $log)) { Write-Output "NO_LOG $log"; exit 1 }
Get-Content $log -Tail 200 |
  Where-Object { $_ -match 'fallback|gdi|cursor|WouldBlock|Rebuild|resolution|capture|pipeline loop|encoder' } |
  Select-Object -Last 60
