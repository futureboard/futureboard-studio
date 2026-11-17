#include "decoder_hevc.h"

#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <va/va.h>
#include <va/va_dec_hevc.h>

#include "decoder_shared.h"
#include "drm_device.h"
#include "hevc/hevc_dpb.h"
#include "platform_api_table.h"
#include "platform_context.h"
#include "src/codecs/hevc/hevc_bitstream.h"
#include "src/codecs/hevc/hevc_slice.h"
#include "src/codecs/hevc/hevc_vps_sps_pps.h"
#include "src/common/mkff_log_util.h"
#include "va_display.h"
#include "video_frame.h"

#define DEFAULT_POOL_MIN 6
#define DEFAULT_POOL_MAX 16
#define READY_QUEUE_CAPACITY HEVC_OUTPUT_QUEUE_MAX

typedef struct LinuxHevcVideoDecoder {
    MKFF_HandleCommon common; /* must be first */
    MKFF_VideoCodec   codec;  /* MKFF_VIDEO_CODEC_HEVC — dispatch tag */

    DecoderShared *shared;
    int            initialized;
    VAProfile      va_profile;
    MKFF_VideoProfile mkff_profile;
    MKFF_PixelFormat  output_format;
    uint32_t          bit_depth;
    uint32_t          chroma_format_idc;

    HevcVPS      vps_table[HEVC_MAX_VPS_COUNT];
    HevcSPS      sps_table[HEVC_MAX_SPS_COUNT];
    HevcPPS      pps_table[HEVC_MAX_PPS_COUNT];
    HevcPocState poc_state;
    HevcDpb      dpb;

    VAPictureHEVC pic_refs[15];

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
} LinuxHevcVideoDecoder;

#define DLOG(dec, lvl, ...) \
    MKFF_LOG((dec)->log_callback, (dec)->log_user_data, (dec)->log_min_level, (lvl), \
             "mkff.platform.linux.hevc", __VA_ARGS__)

static void set_last_error(LinuxHevcVideoDecoder *dec, const char *msg) {
    snprintf(dec->last_error, sizeof(dec->last_error), "%s", msg);
    DLOG(dec, MKFF_LOG_LEVEL_ERROR, "%s", msg);
}

static int hevc_nal_is_ref(uint8_t nal_unit_type) {
    if (nal_unit_type >= 16 && nal_unit_type <= 21) {
        return 1;
    }
    if (nal_unit_type <= 9) {
        return (nal_unit_type & 1) != 0;
    }
    return 0;
}

static int au_has_later_vcl(const HevcNalUnit *nals, size_t nal_count, size_t index) {
    for (size_t i = index + 1; i < nal_count; i++) {
        if (hevc_nal_is_vcl(nals[i].nal_unit_type)) {
            return 1;
        }
    }
    return 0;
}

static void push_ready(LinuxHevcVideoDecoder *dec, LinuxVideoFrame *frame) {
    if (!frame) return;
    if (dec->ready_count >= READY_QUEUE_CAPACITY) {
        linux_video_frame_release((MKFF_VideoFrame *)frame);
        return;
    }
    dec->ready_queue[dec->ready_count++] = frame;
}

