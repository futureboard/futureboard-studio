#include "software_decoder.h"

#include <stddef.h>
#include <stdlib.h>
#include <string.h>

#include "mkff/cpu_planes.h"
#include "src/common/mkff_common.h"

#if defined(MKFF_ENABLE_HEVC_SOFTWARE)
#include "src/codecs/hevc/software/hevc_sw_decoder.h"

typedef struct SwVideoFrame {
    MKFF_HandleCommon common;
    uint32_t refcount;
    HevcSwFrame frame;
} SwVideoFrame;

typedef struct SwVideoDecoder {
    MKFF_HandleCommon common;
    HevcSwDecoder *hevc;
    MKFF_VideoCodec codec;
} SwVideoDecoder;

static const MKFF_PlatformAPI kSoftwarePlatformAPI;

static MKFF_VideoFrame *sw_frame_retain(MKFF_VideoFrame *frame) {
    SwVideoFrame *f = (SwVideoFrame *)frame;
    if (!f) {
        return NULL;
    }
    f->refcount++;
    return frame;
}

static void sw_frame_release(MKFF_VideoFrame *frame) {
    SwVideoFrame *f = (SwVideoFrame *)frame;
    if (!f) {
        return;
    }
    if (--f->refcount == 0) {
        hevc_sw_frame_free(&f->frame);
        free(f);
    }
}

static MKFF_Result sw_frame_get_info(const MKFF_VideoFrame *frame, MKFF_VideoFrameInfo *out_info) {
    const SwVideoFrame *f = (const SwVideoFrame *)frame;
    if (!f || !out_info) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }
    uint32_t requested = out_info->struct_size;
    MKFF_INIT_STRUCT_HEADER(out_info);
    if (requested) {
        out_info->struct_size = requested;
    }
    out_info->width = f->frame.width;
    out_info->height = f->frame.height;
    out_info->format = f->frame.format;
    out_info->pts = f->frame.pts;
    out_info->dts = f->frame.dts;
    out_info->is_key_frame = f->frame.is_key_frame;
    return MKFF_RESULT_OK;
}

static MKFF_Result sw_frame_map_cpu_planes(const MKFF_VideoFrame *frame, MKFF_CpuPlaneDesc *out_planes) {
    const SwVideoFrame *f = (const SwVideoFrame *)frame;
    if (!f || !out_planes) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }
    uint32_t requested = out_planes->struct_size;
    MKFF_INIT_STRUCT_HEADER(out_planes);
    if (requested) {
        out_planes->struct_size = requested;
    }
    out_planes->format = f->frame.format;
    out_planes->width = f->frame.width;
    out_planes->height = f->frame.height;
    out_planes->plane_count = 2;
    out_planes->data[0] = f->frame.data[0];
    out_planes->data[1] = f->frame.data[1];
    out_planes->stride[0] = f->frame.stride[0];
    out_planes->stride[1] = f->frame.stride[1];
    out_planes->height_lines[0] = f->frame.height;
    out_planes->height_lines[1] = f->frame.height / 2;
    return MKFF_RESULT_OK;
}

static void sw_frame_unmap_cpu_planes(const MKFF_VideoFrame *frame, MKFF_CpuPlaneDesc *planes) {
    (void)frame;
    (void)planes;
}

static void sw_decoder_destroy(void *decoder) {
    SwVideoDecoder *dec = (SwVideoDecoder *)decoder;
    if (!dec) {
        return;
    }
    hevc_sw_decoder_destroy(dec->hevc);
    free(dec);
}

static MKFF_Result sw_decoder_submit(void *decoder,
                                     const uint8_t *annex_b_data,
                                     size_t annex_b_size,
                                     int64_t pts,
                                     int64_t dts) {
    SwVideoDecoder *dec = (SwVideoDecoder *)decoder;
    return hevc_sw_decoder_submit(dec->hevc, annex_b_data, annex_b_size, pts, dts);
}

