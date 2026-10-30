# 指针速度诊断探针（只读 + 一次可回滚的写入尝试）
#
# 用途：在被控端回答「为什么 SPI_SETMOUSESPEED(15) 之后读回来还是 10」。
# 分别看三处事实，任何一处不一致都能定位到具体环节：
#   1. HKCU\Control Panel\Mouse 的 MouseSpeed —— 写是否落盘
#   2. SPI_GETMOUSESPEED 读回 —— 系统内存里的当前值（应用真正读的那个）
#   3. SPI_SETMOUSESPEED 两种参数形态的返回值 + last error
#
# 参数：
#   -Speed 要尝试的速度步（1..20），默认 15
#   -Apply 真正执行写入尝试；不带则完全只读
#   -Restore 结束后把速度恢复成调用前的值（-Apply 时默认建议带上）
param(
    [int]$Speed = 15,
    [switch]$Apply,
    [switch]$Restore
)

$ErrorActionPreference = 'Continue'

Add-Type -MemberDefinition @'
[DllImport("user32.dll", SetLastError=true, EntryPoint="SystemParametersInfoW")]
public static extern bool SetInt(uint action, uint uiParam, IntPtr pvParam, uint winIni);
[DllImport("user32.dll", SetLastError=true, EntryPoint="SystemParametersInfoW")]
public static extern bool GetInt(uint action, uint uiParam, ref int pvParam, uint winIni);
'@ -Name Spi -Namespace W32

$GETSPEED = 0x0070
$SETSPEED = 0x0071
$UPDATE = 0x0001
$SENDCHANGE = 0x0002

function Read-SpiSpeed {
    $v = 0
    $ok = [W32.Spi]::GetInt($GETSPEED, 0, [ref]$v, 0)
    [pscustomobject]@{ Ok = $ok; Value = $v; LastError = [Runtime.InteropServices.Marshal]::GetLastWin32Error() }
}

# MouseSensitivity 才是速度步（1..20）。MouseSpeed 是「增强指针精确度」开关（0/1/2），
# 名字很像，但和速度不是一件事。
function Read-RegSpeed {
    try { (Get-ItemProperty 'HKCU:\Control Panel\Mouse' -Name MouseSensitivity -ErrorAction Stop).MouseSensitivity }
    catch { '<missing>' }
}

function Read-RegAccel {
    try { (Get-ItemProperty 'HKCU:\Control Panel\Mouse' -Name MouseSpeed -ErrorAction Stop).MouseSpeed }
    catch { '<missing>' }
}

$me = Get-Process -Id $PID
"USER=$env:USERNAME SESSION=$($me.SessionId) ELEVATED=$((New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator))"
"REG_BEFORE sensitivity=$(Read-RegSpeed) accel=$(Read-RegAccel)"
$before = Read-SpiSpeed
"SPI_BEFORE ok=$($before.Ok) speed=$($before.Value) err=$($before.LastError)"

if (-not $Apply) {
    'MODE=readonly'
    exit 0
}

# 形态 A：值放 uiParam，pvParam 为空（本项目的写法）
[Runtime.InteropServices.Marshal]::GetLastWin32Error() | Out-Null
$okA = [W32.Spi]::SetInt($SETSPEED, [uint32]$Speed, [IntPtr]::Zero, [uint32]($UPDATE -bor $SENDCHANGE))
$errA = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
$afterA = Read-SpiSpeed
"FORM_A(uiParam=$Speed,flags=UPDATE|SENDCHANGE) ok=$okA err=$errA spi=$($afterA.Value) reg=$(Read-RegSpeed)"

# 形态 B：值放 pvParam，uiParam 为 0（SDK 文档的写法）
[Runtime.InteropServices.Marshal]::GetLastWin32Error() | Out-Null
$okB = [W32.Spi]::SetInt($SETSPEED, 0, [IntPtr][int]$Speed, [uint32]($UPDATE -bor $SENDCHANGE))
$errB = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
$afterB = Read-SpiSpeed
"FORM_B(pvParam=$Speed,flags=UPDATE|SENDCHANGE) ok=$okB err=$errB spi=$($afterB.Value) reg=$(Read-RegSpeed)"

# 形态 B 去掉广播：只落盘，不挨个窗口投 WM_SETTINGCHANGE
[Runtime.InteropServices.Marshal]::GetLastWin32Error() | Out-Null
$okC = [W32.Spi]::SetInt($SETSPEED, 0, [IntPtr][int]$Speed, [uint32]$UPDATE)
$errC = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
$afterC = Read-SpiSpeed
"FORM_B_NOBCAST(pvParam=$Speed,flags=UPDATE) ok=$okC err=$errC spi=$($afterC.Value) reg=$(Read-RegSpeed)"

if ($Restore) {
    $target = if ($before.Ok) { $before.Value } else { 10 }
    $okR = [W32.Spi]::SetInt($SETSPEED, 0, [IntPtr][int]$target, [uint32]($UPDATE -bor $SENDCHANGE))
    $afterR = Read-SpiSpeed
    "RESTORE(to=$target) ok=$okR spi=$($afterR.Value) reg=$(Read-RegSpeed)"
}
