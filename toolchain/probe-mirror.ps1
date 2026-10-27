$cands = @(
  'https://mirrors.tuna.tsinghua.edu.cn/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe',
  'https://mirrors.ustc.edu.cn/rust-static/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe',
  'https://rsproxy.cn/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe',
  'https://static.rust-lang.org/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe',
  'https://win.rustup.rs/x86_64',
  'https://rsproxy.cn/index/config.json',
  'https://mirrors.ustc.edu.cn/crates.io-index/config.json'
)
$lines = New-Object System.Collections.Generic.List[string]
foreach($u in $cands){
  try {
    $r = Invoke-WebRequest -Uri $u -Method Get -TimeoutSec 15 -UseBasicParsing -MaximumRedirection 5
    $len = $r.Headers['Content-Length']
    $lines.Add("OK  $u  HTTP $($r.StatusCode) len=$len")
  } catch {
    $code = if($_.Exception.Response){ [int]$_.Exception.Response.StatusCode } else { 'NA' }
    $lines.Add("ERR($code) $u  $($_.Exception.Message.Split([char]10)[0])")
  }
}
[IO.File]::WriteAllLines('C:\Users\user\Desktop\cloud-stream\toolchain\mirror-probe.txt', $lines, (New-Object Text.UTF8Encoding $false))
