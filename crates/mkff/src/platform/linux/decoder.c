#include "decoder.h"

#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <va/va.h>

#include "decoder_hevc.h"
#include "decoder_shared.h"
#include "drm_device.h"
#include "h264/h264_dpb.h"
#include "src/codecs/h264/h264_slice.h"
#include "src/codecs/h264/h264_sps_pps.h"
#include "platform_api_table.h"
#include "platform_context.h"
#include "src/common/mkff_log_util.h"
#include "va_display.h"
#include "video_frame.h"

#define DEFAULT_POOL_MIN 6
#define DEFAULT_POOL_MAX 16
#define READY_QUEUE_CAPACITY H264_OUTPUT_QUEUE_MAX

typedef struct LinuxVideoDecoder {
    MKFF_HandleCommon common; /* must be first */
    MKFF_VideoCodec   codec;  /* MKFF_VIDEO_CODEC_H264 — shared offset with HEVC decoder */

    DecoderShared *shared;
    int            initialized;
    VAProfile      va_profile;
    MKFF_VideoProfile mkff_profile;

    H264SPS      sps_table[H264_MAX_SPS_COUNT];
    H264PPS      pps_table[H264_MAX_PPS_COUNT];
    H264PocState poc_state;
    H264Dpb      dpb;

    uint32_t active_sps_id;
    uint32_t max_num_ref_frames;
    uint32_t max_frame_num;
    uint32_t coded_width, coded_height;
    uint32_t display_width, display_height;

    uint32_t requested_max_surfaces;

    LinuxVideoFrame *ready_queue[READY_QUEUE_CAPACITY];
    uint32_t         ready_count;
    int              flushed;

    MKFF_LogCallback log_callback;
    void            *log_user_data;
    MKFF_LogLevel    log_min_level;

    char last_error[256];
} LinuxVideoDecoder;

#define DLOG(dec, lvl, ...) MKFF_LOG((dec)->log_callback, (dec)->log_user_data, (dec)->log_min_level, (lvl), "mkff.platform.linux.h264", __VA_ARGS__)

static void set_last_error(LinuxVideoDecoder *dec, const char *msg) {
    snprintf(dec->last_error, sizeof(dec->last_error), "%s", msg);
    DLOG(dec, MKFF_LOG_LEVEL_ERROR, "%s", msg);
}

static MKFF_VideoProfile map_va_profile(VAProfile p) {
    switch (p) {
        case VAProfileH264ConstrainedBaseline: return MKFF_VIDEO_PROFILE_H264_CONSTRAINED_BASELINE;
        case VAProfileH264Main:                return MKFF_VIDEO_PROFILE_H264_MAIN;
        case VAProfileH264High:                return MKFF_VIDEO_PROFILE_H264_HIGH;
        default:                                return MKFF_VIDEO_PROFILE_UNKNOWN;
    }
}

