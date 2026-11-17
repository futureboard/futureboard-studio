add_library(mkff_platform_windows SHARED
    d3d11_device.c
    decoder.c
    decoder_shared.c
    guids.c
    h264_dxva_dpb.c
    hevc_dxva_dpb.c
    platform_api.c
    video_frame.c
)

target_include_directories(mkff_platform_windows
    PRIVATE
        ${PROJECT_SOURCE_DIR}/include
        ${PROJECT_SOURCE_DIR}
)

target_link_libraries(mkff_platform_windows
    PRIVATE
        mkff_codec_h264
        mkff_codec_hevc
        d3d11
        dxgi
        dxguid
)

target_compile_definitions(mkff_platform_windows PRIVATE MKFF_BUILDING_SHARED)

# Stable Windows artifact name expected by the platform loader
# (MKFF.Platform.Windows.dll, loaded from next to MKFF.dll).
set_target_properties(mkff_platform_windows PROPERTIES OUTPUT_NAME "MKFF.Platform.Windows")

install(TARGETS mkff_platform_windows
    LIBRARY DESTINATION ${CMAKE_INSTALL_LIBDIR}
    ARCHIVE DESTINATION ${CMAKE_INSTALL_LIBDIR}
    RUNTIME DESTINATION ${CMAKE_INSTALL_BINDIR}
)
