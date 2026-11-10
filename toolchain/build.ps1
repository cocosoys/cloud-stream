<#
.SYNOPSIS
    在本仓库里跑 cargo，先把 MSVC 的构建环境导进来。

.DESCRIPTION
    这个脚本存在的原因是一个真实踩到的坑，不是洁癖。

    `rustc` 在 Windows/MSVC 上链接时会去 PATH 里找 `link.exe`。而在下面两种环境里
    这一步会静默走错或直接失败：

    * **Git Bash**：`PortableGit/usr/bin/link.exe` 是 GNU coreutils 的 `link`
      （一个要求"源 目标"两个参数的硬链接工具）。Windows 的进程搜索会给 `link`
      补上 `.exe`，于是 rustc 拿到的是 coreutils，报出
      `link: missing operand after '\377\376'` —— 一个 BOM 的十六进制，跟 Rust
      毫无关系，看日志的人会以为源码里混进了 BOM。
    * **裸 PowerShell / cmd**：PATH 里根本没有 MSVC，`cargo` 报
      `linker link.exe not found`。而 `cargo check` 是不链接的，所以"我刚 check
      过没问题"会让人误判代码有错。

    两者都不是代码问题，但都会让人以为是代码问题。`cargo check` 不链接这一点让它
    更具欺骗性：check 全绿，`cargo build` 立刻倒下。

    脚本做两件事：用 vswhere 找到 VS 的 MSVC 工具集、扫 Windows Kits 找 SDK，
    然后把 PATH / LIB / INCLUDE 拼成 MSVC 原生工具提示符里的那三份。之后转手给
    cargo，行为与在"x64 Native Tools Command Prompt for VS"里手敲完全一致。

.PARAMETER Task
    check（不链接，最快）、build（默认）、test（含 lib 单元测试）、clippy、doc、
    clippy-fix、clean、run。

.PARAMETER Package
    要操作的 crate，默认 stream-server。

.PARAMETER CargoArgs
    透传给 cargo 的额外参数，例如 `--lib`、`--release`、`-- --nocapture`。

.EXAMPLE
    .\toolchain\build.ps1 -Task test
    跑全部单元测试。

.EXAMPLE
    .\toolchain\build.ps1 -Task build -CargoArgs --release
    出 release 二进制。

.NOTES
    需要 VS 2019/2022 的 C++ 工具集（含 Windows SDK）。只装了 VS Code 不行 ——
    那是编辑器，不带编译器。
#>
[CmdletBinding()]
param(
    [ValidateSet('check', 'build', 'test', 'clippy', 'clippy-fix', 'doc', 'clean', 'run')]
    [string]$Task = 'build',

    [string]$Package = 'stream-server',

    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$CargoArgs = @()
)

$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot

# ---------------------------------------------------------------- VS / MSVC

