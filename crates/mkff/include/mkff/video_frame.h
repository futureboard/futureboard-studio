#ifndef MKFF_VIDEO_FRAME_H
#define MKFF_VIDEO_FRAME_H

#include <stdint.h>
#include "mkff/abi.h"
#include "mkff/error.h"
#include "mkff/export.h"
#include "mkff/types.h"

MKFF_BEGIN_DECLS

typedef struct MKFF_VideoFrameInfo {
    MKFF_STRUCT_HEADER;

    uint32_t width;
    uint32_t height;
    MKFF_PixelFormat format;

    int64_t pts; /* integer timestamp in the decoder's input time_base; MKFF_TIMESTAMP_NONE if unknown */
    int64_t dts;

    uint32_t is_key_frame; /* 0 or 1 */
    uint32_t pad0;
} MKFF_VideoFrameInfo;

#define MKFF_TIMESTAMP_NONE INT64_MIN

/* MKFF_VideoFrame is reference-counted. mkff_video_decoder_receive()
 * returns a frame already owning one reference. The GPU surface backing
 * the frame cannot be returned to the decoder's surface pool until every
 * reference has been released. */
MKFF_API MKFF_VideoFrame *mkff_video_frame_retain(MKFF_VideoFrame *frame);
MKFF_API void             mkff_video_frame_release(MKFF_VideoFrame *frame);

MKFF_API MKFF_Result mkff_video_frame_get_info(const MKFF_VideoFrame *frame, MKFF_VideoFrameInfo *out_info);

MKFF_END_DECLS

#endif /* MKFF_VIDEO_FRAME_H */
