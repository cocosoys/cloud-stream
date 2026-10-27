@echo off
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" > "C:\Users\user\Desktop\cloud-stream\toolchain\vcvars-log.txt" 2>&1
echo === where link ===
where link.exe
echo === where cl ===
where cl.exe
echo INCLUDE_FIRST=%INCLUDE:~0,120%
echo LIB_FIRST=%LIB:~0,120%
echo VCINSTALLDIR=%VCINSTALLDIR%
echo VCToolsInstallDir=%VCToolsInstallDir%
