#ifndef MKFF_CORE_SOFTWARE_DECODER_H
#define MKFF_CORE_SOFTWARE_DECODER_H

#include "mkff/video_decoder.h"

/* In-core software HEVC decoder (libhevc). Returns CODEC_UNAVAILABLE when
 * MKFF_ENABLE_HEVC_SOFTWARE is off at build time. */
MKFF_Result mkff_software_video_decoder_create(const MKFF_VideoDecoderDesc *desc,
                                               MKFF_VideoDecoder **out_decoder);

int mkff_software_hevc_available(void);

#endif /* MKFF_CORE_SOFTWARE_DECODER_H */
