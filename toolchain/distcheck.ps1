$ErrorActionPreference = 'SilentlyContinue'
Write-Output '=== index.html 插桩 ==='
Select-String -Path C:\dev\selkies-web\index.html -Pattern 'SELKIES_STREAMING_MODE' | ForEach-Object { $_.Line }
Write-Output '=== assets JS ==='
(Get-ChildItem C:\dev\selkies-web\assets -Filter '*.js' | Select-Object -ExpandProperty Name) -join ','
Write-Output '=== index.html hash ==='
Get-FileHash C:\dev\selkies-web\index.html -Algorithm SHA256 | Select-Object -ExpandProperty Hash
