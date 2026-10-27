$env:Path = 'C:\dev\git\cmd;' + $env:Path
$repos = @('webmproject/libyuv','lemenkov/libyuv','google/libyuv','RustDesk/libyuv','libyuv/libyuv')
$lines = New-Object System.Collections.Generic.List[string]
foreach($r in $repos){
  $o = git ls-remote "git@github.com:$r.git" HEAD 2>&1
  $code = $LASTEXITCODE
  $lines.Add("== $r exit=$code")
  foreach($x in ($o | Select-Object -First 2)){ $lines.Add("  $x") }
}
[IO.File]::WriteAllLines('C:\Users\user\Desktop\cloud-stream\toolchain\probe-git.txt', $lines, (New-Object Text.UTF8Encoding $false))
