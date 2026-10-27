vcpkg_check_linkage(ONLY_STATIC_LIBRARY)

# Source: GitHub SSH mirror of chromium.googlesource.com/libyuv/libyuv
# (google source host unreachable from this network; GitHub 22/tcp SSH is open).
# Mirror HEAD pinned at 47ba8e2160b71a7cf95c0720cd8944ab771e533e (LIBYUV_VERSION 1974).
vcpkg_from_git(
    OUT_SOURCE_PATH SOURCE_PATH
    URL git@github.com:lemenkov/libyuv.git
    REF 47ba8e2160b71a7cf95c0720cd8944ab771e533e
)

# Stage 0/1: MSVC direct compile only. clang-cl acceleration is deferred to stage 3
# after the capture/encode baseline numbers are recorded.
vcpkg_cmake_configure(
    SOURCE_PATH "${SOURCE_PATH}"
    DISABLE_PARALLEL_CONFIGURE
    OPTIONS
        -DLIBYUV_DISABLE_JPEG=ON
    OPTIONS_DEBUG
        -DCMAKE_DEBUG_POSTFIX=d
)

vcpkg_cmake_install()
vcpkg_copy_pdbs()

# Upstream CMakeLists always builds the shared target and tools; strip them out
# of the static triplet layout.
if(VCPKG_LIBRARY_LINKAGE STREQUAL "static")
    file(REMOVE_RECURSE "${CURRENT_PACKAGES_DIR}/debug/bin" "${CURRENT_PACKAGES_DIR}/bin")
endif()

file(REMOVE_RECURSE "${CURRENT_PACKAGES_DIR}/debug/include")
file(REMOVE_RECURSE "${CURRENT_PACKAGES_DIR}/debug/share")

file(MAKE_DIRECTORY "${CURRENT_PACKAGES_DIR}/share/${PORT}")
configure_file("${CMAKE_CURRENT_LIST_DIR}/libyuv-config.cmake"
    "${CURRENT_PACKAGES_DIR}/share/${PORT}/libyuv-config.cmake" COPYONLY)
file(INSTALL "${SOURCE_PATH}/LICENSE"
    DESTINATION "${CURRENT_PACKAGES_DIR}/share/${PORT}" RENAME copyright)
