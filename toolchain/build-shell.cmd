@echo off
rem Initialize MSVC environment, then run cargo for the shell smoke crate.
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul
set "PATH=%USERPROFILE%\.cargo\bin;C:\dev\git\cmd;C:\dev\vcpkg;%PATH%"
set VCPKG_ROOT=C:\dev\vcpkg
set VCPKG_MAX_CONCURRENCY=2
cd /d C:\Users\user\Desktop\cloud-stream\crates\shell
cargo build %*
