$lines = New-Object System.Collections.Generic.List[string]
function Add($s){ $lines.Add($s) }

$bt = 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools'
$bin = Join-Path $bt 'VC\Tools\MSVC\14.44.35207\bin\Hostx64\x64'
foreach($n in 'link.exe','cl.exe','lib.exe','nmake.exe'){
  $f = Join-Path $bin $n
  if(Test-Path $f){ Add("$n OK $([math]::Round((Get-Item $f).Length/1KB,0)) KB") } else { Add("$n MISSING") }
}
Add('--- vcvars ---')
foreach($p in (Join-Path $bt 'VC\Auxiliary\Build\vcvars64.bat'), (Join-Path $bt 'Common7\Tools\vsdevcmd.bat')){
  Add("$p : $(Test-Path $p)")
}
$inc = Join-Path $bt 'VC\Tools\MSVC\14.44.35207\include'
Add("MSVC include files: $((Get-ChildItem $inc -File -ErrorAction SilentlyContinue | Measure-Object).Count)")
$sdklib = 'C:\Program Files (x86)\Windows Kits\10\Lib\10.0.22621.0\um\x64'
Add("SDK x64 lib files: $((Get-ChildItem $sdklib -File -ErrorAction SilentlyContinue | Measure-Object).Count)")
$sdkinc = 'C:\Program Files (x86)\Windows Kits\10\Include\10.0.22621.0\um'
Add("SDK um include files: $((Get-ChildItem $sdkinc -File -ErrorAction SilentlyContinue | Measure-Object).Count)")

Add('--- MSVC subdirs ---')
Get-ChildItem (Join-Path $bt 'VC\Tools\MSVC\14.44.35207') -Directory -ErrorAction SilentlyContinue | ForEach-Object { Add("  " + $_.Name) }

Add('--- vswhere -all ---')
$vs = 'C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe'
Add((& $vs -all -products * -format json 2>&1 | Out-String))

Add('--- env VCINSTALLDIR ---')
Add("VCINSTALLDIR=$env:VCINSTALLDIR")
Add("VSINSTALLDIR=$env:VSINSTALLDIR")

$out = 'C:\Users\user\Desktop\cloud-stream\toolchain\vc-probe.txt'
[IO.File]::WriteAllLines($out, $lines, (New-Object Text.UTF8Encoding $false))