static MKFF_Result ensure_va_initialized(LinuxVideoDecoder *dec, const H264SPS *sps) {
    uint32_t coded_w = sps->pic_width_in_mbs * 16;
    uint32_t coded_h = sps->pic_height_in_map_units * 16;

    if (dec->initialized) {
        if (coded_w != dec->coded_width || coded_h != dec->coded_height) {
            set_last_error(dec, "mid-stream resolution change is not supported in this milestone");
            return MKFF_RESULT_ERROR_NOT_SUPPORTED;
        }
        return MKFF_RESULT_OK;
    }

    char err[256] = {0};
    int drm_fd = linux_open_drm_device(NULL, err, sizeof(err));
    if (drm_fd < 0) {
        set_last_error(dec, err[0] ? err : "failed to open a DRM render node");
        return MKFF_RESULT_ERROR_DEVICE;
    }

    VADisplay dpy = linux_va_open(drm_fd, err, sizeof(err));
    if (!dpy) {
        close(drm_fd);
        set_last_error(dec, err[0] ? err : "VA-API initialization failed");
        return MKFF_RESULT_ERROR_DEVICE;
    }

    VAProfile candidates[4];
    int num_candidates = linux_va_h264_profile_candidates(sps->profile_idc, candidates, 4);
    if (num_candidates == 0) {
        linux_va_close(dpy);
        close(drm_fd);
        set_last_error(dec, "H.264 profile_idc is outside Baseline/Main/High");
        return MKFF_RESULT_ERROR_NOT_SUPPORTED;
    }

    VAProfile selected = linux_va_select_supported_profile(dpy, candidates, num_candidates);
    if (selected == VAProfileNone) {
        linux_va_close(dpy);
        close(drm_fd);
        set_last_error(dec, "no VA-API driver profile+VLD entrypoint available for this H.264 stream");
        return MKFF_RESULT_ERROR_NOT_SUPPORTED;
    }

    VAConfigAttrib attrib;
    attrib.type = VAConfigAttribRTFormat;
    attrib.value = VA_RT_FORMAT_YUV420;
    VAConfigID config;
    VAStatus status = vaCreateConfig(dpy, selected, VAEntrypointVLD, &attrib, 1, &config);
    if (status != VA_STATUS_SUCCESS) {
        set_last_error(dec, vaErrorStr(status));
        linux_va_close(dpy);
        close(drm_fd);
        return MKFF_RESULT_ERROR_DEVICE;
    }

    uint32_t pool_capacity = dec->requested_max_surfaces;
    if (pool_capacity == 0) {
        uint32_t base = sps->max_num_ref_frames + 4;
        pool_capacity = base < DEFAULT_POOL_MIN ? DEFAULT_POOL_MIN : (base > DEFAULT_POOL_MAX ? DEFAULT_POOL_MAX : base);
    }

    DecoderShared *shared = decoder_shared_create(pool_capacity, dec->log_callback, dec->log_user_data, dec->log_min_level);
    if (!shared) {
        vaDestroyConfig(dpy, config);
        linux_va_close(dpy);
        close(drm_fd);
        set_last_error(dec, "out of memory allocating the surface pool");
        return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
    }
    shared->drm_fd = drm_fd;
    shared->va_dpy = dpy;
    shared->va_initialized = 1;
    shared->va_config = config;
    shared->coded_width = coded_w;
    shared->coded_height = coded_h;
    shared->profile = map_va_profile(selected);

    VASurfaceAttrib surf_attrib;
    surf_attrib.type = VASurfaceAttribPixelFormat;
    surf_attrib.flags = VA_SURFACE_ATTRIB_SETTABLE;
    surf_attrib.value.type = VAGenericValueTypeInteger;
    surf_attrib.value.value.i = VA_FOURCC_NV12;

    status = vaCreateSurfaces(dpy, VA_RT_FORMAT_YUV420, coded_w, coded_h, shared->pool_surfaces, pool_capacity, &surf_attrib, 1);
    if (status != VA_STATUS_SUCCESS) {
        set_last_error(dec, vaErrorStr(status));
        decoder_shared_unref(shared);
        return MKFF_RESULT_ERROR_DEVICE;
    }

    VAContextID context;
    status = vaCreateContext(dpy, config, (int)coded_w, (int)coded_h, VA_PROGRESSIVE, shared->pool_surfaces, (int)pool_capacity, &context);
    if (status != VA_STATUS_SUCCESS) {
        set_last_error(dec, vaErrorStr(status));
        decoder_shared_unref(shared);
        return MKFF_RESULT_ERROR_DEVICE;
    }
    shared->va_context = context;
    shared->va_context_created = 1;

    dec->shared = shared;
    dec->initialized = 1;
    dec->va_profile = selected;
    dec->mkff_profile = map_va_profile(selected);
    dec->coded_width = coded_w;
    dec->coded_height = coded_h;

    DLOG(dec, MKFF_LOG_LEVEL_INFO, "initialized VA-API decode: profile=%d %ux%u pool_capacity=%u", (int)selected, coded_w, coded_h, pool_capacity);
    return MKFF_RESULT_OK;
}

