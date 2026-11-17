#ifndef MKFF_HEVC_VPS_SPS_PPS_H
#define MKFF_HEVC_VPS_SPS_PPS_H

#include <stddef.h>
#include <stdint.h>

#include "hevc_bitstream.h"

#define HEVC_MAX_VPS_COUNT 16
#define HEVC_MAX_SPS_COUNT 16
#define HEVC_MAX_PPS_COUNT 64
#define HEVC_MAX_SUB_LAYERS 7
#define HEVC_MAX_SHORT_TERM_RPS_COUNT 64
#define HEVC_MAX_LONG_TERM_REFS 32
#define HEVC_MAX_DPB_SIZE 16

typedef struct HevcProfileTierLevel {
    uint8_t general_profile_space;
    uint8_t general_tier_flag;
    uint8_t general_profile_idc;
    uint32_t general_profile_compatibility_flags;
    uint8_t general_progressive_source_flag;
    uint8_t general_interlaced_source_flag;
    uint8_t general_non_packed_constraint_flag;
    uint8_t general_frame_only_constraint_flag;
    uint8_t general_level_idc;
} HevcProfileTierLevel;

typedef struct HevcShortTermRps {
    int      inter_ref_pic_set_prediction_flag;
    uint32_t delta_idx_minus1;
    int      delta_rps_sign;
    uint32_t abs_delta_rps_minus1;
    uint32_t num_negative_pics;
    uint32_t num_positive_pics;
    int32_t  delta_poc_s0[HEVC_MAX_DPB_SIZE];
    int32_t  delta_poc_s1[HEVC_MAX_DPB_SIZE];
    uint8_t  used_by_curr_pic_s0[HEVC_MAX_DPB_SIZE];
    uint8_t  used_by_curr_pic_s1[HEVC_MAX_DPB_SIZE];
    uint32_t num_delta_pocs; /* num_negative + num_positive after prediction expand */
} HevcShortTermRps;

typedef struct HevcVPS {
    int      valid;
    uint32_t vps_video_parameter_set_id;
    uint32_t vps_max_layers_minus1;
    uint32_t vps_max_sub_layers_minus1;
    int      vps_temporal_id_nesting_flag;
    HevcProfileTierLevel ptl;
} HevcVPS;

typedef struct HevcSPS {
    int      valid;
    uint32_t sps_video_parameter_set_id;
    uint32_t sps_max_sub_layers_minus1;
    int      sps_temporal_id_nesting_flag;
    HevcProfileTierLevel ptl;

    uint32_t sps_seq_parameter_set_id;
    uint32_t chroma_format_idc; /* must be 1 (4:2:0) */
    int      separate_colour_plane_flag;

    uint32_t pic_width_in_luma_samples;
    uint32_t pic_height_in_luma_samples;

    int      conformance_window_flag;
    uint32_t conf_win_left_offset;
    uint32_t conf_win_right_offset;
    uint32_t conf_win_top_offset;
    uint32_t conf_win_bottom_offset;

    uint32_t bit_depth_luma_minus8;   /* 0 or 2 */
    uint32_t bit_depth_chroma_minus8; /* 0 or 2 */

    uint32_t log2_max_pic_order_cnt_lsb; /* = minus4 + 4 */

    uint32_t sps_max_dec_pic_buffering_minus1[HEVC_MAX_SUB_LAYERS];
    uint32_t sps_max_num_reorder_pics[HEVC_MAX_SUB_LAYERS];
    uint32_t sps_max_latency_increase_plus1[HEVC_MAX_SUB_LAYERS];

    uint32_t log2_min_luma_coding_block_size_minus3;
    uint32_t log2_diff_max_min_luma_coding_block_size;
    uint32_t log2_min_luma_transform_block_size_minus2;
    uint32_t log2_diff_max_min_luma_transform_block_size;
    uint32_t max_transform_hierarchy_depth_inter;
    uint32_t max_transform_hierarchy_depth_intra;

    int      scaling_list_enabled_flag;
    int      amp_enabled_flag;
    int      sample_adaptive_offset_enabled_flag;
    int      pcm_enabled_flag;

    uint32_t num_short_term_ref_pic_sets;
    HevcShortTermRps short_term_ref_pic_set[HEVC_MAX_SHORT_TERM_RPS_COUNT];

    int      long_term_ref_pics_present_flag;
    uint32_t num_long_term_ref_pics_sps;
    uint32_t lt_ref_pic_poc_lsb_sps[HEVC_MAX_LONG_TERM_REFS];
    uint8_t  used_by_curr_pic_lt_sps_flag[HEVC_MAX_LONG_TERM_REFS];

    int      sps_temporal_mvp_enabled_flag;
    int      strong_intra_smoothing_enabled_flag;

    /* Derived */
    uint32_t bit_depth; /* 8 or 10 */
    uint32_t ctb_log2_size_y;
    uint32_t pic_width_in_ctbs_y;
    uint32_t pic_height_in_ctbs_y;
} HevcSPS;

