@echo off
rem Usage: install-vcpkg-libs.cmd libyuv | "libvpx[realtime]"
rem Port argument may carry feature brackets; triplet x64-windows-static is appended.
setlocal

call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul

set "PATH=%USERPROFILE%\.cargo\bin;C:\dev\git\cmd;C:\dev\vcpkg;C:\dev\cmake\cmake-4.4.3\bin;C:\dev\tools\ninja;C:\dev\tools\pwsh;C:\dev\tools\python3;C:\dev\llvm\bin;C:\Program Files\7-Zip;%PATH%"
set VCPKG_ROOT=C:\dev\vcpkg
set VCPKG_MAX_CONCURRENCY=2
set LIBCLANG_PATH=C:\dev\llvm\bin
set OVERLAY=C:\Users\user\Desktop\cloud-stream\vcpkg-ports

set PORT=%1
if "%PORT%"=="" (
  echo Usage: %0 libyuv ^| "libvpx[realtime]"
  exit /b 2
)

echo === Installing %PORT%:x64-windows-static (overlay %OVERLAY%) ===
vcpkg install "%PORT%:x64-windows-static" --overlay-ports="%OVERLAY%"
if errorlevel 1 (
  echo [FAIL] vcpkg install %PORT%
  exit /b 1
)
echo [OK] %PORT% installed.
endlocal
