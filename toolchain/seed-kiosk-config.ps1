# 在被控机的 stream-config.toml 里补上 kiosk 段（幂等）。
#
# 用 UTF-8 无 BOM 写回：TOML 解析器不认 BOM，而 PowerShell 5.1 的
# `-Encoding UTF8` 会加一个。读全文再写回比 Add-Content 可控。

param(
    [string]$ConfigPath = 'C:\Program Files\cloud-stream\stream-config.toml',
    [string]$PasswordHash = ''
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path $ConfigPath)) { throw "no config at $ConfigPath" }
Copy-Item $ConfigPath ($ConfigPath + '.bak') -Force

$text = [IO.File]::ReadAllText($ConfigPath)

# 删掉上一次写进去的 kiosk 段，再重写：这样重复执行不会留下两份键
# （TOML 里同名键出现两次是错误，不是"后一个赢"）。
$lines = $text -split "`r?`n"
$kept = New-Object System.Collections.Generic.List[string]
$skipping = $false
foreach ($line in $lines) {
    if ($line -match '^#\s*-+\s*输入锁定') { $skipping = $true; continue }
    if ($skipping) {
        if ($line -match '^kiosk' -or $line -match '^\s*$' -or $line -match '^#') { continue }
        $skipping = $false
    }
    $kept.Add($line)
}
$text = ($kept -join "`r`n").TrimEnd()

$block = @()

$block += ''
$block += '# ---- 输入锁定（kiosk）----'
$block += '# 把网页进来的输入关在游戏里：注入端过滤 + 注册表策略 + 低级键盘钩子。'
$block += 'kiosk = true'
$block += '# 游戏启动后自动上锁。本轮验证先关着，需要在面板上手动点。'
$block += 'kiosk_autolock = false'
$block += 'kiosk_policy = true'
$block += 'kiosk_hook = true'
if ($PasswordHash -ne '') {
    $block += '# 口令哈希，用 stream-server.exe --kiosk-hash <口令> 生成。'
    $block += ('kiosk_password = "' + $PasswordHash + '"')
}

$text = $text + "`r`n" + ($block -join "`r`n") + "`r`n"
[IO.File]::WriteAllText($ConfigPath, $text, (New-Object System.Text.UTF8Encoding $false))

Write-Output ('config bytes = ' + ([IO.File]::ReadAllBytes($ConfigPath)).Length)
Select-String -Path $ConfigPath -Pattern '^kiosk' | ForEach-Object { $_.Line }
