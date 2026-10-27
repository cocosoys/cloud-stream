$lines = New-Object System.Collections.Generic.List[string]
function Add($s){ $lines.Add($s) }

Add('=== rust 1.75.0 components on static.rust-lang.org (GET) ===')
foreach($f in 'rust-1.75.0-x86_64-pc-windows-msvc.tar.gz.sha256','cargo-1.75.0-x86_64-pc-windows-msvc.tar.gz.sha256','rust-std-1.75.0-x86_64-pc-windows-msvc.tar.gz.sha256'){
  $u = "https://static.rust-lang.org/dist/$f"
  try { $r = Invoke-WebRequest -Uri $u -Method Get -UseBasicParsing -TimeoutSec 20; Add("OK  $f HTTP $($r.StatusCode)") }
  catch { $code = if($_.Exception.Response){[int]$_.Exception.Response.StatusCode}else{'NA'}; Add("ERR($code) $f") }
}

Add('')
Add('=== git ls-remote vcpkg through mirrors ===')
$env:Path = 'C:\dev\git\cmd;' + $env:Path
$mirrors = @(
  'https://ghfast.top/https://github.com/microsoft/vcpkg.git',
  'https://gh-proxy.com/https://github.com/microsoft/vcpkg.git',
  'https://mirror.ghproxy.com/https://github.com/microsoft/vcpkg.git',
  'https://github.com/microsoft/vcpkg.git'
)
foreach($m in $mirrors){
  Add("--- $m")
  $job = Start-Job { param($u) & git ls-remote $u HEAD 2>&1 | Select-Object -First 2 } -ArgumentList $m
  if(Wait-Job $job -Timeout 25){ Add((Receive-Job $job | Out-String).Trim()) } else { Add('TIMEOUT 25s'); Stop-Job $job }
  Remove-Job $job -Force
}

[IO.File]::WriteAllLines('C:\Users\user\Desktop\cloud-stream\toolchain\probe2.txt', $lines, (New-Object Text.UTF8Encoding $false))
