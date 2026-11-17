#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "mkff/mkff.h"

static int has_start_code(const uint8_t *data, size_t size) {
    if (size < 4) return 0;
    for (size_t i = 0; i + 4 <= size; i++) {
        if (data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1) {
            return 1;
        }
        if (i + 3 <= size && data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1) {
            return 1;
        }
    }
    return 0;
}

static char *join_path(const char *a, const char *b) {
    size_t na = strlen(a);
    size_t nb = strlen(b);
    int need_sep = (na > 0 && a[na - 1] != '/' && a[na - 1] != '\\');
    char *out = (char *)malloc(na + nb + (need_sep ? 2 : 1));
    if (!out) return NULL;
    memcpy(out, a, na);
    size_t o = na;
    if (need_sep) out[o++] = '/';
    memcpy(out + o, b, nb);
    out[o + nb] = '\0';
    return out;
}

static void test_demux_file(const char *path, MKFF_VideoCodec expect_codec,
                            uint32_t expect_w, uint32_t expect_h) {
    MKFF_Mp4Demux *demux = NULL;
    MKFF_Result r = mkff_mp4_demux_open_path(path, &demux);
    if (r != MKFF_RESULT_OK) {
        fprintf(stderr, "open_path failed for %s: %s\n", path, mkff_result_to_string(r));
        exit(1);
    }
    assert(demux != NULL);

    MKFF_Mp4VideoTrackInfo info;
    MKFF_INIT_STRUCT_HEADER(&info);
    r = mkff_mp4_demux_get_video_track(demux, &info);
    assert(r == MKFF_RESULT_OK);
    assert(info.codec == expect_codec);
    assert(info.width == expect_w);
    assert(info.height == expect_h);
    assert(info.sample_count >= 1);
    assert(info.timescale > 0);

    MKFF_Mp4AccessUnit au;
    MKFF_INIT_STRUCT_HEADER(&au);
    r = mkff_mp4_demux_read_access_unit(demux, &au);
    assert(r == MKFF_RESULT_OK);
    assert(au.data != NULL);
    assert(au.size > 4);
    assert(has_start_code(au.data, au.size));
    assert(au.sync == 1);
    assert(au.sample_index == 0);

    /* Seek back to sample 0 and re-read. */
    assert(mkff_mp4_demux_seek_sample(demux, 0) == MKFF_RESULT_OK);
    MKFF_INIT_STRUCT_HEADER(&au);
    assert(mkff_mp4_demux_read_access_unit(demux, &au) == MKFF_RESULT_OK);
    assert(au.sample_index == 0);

    /* Exhaust samples then expect EOS. */
    for (;;) {
        MKFF_INIT_STRUCT_HEADER(&au);
        r = mkff_mp4_demux_read_access_unit(demux, &au);
        if (r == MKFF_RESULT_END_OF_STREAM) break;
        assert(r == MKFF_RESULT_OK);
    }

    mkff_mp4_demux_destroy(demux);
    printf("demux ok: %s codec=%d %ux%u samples=%u\n",
           path, (int)expect_codec, expect_w, expect_h, info.sample_count);
}

