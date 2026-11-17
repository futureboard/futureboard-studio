set(MKFF_CORE_SOURCES
    context.c
    error.c
    platform_loader.c
    video_decoder.c
    video_frame.c
    software_decoder.c
    cpu_features.c
    simd/checksum.c
    ${PROJECT_SOURCE_DIR}/src/demux/mp4/mp4_demux.c
)

if(CMAKE_SYSTEM_NAME STREQUAL "Linux")
    list(APPEND MKFF_CORE_SOURCES linux_forwarding.c)
elseif(CMAKE_SYSTEM_NAME STREQUAL "Darwin")
    list(APPEND MKFF_CORE_SOURCES macos_forwarding.c)
elseif(CMAKE_SYSTEM_NAME STREQUAL "Windows")
    list(APPEND MKFF_CORE_SOURCES windows_forwarding.c)
endif()

if(CMAKE_SYSTEM_NAME STREQUAL "Linux")
    list(APPEND MKFF_CORE_SOURCES simd/checksum_avx2.S)
else()
    # No GNU-syntax assembler under MSVC; macOS's Mach-O assembler
    # doesn't accept this file's ELF-only GAS directives. Fall back to
    # the always-correct scalar checksum path (see checksum.c).
    add_compile_definitions(MKFF_NO_AVX2_ASM)
endif()

add_library(mkff SHARED ${MKFF_CORE_SOURCES})

target_include_directories(mkff
    PUBLIC
        $<BUILD_INTERFACE:${PROJECT_SOURCE_DIR}/include>
        $<INSTALL_INTERFACE:include>
    PRIVATE
        ${PROJECT_SOURCE_DIR}
)

target_compile_definitions(mkff PRIVATE MKFF_BUILDING_SHARED)

if(MKFF_ENABLE_HEVC_SOFTWARE)
    target_compile_definitions(mkff PRIVATE MKFF_ENABLE_HEVC_SOFTWARE)
    target_sources(mkff PRIVATE
        ${PROJECT_SOURCE_DIR}/src/codecs/hevc/software/hevc_sw_decoder.c
    )
    # Parser used to detect Main10 early (libhevc is 8-bit only).
    target_link_libraries(mkff PRIVATE libhevcdec mkff_codec_hevc)
endif()

if(CMAKE_SYSTEM_NAME STREQUAL "Darwin")
    find_library(COREFOUNDATION_LIBRARY CoreFoundation REQUIRED)
    target_link_libraries(mkff PRIVATE ${COREFOUNDATION_LIBRARY})
elseif(NOT WIN32)
    target_link_libraries(mkff PRIVATE ${CMAKE_DL_LIBS})
endif()

set_target_properties(mkff PROPERTIES
    VERSION ${PROJECT_VERSION}
    SOVERSION 0
    C_VISIBILITY_PRESET hidden
)

if(WIN32)
    # Stable Windows artifact names expected by the Rust bindings and
    # platform loader (MKFF.dll / MKFF.lib).
    set_target_properties(mkff PROPERTIES OUTPUT_NAME "MKFF")
elseif(APPLE)
    # @rpath install name so Cargo can point binaries at the bundled
    # lib/ directory without rewriting dylib ids after install.
    set_target_properties(mkff PROPERTIES
        MACOSX_RPATH ON
        INSTALL_NAME_DIR "@rpath"
        BUILD_WITH_INSTALL_NAME_DIR ON
    )
else()
    # $ORIGIN so dlopen() of the platform module next to libmkff.so
    # keeps working from an install / cmake-rs prefix tree.
    set_target_properties(mkff PROPERTIES
        INSTALL_RPATH "$ORIGIN"
        BUILD_RPATH "$ORIGIN"
    )
endif()

install(TARGETS mkff
    LIBRARY DESTINATION ${CMAKE_INSTALL_LIBDIR}
    ARCHIVE DESTINATION ${CMAKE_INSTALL_LIBDIR}
    RUNTIME DESTINATION ${CMAKE_INSTALL_BINDIR}
)
