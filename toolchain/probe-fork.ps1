$repos = @('rustdesk-org/rust-webm','rustdesk-org/nokhwa','rustdesk-org/confy','rustdesk-org/tokio-socks','rustdesk-org/sysinfo','rustdesk-org/default_net','rustdesk-org/machine-uid','rustdesk-org/webrtc')
$lines = foreach ($r in $repos) {
  $q = & 'C:\dev\git\cmd\git.exe' ls-remote "git@github.com:$r.git" HEAD 2>&1
  "{0} exit={1} :: {2}" -f $r, $LASTEXITCODE, ($q -join ' | ')
}
[IO.File]::WriteAllText('C:\Users\user\Desktop\cloud-stream\toolchain\fork-probe.txt', ($lines -join "`r`n") + "`r`n", (New-Object Text.UTF8Encoding $false))
$lines