static MKFF_VideoCodec decoder_codec(const void *decoder_v) {
    /* Both LinuxVideoDecoder and LinuxHevcVideoDecoder store codec immediately
     * after MKFF_HandleCommon. */
    return *(const MKFF_VideoCodec *)((const char *)decoder_v + sizeof(MKFF_HandleCommon));
}

MKFF_Result linux_video_decoder_create(MKFF_PlatformContext *pctx, const MKFF_VideoDecoderDesc *desc, void **out_decoder) {
    if (desc->codec == MKFF_VIDEO_CODEC_HEVC) {
        return linux_hevc_video_decoder_create(pctx, desc, out_decoder);
    }
    if (desc->codec != MKFF_VIDEO_CODEC_H264) {
        return MKFF_RESULT_ERROR_NOT_SUPPORTED;
    }

    LinuxVideoDecoder *dec = (LinuxVideoDecoder *)calloc(1, sizeof(LinuxVideoDecoder));
    if (!dec) {
        return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
    }

    dec->common.api = mkff_linux_platform_api();
    dec->codec = MKFF_VIDEO_CODEC_H264;
    dec->log_callback = pctx->log_callback;
    dec->log_user_data = pctx->log_user_data;
    dec->log_min_level = pctx->log_min_level;
    dec->requested_max_surfaces = desc->max_surfaces;
    h264_dpb_init(&dec->dpb);

    *out_decoder = dec;
    return MKFF_RESULT_OK;
}

void linux_video_decoder_destroy(void *decoder_v) {
    if (!decoder_v) return;
    if (decoder_codec(decoder_v) == MKFF_VIDEO_CODEC_HEVC) {
        linux_hevc_video_decoder_destroy(decoder_v);
        return;
    }

    LinuxVideoDecoder *dec = (LinuxVideoDecoder *)decoder_v;

    h264_dpb_reset(&dec->dpb);
    for (uint32_t i = 0; i < dec->ready_count; i++) {
        linux_video_frame_release((MKFF_VideoFrame *)dec->ready_queue[i]);
    }
    if (dec->shared) {
        decoder_shared_unref(dec->shared);
    }
    free(dec);
}

static void push_ready(LinuxVideoDecoder *dec, LinuxVideoFrame *frame) {
    if (!frame) return;
    if (dec->ready_count >= READY_QUEUE_CAPACITY) {
        /* Defensive: should not happen given DPB/output-queue sizing. */
        linux_video_frame_release((MKFF_VideoFrame *)frame);
        return;
    }
    dec->ready_queue[dec->ready_count++] = frame;
}

static VAPictureH264 invalid_va_picture(void) {
    VAPictureH264 p;
    memset(&p, 0, sizeof(p));
    p.picture_id = VA_INVALID_SURFACE;
    p.flags = VA_PICTURE_H264_INVALID;
    return p;
}

