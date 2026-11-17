cmake_minimum_required(VERSION 3.20)

# GNU-syntax (AT&T) assembly (src/core/simd/checksum_avx2.S) uses ELF-only
# GAS directives (.type/.size/.note.GNU-stack) and assumes no C-symbol
# underscore prefix, so it's only ever compiled on Linux (see the
# CMAKE_SYSTEM_NAME check below) — MSVC has no GNU-syntax assembler at
# all, and macOS's Mach-O assembler rejects those ELF directives.
# checksum.c falls back to the always-correct scalar path wherever
# MKFF_NO_AVX2_ASM is defined.
#
# WIN32/APPLE/UNIX aren't set until project()/enable_language() has run
# at least once, so the *host* equivalents (CMAKE_HOST_WIN32) are used
# here to decide which languages to enable in the very first project()
# call.
if(CMAKE_HOST_WIN32)
    project(mkff VERSION 0.1.0 DESCRIPTION "MediaKit Foundation Framework" LANGUAGES C)
else()
    project(mkff VERSION 0.1.0 DESCRIPTION "MediaKit Foundation Framework" LANGUAGES C ASM)
endif()

set(CMAKE_C_STANDARD 11)
set(CMAKE_C_STANDARD_REQUIRED ON)
set(CMAKE_C_EXTENSIONS OFF)

if(NOT CMAKE_BUILD_TYPE AND NOT CMAKE_CONFIGURATION_TYPES)
    set(CMAKE_BUILD_TYPE Debug CACHE STRING "Build type" FORCE)
endif()

# Embedding / Cargo-driven builds flip these off so cmake-rs only builds
# the shared libraries it needs. MKFF_BUILD_RUST_BINDINGS must stay OFF
# when invoked from mkff-sys/build.rs — otherwise CMake would recurse
# into Cargo while Cargo is already driving the native build.
option(MKFF_BUILD_CLI "Build the mkff CLI" ON)
option(MKFF_BUILD_TESTS "Build the CTest suite" ON)
option(MKFF_BUILD_RUST_BINDINGS "Build Rust bindings from CMake (never from Cargo)" OFF)
option(MKFF_BUILD_EXAMPLES "Build example programs" OFF)
option(MKFF_ENABLE_HEVC "Build portable HEVC parser / enable HEVC decode paths" ON)
option(MKFF_ENABLE_HEVC_SOFTWARE "FetchContent libhevc software HEVC fallback" ON)

set(CMAKE_ARCHIVE_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/lib)
set(CMAKE_LIBRARY_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/lib)
set(CMAKE_RUNTIME_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/bin)

# Keep install layout predictable for cmake-rs (and cmake --install):
#   <prefix>/lib/      shared libraries + import libs
#   <prefix>/bin/      Windows DLLs + CLI
#   <prefix>/include/  public headers
include(GNUInstallDirs)

if(NOT (CMAKE_SYSTEM_NAME STREQUAL "Linux" OR CMAKE_SYSTEM_NAME STREQUAL "Darwin" OR CMAKE_SYSTEM_NAME STREQUAL "Windows"))
    message(FATAL_ERROR "mkff has platform backends for Linux, macOS (Darwin), and Windows only.")
endif()

if(CMAKE_SYSTEM_NAME STREQUAL "Linux")
    find_package(PkgConfig REQUIRED)
    pkg_check_modules(LIBVA REQUIRED IMPORTED_TARGET libva)
    pkg_check_modules(LIBVA_DRM REQUIRED IMPORTED_TARGET libva-drm)
    pkg_check_modules(LIBDRM REQUIRED IMPORTED_TARGET libdrm)
    find_package(Threads REQUIRED)
endif()

if(MSVC)
    add_compile_options(/W4)
    # <stdatomic.h> (used by the Windows/macOS DecoderShared refcounts)
    # is only usable under MSVC with C11 atomics explicitly opted into
    # -- without this flag, vcruntime_c11_stdatomic.h itself fails to
    # compile with "C atomic support is not enabled".
    add_compile_options(/experimental:c11atomics)
else()
    add_compile_options(-Wall -Wextra)
endif()

if(NOT WIN32)
    add_compile_definitions(_GNU_SOURCE)
endif()

add_subdirectory(src/codecs)

if(MKFF_ENABLE_HEVC_SOFTWARE)
    include(${CMAKE_CURRENT_SOURCE_DIR}/cmake/MkffLibhevc.cmake)
endif()

add_subdirectory(src/core)

if(CMAKE_SYSTEM_NAME STREQUAL "Linux")
    add_subdirectory(src/platform/linux)
elseif(CMAKE_SYSTEM_NAME STREQUAL "Darwin")
    add_subdirectory(src/platform/macos)
elseif(CMAKE_SYSTEM_NAME STREQUAL "Windows")
    add_subdirectory(src/platform/windows)
endif()

if(MKFF_BUILD_CLI)
    add_subdirectory(src/cli)
endif()

if(MKFF_BUILD_TESTS)
    enable_testing()
    add_subdirectory(tests)
endif()

if(MKFF_BUILD_EXAMPLES)
    if(EXISTS "${CMAKE_CURRENT_SOURCE_DIR}/examples/CMakeLists.txt")
        add_subdirectory(examples)
    endif()
endif()

if(MKFF_BUILD_RUST_BINDINGS)
    message(FATAL_ERROR
        "MKFF_BUILD_RUST_BINDINGS is not implemented as a CMake-driven build. "
        "Build the Rust crates with Cargo (bundled native build is the default). "
        "When Cargo invokes this CMake project via mkff-sys, leave "
        "MKFF_BUILD_RUST_BINDINGS=OFF to avoid Cargo/CMake recursion.")
endif()

install(DIRECTORY include/mkff DESTINATION ${CMAKE_INSTALL_INCLUDEDIR})
