# Local triplet: this machine has VS 2022 *BuildTools* only, and its
# installation is not registered with vswhere in a way vcpkg accepts
# ("Unable to find a valid Visual Studio instance"). The shell that runs
# vcpkg already exports cl.exe / INCLUDE / LIB from vcvars64.bat, so vcpkg
# must NOT try to locate or initialise a VS instance of its own.
#
# Keeping this here (rather than in the vcpkg tree) makes the workaround
# reproducible for anyone rebuilding the workspace.
set(VCPKG_TARGET_ARCHITECTURE x64)
set(VCPKG_CRT_LINKAGE static)
set(VCPKG_LIBRARY_LINKAGE static)
set(VCPKG_LOAD_VCVARS_ENV OFF)
