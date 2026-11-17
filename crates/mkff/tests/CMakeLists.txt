include(CTest)

# --- test_context: exercises the public libmkff API ---
add_executable(test_context test_context.c)
target_include_directories(test_context PRIVATE ${PROJECT_SOURCE_DIR}/include)
target_link_libraries(test_context PRIVATE mkff)
add_test(NAME test_context COMMAND test_context)

# --- test_mp4_demux: progressive MP4 demux (+ HEVC SW decode when built) ---
add_executable(test_mp4_demux test_mp4_demux.c)
target_include_directories(test_mp4_demux PRIVATE ${PROJECT_SOURCE_DIR}/include)
target_link_libraries(test_mp4_demux PRIVATE mkff)
add_test(NAME test_mp4_demux COMMAND test_mp4_demux ${PROJECT_SOURCE_DIR}/testdata)

# --- test_bitreader / test_h264_sps: white-box tests of the portable
# H.264 parser (src/codecs/h264/), so they need neither a GPU nor any
# platform-specific decode API. ---
add_executable(test_bitreader
    test_bitreader.c
    ${PROJECT_SOURCE_DIR}/src/codecs/h264/h264_bitstream.c
)
target_include_directories(test_bitreader PRIVATE ${PROJECT_SOURCE_DIR} ${PROJECT_SOURCE_DIR}/tests)
add_test(NAME test_bitreader COMMAND test_bitreader)

add_executable(test_h264_sps
    test_h264_sps.c
    ${PROJECT_SOURCE_DIR}/src/codecs/h264/h264_bitstream.c
    ${PROJECT_SOURCE_DIR}/src/codecs/h264/h264_sps_pps.c
)
target_include_directories(test_h264_sps PRIVATE ${PROJECT_SOURCE_DIR} ${PROJECT_SOURCE_DIR}/tests)
add_test(NAME test_h264_sps COMMAND test_h264_sps)

# --- HEVC portable parser tests (GPU-free) ---
add_executable(test_hevc_bitstream
    test_hevc_bitstream.c
    ${PROJECT_SOURCE_DIR}/src/codecs/hevc/hevc_bitstream.c
)
target_include_directories(test_hevc_bitstream PRIVATE ${PROJECT_SOURCE_DIR} ${PROJECT_SOURCE_DIR}/tests)
add_test(NAME test_hevc_bitstream COMMAND test_hevc_bitstream)

add_executable(test_hevc_sps
    test_hevc_sps.c
    ${PROJECT_SOURCE_DIR}/src/codecs/hevc/hevc_bitstream.c
    ${PROJECT_SOURCE_DIR}/src/codecs/hevc/hevc_vps_sps_pps.c
)
target_include_directories(test_hevc_sps PRIVATE ${PROJECT_SOURCE_DIR} ${PROJECT_SOURCE_DIR}/tests)
add_test(NAME test_hevc_sps COMMAND test_hevc_sps)

add_executable(test_hevc_poc
    test_hevc_poc.c
    ${PROJECT_SOURCE_DIR}/src/codecs/hevc/hevc_bitstream.c
    ${PROJECT_SOURCE_DIR}/src/codecs/hevc/hevc_vps_sps_pps.c
    ${PROJECT_SOURCE_DIR}/src/codecs/hevc/hevc_slice.c
)
target_include_directories(test_hevc_poc PRIVATE ${PROJECT_SOURCE_DIR} ${PROJECT_SOURCE_DIR}/tests)
add_test(NAME test_hevc_poc COMMAND test_hevc_poc)

# --- test_checksum: SIMD dispatch (scalar vs AVX2). AVX2 kernel only
# exists where a GNU-syntax assembler is available (see root
# CMakeLists.txt); the scalar path is always exercised regardless. ---
set(MKFF_TEST_CHECKSUM_SOURCES
    test_checksum.c
    ${PROJECT_SOURCE_DIR}/src/core/simd/checksum.c
    ${PROJECT_SOURCE_DIR}/src/core/cpu_features.c
)
if(CMAKE_SYSTEM_NAME STREQUAL "Linux")
    list(APPEND MKFF_TEST_CHECKSUM_SOURCES ${PROJECT_SOURCE_DIR}/src/core/simd/checksum_avx2.S)