static void test_decode_hevc_mp4(const char *path) {
    MKFF_Mp4Demux *demux = NULL;
    MKFF_Result r = mkff_mp4_demux_open_path(path, &demux);
    assert(r == MKFF_RESULT_OK);

    MKFF_Mp4VideoTrackInfo info;
    MKFF_INIT_STRUCT_HEADER(&info);
    assert(mkff_mp4_demux_get_video_track(demux, &info) == MKFF_RESULT_OK);
    assert(info.codec == MKFF_VIDEO_CODEC_HEVC);

    MKFF_Context *ctx = NULL;
    assert(mkff_context_create(NULL, &ctx) == MKFF_RESULT_OK);

    MKFF_VideoDecoderDesc desc;
    MKFF_INIT_STRUCT_HEADER(&desc);
    desc.codec = MKFF_VIDEO_CODEC_HEVC;
    desc.backend = MKFF_VIDEO_BACKEND_SOFTWARE_ONLY;
    desc.width_hint = info.width;
    desc.height_hint = info.height;

    MKFF_VideoDecoder *decoder = NULL;
    r = mkff_video_decoder_create(ctx, &desc, &decoder);
    if (r == MKFF_RESULT_ERROR_CODEC_UNAVAILABLE || r == MKFF_RESULT_ERROR_NOT_SUPPORTED) {
        printf("hevc sw decode skipped (unavailable): %s\n", mkff_result_to_string(r));
        mkff_mp4_demux_destroy(demux);
        mkff_context_destroy(ctx);
        return;
    }
    assert(r == MKFF_RESULT_OK);

    uint64_t frames = 0;
    for (;;) {
        MKFF_Mp4AccessUnit au;
        MKFF_INIT_STRUCT_HEADER(&au);
        r = mkff_mp4_demux_read_access_unit(demux, &au);
        if (r == MKFF_RESULT_END_OF_STREAM) break;
        assert(r == MKFF_RESULT_OK);
        assert(mkff_video_decoder_submit(decoder, au.data, au.size, au.pts, au.dts) == MKFF_RESULT_OK);

        for (;;) {
            MKFF_VideoFrame *frame = NULL;
            MKFF_Result rr = mkff_video_decoder_receive(decoder, &frame);
            if (rr == MKFF_RESULT_NOT_READY) break;
            if (rr != MKFF_RESULT_OK) break;
            frames++;
            mkff_video_frame_release(frame);
        }
    }
    mkff_video_decoder_flush(decoder);
    for (;;) {
        MKFF_VideoFrame *frame = NULL;
        MKFF_Result rr = mkff_video_decoder_receive(decoder, &frame);
        if (rr != MKFF_RESULT_OK) break;
        frames++;
        mkff_video_frame_release(frame);
    }

    assert(frames >= 1);
    printf("demux->decode hevc sw ok: frames=%llu\n", (unsigned long long)frames);

    mkff_video_decoder_destroy(decoder);
    mkff_context_destroy(ctx);
    mkff_mp4_demux_destroy(demux);
}

static void test_open_memory_roundtrip(const char *path) {
    FILE *f = fopen(path, "rb");
    assert(f != NULL);
    fseek(f, 0, SEEK_END);
    long sz = ftell(f);
    rewind(f);
    uint8_t *buf = (uint8_t *)malloc((size_t)sz);
    assert(buf != NULL);
    assert(fread(buf, 1, (size_t)sz, f) == (size_t)sz);
    fclose(f);

    MKFF_Mp4Demux *demux = NULL;
    assert(mkff_mp4_demux_open_memory(buf, (size_t)sz, &demux) == MKFF_RESULT_OK);
    free(buf); /* demux copied */

    MKFF_Mp4VideoTrackInfo info;
    MKFF_INIT_STRUCT_HEADER(&info);
    assert(mkff_mp4_demux_get_video_track(demux, &info) == MKFF_RESULT_OK);
    assert(info.sample_count >= 1);
    mkff_mp4_demux_destroy(demux);
    printf("open_memory ok: %s\n", path);
}

int main(int argc, char **argv) {
    const char *testdata = NULL;
    if (argc >= 2) {
        testdata = argv[1];
    } else {
        fprintf(stderr, "usage: test_mp4_demux <testdata_dir>\n");
        return 2;
    }

    char *h264_mp4 = join_path(testdata, "tiny_baseline_64x64.mp4");
    char *hevc_mp4 = join_path(testdata, "tiny_main_256x144.mp4");
    assert(h264_mp4 && hevc_mp4);

    test_demux_file(h264_mp4, MKFF_VIDEO_CODEC_H264, 64, 64);
    test_demux_file(hevc_mp4, MKFF_VIDEO_CODEC_HEVC, 256, 144);
    test_open_memory_roundtrip(h264_mp4);
    test_decode_hevc_mp4(hevc_mp4);

    free(h264_mp4);
    free(hevc_mp4);
    printf("test_mp4_demux: OK\n");
    return 0;
}
