# ====== Stage 0 toolchain environment variables (User scope, idempotent) ======
# Run on main SERVER001 (192.168.59.142); no admin required.

$ErrorActionPreference = 'Stop'

# 1) vcpkg root
$vcpkgRoot = 'C:\dev\vcpkg'
[Environment]::SetEnvironmentVariable('VCPKG_ROOT', $vcpkgRoot, 'User')
$env:VCPKG_ROOT = $vcpkgRoot

# 2) 8GB RAM guard: cap vcpkg compile concurrency to 2
[Environment]::SetEnvironmentVariable('VCPKG_MAX_CONCURRENCY', '2', 'User')
$env:VCPKG_MAX_CONCURRENCY = '2'

# 3) bindgen / clang-cli locate libclang.dll
[Environment]::SetEnvironmentVariable('LIBCLANG_PATH', 'C:\dev\llvm\bin', 'User')
$env:LIBCLANG_PATH = 'C:\dev\llvm\bin'

# 4) user PATH append (deduplicated); drop stale 3.31.6 entry
$cargoBin = "$env:USERPROFILE\.cargo\bin"
$paths = @(
    $cargoBin,
    $vcpkgRoot,
    'C:\dev\git\cmd',
    'C:\dev\cmake\cmake-4.4.3\bin',
    'C:\dev\tools\ninja',
    'C:\dev\tools\pwsh',
    'C:\dev\tools\python3',
    'C:\dev\llvm\bin',
    'C:\Program Files\7-Zip'
)
$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$userPath = ($userPath -split ';' | Where-Object { $_ -and $_ -notmatch 'cmake-3\.31\.6' }) -join ';'
foreach ($p in $paths) {
    if ($userPath -notlike "*$p*") {
        $userPath = ($userPath.TrimEnd(';') + ';' + $p)
    }
}
[Environment]::SetEnvironmentVariable('Path', $userPath, 'User')
$env:Path = ($env:Path.TrimEnd(';') + ';' + ($paths -join ';'))

Write-Output "VCPKG_ROOT=$env:VCPKG_ROOT"
Write-Output "VCPKG_MAX_CONCURRENCY=$env:VCPKG_MAX_CONCURRENCY"
Write-Output "LIBCLANG_PATH=$env:LIBCLANG_PATH"
Write-Output 'PATH updated (User level). Reopen terminal for full effect.'
