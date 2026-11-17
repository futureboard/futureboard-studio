/* C smoke test for libmkff_swcodecs (mkff/codecs.h): open every fixture,
 * decode video + audio, and decode an IVF elementary stream by hand.
 *
 *   cargo build -p mkff-codecs-capi
 *   cc -std=c11 -Iinclude bindings/rust/mkff-codecs-capi/tests/c/smoke.c \
 *      -Ltarget/debug -lmkff_swcodecs -o target/codecs_smoke
 *   LD_LIBRARY_PATH=target/debug target/codecs_smoke testdata
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "mkff/codecs.h"

static int failures = 0;

#define CHECK(cond, ...) do { if (!(cond)) { fprintf(stderr, "FAIL %s:%d: ", __FILE__, __LINE__); \
    fprintf(stderr, __VA_ARGS__); fprintf(stderr, " (%s)\n", mkff_sw_last_error()); failures++; } } while (0)

static void check_media(const char *dir, const char *name, int expect_video, int expect_audio) {
    char path[1024];
    snprintf(path, sizeof path, "%s/codecs/%s", dir, name);
    MKFF_SwMedia *m = NULL;
    MKFF_Result r = mkff_sw_media_open_path(path, &m);
    CHECK(r == MKFF_RESULT_OK && m, "open %s: %d", name, r);
    if (!m) return;

    MKFF_SwMediaInfo info;
    MKFF_INIT_STRUCT_HEADER(&info);
    CHECK(mkff_sw_media_get_info(m, &info) == MKFF_RESULT_OK, "info %s", name);
    printf("%-24s %-10s video=%s %ux%u frames=%llu audio_streams=%u\n", name, info.container,
           info.has_video ? info.video_codec : "-", info.width, info.height,
           (unsigned long long)info.frame_count, info.audio_stream_count);
    CHECK((int)info.has_video == expect_video, "%s has_video", name);
    CHECK((info.audio_stream_count > 0) == expect_audio, "%s audio", name);

    for (uint64_t i = 0; info.has_video && i < info.frame_count; i++) {
        MKFF_SwFrame *f = NULL;
        r = mkff_sw_media_read_video_frame(m, i, &f);
        CHECK(r == MKFF_RESULT_OK && f, "%s frame %llu: %d", name, (unsigned long long)i, r);
        if (!f) continue;
        MKFF_SwFrameInfo fi;
        MKFF_INIT_STRUCT_HEADER(&fi);
        CHECK(mkff_sw_frame_get_info(f, &fi) == MKFF_RESULT_OK, "frame info");
        CHECK(fi.width == info.width && fi.height == info.height && fi.plane_count >= 1 && fi.data[0], "%s frame geometry", name);
        size_t nv12 = (size_t)fi.width * fi.height + 2u * ((fi.width + 1) / 2) * ((fi.height + 1) / 2);
        uint8_t *buf = malloc(nv12);
        CHECK(mkff_sw_frame_copy_nv12(f, buf, nv12) == MKFF_RESULT_OK, "nv12");
        CHECK(mkff_sw_frame_copy_nv12(f, buf, nv12 - 1) == MKFF_RESULT_ERROR_INVALID_ARGUMENT, "short nv12 buffer accepted");
        free(buf);
        mkff_sw_frame_destroy(f);
    }

    if (info.audio_stream_count) {
        MKFF_SwAudioStreamInfo ai;
        MKFF_INIT_STRUCT_HEADER(&ai);
        CHECK(mkff_sw_media_get_audio_stream_info(m, 0, &ai) == MKFF_RESULT_OK, "audio info");
        MKFF_SwAudio *a = NULL;
        CHECK(mkff_sw_media_read_audio(m, 0, 0, 4800, 48000, &a) == MKFF_RESULT_OK && a, "%s audio read", name);
        uint32_t rate = 0, ch = 0; size_t frames = 0; const float *pcm = NULL;
        CHECK(a && mkff_sw_audio_get(a, &rate, &ch, &frames, &pcm) == MKFF_RESULT_OK, "audio get");
        CHECK(rate == 48000 && frames == 4800 && ch >= 1 && pcm, "%s audio %u Hz %zu frames", name, rate, frames);
        double peak = 0;
        for (size_t i = 0; pcm && i < frames * ch; i++) peak = pcm[i] > peak ? pcm[i] : peak;
        CHECK(peak > 0.05, "%s audio silent", name);
        mkff_sw_audio_destroy(a);
    }
    mkff_sw_media_destroy(m);
}

static void check_ivf(const char *dir, const char *name, MKFF_VideoCodec codec) {
    char path[1024];
    snprintf(path, sizeof path, "%s/codecs/%s", dir, name);
    FILE *fp = fopen(path, "rb");
    CHECK(fp, "open %s", path);
    if (!fp) return;
    static uint8_t data[1 << 16];
    size_t n = fread(data, 1, sizeof data, fp);
    fclose(fp);

    MKFF_SwVideoDecoder *dec = NULL;
    CHECK(mkff_sw_video_decoder_create(codec, NULL, 0, &dec) == MKFF_RESULT_OK, "create %s", mkff_sw_video_codec_name(codec));
    if (!dec) return;
    int frames = 0;
    MKFF_SwFrame *f = NULL;
    for (size_t pos = 32; pos + 12 <= n;) {
        uint32_t size = data[pos] | data[pos + 1] << 8 | data[pos + 2] << 16 | (uint32_t)data[pos + 3] << 24;
        int64_t pts = data[pos + 4];
        pos += 12;
        CHECK(mkff_sw_video_decoder_submit(dec, data + pos, size, pts) == MKFF_RESULT_OK, "submit");
        pos += size;
        while (mkff_sw_video_decoder_receive(dec, &f) == MKFF_RESULT_OK) { frames++; mkff_sw_frame_destroy(f); }
    }
    mkff_sw_video_decoder_flush(dec);
    MKFF_Result r;
    while ((r = mkff_sw_video_decoder_receive(dec, &f)) == MKFF_RESULT_OK) { frames++; mkff_sw_frame_destroy(f); }
    CHECK(r == MKFF_RESULT_END_OF_STREAM, "end of stream: %d", r);
    CHECK(frames == 6, "%s: %d frames", name, frames);
    printf("%-24s %s elementary: %d frames\n", name, mkff_sw_video_codec_name(codec), frames);
    mkff_sw_video_decoder_destroy(dec);
}

int main(int argc, char **argv) {
    const char *dir = argc > 1 ? argv[1] : "testdata";
    check_media(dir, "vp9_opus.webm", 1, 1);
    check_media(dir, "av1_aac.mp4", 1, 1);
    check_media(dir, "hevc10_ac3.mkv", 1, 1);
    check_media(dir, "mpeg2_mp2.ts", 1, 1);
    check_media(dir, "prores_hq.mov", 1, 1);
    check_media(dir, "dnxhr.mxf", 1, 1);
    check_media(dir, "mjpeg_pcm.avi", 1, 1);
    check_media(dir, "tone.flac", 0, 1);
    check_ivf(dir, "vp8.ivf", MKFF_VIDEO_CODEC_VP8);
    check_ivf(dir, "av1.ivf", MKFF_VIDEO_CODEC_AV1);

    MKFF_SwMedia *m = NULL;
    CHECK(mkff_sw_media_open_path("/nonexistent/file.mkv", &m) == MKFF_RESULT_ERROR_INVALID_ARGUMENT && !m, "missing file");
    CHECK(strlen(mkff_sw_last_error()) > 0, "error message");
    MKFF_SwVideoDecoder *d = NULL;
    CHECK(mkff_sw_video_decoder_create((MKFF_VideoCodec)99, NULL, 0, &d) == MKFF_RESULT_ERROR_CODEC_UNAVAILABLE, "bad codec");

    if (failures) { fprintf(stderr, "%d failure(s)\n", failures); return 1; }
    printf("all ok\n");
    return 0;
}
