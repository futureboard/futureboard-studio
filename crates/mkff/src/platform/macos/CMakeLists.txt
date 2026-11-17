add_library(mkff_platform_macos SHARED
    decoder.c
    decoder_shared.c
    platform_api.c
    video_frame.c
)

target_include_directories(mkff_platform_macos
    PRIVATE
        ${PROJECT_SOURCE_DIR}/include
        ${PROJECT_SOURCE_DIR}
)

find_library(COREFOUNDATION_LIBRARY CoreFoundation REQUIRED)
find_library(COREMEDIA_LIBRARY CoreMedia REQUIRED)
find_library(COREVIDEO_LIBRARY CoreVideo REQUIRED)
find_library(VIDEOTOOLBOX_LIBRARY VideoToolbox REQUIRED)
find_library(IOSURFACE_LIBRARY IOSurface REQUIRED)

target_link_libraries(mkff_platform_macos
    PRIVATE
        mkff_codec_h264
        mkff_codec_hevc
        ${COREFOUNDATION_LIBRARY}
        ${COREMEDIA_LIBRARY}
        ${COREVIDEO_LIBRARY}
        ${VIDEOTOOLBOX_LIBRARY}
        ${IOSURFACE_LIBRARY}
)

target_compile_definitions(mkff_platform_macos PRIVATE MKFF_BUILDING_SHARED)

set_target_properties(mkff_platform_macos PROPERTIES
    C_VISIBILITY_PRESET hidden
    MACOSX_RPATH ON
    INSTALL_NAME_DIR "@rpath"
    BUILD_WITH_INSTALL_NAME_DIR ON
)

install(TARGETS mkff_platform_macos
    LIBRARY DESTINATION ${CMAKE_INSTALL_LIBDIR}
    ARCHIVE DESTINATION ${CMAKE_INSTALL_LIBDIR}
    RUNTIME DESTINATION ${CMAKE_INSTALL_BINDIR}
)
