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

Probe 'libvpx archive direct' 'https://github.com/webmproject/libvpx/archive/v1.15.2.tar.gz'
Probe 'libvpx archive ghfast' 'https://ghfast.top/https://github.com/webmproject/libvpx/archive/v1.15.2.tar.gz'
Probe 'libvpx archive ghproxy' 'https://gh-proxy.com/https://github.com/webmproject/libvpx/archive/v1.15.2.tar.gz'
Probe 'libyuv googlesource' 'https://chromium.googlesource.com/libyuv/libyuv/+/0faf8dd0e004520a61a603a4d2996d5ecc80dc3f/?format=TEXT'
Probe 'libyuv github mirror' 'https://github.com/libyuv/libyuv'
Probe 'msys2 official' 'https://downloads.msys2.org/msys2/x86_64/'
Probe 'vcpkg-tool github' 'https://github.com/microsoft/vcpkg-tool/releases'

[IO.File]::WriteAllLines('C:\Users\user\Desktop\cloud-stream\toolchain\probe-net.txt', $lines, (New-Object Text.UTF8Encoding $false))
