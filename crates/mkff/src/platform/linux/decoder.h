#ifndef MKFF_LINUX_DECODER_H
#define MKFF_LINUX_DECODER_H

#include <stddef.h>
#include <stdint.h>

#include "mkff/error.h"
#include "mkff/log.h"
#include "mkff/platform.h"
#include "mkff/video_decoder.h"
#include "mkff/video_frame.h"

/* Matches MKFF_PlatformAPI's video_decoder_* signatures exactly. */
MKFF_Result linux_video_decoder_create(MKFF_PlatformContext *pctx, const MKFF_VideoDecoderDesc *desc, void **out_decoder);
void        linux_video_decoder_destroy(void *decoder);
MKFF_Result linux_video_decoder_submit(void *decoder, const uint8_t *annex_b_data, size_t annex_b_size, int64_t pts, int64_t dts);
MKFF_Result linux_video_decoder_receive(void *decoder, MKFF_VideoFrame **out_frame);
MKFF_Result linux_video_decoder_flush(void *decoder);
MKFF_Result linux_video_decoder_get_info(const void *decoder, MKFF_VideoDecoderInfo *out_info);

#endif /* MKFF_LINUX_DECODER_H */
