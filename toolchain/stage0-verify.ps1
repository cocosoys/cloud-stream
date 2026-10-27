====== 阶段0 环境自检（0d 验收前置） ======

$fail = 0
function Check($label, $scriptBlock) {
    try {
        $v = & $scriptBlock
        if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($v)) { throw 'empty/failed' }
        Write-Output ("[ OK ] {0}: {1}" -f $label, ($v -join ' ').Trim())
    } catch {
        Write-Output ("[FAIL] {0}: {1}" -f $label, $_.Exception.Message)
        $script:fail = 1
    }
}

Check 'rustc' { rustc --version }
Check 'cargo'  { cargo --version }
Check 'rustup' { rustup --version }
Check 'vcpkg' { vcpkg version }
Check 'git'   { git --version }

Write-Output ''
Write-Output '--- VCPKG_ROOT ---'
Write-Output ("VCPKG_ROOT = " + $env:VCPKG_ROOT)
Write-Output ("VCPKG_MAX_CONCURRENCY = " + $env:VCPKG_MAX_CONCURRENCY)

Write-Output ''
Write-Output '--- vcpkg installed packages ---'
if (Get-Command vcpkg -ErrorAction SilentlyContinue) {
    vcpkg list
} else {
    Write-Output 'vcpkg not on PATH yet'
}

Write-Output ''
Write-Output '--- MSVC / Windows SDK 探测 (vswhere) ---'
$vs = 'C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe'
if (Test-Path $vs) {
    $path = & $vs -latest -property installationPath
    if (-not [string]::IsNullOrWhiteSpace($path)) {
        Write-Output ("VS instance: " + $path)
        $msvc = Get-ChildItem (Join-Path $path 'VC\Tools\MSVC') -Directory -ErrorAction SilentlyContinue | Select-Object -Last 1
        if ($msvc) { Write-Output ("MSVC toolset: " + $msvc.Name) }
        $sdk = Get-ChildItem 'C:\Program Files (x86)\Windows Kits\10\Include' -Directory -ErrorAction SilentlyContinue | Select-Object -Last 1
        if ($sdk) { Write-Output ("Windows SDK: " + $sdk.Name) }
    } else {
        Write-Output '[FAIL] 未检测到 VS/Build Tools 实例'
        $script:fail = 1
    }
} else {
    Write-Output '[FAIL] vswhere 不存在'
    $script:fail = 1
}

Write-Output ''
if ($fail -eq 0) { Write-Output '=== 自检通过：可以进入 0d 空壳构建 ===' }
else { Write-Output '=== 自检存在未通过项，先按清单 Step 1/2/4 修复 ===' ; exit 1 }
