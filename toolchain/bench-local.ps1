# 0e-3 / 0e-4 本机基线：运行 bench-capture, 同步采样进程 CPU 时间与墙钟, 输出帧级指标。
# 用法: pwsh -File bench-local.ps1 [seconds] [fps]
$ErrorActionPreference = 'Stop'
$exe = 'C:\Users\user\Desktop\cloud-stream\target\debug\bench-capture.exe'
$seconds = if ($args.Count -ge 1) { $args[0] } else { '20' }
$fps = if ($args.Count -ge 2) { $args[1] } else { '30' }
$tag = "local-" + $seconds + "s-" + $fps + "fps"
$out = "C:\Users\user\Desktop\cloud-stream\toolchain\bench-$tag.txt"
$err = "C:\Users\user\Desktop\cloud-stream\toolchain\bench-$tag.err.txt"
$cpuSummary = "C:\Users\user\Desktop\cloud-stream\toolchain\bench-$tag-cpu.txt"
$cpuSamples = "C:\Users\user\Desktop\cloud-stream\toolchain\bench-$tag-cpu-samples.csv"

$cores = [Environment]::ProcessorCount
$sw = [Diagnostics.Stopwatch]::StartNew()
$p = Start-Process -FilePath $exe -ArgumentList $seconds, $fps -PassThru `
    -RedirectStandardOutput $out -RedirectStandardError $err -WindowStyle Hidden

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

"wall_s=$([math]::Round($wall,3)) cpu_s=$([math]::Round($cpuTotal,3)) cores=$cores cpu_percent_allcores=$cpuPctAll cpu_percent_onecore=$cpuPctOne exit_code=$exitCode" |
    Set-Content -Path $cpuSummary -Encoding utf8
if ($rows.Count -gt 0) {
    $rows | Export-Csv -Path $cpuSamples -NoTypeInformation -Encoding utf8
}

Write-Output '===== bench stdout ====='
Get-Content $out
Write-Output '===== bench stderr ====='
Get-Content $err
Write-Output '===== cpu summary ====='
Get-Content $cpuSummary
if ($rows.Count -gt 0) {
    Write-Output '===== cpu samples ====='
    Get-Content $cpuSamples
}