static void submit_picture_params(LinuxVideoDecoder *dec, const H264SPS *sps, const H264PPS *pps, const H264SliceHeader *sh, VASurfaceID curr_surface, int32_t poc) {
    VAPictureParameterBufferH264 pp;
    memset(&pp, 0, sizeof(pp));

    pp.CurrPic.picture_id = curr_surface;
    pp.CurrPic.frame_idx = sh->frame_num;
    pp.CurrPic.flags = (sh->nal_ref_idc != 0) ? VA_PICTURE_H264_SHORT_TERM_REFERENCE : 0;
    pp.CurrPic.TopFieldOrderCnt = poc;
    pp.CurrPic.BottomFieldOrderCnt = poc;

    uint32_t nref = h264_dpb_fill_reference_frames(&dec->dpb, pp.ReferenceFrames, 16);
    for (uint32_t i = nref; i < 16; i++) {
        pp.ReferenceFrames[i] = invalid_va_picture();
    }

    pp.picture_width_in_mbs_minus1 = (uint16_t)(sps->pic_width_in_mbs - 1);
    pp.picture_height_in_mbs_minus1 = (uint16_t)(sps->pic_height_in_map_units - 1);
    pp.bit_depth_luma_minus8 = 0;
    pp.bit_depth_chroma_minus8 = 0;
    pp.num_ref_frames = (uint8_t)sps->max_num_ref_frames;

    pp.seq_fields.bits.chroma_format_idc = sps->chroma_format_idc;
    pp.seq_fields.bits.residual_colour_transform_flag = 0;
    pp.seq_fields.bits.gaps_in_frame_num_value_allowed_flag = sps->gaps_in_frame_num_value_allowed_flag;
    pp.seq_fields.bits.frame_mbs_only_flag = sps->frame_mbs_only_flag;
    pp.seq_fields.bits.mb_adaptive_frame_field_flag = 0;
    pp.seq_fields.bits.direct_8x8_inference_flag = sps->direct_8x8_inference_flag;
    pp.seq_fields.bits.MinLumaBiPredSize8x8 = (sps->profile_idc >= 100) ? 1 : 0;
    pp.seq_fields.bits.log2_max_frame_num_minus4 = sps->log2_max_frame_num - 4;
    pp.seq_fields.bits.pic_order_cnt_type = sps->pic_order_cnt_type;
    pp.seq_fields.bits.log2_max_pic_order_cnt_lsb_minus4 = (sps->pic_order_cnt_type == 0) ? sps->log2_max_pic_order_cnt_lsb - 4 : 0;
    pp.seq_fields.bits.delta_pic_order_always_zero_flag = sps->delta_pic_order_always_zero_flag;

    pp.pic_init_qp_minus26 = (int8_t)pps->pic_init_qp_minus26;
    pp.pic_init_qs_minus26 = (int8_t)pps->pic_init_qs_minus26;
    pp.chroma_qp_index_offset = (int8_t)pps->chroma_qp_index_offset;
    pp.second_chroma_qp_index_offset = (int8_t)pps->second_chroma_qp_index_offset;

    pp.pic_fields.bits.entropy_coding_mode_flag = pps->entropy_coding_mode_flag;
    pp.pic_fields.bits.weighted_pred_flag = pps->weighted_pred_flag;
    pp.pic_fields.bits.weighted_bipred_idc = pps->weighted_bipred_idc;
    pp.pic_fields.bits.transform_8x8_mode_flag = pps->transform_8x8_mode_flag;
    pp.pic_fields.bits.field_pic_flag = 0;
    pp.pic_fields.bits.constrained_intra_pred_flag = pps->constrained_intra_pred_flag;
    pp.pic_fields.bits.pic_order_present_flag = pps->bottom_field_pic_order_in_frame_present_flag;
    pp.pic_fields.bits.deblocking_filter_control_present_flag = pps->deblocking_filter_control_present_flag;
    pp.pic_fields.bits.redundant_pic_cnt_present_flag = pps->redundant_pic_cnt_present_flag;
    pp.pic_fields.bits.reference_pic_flag = (sh->nal_ref_idc != 0);

    pp.frame_num = (uint16_t)sh->frame_num;

    VABufferID buf;
    vaCreateBuffer(dec->shared->va_dpy, dec->shared->va_context, VAPictureParameterBufferType, sizeof(pp), 1, &pp, &buf);
    vaRenderPicture(dec->shared->va_dpy, dec->shared->va_context, &buf, 1);

    VAIQMatrixBufferH264 iq;
    memset(&iq, 0, sizeof(iq));
    memset(iq.ScalingList4x4, 16, sizeof(iq.ScalingList4x4));
    memset(iq.ScalingList8x8, 16, sizeof(iq.ScalingList8x8));
    VABufferID iq_buf;
    vaCreateBuffer(dec->shared->va_dpy, dec->shared->va_context, VAIQMatrixBufferType, sizeof(iq), 1, &iq, &iq_buf);
    vaRenderPicture(dec->shared->va_dpy, dec->shared->va_context, &iq_buf, 1);
}

