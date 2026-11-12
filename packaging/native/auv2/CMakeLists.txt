# AUv2 wrapper for a Futureboard nih-plug plug-in (Rodhareist, WhiteSharp).
#
# nih-plug has no Audio Unit wrapper. This builds a `<Name>.component` with
# clap-wrapper (external/clap-wrapper) whose AU entry point loads the plug-in's
# own CLAP build, copied into the component at Contents/PlugIns/<Name>.clap.
# The DSP, parameters, state and editor are the CLAP's: nothing is rebuilt.
#
# Driven by packaging/native/build-auv2.sh; configure it directly with:
#
#   cmake -S packaging/native/auv2 -B target/auv2/<name> -G Ninja \
#     -DFB_AU_NAME=Rodhareist \
#     -DFB_AU_CLAP=vstplugins/rodhareist_vst3/target/bundled/Rodhareist.clap \
#     -DFB_AU_SUBTYPE=Rdhr \
#     -DFB_AU_VERSION=2026.11.1 \
#     -DFB_AU_BUNDLE_ID=com.futureboard.rodhareist
#
# SDKs are the repository's submodules (external/clap, external/AudioUnitSDK),
# so configuring downloads nothing.

cmake_minimum_required(VERSION 3.21)
# 10.15: what the GPUI editor inside the CLAP needs anyway, and the first macOS
# with std::filesystem, so clap-wrapper downloads no filesystem shim.
set(CMAKE_OSX_DEPLOYMENT_TARGET 10.15 CACHE STRING "Minimum macOS version")
set(CMAKE_OSX_ARCHITECTURES "x86_64;arm64" CACHE STRING "Universal")

project(futureboard-auv2 LANGUAGES C CXX OBJC OBJCXX)

# The AudioUnitSDK targets are created in this scope and need C++17.
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

if(NOT APPLE)
  message(FATAL_ERROR "An Audio Unit is built on macOS only.")
endif()

foreach(var FB_AU_NAME FB_AU_CLAP FB_AU_SUBTYPE FB_AU_VERSION FB_AU_BUNDLE_ID)
  if("${${var}}" STREQUAL "")
    message(FATAL_ERROR "${var} is required (see the header of this file).")
  endif()
endforeach()

string(LENGTH "${FB_AU_SUBTYPE}" subtype_len)
if(NOT subtype_len EQUAL 4)
  message(FATAL_ERROR "FB_AU_SUBTYPE must be exactly four characters, got '${FB_AU_SUBTYPE}'.")
endif()

# Relative to where cmake was run, not to this directory.
get_filename_component(FB_AU_CLAP "${FB_AU_CLAP}" ABSOLUTE BASE_DIR "$ENV{PWD}")
if(NOT EXISTS "${FB_AU_CLAP}/Contents/MacOS/${FB_AU_NAME}")
  message(FATAL_ERROR "No CLAP bundle for ${FB_AU_NAME} at ${FB_AU_CLAP}; bundle the plug-in first.")
endif()

get_filename_component(FB_REPO_ROOT "${CMAKE_CURRENT_SOURCE_DIR}/../../.." ABSOLUTE)
set(CLAP_SDK_ROOT "${FB_REPO_ROOT}/external/clap" CACHE PATH "")
set(AUDIOUNIT_SDK_ROOT "${FB_REPO_ROOT}/external/AudioUnitSDK" CACHE PATH "")
set(CLAP_WRAPPER_DONT_ADD_TARGETS TRUE)
set(CLAP_WRAPPER_BUNDLE_VERSION "${FB_AU_VERSION}")
add_subdirectory("${FB_REPO_ROOT}/external/clap-wrapper" clap-wrapper EXCLUDE_FROM_ALL)
# clap-wrapper builds its own sources with -Werror; current Apple clang flags
# deprecations in its vendored fmt and in libc++ it does not control. Keep the
# warnings, do not fail on them.
target_compile_options(clap-wrapper-compile-options INTERFACE -Wno-error)

# The 'aufx' effect type: both plug-ins take audio in and give audio out.
# Manufacturer 'Ftbd' is Futureboard's code for every Futureboard AU.
add_library(${FB_AU_NAME}_auv2 MODULE)
target_add_auv2_wrapper(
  TARGET ${FB_AU_NAME}_auv2
  OUTPUT_NAME "${FB_AU_NAME}"
  BUNDLE_IDENTIFIER "${FB_AU_BUNDLE_ID}.auv2"
  BUNDLE_VERSION "${FB_AU_VERSION}"
  MANUFACTURER_NAME "Futureboard"
  MANUFACTURER_CODE "Ftbd"
  SUBTYPE_CODE "${FB_AU_SUBTYPE}"
  INSTRUMENT_TYPE "aufx"
  MACOS_EMBEDDED_CLAP_LOCATION "${FB_AU_CLAP}"
)