endif()
add_executable(test_checksum ${MKFF_TEST_CHECKSUM_SOURCES})
if(NOT CMAKE_SYSTEM_NAME STREQUAL "Linux")
    target_compile_definitions(test_checksum PRIVATE MKFF_NO_AVX2_ASM)
endif()
target_include_directories(test_checksum PRIVATE ${PROJECT_SOURCE_DIR})
add_test(NAME test_checksum COMMAND test_checksum)

if(CMAKE_SYSTEM_NAME STREQUAL "Linux")
    # --- test_surface_pool: bounded VA surface pool bookkeeping, GPU-free ---
    add_executable(test_surface_pool
        test_surface_pool.c
        ${PROJECT_SOURCE_DIR}/src/platform/linux/decoder_shared.c
    )
    target_include_directories(test_surface_pool PRIVATE ${PROJECT_SOURCE_DIR} ${PROJECT_SOURCE_DIR}/include)
    target_link_libraries(test_surface_pool PRIVATE PkgConfig::LIBVA Threads::Threads)
    add_test(NAME test_surface_pool COMMAND test_surface_pool)
endif()

# --- CLI smoke tests: must not crash even with no GPU present; a clean
# "no device"/"not supported" exit is an acceptable outcome, a crash is
# not. decode-test/benchmark are codec-generic and run on every
# platform; devices/va-info are Linux-only CLI commands (see
# src/cli/CMakeLists.txt). ---
find_program(BASH_PROGRAM bash)
if(BASH_PROGRAM)
    if(CMAKE_SYSTEM_NAME STREQUAL "Linux")
        add_test(NAME cli_devices_smoke
                 COMMAND ${BASH_PROGRAM} -c "$<TARGET_FILE:mkff_cli> devices; test $? -le 1")
        add_test(NAME cli_va_info_smoke
                 COMMAND ${BASH_PROGRAM} -c "$<TARGET_FILE:mkff_cli> va-info; test $? -le 1")
    endif()
    # Exercises Annex-B splitting + SPS/PPS/slice-header parsing +
    # decoder orchestration end to end via the real CLI binary. On a
    # host with no GPU/hardware decoder (e.g. CI) this fails gracefully
    # at device-open with a clean error message; a crash is the only
    # failure mode this test rejects.
    add_test(NAME cli_decode_test_smoke
             COMMAND ${BASH_PROGRAM} -c "$<TARGET_FILE:mkff_cli> decode-test ${PROJECT_SOURCE_DIR}/testdata/tiny_baseline_64x64.h264; test $? -le 1")
    add_test(NAME cli_hevc_decode_test_smoke
             COMMAND ${BASH_PROGRAM} -c "$<TARGET_FILE:mkff_cli> decode-test ${PROJECT_SOURCE_DIR}/testdata/tiny_main_256x144.hevc --codec hevc --frames 1; test $? -le 1")
    add_test(NAME cli_codec_info_hevc_smoke
             COMMAND ${BASH_PROGRAM} -c "$<TARGET_FILE:mkff_cli> codec-info hevc --backend sw; test $? -le 1")
endif()

# Software HEVC path must produce at least one frame when libhevc is linked.
# Uses cmake -E env so the test runs on Windows without bash.
if(MKFF_ENABLE_HEVC_SOFTWARE)
    add_test(NAME cli_hevc_sw_decode_success
             COMMAND $<TARGET_FILE:mkff_cli>
                     decode-test
                     ${PROJECT_SOURCE_DIR}/testdata/tiny_main_256x144.hevc
                     --codec hevc
                     --backend sw
                     --frames 1)
    # libhevc is 8-bit only: Main10 SOFTWARE_ONLY must fail cleanly
    # (CODEC_UNAVAILABLE), not hang or crash. HW Main10 remains supported.
    add_test(NAME cli_hevc_main10_sw_unavailable
             COMMAND $<TARGET_FILE:mkff_cli>
                     decode-test
                     ${PROJECT_SOURCE_DIR}/testdata/tiny_main10_64x64.hevc
                     --codec hevc
                     --backend sw
                     --frames 1)
    set_tests_properties(cli_hevc_main10_sw_unavailable PROPERTIES WILL_FAIL TRUE)
endif()
