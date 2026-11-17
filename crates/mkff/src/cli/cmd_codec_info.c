#include <stdio.h>
#include <string.h>

#include "cli_common.h"
#include "mkff/cpu_planes.h"

int cmd_codec_info(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: codec-info <h264|hevc> [--backend auto|hw|sw]\n");
        return 2;
    }

    MKFF_VideoCodec codec;
    if (cli_parse_codec_name(argv[1], &codec) != 0) {
        fprintf(stderr, "unknown codec (use h264 or hevc)\n");
        return 2;
    }

    MKFF_VideoBackend backend = MKFF_VIDEO_BACKEND_AUTO;
    for (int i = 2; i < argc; i++) {
        if (strcmp(argv[i], "--backend") == 0 && i + 1 < argc) {
            const char *b = argv[++i];
            if (strcmp(b, "auto") == 0) backend = MKFF_VIDEO_BACKEND_AUTO;
            else if (strcmp(b, "hw") == 0 || strcmp(b, "hardware") == 0) backend = MKFF_VIDEO_BACKEND_HARDWARE_ONLY;
            else if (strcmp(b, "sw") == 0 || strcmp(b, "software") == 0) backend = MKFF_VIDEO_BACKEND_SOFTWARE_ONLY;
            else {
                fprintf(stderr, "unknown backend\n");
                return 2;
            }
        }
    }

    MKFF_ContextDesc ctx_desc;
    MKFF_INIT_STRUCT_HEADER(&ctx_desc);
    ctx_desc.log_callback = cli_log_callback;
    ctx_desc.log_min_level = MKFF_LOG_LEVEL_ERROR;

    MKFF_Context *ctx = NULL;
    MKFF_Result result = mkff_context_create(&ctx_desc, &ctx);
    if (result != MKFF_RESULT_OK) {
        fprintf(stderr, "context create failed: %s\n", mkff_result_to_string(result));
        return 1;
    }

    MKFF_VideoDecoderDesc dec_desc;
    MKFF_INIT_STRUCT_HEADER(&dec_desc);
    dec_desc.codec = codec;
    dec_desc.backend = backend;

    MKFF_VideoDecoder *decoder = NULL;
    result = mkff_video_decoder_create(ctx, &dec_desc, &decoder);
    if (result != MKFF_RESULT_OK) {
        printf("codec=%s available=no reason=%s (%s)\n",
               cli_codec_name(codec),
               mkff_result_to_string(result),
               mkff_context_get_last_error(ctx));
        mkff_context_destroy(ctx);
        return 1;
    }

    MKFF_VideoDecoderInfo info;
    MKFF_INIT_STRUCT_HEADER(&info);
    mkff_video_decoder_get_info(decoder, &info);

    printf("codec=%s available=yes\n", cli_codec_name(codec));
    printf("  profile=%s\n", cli_profile_name(info.profile));
    printf("  size=%ux%u\n", info.width, info.height);
    printf("  bit_depth=%u chroma_format_idc=%u\n", info.bit_depth, info.chroma_format_idc);
    printf("  output_format=%s\n", cli_pixel_format_name(info.output_format));
    printf("  backend=%s hardware=%u\n", cli_backend_name(info.backend), info.hardware);
    printf("  surface_pool=%u/%u\n", info.surface_pool_size, info.surface_pool_capacity);
    printf("  cpu_plane_map=%s\n", info.hardware ? "not_supported" : "available_after_decode");

    mkff_video_decoder_destroy(decoder);
    mkff_context_destroy(ctx);
    return 0;
}
