#ifndef MKFF_HEVC_SW_DECODER_H
#define MKFF_HEVC_SW_DECODER_H

#include <stddef.h>
#include <stdint.h>

#include "mkff/error.h"
#include "mkff/types.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef struct HevcSwDecoder HevcSwDecoder;

typedef struct HevcSwFrame {
    uint8_t *data[2]; /* Y, UV (NV12 or P010) */
    uint32_t stride[2];
    uint32_t width;
    uint32_t height;
    MKFF_PixelFormat format;
    int64_t pts;
    int64_t dts;
    uint32_t is_key_frame;
    uint32_t bit_depth;
} HevcSwFrame;

MKFF_Result hevc_sw_decoder_create(uint32_t width_hint, uint32_t height_hint, HevcSwDecoder **out);
void        hevc_sw_decoder_destroy(HevcSwDecoder *dec);

MKFF_Result hevc_sw_decoder_submit(HevcSwDecoder *dec,
                                   const uint8_t *annex_b,
                                   size_t size,
                                   int64_t pts,
                                   int64_t dts);
MKFF_Result hevc_sw_decoder_receive(HevcSwDecoder *dec, HevcSwFrame *out_frame);
MKFF_Result hevc_sw_decoder_flush(HevcSwDecoder *dec);

void hevc_sw_frame_free(HevcSwFrame *frame);

uint32_t hevc_sw_decoder_width(const HevcSwDecoder *dec);
uint32_t hevc_sw_decoder_height(const HevcSwDecoder *dec);
uint32_t hevc_sw_decoder_bit_depth(const HevcSwDecoder *dec);
MKFF_VideoProfile hevc_sw_decoder_profile(const HevcSwDecoder *dec);
MKFF_PixelFormat hevc_sw_decoder_format(const HevcSwDecoder *dec);

#ifdef __cplusplus
}
#endif

#endif /* MKFF_HEVC_SW_DECODER_H */
