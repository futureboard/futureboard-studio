#ifndef MKFF_LINUX_DECODER_HEVC_H
#define MKFF_LINUX_DECODER_HEVC_H

#include "decoder.h"

MKFF_Result linux_hevc_video_decoder_create(MKFF_PlatformContext *pctx, const MKFF_VideoDecoderDesc *desc, void **out_decoder);
void        linux_hevc_video_decoder_destroy(void *decoder);
MKFF_Result linux_hevc_video_decoder_submit(void *decoder, const uint8_t *annex_b_data, size_t annex_b_size, int64_t pts, int64_t dts);
MKFF_Result linux_hevc_video_decoder_receive(void *decoder, MKFF_VideoFrame **out_frame);
MKFF_Result linux_hevc_video_decoder_flush(void *decoder);
MKFF_Result linux_hevc_video_decoder_get_info(const void *decoder, MKFF_VideoDecoderInfo *out_info);

#endif /* MKFF_LINUX_DECODER_HEVC_H */
