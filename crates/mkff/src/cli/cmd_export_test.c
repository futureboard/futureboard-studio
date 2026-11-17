#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "cli_common.h"

int cmd_export_test(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: export-test <input.h264> [--frames N]\n");
        return 2;
    }
    const char *input_path = argv[1];
    long max_frames = 10;

    for (int i = 2; i < argc; i++) {
        if (strcmp(argv[i], "--frames") == 0 && i + 1 < argc) {
            max_frames = strtol(argv[++i], NULL, 10);
        }
    }

    uint8_t *file_data = NULL;
    size_t file_size = 0;
    if (cli_read_file(input_path, &file_data, &file_size) != 0) {
        fprintf(stderr, "failed to read %s\n", input_path);
        return 1;
    }

    CliAuRange *aus = (CliAuRange *)malloc(sizeof(CliAuRange) * 65536);
    size_t au_count = cli_split_access_units(file_data, file_size, aus, 65536);
    if (au_count == 0) {
        fprintf(stderr, "no H.264 access units found in %s\n", input_path);
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
    dec_desc.codec = MKFF_VIDEO_CODEC_H264;

    MKFF_VideoDecoder *decoder = NULL;
    result = mkff_video_decoder_create(ctx, &dec_desc, &decoder);
    if (result != MKFF_RESULT_OK) {
        fprintf(stderr, "mkff_video_decoder_create failed: %s (%s)\n", mkff_result_to_string(result), mkff_context_get_last_error(ctx));
        mkff_context_destroy(ctx);
        free(aus);
        free(file_data);
        return 1;
    }

    long exported = 0;
    int exit_code = 1;

    for (size_t i = 0; i < au_count && exported < max_frames; i++) {
        result = mkff_video_decoder_submit(decoder, file_data + aus[i].offset, aus[i].size, (int64_t)i, (int64_t)i);
        if (result != MKFF_RESULT_OK) {
            fprintf(stderr, "submit failed on AU %zu: %s (%s)\n", i, mkff_result_to_string(result), mkff_context_get_last_error(ctx));
            break;
        }

        for (;;) {
            MKFF_VideoFrame *frame = NULL;
            result = mkff_video_decoder_receive(decoder, &frame);
            if (result != MKFF_RESULT_OK) break;

            MKFF_LinuxDmaBufDesc dmabuf;
            MKFF_INIT_STRUCT_HEADER(&dmabuf);
            MKFF_Result export_result = mkff_linux_video_frame_export_dmabuf(frame, &dmabuf);
            if (export_result != MKFF_RESULT_OK) {
                fprintf(stderr, "dma-buf export failed: %s (%s)\n", mkff_result_to_string(export_result), mkff_context_get_last_error(ctx));
                mkff_video_frame_release(frame);
                continue;
            }

            printf("frame %ld: %ux%u fourcc=0x%08x objects=%u planes=%u\n", exported, dmabuf.width, dmabuf.height, dmabuf.drm_fourcc, dmabuf.num_objects, dmabuf.num_planes);
            for (uint32_t p = 0; p < dmabuf.num_planes; p++) {
                printf("  plane %u: object=%u offset=%u pitch=%u fd=%d modifier=0x%llx\n",
                       p, dmabuf.planes[p].object_index, dmabuf.planes[p].offset, dmabuf.planes[p].pitch,
                       dmabuf.objects[dmabuf.planes[p].object_index].fd,
                       (unsigned long long)dmabuf.objects[dmabuf.planes[p].object_index].modifier);
            }

            mkff_linux_dmabuf_desc_close(&dmabuf);
            mkff_video_frame_release(frame);
            exported++;
            exit_code = 0;

            if (exported >= max_frames) break;
        }
    }

    if (exported < max_frames) {
        mkff_video_decoder_flush(decoder);
        for (;;) {
            MKFF_VideoFrame *frame = NULL;
            result = mkff_video_decoder_receive(decoder, &frame);
            if (result != MKFF_RESULT_OK) break;

            MKFF_LinuxDmaBufDesc dmabuf;
            MKFF_INIT_STRUCT_HEADER(&dmabuf);
            if (mkff_linux_video_frame_export_dmabuf(frame, &dmabuf) == MKFF_RESULT_OK) {
                printf("frame %ld: %ux%u fourcc=0x%08x objects=%u planes=%u\n", exported, dmabuf.width, dmabuf.height, dmabuf.drm_fourcc, dmabuf.num_objects, dmabuf.num_planes);
                mkff_linux_dmabuf_desc_close(&dmabuf);
                exported++;
                exit_code = 0;
            }
            mkff_video_frame_release(frame);
            if (exported >= max_frames) break;
        }
    }

    printf("dma-buf export: %s (%ld frame%s)\n", exported > 0 ? "ok" : "failed", exported, exported == 1 ? "" : "s");

    mkff_video_decoder_destroy(decoder);
    mkff_context_destroy(ctx);
    free(aus);
    free(file_data);
    return exit_code;
}
