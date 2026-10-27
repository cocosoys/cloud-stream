@echo off
rem Initialize MSVC + portable toolchain, then run cargo in the cloud-stream workspace.
rem Usage: build-ws.cmd <cargo args>  e.g. build-ws.cmd build -p bench-capture -j2
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul
set "PATH=%USERPROFILE%\.cargo\bin;C:\dev\git\cmd;C:\dev\vcpkg;C:\dev\cmake\cmake-4.4.3\bin;C:\dev\tools\ninja;C:\dev\tools\pwsh;C:\dev\tools\python3;C:\dev\llvm\bin;C:\Program Files\7-Zip;%PATH%"
set "VCPKG_ROOT=C:\dev\vcpkg"
set "VCPKG_MAX_CONCURRENCY=2"
set "LIBCLANG_PATH=C:\dev\llvm\bin"
cd /d C:\Users\user\Desktop\cloud-stream
cargo %*
