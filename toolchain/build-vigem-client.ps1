<#
.SYNOPSIS
    Build ViGEmClient.dll (x64) into vendor\vigem\.

.DESCRIPTION
    Produces the one binary this repo vendors for #17. Run it once when the
    pinned ViGEmClient commit changes; the result is committed so neither the
    build machine nor the controlled machine ever needs a C++ toolchain.

    Three things about this build are not obvious, and all three cost a cycle
    to discover:

    1. This vcpkg checkout has no `vigemclient` port (2883 ports, none match),
       so there is nothing to install. The DLL is compiled straight from the
       pinned upstream commit.

    2. Upstream's CMakeLists builds its SHARED target *without*
       VIGEM_DYNAMIC / VIGEM_EXPORTS. Without those two defines VIGEM_API
       expands to nothing, the linker exports zero symbols, and the resulting
       DLL loads fine while every GetProcAddress fails. The .vcxproj has them;
       the CMake path does not. That is why this script calls cl.exe directly
       instead of driving CMake.

    3. The deploy machine's vcvars is a BuildTools-only install; cmake's VS
       generator refuses to find it ("could not find any instance of Visual
       Studio"), so the toolchain is assembled by path, the same way
       deploy.ps1's Initialize-BuildEnv does it.

.OUTPUTS
    vendor\vigem\ViGEmClient.dll  (~137 KB, x64, /MT so it carries its CRT)

.EXAMPLE
    pwsh toolchain/build-vigem-client.ps1
    pwsh toolchain/build-vigem-client.ps1 -Source C:\src\ViGEmClient
#>
[CmdletBinding()]
param(
    # Checkout of github.com/nefarius/ViGEmClient. Cloned on demand when absent.
    [string]$Source,
    # Must track the ViGEmBus release the controlled machine runs.
    [string]$Commit = 'b66d02d57e32cc8595369c53418b843e958649b4'
)

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
if (-not $Source) { $Source = Join-Path $repo 'target\vigem-src' }
$out = Join-Path $repo 'vendor\vigem'

# ---- source ---------------------------------------------------------------
if (-not (Test-Path (Join-Path $Source 'src\ViGEmClient.cpp'))) {
    Write-Host "      ..   cloning ViGEmClient $Commit" -ForegroundColor DarkGray
    git clone --quiet https://github.com/nefarius/ViGEmClient.git $Source
    if ($LASTEXITCODE -ne 0) { throw 'git clone failed' }
    git -C $Source checkout --quiet $Commit
    if ($LASTEXITCODE -ne 0) { throw "git checkout $Commit failed" }
}
$head = (git -C $Source rev-parse HEAD).Trim()
if ($head -ne $Commit) {
    Write-Host "      !!   source is at $head, expected $Commit" -ForegroundColor Yellow
}
Write-Host "      OK   source $head" -ForegroundColor Green

# ---- MSVC environment ----------------------------------------------------
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vsRoot = $null
if (Test-Path $vswhere) { $vsRoot = & $vswhere -latest -products * -property installationPath }
if (-not $vsRoot) { $vsRoot = 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools' }

$toolset = Get-ChildItem "$vsRoot\VC\Tools\MSVC" -Directory -ErrorAction SilentlyContinue |
    Sort-Object Name -Descending | Select-Object -First 1
if (-not $toolset) { throw "no MSVC toolset under $vsRoot\VC\Tools\MSVC" }
$sdkVer = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\Include" -Directory -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -match '^10\.\d+\.\d+\.\d+$' } | Sort-Object Name -Descending | Select-Object -First 1
if (-not $sdkVer) { throw 'no Windows SDK Include directory' }
$sdk = "${env:ProgramFiles(x86)}\Windows Kits\10"

$env:PATH = "$($toolset.FullName)\bin\Hostx64\x64;$sdk\bin\$($sdkVer.Name)\x64;$env:PATH"
$env:INCLUDE = @(
    "$($toolset.FullName)\include"
    "$sdk\Include\$($sdkVer.Name)\ucrt"
    "$sdk\Include\$($sdkVer.Name)\um"
    "$sdk\Include\$($sdkVer.Name)\shared"
) -join ';'
$env:LIB = @(
    "$($toolset.FullName)\lib\x64"
    "$sdk\Lib\$($sdkVer.Name)\ucrt\x64"
    "$sdk\Lib\$($sdkVer.Name)\um\x64"
) -join ';'
Write-Host "      OK   MSVC $($toolset.Name) / SDK $($sdkVer.Name)" -ForegroundColor Green

# ---- build ---------------------------------------------------------------
New-Item -ItemType Directory -Force -Path $out | Out-Null
$dll = Join-Path $out 'ViGEmClient.dll'

Push-Location $Source
try {
    # /MT keeps the CRT inside the DLL: the host binary's /MD vs /MT choice is
    # then irrelevant across the C boundary (no allocator hand-off — everything
    # allocated inside is freed by vigem_free inside).
    & cl.exe /nologo /LD /O2 /MT /W3 `
        /DWIN32_LEAN_AND_MEAN /DVIGEM_DYNAMIC /DVIGEM_EXPORTS `
        /I include /I src `
        src\ViGEmClient.cpp `
        /link /INCREMENTAL:NO setupapi.lib /OUT:"$dll"
    if ($LASTEXITCODE -ne 0) { throw "cl.exe failed with $LASTEXITCODE" }
} finally {
    Pop-Location
    # Only the DLL ships; the import library and object file stay behind.
    foreach ($junk in 'ViGEmClient.lib', 'ViGEmClient.exp', 'ViGEmClient.obj') {
        $p = Join-Path $Source $junk
        if (Test-Path $p) { Remove-Item $p -Force }
    }
}

# ---- verify --------------------------------------------------------------
# The whole reason this script does not use CMake: a build without the two
# defines succeeds and produces a DLL that exports nothing. Check every symbol
# the Rust side resolves, and fail the build if one is missing.
$dumpbin = Join-Path $toolset.FullName 'bin\Hostx64\x64\dumpbin.exe'
# Last column of each export line is the symbol name (ordinal, hint, RVA, name).
$exported = @{}
foreach ($line in (& $dumpbin /exports $dll)) {
    if ($line -match '^\s*\d+\s+[0-9A-Fa-f]+\s+[0-9A-Fa-f]+\s+(\S+)\s*$') {
        $exported[$Matches[1]] = $true
    }
}
$needed = @(
    'vigem_alloc', 'vigem_free', 'vigem_connect', 'vigem_disconnect'
    'vigem_target_x360_alloc', 'vigem_target_free', 'vigem_target_add'
    'vigem_target_remove', 'vigem_target_x360_update'
    'vigem_target_x360_register_notification', 'vigem_target_x360_unregister_notification'
)
$missing = @($needed | Where-Object { -not $exported.ContainsKey($_) })
if ($missing.Count -gt 0) {
    throw "ViGEmClient.dll is missing exports: $($missing -join ', ') — were VIGEM_DYNAMIC/VIGEM_EXPORTS dropped?"
}

$size = (Get-Item $dll).Length
Write-Host ("      OK   {0}  {1:N0} B" -f $dll, $size) -ForegroundColor Green
Write-Host ("      OK   {0} exports verified" -f $needed.Count) -ForegroundColor Green
$hash = (Get-FileHash $dll -Algorithm SHA256).Hash
Write-Host "      ..   sha256 $hash" -ForegroundColor DarkGray
Write-Host "      !!   update vendor\vigem\README.md with this hash if it changed" -ForegroundColor Yellow