static void submit_slice(LinuxVideoDecoder *dec, const H264SliceHeader *sh, const H264NalUnit *nal, int32_t poc) {
    VASliceParameterBufferH264 sp;
    memset(&sp, 0, sizeof(sp));

    sp.slice_data_size = (uint32_t)nal->size;
    sp.slice_data_offset = 0;
    sp.slice_data_flag = VA_SLICE_DATA_FLAG_ALL;
    sp.slice_data_bit_offset = (uint16_t)sh->header_bits;
    sp.first_mb_in_slice = (uint16_t)sh->first_mb_in_slice;
    sp.slice_type = (uint8_t)sh->slice_type;
    sp.direct_spatial_mv_pred_flag = (uint8_t)sh->direct_spatial_mv_pred_flag;
    sp.num_ref_idx_l0_active_minus1 = (uint8_t)sh->num_ref_idx_l0_active_minus1;
    sp.num_ref_idx_l1_active_minus1 = (uint8_t)sh->num_ref_idx_l1_active_minus1;
    sp.cabac_init_idc = (uint8_t)sh->cabac_init_idc;
    sp.slice_qp_delta = (int8_t)sh->slice_qp_delta;
    sp.disable_deblocking_filter_idc = (uint8_t)sh->disable_deblocking_filter_idc;
    sp.slice_alpha_c0_offset_div2 = (int8_t)sh->slice_alpha_c0_offset_div2;
    sp.slice_beta_offset_div2 = (int8_t)sh->slice_beta_offset_div2;

    int used_long_term = 0;
    uint32_t n0 = h264_dpb_build_ref_pic_list0(&dec->dpb, sh, poc, dec->max_frame_num, sp.RefPicList0, 32, &used_long_term);
    uint32_t n1 = h264_dpb_build_ref_pic_list1(&dec->dpb, sh, poc, dec->max_frame_num, sp.RefPicList1, 32, &used_long_term);
    for (uint32_t i = n0; i < 32; i++) sp.RefPicList0[i] = invalid_va_picture();
    for (uint32_t i = n1; i < 32; i++) sp.RefPicList1[i] = invalid_va_picture();
    if (used_long_term) {
        DLOG(dec, MKFF_LOG_LEVEL_WARN, "%s", "slice requested long-term reference reordering, which this milestone's DPB does not support; ignored");
    }

    if (sh->has_pred_weight_table) {
        sp.luma_log2_weight_denom = (uint8_t)sh->luma_log2_weight_denom;
        sp.chroma_log2_weight_denom = (uint8_t)sh->chroma_log2_weight_denom;
        sp.luma_weight_l0_flag = 1;
        sp.chroma_weight_l0_flag = 1;
        memcpy(sp.luma_weight_l0, sh->luma_weight_l0, sizeof(sp.luma_weight_l0));
        memcpy(sp.luma_offset_l0, sh->luma_offset_l0, sizeof(sp.luma_offset_l0));
        memcpy(sp.chroma_weight_l0, sh->chroma_weight_l0, sizeof(sp.chroma_weight_l0));
        memcpy(sp.chroma_offset_l0, sh->chroma_offset_l0, sizeof(sp.chroma_offset_l0));
        if (sh->slice_type == H264_SLICE_TYPE_B) {
            sp.luma_weight_l1_flag = 1;
            sp.chroma_weight_l1_flag = 1;
            memcpy(sp.luma_weight_l1, sh->luma_weight_l1, sizeof(sp.luma_weight_l1));
            memcpy(sp.luma_offset_l1, sh->luma_offset_l1, sizeof(sp.luma_offset_l1));
            memcpy(sp.chroma_weight_l1, sh->chroma_weight_l1, sizeof(sp.chroma_weight_l1));
            memcpy(sp.chroma_offset_l1, sh->chroma_offset_l1, sizeof(sp.chroma_offset_l1));
        }
    }

    VABufferID buf;
    vaCreateBuffer(dec->shared->va_dpy, dec->shared->va_context, VASliceParameterBufferType, sizeof(sp), 1, &sp, &buf);
    vaRenderPicture(dec->shared->va_dpy, dec->shared->va_context, &buf, 1);

    VABufferID data_buf;
    vaCreateBuffer(dec->shared->va_dpy, dec->shared->va_context, VASliceDataBufferType, (unsigned int)nal->size, 1, (void *)nal->data, &data_buf);
    vaRenderPicture(dec->shared->va_dpy, dec->shared->va_context, &data_buf, 1);
}

