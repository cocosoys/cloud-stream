$lines = New-Object System.Collections.Generic.List[string]
function Probe($label,$u){
  try {
    $r = Invoke-WebRequest -Uri $u -Method Head -UseBasicParsing -TimeoutSec 15 -MaximumRedirection 5
    $len = $r.Headers['Content-Length']
    $lines.Add("OK  $label [$($r.StatusCode)] len=$len")
  } catch {
    $code = if($_.Exception.Response){[int]$_.Exception.Response.StatusCode}else{'NA'}
    $lines.Add("ERR($code) $label :: $u")
  }
}
$pkg = 'msys/x86_64/msys2-runtime-3.6.10-4-x86_64.pkg.tar.zst'
Probe 'msys tuna' "https://mirrors.tuna.tsinghua.edu.cn/msys2/$pkg"
Probe 'msys ustc' "https://mirrors.ustc.edu.cn/msys2/$pkg"
Probe 'msys yandex' "https://mirror.yandex.ru/mirrors/msys2/$pkg"
Probe 'msys selfnet' "https://mirror.selfnet.de/msys2/$pkg"
Probe 'msys repo.msys2.org' "https://repo.msys2.org/$pkg"
Probe 'libyuv ghfast archive commit' 'https://ghfast.top/https://github.com/libyuv/libyuv/archive/0faf8dd0e004520a61a603a4d2996d5ecc80dc3f.tar.gz'

[IO.File]::WriteAllLines('C:\Users\user\Desktop\cloud-stream\toolchain\probe-net2.txt', $lines, (New-Object Text.UTF8Encoding $false))