function Find-MsvcToolset {
    <#
        返回 MSVC 工具集目录（…\VC\Tools\MSVC\14.44.35207），取版本最高的一个。
        多版本并存是常态（升级 VS 不会删旧的），拿最高的那个和 vcvars 的默认行为
        一致。
    #>
    $roots = New-Object System.Collections.Generic.List[string]

    # ① vswhere：登记的 VS 安装。注意它返回空数组是常态而非异常 —— 手工解压
    #    的 BuildTools、被精简过的安装、或者 Installer 状态损坏时，机器上明明有
    #    完整的工具集，vswhere 也会说没有（这台开发机就是 `[]`）。
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (Test-Path $vswhere) {
        $fromVswhere = & $vswhere -latest -products * `
            -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
            -property installationPath 2>$null
        if ($fromVswhere) { $roots.Add((@($fromVswhere)[0])) }
    }

    # ② 目录扫描：因为上一条不保证，这一段是并列的第一手段，不是兜底。
    #    结构固定为 <…>\Microsoft Visual Studio\<年份>\<版本>\VC\Tools\MSVC。
    foreach ($base in @($env:ProgramFiles, ${env:ProgramFiles(x86)})) {
        if (-not $base) { continue }
        $vsRoot = Join-Path $base 'Microsoft Visual Studio'
        if (-not (Test-Path $vsRoot)) { continue }
        Get-ChildItem $vsRoot -Directory -ErrorAction SilentlyContinue | ForEach-Object {
            Get-ChildItem $_.FullName -Directory -ErrorAction SilentlyContinue | ForEach-Object {
                if (Test-Path (Join-Path $_.FullName 'VC\Tools\MSVC')) { $roots.Add($_.FullName) }
            }
        }
    }

    foreach ($root in ($roots | Select-Object -Unique)) {
        $tools = Join-Path $root 'VC\Tools\MSVC'
        if (-not (Test-Path $tools)) { continue }
        $best = Get-ChildItem $tools -Directory -ErrorAction SilentlyContinue |
            Where-Object { Test-Path (Join-Path $_.FullName 'bin\Hostx64\x64\link.exe') } |
            Sort-Object { [version]($_.Name -replace '[^\d\.]', '') } -Descending |
            Select-Object -First 1
        if ($best) { return $best.FullName }
    }
    return $null
}

function Find-WindowsSdk {
    <#
        返回 SDK 版本号（10.0.22621.0）。判据是 Lib\<ver>\ucrt\x64 与
        Include\<ver>\um 同时存在 —— 只有 Include 没有 Lib 的 SDK 装不全，链接时
        会缺 libucrt.lib。
    #>
    $sdkRoot = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10'
    if (-not (Test-Path $sdkRoot)) { return $null }
    $best = Get-ChildItem (Join-Path $sdkRoot 'Lib') -Directory -ErrorAction SilentlyContinue |
        Where-Object {
            (Test-Path (Join-Path $_.FullName 'ucrt\x64')) -and
            (Test-Path (Join-Path $sdkRoot "Include\$($_.Name)\um"))
        } |
        Sort-Object { [version]$_.Name } -Descending |
        Select-Object -First 1
    if ($best) { return $best.Name }
    return $null
}

$msvc = Find-MsvcToolset
if (-not $msvc) {
    throw "没找到 MSVC C++ 工具集。装 Visual Studio 2019/2022 的「使用 C++ 的桌面开发」工作负载，或用 winget install Microsoft.VisualStudio.2022.BuildTools --override '--add Microsoft.VisualStudio.Workload.VCTools'。"
}
$sdkVersion = Find-WindowsSdk
if (-not $sdkVersion) {
    throw "没找到 Windows SDK（需要 Lib\<版本>\ucrt\x64）。在 VS Installer 里勾「Windows 11 SDK」。"
}
$sdkRoot = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10"

# 与前缀顺序有关：MSVC 的 link.exe 必须排在 Git 的 usr/bin 前面，
# 否则 Git Bash 场景下仍然是 coreutils 的 link 胜出。
$env:PATH = @(
    (Join-Path $msvc 'bin\Hostx64\x64')
    (Join-Path $sdkRoot "bin\$sdkVersion\x64")
    $env:PATH
) -join ';'

$env:LIB = @(
    (Join-Path $msvc 'lib\x64')
    (Join-Path $sdkRoot "Lib\$sdkVersion\ucrt\x64")
    (Join-Path $sdkRoot "Lib\$sdkVersion\um\x64")
) -join ';'

$env:INCLUDE = @(
    (Join-Path $msvc 'include')
    (Join-Path $sdkRoot "Include\$sdkVersion\ucrt")
    (Join-Path $sdkRoot "Include\$sdkVersion\um")
    (Join-Path $sdkRoot "Include\$sdkVersion\shared")
    (Join-Path $sdkRoot "Include\$sdkVersion\winrt")
    (Join-Path $sdkRoot "Include\$sdkVersion\cppwinrt")
) -join ';'

# ---------------------------------------------------------------- 原生依赖

# 这三个是 Cargo.toml 里 build script 要找的东西。它们以前写死在
# `install-vcpkg-libs.cmd` / `build-ws.cmd` 的 set 行里（VCPKG_ROOT=C:\dev\vcpkg、
# LIBCLANG_PATH=C:\dev\llvm\bin），换一台机器就得手工改脚本 —— 这是"别人克隆下来
# 编不过"的头号原因。这里改成：环境变量优先，没有就探默认位置，只在不缺的时候
# 才报错，并且把实际用的路径打出来。

function Set-NativeDep {
    param([string]$Name, [string[]]$Candidates, [string]$Hint)

    # 动态名字取环境变量只能用 .NET 或 Get-Item："env:$Name" 里的 $Name 不会被
    # 展开成一个合法的 Provider 路径写法以外的东西，$env:$Name 更是语法错误。
    $existing = [Environment]::GetEnvironmentVariable($Name)
    if ($existing) { return $existing }

    foreach ($c in $Candidates) {
        if ($c -and (Test-Path $c)) {
            [Environment]::SetEnvironmentVariable($Name, $c)
            return $c
        }
    }
    Write-Warning "$Name 未设置且没探到（试过：$($Candidates -join '、')）。$Hint"
    return $null
}

$vcpkg = Set-NativeDep -Name 'VCPKG_ROOT' -Candidates @(
    'C:\dev\vcpkg', 'C:\vcpkg', 'C:\src\vcpkg'
) -Hint 'libyuv / libvpx 走 vcpkg 静态三元组；按 README「构建」一节装好，或把 VCPKG_ROOT 指向安装目录。'

$libclang = Set-NativeDep -Name 'LIBCLANG_PATH' -Candidates @(
    'C:\dev\llvm\bin', (Join-Path $env:ProgramFiles 'LLVM\bin')
) -Hint 'bindgen 需要 libclang；装 LLVM 或把 LIBCLANG_PATH 指向它的 bin 目录。'

# libopus 是本地 cmake 构建的静态库（见 Cargo.toml:67），audiopus_sys 认
# LIBOPUS_LIB_DIR。三元组是 x64-windows（**不是** -static，两个目录同时存在时
# 选错会 LNK1104）。
$opusCandidates = @('C:\dev\opus-static\lib')
if ($vcpkg) { $opusCandidates += (Join-Path $vcpkg 'installed\x64-windows\lib') }
$opus = $env:LIBOPUS_LIB_DIR
if (-not $opus) {
    foreach ($c in $opusCandidates) {
        if (Test-Path (Join-Path $c 'opus.lib')) { $opus = $c; break }
    }
    if ($opus) { [Environment]::SetEnvironmentVariable('LIBOPUS_LIB_DIR', $opus) }
}
if (-not $opus) {
    Write-Warning "LIBOPUS_LIB_DIR 未设置且没探到 opus.lib（试过：$($opusCandidates -join '、')）。音频上行会链接失败，见 docs/stage3-audio-design.md。"
}

# ---------------------------------------------------------------- 汇报

$msvcVersion = Split-Path -Leaf $msvc
Write-Host "[build] MSVC $msvcVersion | SDK $sdkVersion | target $(if ($env:CARGO_BUILD_TARGET) { $env:CARGO_BUILD_TARGET } else { 'host' })" -ForegroundColor DarkGray
Write-Host "[build] vcpkg $(if ($vcpkg) { $vcpkg } else { '(缺失)' }) | llvm $(if ($libclang) { $libclang } else { '(缺失)' }) | opus $(if ($opus) { $opus } else { '(缺失)' })" -ForegroundColor DarkGray

# 兜底校验：PATH 改动后如果解析到的还不是我们那一个，说明有别的东西在抢。
$resolved = (Get-Command link.exe -ErrorAction SilentlyContinue).Source
if ($resolved -and $resolved -ne (Join-Path $msvc 'bin\Hostx64\x64\link.exe')) {
    Write-Warning "link.exe 解析到 $resolved，不是 MSVC 的那个；链接可能仍然失败。"
}

# ---------------------------------------------------------------- cargo

Push-Location $repoRoot
try {
    switch ($Task) {
        'check'      { cargo check  -p $Package @CargoArgs }
        'build'      { cargo build  -p $Package @CargoArgs }
        'test'       { cargo test   -p $Package @CargoArgs }
        'clippy'     { cargo clippy -p $Package @CargoArgs -- -D warnings }
        'clippy-fix' { cargo clippy -p $Package @CargoArgs --fix --allow-dirty --allow-staged }
        'doc'        { cargo doc    -p $Package @CargoArgs --no-deps --document-private-items }
        'clean'      { cargo clean  -p $Package @CargoArgs }
        'run'        { cargo run    -p $Package @CargoArgs }
    }
    $code = $LASTEXITCODE
}
finally {
    Pop-Location
}
exit $code