typedef struct HevcPPS {
    int      valid;
    uint32_t pps_pic_parameter_set_id;
    uint32_t pps_seq_parameter_set_id;

    int      dependent_slice_segments_enabled_flag;
    int      output_flag_present_flag;
    uint32_t num_extra_slice_header_bits;
    int      sign_data_hiding_enabled_flag;
    int      cabac_init_present_flag;

    uint32_t num_ref_idx_l0_default_active_minus1;
    uint32_t num_ref_idx_l1_default_active_minus1;
    int32_t  init_qp_minus26;
    int      constrained_intra_pred_flag;
    int      transform_skip_enabled_flag;
    int      cu_qp_delta_enabled_flag;
    uint32_t diff_cu_qp_delta_depth;

    int32_t  pps_cb_qp_offset;
    int32_t  pps_cr_qp_offset;
    int      pps_slice_chroma_qp_offsets_present_flag;
    int      weighted_pred_flag;
    int      weighted_bipred_flag;
    int      transquant_bypass_enabled_flag;
    int      tiles_enabled_flag;
    int      entropy_coding_sync_enabled_flag;

    int      pps_loop_filter_across_slices_enabled_flag;
    int      deblocking_filter_control_present_flag;
    int      deblocking_filter_override_enabled_flag;
    int      pps_deblocking_filter_disabled_flag;
    int32_t  pps_beta_offset_div2;
    int32_t  pps_tc_offset_div2;

    int      lists_modification_present_flag;
    uint32_t log2_parallel_merge_level_minus2;
    int      slice_segment_header_extension_present_flag;
} HevcPPS;

/* Returns 0 on success, -1 malformed, -2 unsupported (chroma/bit depth/profile). */
int hevc_parse_vps(const uint8_t *nal_data, size_t nal_size, uint8_t *rbsp_scratch, HevcVPS *out_vps);
int hevc_parse_sps(const uint8_t *nal_data, size_t nal_size, uint8_t *rbsp_scratch, HevcSPS *out_sps);
int hevc_parse_pps(const uint8_t *nal_data, size_t nal_size, uint8_t *rbsp_scratch,
                   const HevcSPS *sps_table, HevcPPS *out_pps);

/* Parse short_term_ref_pic_set into table[st_rps_idx]. from_slice=1 for
 * slice-embedded sets (idx == num_st_rps). */
int hevc_parse_short_term_ref_pic_set(HevcBitReader *br, HevcShortTermRps *table,
                                      uint32_t st_rps_idx, uint32_t num_st_rps, int from_slice);

int hevc_expand_short_term_rps(HevcShortTermRps *rps_table, uint32_t st_rps_idx,
                               uint32_t num_short_term_ref_pic_sets);

#endif /* MKFF_HEVC_VPS_SPS_PPS_H */
