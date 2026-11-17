#include "cli_common.h"

#include <stdio.h>
#include <stdlib.h>
#include <time.h>

#if defined(_WIN32)
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#endif

int cli_read_file(const char *path, uint8_t **out_data, size_t *out_size) {
    FILE *f = fopen(path, "rb");
    if (!f) return -1;

    if (fseek(f, 0, SEEK_END) != 0) {
        fclose(f);
        return -1;
    }
    long size = ftell(f);
    if (size < 0) {
        fclose(f);
        return -1;
    }
    rewind(f);

    uint8_t *buf = (uint8_t *)malloc((size_t)size > 0 ? (size_t)size : 1);
    if (!buf) {
        fclose(f);
        return -1;
    }

    size_t read = fread(buf, 1, (size_t)size, f);
    fclose(f);
    if (read != (size_t)size) {
        free(buf);
        return -1;
    }

    *out_data = buf;
    *out_size = (size_t)size;
    return 0;
}

static long find_start_code(const uint8_t *data, size_t size, size_t from) {
    if (size < 3) return -1;
    for (size_t i = from; i + 3 <= size; i++) {
        if (data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1) {
            return (long)i;
        }
    }
    return -1;
}

static int is_vcl_nal_type(uint8_t nal_unit_type) {
    return nal_unit_type == 1 || nal_unit_type == 5;
}

size_t cli_split_access_units(const uint8_t *data, size_t size, CliAuRange *out_ranges, size_t capacity) {
    long first = find_start_code(data, size, 0);
    if (first < 0) return 0;

    size_t au_start = (size_t)first;
    int seen_vcl_in_current_au = 0;
    size_t count = 0;

    size_t pos = (size_t)first + 3;
    while (pos <= size) {
        long next = find_start_code(data, size, pos);
        size_t nal_start = pos;
        size_t nal_end = (next < 0) ? size : (size_t)next;

        if (nal_end > nal_start) {
            uint8_t nal_unit_type = data[nal_start] & 0x1F;
            if (is_vcl_nal_type(nal_unit_type)) {
                if (seen_vcl_in_current_au && count < capacity) {
                    out_ranges[count].offset = au_start;
                    out_ranges[count].size = nal_start - 3 - au_start; /* exclude the new AU's start code */
                    count++;
                    au_start = nal_start - 3;
                }
                seen_vcl_in_current_au = 1;
            }
        }

        if (next < 0) break;
        pos = (size_t)next + 3;
    }

    if (au_start < size && count < capacity) {
        out_ranges[count].offset = au_start;
        out_ranges[count].size = size - au_start;
        count++;
    }

    return count;
}

double cli_now_seconds(void) {
#if defined(_WIN32)
    LARGE_INTEGER counter, frequency;
    QueryPerformanceCounter(&counter);
    QueryPerformanceFrequency(&frequency);
    return (double)counter.QuadPart / (double)frequency.QuadPart;
#else
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec / 1e9;
#endif
}

void cli_log_callback(void *user_data, MKFF_LogLevel level, const char *component, const char *message) {
    (void)user_data;
    static const char *kLevelNames[] = {"TRACE", "DEBUG", "INFO", "WARN", "ERROR"};
    const char *level_name = (level >= 0 && level <= MKFF_LOG_LEVEL_ERROR) ? kLevelNames[level] : "?";
    fprintf(stderr, "[%s] %s: %s\n", level_name, component, message);
}

size_t cli_split_hevc_access_units(const uint8_t *data, size_t size, CliAuRange *out_ranges, size_t capacity) {
    long first = find_start_code(data, size, 0);
    if (first < 0) return 0;

    size_t au_start = (size_t)first;
    int seen_vcl_in_current_au = 0;
    size_t count = 0;

    size_t pos = (size_t)first + 3;
    while (pos <= size) {
        long next = find_start_code(data, size, pos);
        size_t nal_start = pos;
        size_t nal_end = (next < 0) ? size : (size_t)next;

        if (nal_end > nal_start + 1) {
            uint8_t nal_unit_type = (uint8_t)((data[nal_start] >> 1) & 0x3F);
            int is_vcl = (nal_unit_type <= 9) || (nal_unit_type >= 16 && nal_unit_type <= 21);
            if (is_vcl) {
                if (seen_vcl_in_current_au && count < capacity) {
                    out_ranges[count].offset = au_start;
                    out_ranges[count].size = nal_start - 3 - au_start;
                    count++;
                    au_start = nal_start - 3;
                }
                seen_vcl_in_current_au = 1;
            }
        }

        if (next < 0) break;
        pos = (size_t)next + 3;
    }

    if (au_start < size && count < capacity) {
        out_ranges[count].offset = au_start;
        out_ranges[count].size = size - au_start;
        count++;
    }

    return count;
}

const char *cli_pixel_format_name(MKFF_PixelFormat format) {
    switch (format) {
        case MKFF_PIXEL_FORMAT_NV12: return "NV12";
        case MKFF_PIXEL_FORMAT_P010: return "P010";
        default: return "unknown";
    }
}

const char *cli_backend_name(MKFF_VideoBackend backend) {
    switch (backend) {
        case MKFF_VIDEO_BACKEND_AUTO: return "auto";
        case MKFF_VIDEO_BACKEND_HARDWARE_ONLY: return "hardware";
        case MKFF_VIDEO_BACKEND_SOFTWARE_ONLY: return "software";
        default: return "unknown";
    }
}

int cli_parse_codec_name(const char *name, MKFF_VideoCodec *out_codec) {
    if (!name || !out_codec) return -1;
    if (strcmp(name, "h264") == 0 || strcmp(name, "avc") == 0) {
        *out_codec = MKFF_VIDEO_CODEC_H264;
        return 0;
    }
    if (strcmp(name, "hevc") == 0 || strcmp(name, "h265") == 0) {
        *out_codec = MKFF_VIDEO_CODEC_HEVC;
        return 0;
    }
    return -1;
}

const char *cli_profile_name(MKFF_VideoProfile profile) {
    switch (profile) {
        case MKFF_VIDEO_PROFILE_H264_BASELINE: return "H.264 Baseline";
        case MKFF_VIDEO_PROFILE_H264_CONSTRAINED_BASELINE: return "H.264 Constrained Baseline";
        case MKFF_VIDEO_PROFILE_H264_MAIN: return "H.264 Main";
        case MKFF_VIDEO_PROFILE_H264_HIGH: return "H.264 High";
        case MKFF_VIDEO_PROFILE_HEVC_MAIN: return "HEVC Main";
        case MKFF_VIDEO_PROFILE_HEVC_MAIN10: return "HEVC Main10";
        case MKFF_VIDEO_PROFILE_VP9_PROFILE0: return "VP9 Profile 0";
        case MKFF_VIDEO_PROFILE_VP9_PROFILE2: return "VP9 Profile 2";
        case MKFF_VIDEO_PROFILE_AV1_MAIN: return "AV1 Main";
        default: return "unknown";
    }
}

const char *cli_codec_name(MKFF_VideoCodec codec) {
    switch (codec) {
        case MKFF_VIDEO_CODEC_H264: return "H.264";
        case MKFF_VIDEO_CODEC_HEVC: return "HEVC";
        case MKFF_VIDEO_CODEC_VP9: return "VP9";
        case MKFF_VIDEO_CODEC_AV1: return "AV1";
        default: return "unknown";
    }
}
