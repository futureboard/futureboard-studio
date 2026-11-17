#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "cli_common.h"

int cmd_benchmark(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: benchmark <input> [--codec h264|hevc] [--seconds N]\n");
        return 2;
    }
    const char *input_path = argv[1];
    double duration_seconds = 10.0;
    MKFF_VideoCodec codec = MKFF_VIDEO_CODEC_H264;

    for (int i = 2; i < argc; i++) {
        if (strcmp(argv[i], "--seconds") == 0 && i + 1 < argc) {
            duration_seconds = strtod(argv[++i], NULL);
        } else if (strcmp(argv[i], "--codec") == 0 && i + 1 < argc) {
            if (cli_parse_codec_name(argv[++i], &codec) != 0) {
                fprintf(stderr, "unknown codec (use h264 or hevc)\n");
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
    ctx_desc.log_min_level = MKFF_LOG_LEVEL_ERROR;

    MKFF_Context *ctx = NULL;
    MKFF_Result result = mkff_context_create(&ctx_desc, &ctx);
    if (result != MKFF_RESULT_OK) {
        fprintf(stderr, "mkff_context_create failed: %s\n", mkff_result_to_string(result));
        free(aus);
        free(file_data);
        return 1;
    }

    uint64_t total_frames = 0;
    double start_time = cli_now_seconds();
    double elapsed = 0.0;
    int passes = 0;

    while (elapsed < duration_seconds) {
        MKFF_VideoDecoderDesc dec_desc;
        MKFF_INIT_STRUCT_HEADER(&dec_desc);
        dec_desc.codec = codec;
        dec_desc.backend = MKFF_VIDEO_BACKEND_AUTO;

        MKFF_VideoDecoder *decoder = NULL;
        result = mkff_video_decoder_create(ctx, &dec_desc, &decoder);
        if (result != MKFF_RESULT_OK) {
            fprintf(stderr, "mkff_video_decoder_create failed: %s (%s)\n", mkff_result_to_string(result), mkff_context_get_last_error(ctx));
            mkff_context_destroy(ctx);
            free(aus);
            free(file_data);
            return 1;
        }

        for (size_t i = 0; i < au_count; i++) {
            result = mkff_video_decoder_submit(decoder, file_data + aus[i].offset, aus[i].size, (int64_t)i, (int64_t)i);
            if (result != MKFF_RESULT_OK) break;

            for (;;) {
                MKFF_VideoFrame *frame = NULL;
                result = mkff_video_decoder_receive(decoder, &frame);
                if (result != MKFF_RESULT_OK) break;
                total_frames++;
                mkff_video_frame_release(frame);
            }
        }
        mkff_video_decoder_flush(decoder);
        for (;;) {
            MKFF_VideoFrame *frame = NULL;
            result = mkff_video_decoder_receive(decoder, &frame);
            if (result != MKFF_RESULT_OK) break;
            total_frames++;
            mkff_video_frame_release(frame);
        }

        mkff_video_decoder_destroy(decoder);
        passes++;
        elapsed = cli_now_seconds() - start_time;

        if (total_frames == 0) {
            /* Nothing decoded at all (e.g. no GPU device available):
             * bail rather than spin for the full duration. */
            break;
        }
    }

    double fps = (elapsed > 0.0) ? (double)total_frames / elapsed : 0.0;

    printf("input:          %s (%zu access units, %d pass%s)\n", input_path, au_count, passes, passes == 1 ? "" : "es");
    printf("elapsed:        %.2fs\n", elapsed);
    printf("decoded frames: %llu\n", (unsigned long long)total_frames);
    printf("decode FPS:     %.2f\n", fps);

    mkff_context_destroy(ctx);
    free(aus);
    free(file_data);
    return total_frames > 0 ? 0 : 1;
}