MKFF_Result linux_video_decoder_submit(void *decoder_v, const uint8_t *annex_b_data, size_t annex_b_size, int64_t pts, int64_t dts) {
    if (decoder_codec(decoder_v) == MKFF_VIDEO_CODEC_HEVC) {
        return linux_hevc_video_decoder_submit(decoder_v, annex_b_data, annex_b_size, pts, dts);
    }

    LinuxVideoDecoder *dec = (LinuxVideoDecoder *)decoder_v;

    if (annex_b_size == 0) {
        return MKFF_RESULT_OK;
    }

    H264NalUnit nals[H264_MAX_NAL_UNITS_PER_AU];
    size_t nal_count = h264_split_annex_b(annex_b_data, annex_b_size, nals, H264_MAX_NAL_UNITS_PER_AU);
    if (nal_count == 0) {
        set_last_error(dec, "no NAL units found (missing Annex-B start code)");
        return MKFF_RESULT_ERROR_BITSTREAM;
    }

    uint8_t *rbsp_scratch = (uint8_t *)malloc(annex_b_size);
    if (!rbsp_scratch) {
        return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
    }

    int picture_open = 0;
    VASurfaceID pic_surface = VA_INVALID_SURFACE;
    uint32_t pic_pool_index = 0;
    LinuxVideoFrame *pic_frame = NULL;
    const H264SPS *pic_sps = NULL;
    H264SliceHeader pic_first_sh;
    int32_t pic_poc = 0;
    MKFF_Result result = MKFF_RESULT_OK;

    for (size_t i = 0; i < nal_count && result == MKFF_RESULT_OK; i++) {
        const H264NalUnit *nal = &nals[i];

        if (nal->nal_unit_type == 7) { /* SPS */
            H264SPS tmp;
            int rc = h264_parse_sps(nal->data, nal->size, rbsp_scratch, &tmp);
            if (rc == 0 && tmp.seq_parameter_set_id < H264_MAX_SPS_COUNT) {
                dec->sps_table[tmp.seq_parameter_set_id] = tmp;
            } else if (tmp.seq_parameter_set_id < H264_MAX_SPS_COUNT) {
                DLOG(dec, MKFF_LOG_LEVEL_WARN, "rejecting SPS id=%u (rc=%d): outside this milestone's supported subset", tmp.seq_parameter_set_id, rc);
                dec->sps_table[tmp.seq_parameter_set_id].valid = 0;
            }
            continue;
        }

        if (nal->nal_unit_type == 8) { /* PPS */
            H264PPS tmp;
            int rc = h264_parse_pps(nal->data, nal->size, rbsp_scratch, dec->sps_table, &tmp);
            if (rc == 0 && tmp.pic_parameter_set_id < H264_MAX_PPS_COUNT) {
                dec->pps_table[tmp.pic_parameter_set_id] = tmp;
            } else if (tmp.pic_parameter_set_id < H264_MAX_PPS_COUNT) {
                DLOG(dec, MKFF_LOG_LEVEL_WARN, "rejecting PPS id=%u (rc=%d): outside this milestone's supported subset", tmp.pic_parameter_set_id, rc);
                dec->pps_table[tmp.pic_parameter_set_id].valid = 0;
            }
            continue;
        }

        if (nal->nal_unit_type != 1 && nal->nal_unit_type != 5) {
            continue; /* SEI, AUD, etc.: not needed to drive VLD decode */
        }

        H264SliceHeader sh;
        int rc = h264_parse_slice_header(nal, rbsp_scratch, dec->sps_table, dec->pps_table, &sh);
        if (rc != 0) {
            DLOG(dec, MKFF_LOG_LEVEL_WARN, "dropping slice NAL (rc=%d)", rc);
            continue;
        }

        const H264PPS *pps = &dec->pps_table[sh.pic_parameter_set_id];
        const H264SPS *sps = &dec->sps_table[pps->seq_parameter_set_id];

        if (!picture_open) {
            if (!dec->initialized && !sh.is_idr) {
                DLOG(dec, MKFF_LOG_LEVEL_DEBUG, "%s", "dropping access unit before the first IDR");
                break;
            }

            MKFF_Result init_result = ensure_va_initialized(dec, sps);
            if (init_result != MKFF_RESULT_OK) {
                result = init_result;
                break;
            }

            dec->active_sps_id = pps->seq_parameter_set_id;
            dec->max_num_ref_frames = sps->max_num_ref_frames;
            dec->max_frame_num = (uint32_t)1 << sps->log2_max_frame_num;

            uint32_t crop_unit_x = 2; /* SubWidthC for 4:2:0 */
            uint32_t crop_unit_y = 2; /* SubHeightC(=2) * (2 - frame_mbs_only_flag(=1)) */
            dec->display_width = dec->coded_width;
            dec->display_height = dec->coded_height;
            if (sps->frame_cropping_flag) {
                dec->display_width -= (sps->crop_left + sps->crop_right) * crop_unit_x;
                dec->display_height -= (sps->crop_top + sps->crop_bottom) * crop_unit_y;
            }

            if (decoder_shared_pool_checkout(dec->shared, &pic_pool_index, &pic_surface) != 0) {
                set_last_error(dec, "surface pool exhausted: caller must release frames before submitting more");
                result = MKFF_RESULT_ERROR_POOL_EXHAUSTED;
                break;
            }

            pic_frame = linux_video_frame_create(dec->shared, pic_pool_index, pic_surface, dec->display_width, dec->display_height, pts, dts, sh.is_idr, MKFF_PIXEL_FORMAT_NV12);
            if (!pic_frame) {
                decoder_shared_pool_release(dec->shared, pic_pool_index);
                result = MKFF_RESULT_ERROR_OUT_OF_MEMORY;
                break;
            }

            pic_poc = h264_compute_poc(sps, &sh, &dec->poc_state);
            pic_first_sh = sh;
            pic_sps = sps;
            picture_open = 1;

            vaBeginPicture(dec->shared->va_dpy, dec->shared->va_context, pic_surface);
            submit_picture_params(dec, sps, pps, &sh, pic_surface, pic_poc);
        }

        submit_slice(dec, &sh, nal, pic_poc);
    }

    if (picture_open) {
        VAStatus end_status = vaEndPicture(dec->shared->va_dpy, dec->shared->va_context);
        if (end_status != VA_STATUS_SUCCESS) {
            DLOG(dec, MKFF_LOG_LEVEL_ERROR, "vaEndPicture failed: %s", vaErrorStr(end_status));
        }
        VAStatus sync_status = vaSyncSurface(dec->shared->va_dpy, pic_surface);
        if (sync_status != VA_STATUS_SUCCESS) {
            DLOG(dec, MKFF_LOG_LEVEL_WARN, "vaSyncSurface reported an issue: %s", vaErrorStr(sync_status));
        }

        h264_dpb_add_reference(&dec->dpb, pic_frame, pic_first_sh.frame_num, pic_poc, pic_sps->max_num_ref_frames, pic_first_sh.nal_ref_idc != 0);
        h264_dpb_push_output(&dec->dpb, pic_frame, pic_poc);
        linux_video_frame_release((MKFF_VideoFrame *)pic_frame); /* drop local creation reference */

        uint32_t max_reorder = pic_sps->max_num_ref_frames;
        if (max_reorder > 16) max_reorder = 16;
        push_ready(dec, h264_dpb_bump_if_needed(&dec->dpb, max_reorder));
    }

    free(rbsp_scratch);
    return result;
}

