# Prefetch vcpkg internal tools (pwsh 7.6.6, python3.14.7 amd64) via ghfast mirror,
# verify sha512, extract under C:\dev\tools so vcpkg find_program hits them on PATH.
$ErrorActionPreference = 'Stop'
$dl = 'C:\dev\tools\downloads'
New-Item -ItemType Directory -Force $dl | Out-Null
$7z = 'C:\Program Files\7-Zip\7z.exe'

$tools = @(
    @{
        Name='pwsh'
        Url='https://ghfast.top/https://github.com/PowerShell/PowerShell/releases/download/v7.6.6/PowerShell-7.6.6-win-x64.zip'
        File='PowerShell-7.6.6-win-x64.zip'
        Sha512='d95037cfa3ea9df5287a409680bb2cf122451760e9b07e3d9b61cda2155262326911d24643cb1018da6fd899a9c5bd1f30055ca7b74f785b72b6dd99ef3a54fa'
        OutDir='C:\dev\tools\pwsh'
        ExeRel='pwsh.exe'
    },
    @{
        Name='python3'
        Url='https://www.python.org/ftp/python/3.14.7/python-3.14.7-embed-amd64.zip'
        File='python-3.14.7-embed-amd64.zip'
        Sha512='c62810365c120d192767c02b471ea38d100d29ae8332fa00318d1ee66fa7f75b14f01962d3a1b47e935994d2b1e4a76f3bc44bc2bc67cbf0618a2214dfb73a02'
        OutDir='C:\dev\tools\python3'
        ExeRel='python.exe'
    }
)

foreach ($t in $tools) {
    $zip = Join-Path $dl $t.File
    Write-Output "=== $($t.Name) ==="
    if (-not (Test-Path $zip)) {
        Write-Output "Downloading $($t.Url)"
        Invoke-WebRequest -Uri $t.Url -OutFile $zip -UseBasicParsing -TimeoutSec 600
    }
    $hash = (Get-FileHash $zip -Algorithm SHA512).Hash.ToLower()
    if ($hash -ne $t.Sha512) {
        throw "SHA512 mismatch for $($t.File)`n got=$hash`n exp=$($t.Sha512)"
    }
    Write-Output "sha512 OK"
    if (Test-Path $t.OutDir) { Remove-Item $t.OutDir -Recurse -Force }
    New-Item -ItemType Directory -Force $t.OutDir | Out-Null
    & $7z x $zip "-o$($t.OutDir)" -y | Out-Null
    if (-not (Test-Path (Join-Path $t.OutDir $t.ExeRel))) {
        $sub = Get-ChildItem $t.OutDir -Directory | Select-Object -First 1
        if ($sub) { Copy-Item (Join-Path $sub.FullName '*') $t.OutDir -Force -Recurse }
    }
    $exe = Join-Path $t.OutDir $t.ExeRel
    if (-not (Test-Path $exe)) { throw "executable not found: $exe" }
    Write-Output "extracted: $exe"
}

# Mirror archives into vcpkg downloads cache so vcpkg never re-downloads by hash
Copy-Item (Join-Path $dl 'PowerShell-7.6.6-win-x64.zip') 'C:\dev\vcpkg\downloads\' -Force
Copy-Item (Join-Path $dl 'python-3.14.7-embed-amd64.zip') 'C:\dev\vcpkg\downloads\' -Force

Write-Output '=== versions ==='
& 'C:\dev\tools\pwsh\pwsh.exe' -NoProfile -Command '$PSVersionTable.PSVersion.ToString()'
& 'C:\dev\tools\python3\python.exe' --version
