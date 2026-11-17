# 逐区块体检 .mca：把"崩溃在哪个 chunk、坏成什么样"变成可读的一行行事实。
# 只读，不写任何文件。
param([string]$Root = 'C:\Users\Admin\Desktop\.minecraft\saves')

Add-Type -AssemblyName System.IO.Compression

function Analyze-Region([string]$path) {
    $bytes = [System.IO.File]::ReadAllBytes($path)
    $len = $bytes.Length
    $sectors = [int]($len / 4096)
    $total = 0; $ok = 0; $bad = @()
    for ($i = 0; $i -lt 1024; $i++) {
        $p = $i * 4
        $off = ($bytes[$p] -shl 16) -bor ($bytes[$p + 1] -shl 8) -bor $bytes[$p + 2]
        if ($off -eq 0) { continue }
        $total++
        $cnt = $bytes[$p + 3]
        $cx = $i % 32; $cz = [int][math]::Floor($i / 32)
        $start = $off * 4096
        if (($off + $cnt) -gt $sectors) {
            $bad += [pscustomobject]@{ chunk = "$cx,$cz"; kind = 'header-overrun'
                detail = "off=$off sectors=$cnt fileSectors=$sectors" }
            continue
        }
        if ($start + 5 -gt $len) {
            $bad += [pscustomobject]@{ chunk = "$cx,$cz"; kind = 'beyond-eof'; detail = "start=$start len=$len" }
            continue
        }
        # 4 字节长度 + 1 字节压缩类型
        $dlen = [BitConverter]::ToInt32($bytes, $start)
        $ctype = $bytes[$start + 4]
        if ($dlen -le 0 -or ($start + 4 + $dlen) -gt $len) {
            $bad += [pscustomobject]@{ chunk = "$cx,$cz"; kind = 'bad-length'
                detail = "declared=$dlen avail=$($len - $start - 4) ctype=$ctype" }
            continue
        }
        # 声明的数据区是不是一整片 0（写回缓存丢了，文件长度在、内容是空洞）
        $zeros = 0
        for ($k = $start + 5; $k -lt ($start + 4 + $dlen) -and $zeros -lt 64; $k++) {
            if ($bytes[$k] -eq 0) { $zeros++ } else { break }
        }
        if ($zeros -ge 64) {
            $bad += [pscustomobject]@{ chunk = "$cx,$cz"; kind = 'zero-hole'; detail = "declared=$dlen ctype=$ctype" }
            continue
        }
        if ($ctype -ne 2) {
            $bad += [pscustomobject]@{ chunk = "$cx,$cz"; kind = 'ctype'; detail = "ctype=$ctype declared=$dlen" }
            continue
        }
        $ms = New-Object System.IO.MemoryStream(,$bytes[($start + 5)..($start + 4 + $dlen - 1)])
        $ds = New-Object System.IO.Compression.DeflateStream($ms, [System.IO.Compression.CompressionMode]::Decompress)
        try {
            $sink = New-Object System.IO.MemoryStream
            $ds.CopyTo($sink)
            $ok++
            $sink.Dispose()
        } catch {
            $bad += [pscustomobject]@{ chunk = "$cx,$cz"; kind = 'zlib-eof'
                detail = ("declared={0} msg={1}" -f $dlen, $_.Exception.Message) }
        } finally {
            $ds.Dispose(); $ms.Dispose()
        }
    }
    "== {0}  len={1} sectors={2} chunks={3} ok={4} bad={5}" -f (Split-Path $path -Leaf), $len, $sectors, $total, $ok, $bad.Count
    foreach ($b in $bad) { "   BAD chunk {0}  {1}  {2}" -f $b.chunk, $b.kind, $b.detail }
}

foreach ($w in Get-ChildItem $Root -Directory) {
    foreach ($sub in @('region', 'DIM-1\region', 'DIM1\region')) {
        $dir = Join-Path $w.FullName $sub
        if (-not (Test-Path $dir)) { continue }
        foreach ($f in Get-ChildItem $dir -Filter *.mca) {
            "[$($w.Name)\$sub]"
            Analyze-Region $f.FullName
        }
    }
}