static MKFF_Result ensure_va_initialized_hevc(LinuxHevcVideoDecoder *dec, const HevcSPS *sps) {
    uint32_t coded_w = sps->pic_width_in_luma_samples;
    uint32_t coded_h = sps->pic_height_in_luma_samples;
    uint32_t bit_depth = sps->bit_depth;
    unsigned int rt_format = (bit_depth == 10) ? VA_RT_FORMAT_YUV420_10 : VA_RT_FORMAT_YUV420;
    uint32_t fourcc = (bit_depth == 10) ? VA_FOURCC_P010 : VA_FOURCC_NV12;
    MKFF_PixelFormat pixel_format = (bit_depth == 10) ? MKFF_PIXEL_FORMAT_P010 : MKFF_PIXEL_FORMAT_NV12;

    if (dec->initialized) {
        if (coded_w != dec->coded_width || coded_h != dec->coded_height || bit_depth != dec->bit_depth) {
            set_last_error(dec, "mid-stream resolution/bit-depth change is not supported in this milestone");
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

    VAProfile candidates[2];
    int num_candidates = linux_va_hevc_profile_candidates(bit_depth, candidates, 2);
    if (num_candidates == 0) {
        linux_va_close(dpy);
        close(drm_fd);
        set_last_error(dec, "HEVC bit depth is outside Main/Main10");
        return MKFF_RESULT_ERROR_NOT_SUPPORTED;
    }

    VAProfile selected = linux_va_select_supported_profile(dpy, candidates, num_candidates);
    if (selected == VAProfileNone) {
        linux_va_close(dpy);
        close(drm_fd);
        set_last_error(dec, bit_depth == 10
            ? "no VA-API driver profile+VLD entrypoint available for HEVC Main10"
            : "no VA-API driver profile+VLD entrypoint available for HEVC Main");
        return MKFF_RESULT_ERROR_NOT_SUPPORTED;
    }

    VAConfigAttrib attrib;
    attrib.type = VAConfigAttribRTFormat;
    attrib.value = rt_format;
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
        uint32_t max_dpb = sps->sps_max_dec_pic_buffering_minus1[sps->sps_max_sub_layers_minus1] + 1;
        uint32_t base = max_dpb + 3;
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
    shared->profile = (bit_depth == 10) ? MKFF_VIDEO_PROFILE_HEVC_MAIN10 : MKFF_VIDEO_PROFILE_HEVC_MAIN;

    VASurfaceAttrib surf_attrib;
    surf_attrib.type = VASurfaceAttribPixelFormat;
    surf_attrib.flags = VA_SURFACE_ATTRIB_SETTABLE;
    surf_attrib.value.type = VAGenericValueTypeInteger;
    surf_attrib.value.value.i = (int)fourcc;

    status = vaCreateSurfaces(dpy, rt_format, coded_w, coded_h, shared->pool_surfaces, pool_capacity, &surf_attrib, 1);
    if (status != VA_STATUS_SUCCESS) {
        set_last_error(dec, vaErrorStr(status));
        decoder_shared_unref(shared);
        return MKFF_RESULT_ERROR_DEVICE;
    }

    VAContextID context;
    status = vaCreateContext(dpy, config, (int)coded_w, (int)coded_h, VA_PROGRESSIVE,
                             shared->pool_surfaces, (int)pool_capacity, &context);
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
    dec->mkff_profile = (bit_depth == 10) ? MKFF_VIDEO_PROFILE_HEVC_MAIN10 : MKFF_VIDEO_PROFILE_HEVC_MAIN;
    dec->output_format = pixel_format;
    dec->bit_depth = bit_depth;
    dec->chroma_format_idc = 1;
    dec->coded_width = coded_w;
    dec->coded_height = coded_h;

    DLOG(dec, MKFF_LOG_LEVEL_INFO, "initialized VA-API HEVC decode: profile=%d %ux%u bit_depth=%u pool_capacity=%u",
         (int)selected, coded_w, coded_h, bit_depth, pool_capacity);
    return MKFF_RESULT_OK;
}

static void submit_picture_params_hevc(LinuxHevcVideoDecoder *dec,
                                       const HevcSPS *sps,
                                       const HevcPPS *pps,
                                       const HevcSliceHeader *first_sh,
                                       VASurfaceID curr_surface,
                                       int32_t poc) {
    VAPictureParameterBufferHEVC pp;
    memset(&pp, 0, sizeof(pp));

    pp.CurrPic.picture_id = curr_surface;
    pp.CurrPic.pic_order_cnt = poc;
    pp.CurrPic.flags = 0;

    hevc_dpb_fill_reference_frames(&dec->dpb, poc, &first_sh->st_rps, pp.ReferenceFrames);
    memcpy(dec->pic_refs, pp.ReferenceFrames, sizeof(dec->pic_refs));

    pp.pic_width_in_luma_samples = (uint16_t)sps->pic_width_in_luma_samples;
    pp.pic_height_in_luma_samples = (uint16_t)sps->pic_height_in_luma_samples;

    pp.pic_fields.bits.chroma_format_idc = sps->chroma_format_idc;
    pp.pic_fields.bits.separate_colour_plane_flag = (uint32_t)sps->separate_colour_plane_flag;
    pp.pic_fields.bits.pcm_enabled_flag = 0;
    pp.pic_fields.bits.scaling_list_enabled_flag = (uint32_t)sps->scaling_list_enabled_flag;
    pp.pic_fields.bits.transform_skip_enabled_flag = (uint32_t)pps->transform_skip_enabled_flag;
    pp.pic_fields.bits.amp_enabled_flag = (uint32_t)sps->amp_enabled_flag;
    pp.pic_fields.bits.strong_intra_smoothing_enabled_flag = (uint32_t)sps->strong_intra_smoothing_enabled_flag;
    pp.pic_fields.bits.sign_data_hiding_enabled_flag = (uint32_t)pps->sign_data_hiding_enabled_flag;
    pp.pic_fields.bits.constrained_intra_pred_flag = (uint32_t)pps->constrained_intra_pred_flag;
    pp.pic_fields.bits.cu_qp_delta_enabled_flag = (uint32_t)pps->cu_qp_delta_enabled_flag;
    pp.pic_fields.bits.weighted_pred_flag = (uint32_t)pps->weighted_pred_flag;
    pp.pic_fields.bits.weighted_bipred_flag = (uint32_t)pps->weighted_bipred_flag;
    pp.pic_fields.bits.transquant_bypass_enabled_flag = (uint32_t)pps->transquant_bypass_enabled_flag;
    pp.pic_fields.bits.tiles_enabled_flag = 0;
    pp.pic_fields.bits.entropy_coding_sync_enabled_flag = (uint32_t)pps->entropy_coding_sync_enabled_flag;
    pp.pic_fields.bits.pps_loop_filter_across_slices_enabled_flag =
        (uint32_t)pps->pps_loop_filter_across_slices_enabled_flag;
    pp.pic_fields.bits.loop_filter_across_tiles_enabled_flag = 0;
    pp.pic_fields.bits.pcm_loop_filter_disabled_flag = 0;
    pp.pic_fields.bits.NoPicReorderingFlag =
        (sps->sps_max_num_reorder_pics[sps->sps_max_sub_layers_minus1] == 0) ? 1u : 0u;
    pp.pic_fields.bits.NoBiPredFlag = 0;

    pp.sps_max_dec_pic_buffering_minus1 =
        (uint8_t)sps->sps_max_dec_pic_buffering_minus1[sps->sps_max_sub_layers_minus1];
    pp.bit_depth_luma_minus8 = (uint8_t)sps->bit_depth_luma_minus8;
    pp.bit_depth_chroma_minus8 = (uint8_t)sps->bit_depth_chroma_minus8;
    pp.log2_min_luma_coding_block_size_minus3 = (uint8_t)sps->log2_min_luma_coding_block_size_minus3;
    pp.log2_diff_max_min_luma_coding_block_size = (uint8_t)sps->log2_diff_max_min_luma_coding_block_size;
    pp.log2_min_transform_block_size_minus2 = (uint8_t)sps->log2_min_luma_transform_block_size_minus2;
    pp.log2_diff_max_min_transform_block_size = (uint8_t)sps->log2_diff_max_min_luma_transform_block_size;
    pp.max_transform_hierarchy_depth_intra = (uint8_t)sps->max_transform_hierarchy_depth_intra;
    pp.max_transform_hierarchy_depth_inter = (uint8_t)sps->max_transform_hierarchy_depth_inter;
    pp.init_qp_minus26 = (int8_t)pps->init_qp_minus26;
    pp.diff_cu_qp_delta_depth = (uint8_t)pps->diff_cu_qp_delta_depth;
    pp.pps_cb_qp_offset = (int8_t)pps->pps_cb_qp_offset;
    pp.pps_cr_qp_offset = (int8_t)pps->pps_cr_qp_offset;
    pp.log2_parallel_merge_level_minus2 = (uint8_t)pps->log2_parallel_merge_level_minus2;

    int irap = hevc_nal_is_irap(first_sh->nal_unit_type);
    int idr = hevc_nal_is_idr(first_sh->nal_unit_type);

    pp.slice_parsing_fields.bits.lists_modification_present_flag = (uint32_t)pps->lists_modification_present_flag;
    pp.slice_parsing_fields.bits.long_term_ref_pics_present_flag = (uint32_t)sps->long_term_ref_pics_present_flag;
    pp.slice_parsing_fields.bits.sps_temporal_mvp_enabled_flag = (uint32_t)sps->sps_temporal_mvp_enabled_flag;
    pp.slice_parsing_fields.bits.cabac_init_present_flag = (uint32_t)pps->cabac_init_present_flag;
    pp.slice_parsing_fields.bits.output_flag_present_flag = (uint32_t)pps->output_flag_present_flag;
    pp.slice_parsing_fields.bits.dependent_slice_segments_enabled_flag =
        (uint32_t)pps->dependent_slice_segments_enabled_flag;
    pp.slice_parsing_fields.bits.pps_slice_chroma_qp_offsets_present_flag =
        (uint32_t)pps->pps_slice_chroma_qp_offsets_present_flag;
    pp.slice_parsing_fields.bits.sample_adaptive_offset_enabled_flag =
        (uint32_t)sps->sample_adaptive_offset_enabled_flag;
    pp.slice_parsing_fields.bits.deblocking_filter_override_enabled_flag =
        (uint32_t)pps->deblocking_filter_override_enabled_flag;
    pp.slice_parsing_fields.bits.pps_disable_deblocking_filter_flag =
        (uint32_t)pps->pps_deblocking_filter_disabled_flag;
    pp.slice_parsing_fields.bits.slice_segment_header_extension_present_flag =
        (uint32_t)pps->slice_segment_header_extension_present_flag;
    pp.slice_parsing_fields.bits.RapPicFlag = irap ? 1u : 0u;
    pp.slice_parsing_fields.bits.IdrPicFlag = idr ? 1u : 0u;
    pp.slice_parsing_fields.bits.IntraPicFlag =
        (first_sh->slice_type == HEVC_SLICE_TYPE_I || irap) ? 1u : 0u;

    pp.log2_max_pic_order_cnt_lsb_minus4 = (uint8_t)(sps->log2_max_pic_order_cnt_lsb - 4);
    pp.num_short_term_ref_pic_sets = (uint8_t)sps->num_short_term_ref_pic_sets;
    pp.num_long_term_ref_pic_sps = (uint8_t)sps->num_long_term_ref_pics_sps;
    pp.num_ref_idx_l0_default_active_minus1 = (uint8_t)pps->num_ref_idx_l0_default_active_minus1;
    pp.num_ref_idx_l1_default_active_minus1 = (uint8_t)pps->num_ref_idx_l1_default_active_minus1;
    pp.pps_beta_offset_div2 = (int8_t)pps->pps_beta_offset_div2;
    pp.pps_tc_offset_div2 = (int8_t)pps->pps_tc_offset_div2;
    pp.num_extra_slice_header_bits = (uint8_t)pps->num_extra_slice_header_bits;

    if (!first_sh->short_term_ref_pic_set_sps_flag) {
        pp.st_rps_bits = first_sh->st_rps_bits ? first_sh->st_rps_bits : first_sh->short_term_ref_pic_set_size;
    }

    VABufferID buf;
    vaCreateBuffer(dec->shared->va_dpy, dec->shared->va_context, VAPictureParameterBufferType,
                   sizeof(pp), 1, &pp, &buf);
    vaRenderPicture(dec->shared->va_dpy, dec->shared->va_context, &buf, 1);

    if (sps->scaling_list_enabled_flag) {
        VAIQMatrixBufferHEVC iq;
        memset(&iq, 16, sizeof(iq));
        memset(iq.ScalingListDC16x16, 16, sizeof(iq.ScalingListDC16x16));
        memset(iq.ScalingListDC32x32, 16, sizeof(iq.ScalingListDC32x32));
        VABufferID iq_buf;
        vaCreateBuffer(dec->shared->va_dpy, dec->shared->va_context, VAIQMatrixBufferType,
                       sizeof(iq), 1, &iq, &iq_buf);
        vaRenderPicture(dec->shared->va_dpy, dec->shared->va_context, &iq_buf, 1);
    }
}

static void submit_slice_hevc(LinuxHevcVideoDecoder *dec,
                              const HevcSliceHeader *sh,
                              const HevcSliceHeader *indep_sh,
                              const HevcNalUnit *nal,
                              int32_t poc,
                              int last_slice) {
    /* Dependent slices inherit prediction syntax from the independent header. */
    const HevcSliceHeader *pred = (sh->dependent_slice_segment_flag && indep_sh) ? indep_sh : sh;

    /* Long-format VA-API HEVC expects start-code-prefixed slice data. */
    static const uint8_t start_code[3] = {0, 0, 1};
    size_t payload_size = sizeof(start_code) + nal->size;
    uint8_t *payload = (uint8_t *)malloc(payload_size);
    if (!payload) {
        set_last_error(dec, "out of memory allocating HEVC slice payload");
        return;
    }
    memcpy(payload, start_code, sizeof(start_code));
    memcpy(payload + sizeof(start_code), nal->data, nal->size);

    VASliceParameterBufferHEVC sp;
    memset(&sp, 0, sizeof(sp));

    /* Buffer layout: [start code][NAL]. Size/offsets are relative to the NAL
     * header per VASliceParameterBufferHEVC; byte_offset skips the parsed
     * slice segment header (RBSP bit count, incl. 2-byte NAL header). */
    sp.slice_data_size = (uint32_t)nal->size;
    sp.slice_data_offset = (uint32_t)sizeof(start_code);
    sp.slice_data_flag = VA_SLICE_DATA_FLAG_ALL;
    sp.slice_data_byte_offset = (uint32_t)((sh->header_bits + 7) / 8);
    sp.slice_segment_address = sh->slice_segment_address;

    hevc_dpb_build_ref_pic_lists(dec->pic_refs, poc, &pred->st_rps, pred->slice_type,
                                 pred->num_ref_idx_l0_active_minus1, pred->num_ref_idx_l1_active_minus1,
                                 sp.RefPicList);

    sp.LongSliceFlags.fields.LastSliceOfPic = last_slice ? 1u : 0u;
    sp.LongSliceFlags.fields.dependent_slice_segment_flag = (uint32_t)sh->dependent_slice_segment_flag;
    sp.LongSliceFlags.fields.slice_type = pred->slice_type & 0x3u;
    sp.LongSliceFlags.fields.color_plane_id = pred->colour_plane_id & 0x3u;
    sp.LongSliceFlags.fields.slice_sao_luma_flag = (uint32_t)pred->slice_sao_luma_flag;
    sp.LongSliceFlags.fields.slice_sao_chroma_flag = (uint32_t)pred->slice_sao_chroma_flag;
    sp.LongSliceFlags.fields.mvd_l1_zero_flag = (uint32_t)pred->mvd_l1_zero_flag;
    sp.LongSliceFlags.fields.cabac_init_flag = (uint32_t)pred->cabac_init_flag;
    sp.LongSliceFlags.fields.slice_temporal_mvp_enabled_flag = (uint32_t)pred->slice_temporal_mvp_enabled_flag;
    sp.LongSliceFlags.fields.slice_deblocking_filter_disabled_flag =
        (uint32_t)pred->slice_deblocking_filter_disabled_flag;
    sp.LongSliceFlags.fields.collocated_from_l0_flag = (uint32_t)pred->collocated_from_l0_flag;
    sp.LongSliceFlags.fields.slice_loop_filter_across_slices_enabled_flag =
        (uint32_t)pred->slice_loop_filter_across_slices_enabled_flag;

    if (pred->slice_temporal_mvp_enabled_flag) {
        sp.collocated_ref_idx = (uint8_t)pred->collocated_ref_idx;
    } else {
        sp.collocated_ref_idx = 0xFF;
    }
    sp.num_ref_idx_l0_active_minus1 = (uint8_t)pred->num_ref_idx_l0_active_minus1;
    sp.num_ref_idx_l1_active_minus1 = (uint8_t)pred->num_ref_idx_l1_active_minus1;
    sp.slice_qp_delta = (int8_t)pred->slice_qp_delta;
    sp.slice_cb_qp_offset = (int8_t)pred->slice_cb_qp_offset;
    sp.slice_cr_qp_offset = (int8_t)pred->slice_cr_qp_offset;
    sp.slice_beta_offset_div2 = (int8_t)pred->slice_beta_offset_div2;
    sp.slice_tc_offset_div2 = (int8_t)pred->slice_tc_offset_div2;
    sp.five_minus_max_num_merge_cand = (uint8_t)pred->five_minus_max_num_merge_cand;
    sp.num_entry_point_offsets = (uint16_t)pred->num_entry_point_offsets;

    if (pred->has_pred_weight_table) {
        sp.luma_log2_weight_denom = (uint8_t)pred->luma_log2_weight_denom;
        sp.delta_chroma_log2_weight_denom = (int8_t)pred->delta_chroma_log2_weight_denom;
        memcpy(sp.delta_luma_weight_l0, pred->delta_luma_weight_l0, sizeof(sp.delta_luma_weight_l0));
        memcpy(sp.luma_offset_l0, pred->luma_offset_l0, sizeof(sp.luma_offset_l0));
        memcpy(sp.delta_chroma_weight_l0, pred->delta_chroma_weight_l0, sizeof(sp.delta_chroma_weight_l0));
        memcpy(sp.ChromaOffsetL0, pred->chroma_offset_l0, sizeof(sp.ChromaOffsetL0));
        if (pred->slice_type == HEVC_SLICE_TYPE_B) {
            memcpy(sp.delta_luma_weight_l1, pred->delta_luma_weight_l1, sizeof(sp.delta_luma_weight_l1));
            memcpy(sp.luma_offset_l1, pred->luma_offset_l1, sizeof(sp.luma_offset_l1));
            memcpy(sp.delta_chroma_weight_l1, pred->delta_chroma_weight_l1, sizeof(sp.delta_chroma_weight_l1));
            memcpy(sp.ChromaOffsetL1, pred->chroma_offset_l1, sizeof(sp.ChromaOffsetL1));
        }
    }

    VABufferID buf;
    vaCreateBuffer(dec->shared->va_dpy, dec->shared->va_context, VASliceParameterBufferType,
                   sizeof(sp), 1, &sp, &buf);
    vaRenderPicture(dec->shared->va_dpy, dec->shared->va_context, &buf, 1);

    VABufferID data_buf;
    vaCreateBuffer(dec->shared->va_dpy, dec->shared->va_context, VASliceDataBufferType,
                   (unsigned int)payload_size, 1, payload, &data_buf);
    vaRenderPicture(dec->shared->va_dpy, dec->shared->va_context, &data_buf, 1);

    free(payload);
}

MKFF_Result linux_hevc_video_decoder_create(MKFF_PlatformContext *pctx,
                                            const MKFF_VideoDecoderDesc *desc,
                                            void **out_decoder) {
    if (desc->codec != MKFF_VIDEO_CODEC_HEVC) {
        return MKFF_RESULT_ERROR_NOT_SUPPORTED;
    }

    if (desc->struct_size >= offsetof(MKFF_VideoDecoderDesc, backend) + sizeof(MKFF_VideoBackend)
        && desc->backend == MKFF_VIDEO_BACKEND_SOFTWARE_ONLY) {
        return MKFF_RESULT_ERROR_NOT_SUPPORTED;
    }

    LinuxHevcVideoDecoder *dec = (LinuxHevcVideoDecoder *)calloc(1, sizeof(LinuxHevcVideoDecoder));
    if (!dec) {
        return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
    }

    dec->common.api = mkff_linux_platform_api();
    dec->codec = MKFF_VIDEO_CODEC_HEVC;
    dec->log_callback = pctx->log_callback;
    dec->log_user_data = pctx->log_user_data;
    dec->log_min_level = pctx->log_min_level;
    dec->requested_max_surfaces = desc->max_surfaces;
    dec->output_format = MKFF_PIXEL_FORMAT_NV12;
    dec->bit_depth = 8;
    dec->chroma_format_idc = 1;
    hevc_dpb_init(&dec->dpb);

    *out_decoder = dec;
    return MKFF_RESULT_OK;
}

void linux_hevc_video_decoder_destroy(void *decoder_v) {
    LinuxHevcVideoDecoder *dec = (LinuxHevcVideoDecoder *)decoder_v;
    if (!dec) return;

    hevc_dpb_reset(&dec->dpb);
    for (uint32_t i = 0; i < dec->ready_count; i++) {
        linux_video_frame_release((MKFF_VideoFrame *)dec->ready_queue[i]);
    }
    if (dec->shared) {
        decoder_shared_unref(dec->shared);
    }
    free(dec);
}

MKFF_Result linux_hevc_video_decoder_submit(void *decoder_v,
                                            const uint8_t *annex_b_data,
                                            size_t annex_b_size,
                                            int64_t pts,
                                            int64_t dts) {
    LinuxHevcVideoDecoder *dec = (LinuxHevcVideoDecoder *)decoder_v;

    if (annex_b_size == 0) {
        return MKFF_RESULT_OK;
    }

    HevcNalUnit nals[HEVC_MAX_NAL_UNITS_PER_AU];
    size_t nal_count = hevc_split_annex_b(annex_b_data, annex_b_size, nals, HEVC_MAX_NAL_UNITS_PER_AU);
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
    const HevcSPS *pic_sps = NULL;
    HevcSliceHeader pic_first_sh;
    int32_t pic_poc = 0;
    MKFF_Result result = MKFF_RESULT_OK;

    for (size_t i = 0; i < nal_count && result == MKFF_RESULT_OK; i++) {
        const HevcNalUnit *nal = &nals[i];

        if (nal->nal_unit_type == HEVC_NAL_VPS_NUT) {
            HevcVPS tmp;
            int rc = hevc_parse_vps(nal->data, nal->size, rbsp_scratch, &tmp);
            if (rc == 0 && tmp.vps_video_parameter_set_id < HEVC_MAX_VPS_COUNT) {
                dec->vps_table[tmp.vps_video_parameter_set_id] = tmp;
            }
            continue;
        }

        if (nal->nal_unit_type == HEVC_NAL_SPS_NUT) {
            HevcSPS tmp;
            int rc = hevc_parse_sps(nal->data, nal->size, rbsp_scratch, &tmp);
            if (rc == 0 && tmp.sps_seq_parameter_set_id < HEVC_MAX_SPS_COUNT) {
                dec->sps_table[tmp.sps_seq_parameter_set_id] = tmp;
            } else if (tmp.sps_seq_parameter_set_id < HEVC_MAX_SPS_COUNT) {
                DLOG(dec, MKFF_LOG_LEVEL_WARN,
                     "rejecting HEVC SPS id=%u (rc=%d): outside supported Main/Main10 4:2:0 subset",
                     tmp.sps_seq_parameter_set_id, rc);
                dec->sps_table[tmp.sps_seq_parameter_set_id].valid = 0;
            }
            continue;
        }

        if (nal->nal_unit_type == HEVC_NAL_PPS_NUT) {
            HevcPPS tmp;
            int rc = hevc_parse_pps(nal->data, nal->size, rbsp_scratch, dec->sps_table, &tmp);
            if (rc == 0 && tmp.pps_pic_parameter_set_id < HEVC_MAX_PPS_COUNT) {
                if (tmp.tiles_enabled_flag || tmp.entropy_coding_sync_enabled_flag) {
                    DLOG(dec, MKFF_LOG_LEVEL_WARN,
                         "rejecting HEVC PPS id=%u: tiles/WPP not supported in this milestone",
                         tmp.pps_pic_parameter_set_id);
                    dec->pps_table[tmp.pps_pic_parameter_set_id].valid = 0;
                } else {
                    dec->pps_table[tmp.pps_pic_parameter_set_id] = tmp;
                }
            } else if (tmp.pps_pic_parameter_set_id < HEVC_MAX_PPS_COUNT) {
                DLOG(dec, MKFF_LOG_LEVEL_WARN, "rejecting HEVC PPS id=%u (rc=%d)",
                     tmp.pps_pic_parameter_set_id, rc);
                dec->pps_table[tmp.pps_pic_parameter_set_id].valid = 0;
            }
            continue;
        }

        if (!hevc_nal_is_vcl(nal->nal_unit_type)) {
            continue;
        }

        HevcSliceHeader sh;
        int rc = hevc_parse_slice_header(nal, rbsp_scratch, dec->sps_table, dec->pps_table, &sh);
        if (rc != 0) {
            DLOG(dec, MKFF_LOG_LEVEL_WARN, "dropping HEVC slice NAL (rc=%d)", rc);
            continue;
        }

        const HevcPPS *pps = &dec->pps_table[sh.slice_pic_parameter_set_id];
        const HevcSPS *sps = &dec->sps_table[pps->pps_seq_parameter_set_id];

        if (!picture_open) {
            if (!sh.first_slice_segment_in_pic_flag) {
                DLOG(dec, MKFF_LOG_LEVEL_DEBUG, "%s",
                     "dropping dependent/continuation slice before picture open");
                continue;
            }

            if (!dec->initialized && !hevc_nal_is_irap(sh.nal_unit_type)) {
                DLOG(dec, MKFF_LOG_LEVEL_DEBUG, "%s", "dropping access unit before the first IRAP");
                break;
            }

            MKFF_Result init_result = ensure_va_initialized_hevc(dec, sps);
            if (init_result != MKFF_RESULT_OK) {
                result = init_result;
                break;
            }

            dec->display_width = sps->pic_width_in_luma_samples;
            dec->display_height = sps->pic_height_in_luma_samples;
            if (sps->conformance_window_flag) {
                dec->display_width -= (sps->conf_win_left_offset + sps->conf_win_right_offset) * 2;
                dec->display_height -= (sps->conf_win_top_offset + sps->conf_win_bottom_offset) * 2;
            }
            if (dec->shared) {
                dec->shared->display_width = dec->display_width;
                dec->shared->display_height = dec->display_height;
            }

            pic_poc = hevc_compute_poc(sps, &sh, &dec->poc_state);

            if (hevc_nal_is_idr(sh.nal_unit_type)
                || (hevc_nal_is_irap(sh.nal_unit_type) && sh.no_output_of_prior_pics_flag)) {
                hevc_dpb_clear_references(&dec->dpb, 1);
            }

            if (decoder_shared_pool_checkout(dec->shared, &pic_pool_index, &pic_surface) != 0) {
                set_last_error(dec, "surface pool exhausted: caller must release frames before submitting more");
                result = MKFF_RESULT_ERROR_POOL_EXHAUSTED;
                break;
            }

            pic_frame = linux_video_frame_create(dec->shared, pic_pool_index, pic_surface,
                                                 dec->display_width, dec->display_height,
                                                 pts, dts, hevc_nal_is_irap(sh.nal_unit_type),
                                                 dec->output_format);
            if (!pic_frame) {
                decoder_shared_pool_release(dec->shared, pic_pool_index);
                result = MKFF_RESULT_ERROR_OUT_OF_MEMORY;
                break;
            }

            pic_first_sh = sh;
            pic_sps = sps;
            picture_open = 1;

            vaBeginPicture(dec->shared->va_dpy, dec->shared->va_context, pic_surface);
            submit_picture_params_hevc(dec, sps, pps, &sh, pic_surface, pic_poc);
        }

        int last_slice = !au_has_later_vcl(nals, nal_count, i);
        submit_slice_hevc(dec, &sh, &pic_first_sh, nal, pic_poc, last_slice);
    }

    if (picture_open && result == MKFF_RESULT_OK) {
        VAStatus end_status = vaEndPicture(dec->shared->va_dpy, dec->shared->va_context);
        if (end_status != VA_STATUS_SUCCESS) {
            DLOG(dec, MKFF_LOG_LEVEL_ERROR, "vaEndPicture failed: %s", vaErrorStr(end_status));
        }
        VAStatus sync_status = vaSyncSurface(dec->shared->va_dpy, pic_surface);
        if (sync_status != VA_STATUS_SUCCESS) {
            DLOG(dec, MKFF_LOG_LEVEL_WARN, "vaSyncSurface reported an issue: %s", vaErrorStr(sync_status));
        }

        hevc_dpb_update_after_decode(&dec->dpb, pic_frame, pic_poc, &pic_first_sh.st_rps,
                                     hevc_nal_is_ref(pic_first_sh.nal_unit_type));
        if (pic_first_sh.pic_output_flag) {
            hevc_dpb_push_output(&dec->dpb, pic_frame, pic_poc);
        }
        linux_video_frame_release((MKFF_VideoFrame *)pic_frame);

        uint32_t max_reorder = pic_sps->sps_max_num_reorder_pics[pic_sps->sps_max_sub_layers_minus1];
        if (max_reorder > 16) max_reorder = 16;
        push_ready(dec, hevc_dpb_bump_if_needed(&dec->dpb, max_reorder));
    } else if (picture_open) {
        vaEndPicture(dec->shared->va_dpy, dec->shared->va_context);
        linux_video_frame_release((MKFF_VideoFrame *)pic_frame);
    }

    free(rbsp_scratch);
    return result;
}

MKFF_Result linux_hevc_video_decoder_receive(void *decoder_v, MKFF_VideoFrame **out_frame) {
    LinuxHevcVideoDecoder *dec = (LinuxHevcVideoDecoder *)decoder_v;

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

MKFF_Result linux_hevc_video_decoder_flush(void *decoder_v) {
    LinuxHevcVideoDecoder *dec = (LinuxHevcVideoDecoder *)decoder_v;

    LinuxVideoFrame *f;
    while ((f = hevc_dpb_bump_one(&dec->dpb)) != NULL) {
        push_ready(dec, f);
    }
    dec->flushed = 1;
    return MKFF_RESULT_OK;
}

MKFF_Result linux_hevc_video_decoder_get_info(const void *decoder_v, MKFF_VideoDecoderInfo *out_info) {
    const LinuxHevcVideoDecoder *dec = (const LinuxHevcVideoDecoder *)decoder_v;

    uint32_t requested_size = out_info->struct_size;
    memset(out_info, 0, sizeof(*out_info));
    MKFF_INIT_STRUCT_HEADER(out_info);
    out_info->struct_size = requested_size ? requested_size : (uint32_t)sizeof(*out_info);

    out_info->width = dec->display_width;
    out_info->height = dec->display_height;
    out_info->profile = dec->mkff_profile;
    out_info->entrypoint = dec->initialized ? MKFF_VIDEO_ENTRYPOINT_VLD : MKFF_VIDEO_ENTRYPOINT_UNKNOWN;
    out_info->output_format = dec->output_format ? dec->output_format : MKFF_PIXEL_FORMAT_NV12;

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
        out_info->bit_depth = dec->bit_depth ? dec->bit_depth : (dec->initialized ? 8u : 0u);
        out_info->chroma_format_idc = dec->initialized ? dec->chroma_format_idc : 0u;
        out_info->hardware = dec->initialized ? 1u : 0u;
    }

    return MKFF_RESULT_OK;
}
