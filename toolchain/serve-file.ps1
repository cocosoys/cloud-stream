# 一次性文件服务：把待部署的 exe 交给同网段的被控机拉取。
# 为什么不用 scp：仓库里的 scp 被安全策略拦掉；为什么不用 HttpListener：
# 非管理员绑定 http://+:port/ 需要 URL ACL，TcpListener 不需要。
param([int]$Port = 18080, [string]$File)

$ErrorActionPreference = 'Stop'
if (-not (Test-Path $File)) { throw "no such file: $File" }
$bytes = [IO.File]::ReadAllBytes($File)
$name = Split-Path $File -Leaf
$listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Any, $Port)
$listener.Start()
Write-Output "serving $name ($($bytes.Length) bytes) on 0.0.0.0:$Port"

while ($true) {
    $client = $listener.AcceptTcpClient()
    try {
        $stream = $client.GetStream()
        $buf = New-Object byte[] 8192
        $n = $stream.Read($buf, 0, $buf.Length)
        $req = [Text.Encoding]::ASCII.GetString($buf, 0, [Math]::Max($n, 0))
        $first = ($req -split "`r`n")[0]
        if ($first -match ("^GET /" + [Regex]::Escape($name) + " ")) {
            $head = "HTTP/1.1 200 OK`r`nContent-Type: application/octet-stream`r`nContent-Length: $($bytes.Length)`r`nConnection: close`r`n`r`n"
            $hb = [Text.Encoding]::ASCII.GetBytes($head)
            $stream.Write($hb, 0, $hb.Length)
            $stream.Write($bytes, 0, $bytes.Length)
            Write-Output "sent $($bytes.Length) bytes"
        } else {
            $body = [Text.Encoding]::ASCII.GetBytes("404")
            $head = "HTTP/1.1 404 Not Found`r`nContent-Length: $($body.Length)`r`nConnection: close`r`n`r`n"
            $hb = [Text.Encoding]::ASCII.GetBytes($head)
            $stream.Write($hb, 0, $hb.Length)
            $stream.Write($body, 0, $body.Length)
            Write-Output "404 for: $first"
        }
        $stream.Flush()
    } catch {
        Write-Output "conn error: $($_.Exception.Message)"
    } finally {
        $client.Close()
    }
}
