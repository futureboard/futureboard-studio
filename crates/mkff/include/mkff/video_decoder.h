#ifndef MKFF_VIDEO_DECODER_H
#define MKFF_VIDEO_DECODER_H

#include <stddef.h>
#include <stdint.h>
#include "mkff/abi.h"
#include "mkff/context.h"
#include "mkff/error.h"
#include "mkff/export.h"
#include "mkff/types.h"
#include "mkff/video_frame.h"

MKFF_BEGIN_DECLS

typedef struct MKFF_VideoDecoderDesc {
    MKFF_STRUCT_HEADER;

    MKFF_VideoCodec codec;

    /* Upper bound on concurrently-live decode surfaces (DPB + in-flight +
     * scratch). 0 selects a codec-appropriate default. The platform module
     * enforces this bound; mkff_video_decoder_submit() will report
     * MKFF_RESULT_ERROR_POOL_EXHAUSTED if a caller holds too many frames
     * live via retain(). */
    uint32_t max_surfaces;

    /* Optional dimension hints; 0 defers to the values signalled in-band
     * (SPS for H.264 / HEVC). */
    uint32_t width_hint;
    uint32_t height_hint;

    /* Appended in ABI v1 via struct_size: callers that omit this field
     * (older sizeof) get AUTO. */
    MKFF_VideoBackend backend;
} MKFF_VideoDecoderDesc;

typedef struct MKFF_VideoDecoderInfo {
    MKFF_STRUCT_HEADER;

    uint32_t width;
    uint32_t height;
    MKFF_VideoProfile profile;
    MKFF_VideoEntrypoint entrypoint;
    MKFF_PixelFormat output_format;
    uint32_t surface_pool_size;
    uint32_t surface_pool_capacity;

    /* Appended fields; zeroed for callers with a smaller struct_size. */
    MKFF_VideoBackend backend;
    uint32_t bit_depth;          /* 8 or 10 for supported Main/Main10 */
    uint32_t chroma_format_idc;  /* 1 = 4:2:0 for supported streams */
    uint32_t hardware;           /* 1 if platform HW path, 0 if software */
} MKFF_VideoDecoderInfo;

MKFF_API MKFF_Result mkff_video_decoder_create(MKFF_Context *context,
                                                const MKFF_VideoDecoderDesc *desc,
                                                MKFF_VideoDecoder **out_decoder);
MKFF_API void         mkff_video_decoder_destroy(MKFF_VideoDecoder *decoder);

/* Submits one Annex-B access unit (one or more NAL units belonging to a
 * single coded picture). `pts`/`dts` are opaque integers echoed back on
 * the decoded MKFF_VideoFrame; use MKFF_TIMESTAMP_NONE if not tracked. */
MKFF_API MKFF_Result mkff_video_decoder_submit(MKFF_VideoDecoder *decoder,
                                                const uint8_t *annex_b_data,
                                                size_t annex_b_size,
                                                int64_t pts,
                                                int64_t dts);

/* Pops one decoded frame in output (presentation) order if available.
 * Returns MKFF_RESULT_NOT_READY when no frame is currently available
 * (the caller should submit more data, or call flush at end of stream). */
MKFF_API MKFF_Result mkff_video_decoder_receive(MKFF_VideoDecoder *decoder, MKFF_VideoFrame **out_frame);

/* Signals end of stream: drains the reference/reorder buffer so every
 * remaining frame becomes retrievable via receive(). After the last
 * buffered frame has been received, receive() returns
 * MKFF_RESULT_END_OF_STREAM. */
MKFF_API MKFF_Result mkff_video_decoder_flush(MKFF_VideoDecoder *decoder);

MKFF_API MKFF_Result mkff_video_decoder_get_info(const MKFF_VideoDecoder *decoder, MKFF_VideoDecoderInfo *out_info);

MKFF_END_DECLS

#endif /* MKFF_VIDEO_DECODER_H */
