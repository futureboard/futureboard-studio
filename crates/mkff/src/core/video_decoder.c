#include "mkff/video_decoder.h"

#include <stddef.h>
#include <string.h>

#include "context_internal.h"
#include "software_decoder.h"
#include "src/common/mkff_common.h"

static const MKFF_HandleCommon *as_common(const void *handle) {
    return (const MKFF_HandleCommon *)handle;
}

static MKFF_VideoBackend desc_backend(const MKFF_VideoDecoderDesc *desc) {
    /* Older callers omit backend (struct_size smaller) → AUTO. */
    const size_t backend_end = offsetof(MKFF_VideoDecoderDesc, backend) + sizeof(desc->backend);
    if (desc->struct_size < (uint32_t)backend_end) {
        return MKFF_VIDEO_BACKEND_AUTO;
    }
    return desc->backend;
}

MKFF_Result mkff_video_decoder_create(MKFF_Context *context,
                                       const MKFF_VideoDecoderDesc *desc,
                                       MKFF_VideoDecoder **out_decoder) {
    if (!out_decoder) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }
    *out_decoder = NULL;

    if (!context || !desc || desc->struct_size < sizeof(uint32_t) * 2) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }

    MKFF_VideoBackend backend = desc_backend(desc);
    MKFF_Result hw_result = MKFF_RESULT_ERROR_NOT_SUPPORTED;
    void *hw_decoder = NULL;

    if (backend != MKFF_VIDEO_BACKEND_SOFTWARE_ONLY) {
        const MKFF_PlatformAPI *api = mkff_context_internal_get_platform_api(context);
        if (!api) {
            hw_result = MKFF_RESULT_ERROR_PLATFORM_LOAD;
        } else {
            hw_result = api->video_decoder_create(mkff_context_internal_get_platform_context(context),
                                                 desc,
                                                 &hw_decoder);
            if (hw_result == MKFF_RESULT_OK) {
                *out_decoder = (MKFF_VideoDecoder *)hw_decoder;
                return MKFF_RESULT_OK;
            }
        }
        if (backend == MKFF_VIDEO_BACKEND_HARDWARE_ONLY) {
            return (hw_result == MKFF_RESULT_ERROR_NOT_SUPPORTED)
                       ? MKFF_RESULT_ERROR_CODEC_UNAVAILABLE
                       : hw_result;
        }
        /* AUTO: fall through to software */
    }

    if (desc->codec == MKFF_VIDEO_CODEC_HEVC) {
        MKFF_Result sw = mkff_software_video_decoder_create(desc, out_decoder);
        if (sw == MKFF_RESULT_OK) {
            return MKFF_RESULT_OK;
        }
        if (backend == MKFF_VIDEO_BACKEND_SOFTWARE_ONLY) {
            return sw;
        }
        /* AUTO and both failed */
        if (sw == MKFF_RESULT_ERROR_CODEC_UNAVAILABLE) {
            return MKFF_RESULT_ERROR_CODEC_UNAVAILABLE;
        }
        return (hw_result != MKFF_RESULT_OK) ? hw_result : sw;
    }

    if (backend == MKFF_VIDEO_BACKEND_SOFTWARE_ONLY) {
        return MKFF_RESULT_ERROR_CODEC_UNAVAILABLE;
    }
    return hw_result;
}

void mkff_video_decoder_destroy(MKFF_VideoDecoder *decoder) {
    if (!decoder) return;
    as_common(decoder)->api->video_decoder_destroy(decoder);
}

MKFF_Result mkff_video_decoder_submit(MKFF_VideoDecoder *decoder,
                                       const uint8_t *annex_b_data,
                                       size_t annex_b_size,
                                       int64_t pts,
                                       int64_t dts) {
    if (!decoder || (!annex_b_data && annex_b_size > 0)) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }
    return as_common(decoder)->api->video_decoder_submit(decoder, annex_b_data, annex_b_size, pts, dts);
}

MKFF_Result mkff_video_decoder_receive(MKFF_VideoDecoder *decoder, MKFF_VideoFrame **out_frame) {
    if (!out_frame) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }
    *out_frame = NULL;
    if (!decoder) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }
    return as_common(decoder)->api->video_decoder_receive(decoder, out_frame);
}

MKFF_Result mkff_video_decoder_flush(MKFF_VideoDecoder *decoder) {
    if (!decoder) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }
    return as_common(decoder)->api->video_decoder_flush(decoder);
}

MKFF_Result mkff_video_decoder_get_info(const MKFF_VideoDecoder *decoder, MKFF_VideoDecoderInfo *out_info) {
    if (!decoder || !out_info) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }
    return as_common(decoder)->api->video_decoder_get_info(decoder, out_info);
}
