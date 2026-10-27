$f = 'C:\Users\user\Desktop\cloud-stream\vendor\scrap\src\common\codec.rs'
$bak = 'C:\Users\user\Desktop\cloud-stream\vendor\scrap\src\common\codec.rs.pre-aom-strip.bak'
if (-not (Test-Path $bak)) { Copy-Item -LiteralPath $f -Destination $bak }

$lines = [IO.File]::ReadAllLines($f)
"before lines=$($lines.Count); last kept target=987"
# 983 fn disable_av1 ... 987 '}'; keep 1..987 (1-based)
$kept = $lines[0..986]
# 去掉文件尾部可能残留的空行，保留函数闭合
$kept = $kept.TrimEnd()
$text = ($kept -join "`r`n") + "`r`n"
[IO.File]::WriteAllText($f, $text, (New-Object Text.UTF8Encoding $false))
$after = [IO.File]::ReadAllLines($f)
"after lines=$($after.Count)"
"tail:"
$after | Select-Object -Last 6
