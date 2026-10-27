# 0e-4 被控端(CLIENT001)基线包装: 在 Console 1 交互会话内启动 bench-capture,
# 同步采样 CPU, 结果写 C:\dev\bench\bench-cpu.txt。由交互计划任务调用。
# 用法: run-bench.ps1 [seconds] [fps]
$ErrorActionPreference = 'Continue'
$exe = 'C:\dev\bench\bench-capture.exe'
$seconds = if ($args.Count -ge 1) { $args[0] } else { '20' }
$fps = if ($args.Count -ge 2) { $args[1] } else { '30' }
$dir = 'C:\dev\bench'
$stdout = Join-Path $dir 'bench-result.txt'
$stderr = Join-Path $dir 'bench-result.err.txt'
$summary = Join-Path $dir 'bench-cpu.txt'

$cores = [Environment]::ProcessorCount

"===== ENV =====" | Set-Content $summary -Encoding utf8
"time_start=" + (Get-Date -Format 'yyyy-MM-dd HH:mm:ss.fff') | Add-Content $summary -Encoding utf8
"user=" + $env:USERNAME | Add-Content $summary -Encoding utf8
"cores=$cores" | Add-Content $summary -Encoding utf8
$os = Get-CimInstance Win32_OperatingSystem
"mem_total_mb=" + [math]::Round($os.TotalVisibleMemorySize/1024) + " mem_free_mb=" + [math]::Round($os.FreePhysicalMemory/1024) | Add-Content $summary -Encoding utf8
try {
    Add-Type -AssemblyName System.Windows.Forms
    [System.Windows.Forms.Screen]::AllScreens | ForEach-Object {
        "display: " + $_.Bounds.Width + "x" + $_.Bounds.Height + " primary=" + $_.Primary
    } | Add-Content $summary -Encoding utf8
} catch { "display read failed: $($_.Exception.Message)" | Add-Content $summary -Encoding utf8 }

$sw = [Diagnostics.Stopwatch]::StartNew()
$p = Start-Process -FilePath $exe -ArgumentList $seconds, $fps -PassThru `
    -RedirectStandardOutput $stdout -RedirectStandardError $stderr -WindowStyle Hidden

$rows = @()
$cpuTotal = 0.0
while ($true) {
    Start-Sleep -Milliseconds 1000
    $proc = Get-Process -Id $p.Id -ErrorAction SilentlyContinue
    if ($null -eq $proc) { break }
    $cpuTotal = $proc.TotalProcessorTime.TotalSeconds
    $rows += [pscustomobject]@{
        t_s   = [math]::Round($sw.Elapsed.TotalSeconds, 2)
        cpu_s = [math]::Round($cpuTotal, 3)
    }
}
$sw.Stop()
$p.WaitForExit()
$exitCode = $p.ExitCode
if ($null -eq $exitCode) { $exitCode = 'unknown' }
$wall = $sw.Elapsed.TotalSeconds
$cpuPctAll = [math]::Round(100.0 * $cpuTotal / $wall / $cores, 1)
$cpuPctOne = [math]::Round(100.0 * $cpuTotal / $wall, 1)

"time_end=" + (Get-Date -Format 'yyyy-MM-dd HH:mm:ss.fff') | Add-Content $summary -Encoding utf8
"wall_s=" + [math]::Round($wall,3) + " cpu_s=" + [math]::Round($cpuTotal,3) + " cores=$cores" +
    " cpu_percent_allcores=$cpuPctAll cpu_percent_onecore=$cpuPctOne exit_code=$exitCode" |
    Add-Content $summary -Encoding utf8

"===== BENCH STDOUT =====" | Add-Content $summary -Encoding utf8
Get-Content $stdout | Add-Content $summary -Encoding utf8
"===== BENCH STDERR =====" | Add-Content $summary -Encoding utf8
Get-Content $stderr | Add-Content $summary -Encoding utf8
