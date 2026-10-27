# Prefetch CMake 4.4.3 + Ninja 1.13.2 via ghfast mirror, verify sha512, extract to C:\dev\tools.
$ErrorActionPreference = 'Stop'
$dl = 'C:\dev\tools\downloads'
New-Item -ItemType Directory -Force $dl | Out-Null
$tools = @(
    @{
        Name='cmake'
        Url='https://ghfast.top/https://github.com/Kitware/CMake/releases/download/v4.4.3/cmake-4.4.3-windows-x86_64.zip'
        File='cmake-4.4.3-windows-x86_64.zip'
        Sha512='5f8527e18bffdf5113a64238442093468fce80c8a5f99e1b5eb217e0d73796eb7a2a236ee0c522de3424cb87a1abb3153fcecdb20d8d338c8a607667c9b37c90'
        OutDir='C:\dev\cmake\cmake-4.4.3'
        ExeRel='bin\cmake.exe'
    },
    @{
        Name='ninja'
        Url='https://ghfast.top/https://github.com/ninja-build/ninja/releases/download/v1.13.2/ninja-win.zip'
        File='ninja-win-1.13.2.zip'
        Sha512='55d3d891e8fc6c8ad7f92e172125319896761e57c5125944613d9bbfa5b9374387e9fc1468ad5bcb31464f43fb1c455ea251343942595f42955dc67090aa12ee'
        OutDir='C:\dev\tools\ninja'
        ExeRel='ninja.exe'
    }
)

$7z = 'C:\Program Files\7-Zip\7z.exe'
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
    Write-Output "sha512 OK ($hash.Substring(0,16))"
    if (Test-Path $t.OutDir) { Remove-Item $t.OutDir -Recurse -Force }
    New-Item -ItemType Directory -Force $t.OutDir | Out-Null
    & $7z x $zip "-o$($t.OutDir)" -y | Out-Null
    $exe = Join-Path $t.OutDir $t.ExeRel
    # CMake zip top-level dir cmake-4.4.3-windows-x86_64/; flatten into OutDir/bin if needed
    if (-not (Test-Path $exe)) {
        $sub = Get-ChildItem $t.OutDir -Directory | Select-Object -First 1
        if ($sub) {
            $binDir = Join-Path $t.OutDir 'bin'
            New-Item -ItemType Directory -Force $binDir | Out-Null
            Copy-Item (Join-Path $sub.FullName 'bin\*') $binDir -Force -Recurse
            Copy-Item (Join-Path $sub.FullName 'share') $t.OutDir -Force -Recurse
        }
    }
    $exe = Join-Path $t.OutDir $t.ExeRel
    if (-not (Test-Path $exe)) { throw "executable not found: $exe" }
    Write-Output "extracted: $exe"
}

# Clean failed vcpkg partial downloads
Get-ChildItem 'C:\dev\vcpkg\downloads' -Filter '*.part' -ErrorAction SilentlyContinue | Remove-Item -Force
Write-Output '=== versions ==='
& 'C:\dev\cmake\cmake-4.4.3\bin\cmake.exe' --version | Select-Object -First 1
& 'C:\dev\tools\ninja\ninja.exe' --version
