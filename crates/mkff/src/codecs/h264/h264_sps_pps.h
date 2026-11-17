#ifndef MKFF_LINUX_H264_SPS_PPS_H
#define MKFF_LINUX_H264_SPS_PPS_H

#include <stdint.h>
#include "h264_bitstream.h"

#define H264_MAX_SPS_COUNT 32
#define H264_MAX_PPS_COUNT 256
#define H264_MAX_POC_CYCLE 256

typedef struct H264SPS {
    int      valid;
    uint32_t profile_idc;
    uint32_t level_idc;
    uint32_t seq_parameter_set_id;

    uint32_t chroma_format_idc; /* must be 1 (4:2:0) for this milestone */
    uint32_t bit_depth_luma_minus8;   /* must be 0 (8-bit) */
    uint32_t bit_depth_chroma_minus8; /* must be 0 (8-bit) */

    uint32_t log2_max_frame_num;              /* = log2_max_frame_num_minus4 + 4 */
    uint32_t pic_order_cnt_type;
    uint32_t log2_max_pic_order_cnt_lsb;      /* type 0 */
    int      delta_pic_order_always_zero_flag; /* type 1 */
    int32_t  offset_for_non_ref_pic;           /* type 1 */
    int32_t  offset_for_top_to_bottom_field;   /* type 1 */
    uint32_t num_ref_frames_in_pic_order_cnt_cycle; /* type 1 */
    int32_t  offset_for_ref_frame[H264_MAX_POC_CYCLE];

    uint32_t max_num_ref_frames;
    int      gaps_in_frame_num_value_allowed_flag;

    uint32_t pic_width_in_mbs;
    uint32_t pic_height_in_map_units;
    int      frame_mbs_only_flag; /* must be 1 (progressive) */
    int      mb_adaptive_frame_field_flag;
    int      direct_8x8_inference_flag;

    int      frame_cropping_flag;
    uint32_t crop_left, crop_right, crop_top, crop_bottom;

    int custom_scaling_matrices_present; /* seq_scaling_matrix_present_flag with non-trivial lists */
} H264SPS;

typedef struct H264PPS {
    int      valid;
    uint32_t pic_parameter_set_id;
    uint32_t seq_parameter_set_id;

    int entropy_coding_mode_flag;
    int bottom_field_pic_order_in_frame_present_flag;

    uint32_t num_slice_groups_minus1; /* must be 0: FMO unsupported */

    uint32_t num_ref_idx_l0_default_active_minus1;
    uint32_t num_ref_idx_l1_default_active_minus1;

    int weighted_pred_flag;
    uint32_t weighted_bipred_idc;

    int32_t pic_init_qp_minus26;
    int32_t pic_init_qs_minus26;
    int32_t chroma_qp_index_offset;

    int deblocking_filter_control_present_flag;
    int constrained_intra_pred_flag;
    int redundant_pic_cnt_present_flag;

    int transform_8x8_mode_flag;
    int custom_scaling_matrices_present;
    int32_t second_chroma_qp_index_offset;
} H264PPS;

/* Returns 0 on success. Negative values indicate either a malformed
 * bitstream (-1) or a syntax element outside this milestone's supported
 * subset (-2), e.g. non-4:2:0 chroma, >8-bit depth, interlaced content,
 * FMO, or custom scaling matrices. */
int h264_parse_sps(const uint8_t *nal_data, size_t nal_size, uint8_t *rbsp_scratch, H264SPS *out_sps);
int h264_parse_pps(const uint8_t *nal_data, size_t nal_size, uint8_t *rbsp_scratch, const H264SPS *sps_table, H264PPS *out_pps);

#endif /* MKFF_LINUX_H264_SPS_PPS_H */
