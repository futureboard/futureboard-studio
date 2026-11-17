#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "cli_common.h"

int cmd_decode_test(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: decode-test <input> [--codec h264|hevc] [--frames N] [--backend auto|hw|sw]\n");
        return 2;
    }
    const char *input_path = argv[1];
    long max_frames = -1;
    MKFF_VideoCodec codec = MKFF_VIDEO_CODEC_H264;
    MKFF_VideoBackend backend = MKFF_VIDEO_BACKEND_AUTO;

    for (int i = 2; i < argc; i++) {
        if (strcmp(argv[i], "--frames") == 0 && i + 1 < argc) {
            max_frames = strtol(argv[++i], NULL, 10);
        } else if (strcmp(argv[i], "--codec") == 0 && i + 1 < argc) {
            if (cli_parse_codec_name(argv[++i], &codec) != 0) {
                fprintf(stderr, "unknown codec (use h264 or hevc)\n");
                return 2;
            }
        } else if (strcmp(argv[i], "--backend") == 0 && i + 1 < argc) {
            const char *b = argv[++i];
            if (strcmp(b, "auto") == 0) backend = MKFF_VIDEO_BACKEND_AUTO;
            else if (strcmp(b, "hw") == 0 || strcmp(b, "hardware") == 0) backend = MKFF_VIDEO_BACKEND_HARDWARE_ONLY;
            else if (strcmp(b, "sw") == 0 || strcmp(b, "software") == 0) backend = MKFF_VIDEO_BACKEND_SOFTWARE_ONLY;
            else {
                fprintf(stderr, "unknown backend (use auto|hw|sw)\n");
                return 2;
            }
        }
    }

    uint8_t *file_data = NULL;
    size_t file_size = 0;
    if (cli_read_file(input_path, &file_data, &file_size) != 0) {
        fprintf(stderr, "failed to read %s\n", input_path);
        return 1;
    }

    CliAuRange *aus = (CliAuRange *)malloc(sizeof(CliAuRange) * 65536);
    size_t au_count = (codec == MKFF_VIDEO_CODEC_HEVC)
                          ? cli_split_hevc_access_units(file_data, file_size, aus, 65536)
                          : cli_split_access_units(file_data, file_size, aus, 65536);
    if (au_count == 0) {
        fprintf(stderr, "no %s access units found in %s\n", cli_codec_name(codec), input_path);
        free(aus);
        free(file_data);
        return 1;
    }

    MKFF_ContextDesc ctx_desc;
    MKFF_INIT_STRUCT_HEADER(&ctx_desc);
    ctx_desc.log_callback = cli_log_callback;
    ctx_desc.log_min_level = MKFF_LOG_LEVEL_WARN;

    MKFF_Context *ctx = NULL;
    MKFF_Result result = mkff_context_create(&ctx_desc, &ctx);
    if (result != MKFF_RESULT_OK) {
        fprintf(stderr, "mkff_context_create failed: %s\n", mkff_result_to_string(result));
        free(aus);
        free(file_data);
        return 1;
    }

    MKFF_VideoDecoderDesc dec_desc;
    MKFF_INIT_STRUCT_HEADER(&dec_desc);
    dec_desc.codec = codec;
    dec_desc.backend = backend;

    MKFF_VideoDecoder *decoder = NULL;
    result = mkff_video_decoder_create(ctx, &dec_desc, &decoder);
    if (result != MKFF_RESULT_OK) {
        fprintf(stderr, "mkff_video_decoder_create failed: %s (%s)\n", mkff_result_to_string(result), mkff_context_get_last_error(ctx));
        mkff_context_destroy(ctx);
        free(aus);
        free(file_data);
        return 1;
    }

    uint64_t decoded_frames = 0;
    double start_time = -1.0;
    int stop = 0;

    for (size_t i = 0; i < au_count && !stop; i++) {
        result = mkff_video_decoder_submit(decoder, file_data + aus[i].offset, aus[i].size, (int64_t)i, (int64_t)i);
        if (result != MKFF_RESULT_OK) {
            fprintf(stderr, "submit failed on AU %zu: %s (%s)\n", i, mkff_result_to_string(result), mkff_context_get_last_error(ctx));
            break;
        }

        for (;;) {
            MKFF_VideoFrame *frame = NULL;
            result = mkff_video_decoder_receive(decoder, &frame);
            if (result == MKFF_RESULT_NOT_READY) break;
            if (result != MKFF_RESULT_OK) break;

            if (start_time < 0) start_time = cli_now_seconds();
            decoded_frames++;
            mkff_video_frame_release(frame);

            if (max_frames >= 0 && (long)decoded_frames >= max_frames) {
                stop = 1;
                break;
            }
        }
    }

    if (!stop) {
        mkff_video_decoder_flush(decoder);
        for (;;) {
            MKFF_VideoFrame *frame = NULL;
            result = mkff_video_decoder_receive(decoder, &frame);
            if (result != MKFF_RESULT_OK) break;
            if (start_time < 0) start_time = cli_now_seconds();
            decoded_frames++;
            mkff_video_frame_release(frame);
            if (max_frames >= 0 && (long)decoded_frames >= max_frames) break;
        }
    }

    double end_time = cli_now_seconds();

    MKFF_VideoDecoderInfo info;
    MKFF_INIT_STRUCT_HEADER(&info);
    mkff_video_decoder_get_info(decoder, &info);

    printf("codec=%s backend=%s hardware=%u format=%s %ux%u profile=%s bit_depth=%u frames=%llu",
           cli_codec_name(codec),
           cli_backend_name(info.backend),
           info.hardware,
           cli_pixel_format_name(info.output_format),
           info.width,
           info.height,
           cli_profile_name(info.profile),
           info.bit_depth,
           (unsigned long long)decoded_frames);
    if (start_time >= 0 && end_time > start_time) {
        printf(" fps=%.2f", (double)decoded_frames / (end_time - start_time));
    }
    printf("\n");

    mkff_video_decoder_destroy(decoder);
    mkff_context_destroy(ctx);
    free(aus);
    free(file_data);
    return decoded_frames > 0 ? 0 : 1;
}
