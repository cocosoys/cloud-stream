#!/usr/bin/env bash
# Source this before `cargo` on the Windows build host:
#
#   . toolchain/env-check.sh && cargo check -p stream-server
#
# Why not `vcvars64.bat`: that script shells out to `reg.exe` (to read the
# installed-toolset registry keys), and this sandbox blocks `reg.exe` outright.
# The call dies before it exports anything useful, so `cl.exe` ends up on PATH
# with an empty `INCLUDE` and every C dependency fails with "cannot open
# include file: 'stdio.h'" — which reads like a broken crate, not a broken
# environment, and cost an hour the first time.
#
# The variables are spelled out instead. Two details matter:
#
#   * `INCLUDE`/`LIB` must be **Windows** paths. `cl.exe` is spawned by `cc`
#     directly (no shell in between), so nothing rewrites `/c/Program Files`
#     for it — a POSIX path there just means "header not found".
#   * PATH entries are POSIX because *this* shell is the one that resolves
#     them; cargo/cc find `cl.exe` on PATH and then use the separate INCLUDE.

MSVC_ROOT='C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC\14.44.35207'
SDK_ROOT='C:\Program Files (x86)\Windows Kits\10'
SDK_VER='10.0.22621.0'

export PATH="/c/Program Files (x86)/Microsoft Visual Studio/2022/BuildTools/VC/Tools/MSVC/14.44.35207/bin/Hostx64/x64:$HOME/.cargo/bin:/c/dev/git/cmd:/c/dev/vcpkg:/c/dev/cmake/cmake-4.4.3/bin:/c/dev/tools/ninja:/c/dev/tools/python3:/c/dev/llvm/bin:$PATH"

export INCLUDE="$MSVC_ROOT\\include;$SDK_ROOT\\Include\\$SDK_VER\\ucrt;$SDK_ROOT\\Include\\$SDK_VER\\um;$SDK_ROOT\\Include\\$SDK_VER\\shared;$SDK_ROOT\\Include\\$SDK_VER\\winrt;$SDK_ROOT\\Include\\$SDK_VER\\cppwinrt"
export LIB="$MSVC_ROOT\\lib\\x64;$SDK_ROOT\\Lib\\$SDK_VER\\ucrt\\x64;$SDK_ROOT\\Lib\\$SDK_VER\\um\\x64"

export VCPKG_ROOT="/c/dev/vcpkg"
export VCPKG_MAX_CONCURRENCY=2
export LIBCLANG_PATH="/c/dev/llvm/bin"

# opus is built once and kept outside the repo: the build script links a static
# lib, and rebuilding it per `cargo clean` was the slowest thing in the loop.
export LIBOPUS_DIR='C:\dev\opus-static'
export LIBOPUS_STATIC_ROOT='C:\dev\opus-static'
