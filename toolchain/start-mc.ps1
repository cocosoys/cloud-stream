# MC 1.12.2-Forge 14.23.5.2864 命令行直启（免启动器，输出日志供排障）
#
# 状态（2026-11-11）：**不属于 cloud-stream 的构建或安装链**。它是游戏主机上
# 手动排障时用的辅助脚本，留在仓库里只因为它记着几件踩过的事：
#   * 1.12.2 的 classpath 必须从 version json 的 downloads.artifact.path 拼，
#     不能按 name 猜；natives 不能上 classpath。
#   * 参数要拼成**一个字符串**再交给 Start-Process —— 用数组传会被含空格的
#     路径拆断，症状是"启动到一半找不到主类"。
#   * 三处路径是本机写死的：$mc（.minecraft）/ $ver / $java（jdk8）。换机器
#     先改这三行，别处不用动。
# 目标主机 192.168.59.148 现在连不上（2026-11-11 实测 ssh 超时），.150 上既
# 没有 C:\dev\mc 也没有 jdk8 —— 所以这份大概是它唯一还能读到的副本。真要恢复
# 这套 MC 试玩链，按上面三条核一遍再跑。
$ErrorActionPreference = 'Stop'
$mc = 'C:\Users\Administrator\Desktop\.minecraft'
$ver = '1.12.2-Forge_14.23.5.2864'
$java = 'C:\dev\jdk8\jdk8u504-b01\bin\java.exe'
$log = 'C:\dev\mc\mc.log'
New-Item -ItemType Directory -Force -Path 'C:\dev\mc' | Out-Null

# ---- classpath：json libraries（downloads.artifact.path 优先，跳过 natives） ----
$json = Get-Content "$mc\versions\$ver\$ver.json" -Raw | ConvertFrom-Json
$cp = New-Object System.Collections.Generic.List[string]
foreach ($lib in $json.libraries) {
    $path = $null
    if ($lib.downloads -and $lib.downloads.artifact -and $lib.downloads.artifact.path) {
        $path = $lib.downloads.artifact.path
    } elseif ($lib.name -match '^([^:]+):([^:]+):([^:]+)$') {
        $path = "libraries\$($Matches[1] -replace '\.','\')\$($Matches[2])\$($Matches[3])\$($Matches[2])-$($Matches[3]).jar"
    }
    if ($path) {
        # artifact.path 相对 libraries 根：.minecraft\libraries\<path>
        $full = Join-Path (Join-Path $mc "libraries") $path
        if (Test-Path $full) { $cp.Add($full) } else { "MISSING_LIB=$path" | Out-File $log -Encoding utf8 -Append }
    }
}
$cp.Add("$mc\versions\$ver\$ver.jar")
$classpath = ($cp -join ';')

"=== start $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss') ===" | Out-File $log -Encoding utf8 -Append
"classpath_items=$($cp.Count)" | Out-File $log -Encoding utf8 -Append

# ---- 单字符串参数（避免 Start-Process 数组把含空格路径拆断） ----
$argStr = "-Xmx2G -Xms512M `"-Djava.library.path=$mc\natives-tmp`" -cp `"$classpath`" net.minecraft.launchwrapper.Launch --username CloudPlayer --version $ver --gameDir `"$mc`" --assetsDir `"$mc\assets`" --assetIndex 1.12 --uuid 00000000-0000-0000-0000-000000000000 --accessToken 0 --userType legacy --versionType Forge --tweakClass net.minecraftforge.fml.common.launcher.FMLTweaker"

$p = Start-Process -FilePath $java -ArgumentList $argStr -WorkingDirectory $mc -WindowStyle Normal -PassThru
Start-Sleep -Seconds 8
if ($p.HasExited) {
    "MC_EXITED code=$($p.ExitCode)" | Out-File $log -Encoding utf8 -Append
} else {
    "PID=$($p.Id)" | Out-File $log -Encoding utf8 -Append
}
