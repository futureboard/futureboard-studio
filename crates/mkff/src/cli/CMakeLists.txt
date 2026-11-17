set(MKFF_CLI_SOURCES
    main.c
    cli_common.c
    cmd_decode_test.c
    cmd_benchmark.c
    cmd_codec_info.c
)

# devices/va-info/export-test are Linux (VA-API/DRM/dma-buf) specific
# commands today; decode-test/benchmark are codec-generic and build
# on every platform. Per-platform equivalents (e.g. a VideoToolbox
# capability listing, a D3D11 texture export-test) are a documented
# follow-up, not implemented yet.
if(CMAKE_SYSTEM_NAME STREQUAL "Linux")
    list(APPEND MKFF_CLI_SOURCES cmd_devices.c cmd_va_info.c cmd_export_test.c)
endif()

add_executable(mkff_cli ${MKFF_CLI_SOURCES})

if(CMAKE_SYSTEM_NAME STREQUAL "Linux")
    target_compile_definitions(mkff_cli PRIVATE MKFF_CLI_HAVE_LINUX_COMMANDS)
endif()

set_target_properties(mkff_cli PROPERTIES OUTPUT_NAME mkff)

target_include_directories(mkff_cli PRIVATE ${PROJECT_SOURCE_DIR}/include)
target_link_libraries(mkff_cli PRIVATE mkff)

install(TARGETS mkff_cli RUNTIME DESTINATION ${CMAKE_INSTALL_BINDIR})
