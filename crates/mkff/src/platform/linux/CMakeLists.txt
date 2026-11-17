set(MKFF_PLATFORM_LINUX_SOURCES
    decoder.c
    decoder_hevc.c
    decoder_shared.c
    drm_device.c
    platform_api.c
    va_display.c
    video_frame.c
    h264/h264_dpb.c
    hevc/hevc_dpb.c
)

add_library(mkff_platform_linux SHARED ${MKFF_PLATFORM_LINUX_SOURCES})

target_include_directories(mkff_platform_linux
    PRIVATE
        ${PROJECT_SOURCE_DIR}/include
        ${PROJECT_SOURCE_DIR}
)

target_compile_definitions(mkff_platform_linux PRIVATE MKFF_BUILDING_SHARED)

target_link_libraries(mkff_platform_linux
    PRIVATE
        mkff_codec_h264
        mkff_codec_hevc
        PkgConfig::LIBVA
        PkgConfig::LIBVA_DRM
        PkgConfig::LIBDRM
        Threads::Threads
)

set_target_properties(mkff_platform_linux PROPERTIES
    C_VISIBILITY_PRESET hidden
    INSTALL_RPATH "$ORIGIN"
    BUILD_RPATH "$ORIGIN"
)

install(TARGETS mkff_platform_linux
    LIBRARY DESTINATION ${CMAKE_INSTALL_LIBDIR}
    ARCHIVE DESTINATION ${CMAKE_INSTALL_LIBDIR}
    RUNTIME DESTINATION ${CMAKE_INSTALL_BINDIR}
)