MKFF_Result linux_video_decoder_receive(void *decoder_v, MKFF_VideoFrame **out_frame) {
    if (decoder_codec(decoder_v) == MKFF_VIDEO_CODEC_HEVC) {
        return linux_hevc_video_decoder_receive(decoder_v, out_frame);
    }

    LinuxVideoDecoder *dec = (LinuxVideoDecoder *)decoder_v;

    if (dec->ready_count > 0) {
        *out_frame = (MKFF_VideoFrame *)dec->ready_queue[0];
        for (uint32_t i = 1; i < dec->ready_count; i++) {
            dec->ready_queue[i - 1] = dec->ready_queue[i];
        }
        dec->ready_count--;
        return MKFF_RESULT_OK;
    }

    return dec->flushed ? MKFF_RESULT_END_OF_STREAM : MKFF_RESULT_NOT_READY;
}

MKFF_Result linux_video_decoder_flush(void *decoder_v) {
    if (decoder_codec(decoder_v) == MKFF_VIDEO_CODEC_HEVC) {
        return linux_hevc_video_decoder_flush(decoder_v);
    }

    LinuxVideoDecoder *dec = (LinuxVideoDecoder *)decoder_v;

    LinuxVideoFrame *f;
    while ((f = h264_dpb_bump_one(&dec->dpb)) != NULL) {
        push_ready(dec, f);
    }
    dec->flushed = 1;
    return MKFF_RESULT_OK;
}