static MKFF_Result sw_decoder_receive(void *decoder, MKFF_VideoFrame **out_frame) {
    SwVideoDecoder *dec = (SwVideoDecoder *)decoder;
    if (!out_frame) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }
    *out_frame = NULL;
    HevcSwFrame raw;
    MKFF_Result r = hevc_sw_decoder_receive(dec->hevc, &raw);
    if (r != MKFF_RESULT_OK) {
        return r;
    }
    SwVideoFrame *f = (SwVideoFrame *)calloc(1, sizeof(*f));
    if (!f) {
        hevc_sw_frame_free(&raw);
        return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
    }
    f->common.api = &kSoftwarePlatformAPI;
    f->refcount = 1;
    f->frame = raw;
    *out_frame = (MKFF_VideoFrame *)f;
    return MKFF_RESULT_OK;
}

static MKFF_Result sw_decoder_flush(void *decoder) {
    SwVideoDecoder *dec = (SwVideoDecoder *)decoder;
    return hevc_sw_decoder_flush(dec->hevc);
}

static MKFF_Result sw_decoder_get_info(const void *decoder, MKFF_VideoDecoderInfo *out_info) {
    const SwVideoDecoder *dec = (const SwVideoDecoder *)decoder;
    if (!dec || !out_info) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }
    uint32_t requested = out_info->struct_size;
    MKFF_INIT_STRUCT_HEADER(out_info);
    if (requested) {
        out_info->struct_size = requested;
    }
    out_info->width = hevc_sw_decoder_width(dec->hevc);
    out_info->height = hevc_sw_decoder_height(dec->hevc);
    out_info->profile = hevc_sw_decoder_profile(dec->hevc);
    out_info->entrypoint = MKFF_VIDEO_ENTRYPOINT_VLD;
    out_info->output_format = hevc_sw_decoder_format(dec->hevc);
    out_info->surface_pool_size = 0;
    out_info->surface_pool_capacity = 0;
    out_info->backend = MKFF_VIDEO_BACKEND_SOFTWARE_ONLY;
    out_info->bit_depth = hevc_sw_decoder_bit_depth(dec->hevc);
    out_info->chroma_format_idc = 1;
    out_info->hardware = 0;
    (void)requested;
    return MKFF_RESULT_OK;
}

static const MKFF_PlatformAPI kSoftwarePlatformAPI = {
    .struct_size = sizeof(MKFF_PlatformAPI),
    .abi_version = MKFF_PLATFORM_ABI_VERSION,
    .video_decoder_destroy = sw_decoder_destroy,
    .video_decoder_submit = sw_decoder_submit,
    .video_decoder_receive = sw_decoder_receive,
    .video_decoder_flush = sw_decoder_flush,
    .video_decoder_get_info = sw_decoder_get_info,
    .video_frame_retain = sw_frame_retain,
    .video_frame_release = sw_frame_release,
    .video_frame_get_info = sw_frame_get_info,
    .video_frame_map_cpu_planes = sw_frame_map_cpu_planes,
    .video_frame_unmap_cpu_planes = sw_frame_unmap_cpu_planes,
};

#endif /* MKFF_ENABLE_HEVC_SOFTWARE */

int mkff_software_hevc_available(void) {
#if defined(MKFF_ENABLE_HEVC_SOFTWARE)
    return 1;
#else
    return 0;
#endif
}

MKFF_Result mkff_software_video_decoder_create(const MKFF_VideoDecoderDesc *desc,
                                               MKFF_VideoDecoder **out_decoder) {
    if (!out_decoder) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }
    *out_decoder = NULL;
    if (!desc) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }
#if !defined(MKFF_ENABLE_HEVC_SOFTWARE)
    (void)desc;
    return MKFF_RESULT_ERROR_CODEC_UNAVAILABLE;
#else
    if (desc->codec != MKFF_VIDEO_CODEC_HEVC) {
        return MKFF_RESULT_ERROR_NOT_SUPPORTED;
    }
    HevcSwDecoder *hevc = NULL;
    MKFF_Result r = hevc_sw_decoder_create(desc->width_hint, desc->height_hint, &hevc);
    if (r != MKFF_RESULT_OK) {
        return r;
    }
    SwVideoDecoder *dec = (SwVideoDecoder *)calloc(1, sizeof(*dec));
    if (!dec) {
        hevc_sw_decoder_destroy(hevc);
        return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
    }
    dec->common.api = &kSoftwarePlatformAPI;
    dec->hevc = hevc;
    dec->codec = MKFF_VIDEO_CODEC_HEVC;
    *out_decoder = (MKFF_VideoDecoder *)dec;
    return MKFF_RESULT_OK;
#endif
}
