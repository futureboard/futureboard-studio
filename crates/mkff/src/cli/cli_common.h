#ifndef MKFF_CLI_COMMON_H
#define MKFF_CLI_COMMON_H

#include <stddef.h>
#include <stdint.h>

#include "mkff/linux/dmabuf.h"
#include "mkff/linux/drm.h"
#include "mkff/linux/va.h"
#include "mkff/mkff.h"

int cli_read_file(const char *path, uint8_t **out_data, size_t *out_size);

typedef struct CliAuRange {
    size_t offset;
    size_t size;
} CliAuRange;

/* H.264: VCL types 1 and 5. */
size_t cli_split_access_units(const uint8_t *data, size_t size, CliAuRange *out_ranges, size_t capacity);

/* HEVC: VCL NAL types 0–9 and 16–21 (2-byte NAL header). */
size_t cli_split_hevc_access_units(const uint8_t *data, size_t size, CliAuRange *out_ranges, size_t capacity);

double cli_now_seconds(void);

void cli_log_callback(void *user_data, MKFF_LogLevel level, const char *component, const char *message);

const char *cli_pixel_format_name(MKFF_PixelFormat format);
const char *cli_profile_name(MKFF_VideoProfile profile);
const char *cli_codec_name(MKFF_VideoCodec codec);
const char *cli_backend_name(MKFF_VideoBackend backend);

/* Parse "h264" / "hevc". Returns 0 on success. */
int cli_parse_codec_name(const char *name, MKFF_VideoCodec *out_codec);

#endif /* MKFF_CLI_COMMON_H */