MKFF_Result linux_video_decoder_get_info(const void *decoder_v, MKFF_VideoDecoderInfo *out_info) {
    if (decoder_codec(decoder_v) == MKFF_VIDEO_CODEC_HEVC) {
        return linux_hevc_video_decoder_get_info(decoder_v, out_info);
    }

    const LinuxVideoDecoder *dec = (const LinuxVideoDecoder *)decoder_v;

    uint32_t requested_size = out_info->struct_size;
    memset(out_info, 0, sizeof(*out_info));
    MKFF_INIT_STRUCT_HEADER(out_info);
    out_info->struct_size = requested_size ? requested_size : (uint32_t)sizeof(*out_info);

    out_info->width = dec->display_width;
    out_info->height = dec->display_height;
    out_info->profile = dec->mkff_profile;
    out_info->entrypoint = dec->initialized ? MKFF_VIDEO_ENTRYPOINT_VLD : MKFF_VIDEO_ENTRYPOINT_UNKNOWN;
    out_info->output_format = MKFF_PIXEL_FORMAT_NV12;

    if (dec->shared) {
        uint32_t in_use = 0;
        pthread_mutex_lock(&dec->shared->pool_lock);
        for (uint32_t i = 0; i < dec->shared->pool_capacity; i++) {
            if (dec->shared->pool_in_use[i]) in_use++;
        }
        pthread_mutex_unlock(&dec->shared->pool_lock);
        out_info->surface_pool_size = in_use;
        out_info->surface_pool_capacity = dec->shared->pool_capacity;
    }

    if (out_info->struct_size >= offsetof(MKFF_VideoDecoderInfo, hardware) + sizeof(uint32_t)) {
        out_info->backend = MKFF_VIDEO_BACKEND_HARDWARE_ONLY;
        out_info->bit_depth = dec->initialized ? 8u : 0u;
        out_info->chroma_format_idc = dec->initialized ? 1u : 0u;
        out_info->hardware = dec->initialized ? 1u : 0u;
    }

    return MKFF_RESULT_OK;
}
