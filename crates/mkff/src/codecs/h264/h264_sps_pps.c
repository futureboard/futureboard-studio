#include "h264_sps_pps.h"

#include <string.h>

static void skip_scaling_list(H264BitReader *br, int size) {
    int last_scale = 8;
    int next_scale = 8;
    for (int j = 0; j < size; j++) {
        if (next_scale != 0) {
            int32_t delta_scale = h264_br_se(br);
            next_scale = ((last_scale + delta_scale) % 256 + 256) % 256;
        }
        last_scale = (next_scale == 0) ? last_scale : next_scale;
    }
}

static int profile_has_chroma_extension(uint32_t profile_idc) {
    switch (profile_idc) {
        case 100: case 110: case 122: case 244: case 44:
        case 83:  case 86:  case 118: case 128: case 138:
        case 139: case 134: case 135:
            return 1;
        default:
            return 0;
    }
}

int h264_parse_sps(const uint8_t *nal_data, size_t nal_size, uint8_t *rbsp_scratch, H264SPS *out_sps) {
    memset(out_sps, 0, sizeof(*out_sps));

    H264BitReader br;
    h264_br_init(&br, nal_data, nal_size, rbsp_scratch);
    h264_br_skip_bits(&br, 8); /* NAL header byte */

    int unsupported = 0;

    out_sps->profile_idc = h264_br_u(&br, 8);
    h264_br_skip_bits(&br, 8); /* constraint_set flags + reserved_zero_2bits */
    out_sps->level_idc = h264_br_u(&br, 8);
    out_sps->seq_parameter_set_id = h264_br_ue(&br);

    out_sps->chroma_format_idc = 1; /* inferred default */
    if (profile_has_chroma_extension(out_sps->profile_idc)) {
        out_sps->chroma_format_idc = h264_br_ue(&br);
        if (out_sps->chroma_format_idc == 3) {
            h264_br_skip_bits(&br, 1); /* separate_colour_plane_flag */
        }
        out_sps->bit_depth_luma_minus8 = h264_br_ue(&br);
        out_sps->bit_depth_chroma_minus8 = h264_br_ue(&br);
        h264_br_skip_bits(&br, 1); /* qpprime_y_zero_transform_bypass_flag */

        int seq_scaling_matrix_present_flag = h264_br_flag(&br);
        if (seq_scaling_matrix_present_flag) {
            int count = (out_sps->chroma_format_idc != 3) ? 8 : 12;
            for (int i = 0; i < count; i++) {
                int present = h264_br_flag(&br);
                if (present) {
                    out_sps->custom_scaling_matrices_present = 1;
                    skip_scaling_list(&br, (i < 6) ? 16 : 64);
                }
            }
        }
    }

    if (out_sps->chroma_format_idc != 1) unsupported = 1; /* only 4:2:0 in this milestone */
    if (out_sps->bit_depth_luma_minus8 != 0 || out_sps->bit_depth_chroma_minus8 != 0) unsupported = 1; /* only 8-bit */

    uint32_t log2_max_frame_num_minus4 = h264_br_ue(&br);
    out_sps->log2_max_frame_num = log2_max_frame_num_minus4 + 4;

    out_sps->pic_order_cnt_type = h264_br_ue(&br);
    if (out_sps->pic_order_cnt_type == 0) {
        uint32_t log2_max_pic_order_cnt_lsb_minus4 = h264_br_ue(&br);
        out_sps->log2_max_pic_order_cnt_lsb = log2_max_pic_order_cnt_lsb_minus4 + 4;
    } else if (out_sps->pic_order_cnt_type == 1) {
        out_sps->delta_pic_order_always_zero_flag = h264_br_flag(&br);
        out_sps->offset_for_non_ref_pic = h264_br_se(&br);
        out_sps->offset_for_top_to_bottom_field = h264_br_se(&br);
        out_sps->num_ref_frames_in_pic_order_cnt_cycle = h264_br_ue(&br);
        if (out_sps->num_ref_frames_in_pic_order_cnt_cycle > H264_MAX_POC_CYCLE) {
            br.error = 1;
        } else {
            for (uint32_t i = 0; i < out_sps->num_ref_frames_in_pic_order_cnt_cycle; i++) {
                out_sps->offset_for_ref_frame[i] = h264_br_se(&br);
            }
        }
    } else if (out_sps->pic_order_cnt_type != 2) {
        unsupported = 1;
    }

    out_sps->max_num_ref_frames = h264_br_ue(&br);
    out_sps->gaps_in_frame_num_value_allowed_flag = h264_br_flag(&br);
    out_sps->pic_width_in_mbs = h264_br_ue(&br) + 1;
    out_sps->pic_height_in_map_units = h264_br_ue(&br) + 1;
    out_sps->frame_mbs_only_flag = h264_br_flag(&br);
    if (!out_sps->frame_mbs_only_flag) {
        out_sps->mb_adaptive_frame_field_flag = h264_br_flag(&br);
        unsupported = 1; /* interlaced: out of scope for this milestone */
    }
    out_sps->direct_8x8_inference_flag = h264_br_flag(&br);

    out_sps->frame_cropping_flag = h264_br_flag(&br);
    if (out_sps->frame_cropping_flag) {
        out_sps->crop_left = h264_br_ue(&br);
        out_sps->crop_right = h264_br_ue(&br);
        out_sps->crop_top = h264_br_ue(&br);
        out_sps->crop_bottom = h264_br_ue(&br);
    }
    /* vui_parameters(), if present, is not needed to drive VA-API decode
     * and nothing else in this NAL depends on further bit alignment, so
     * parsing stops here. */

    if (out_sps->custom_scaling_matrices_present) {
        unsupported = 1;
    }

    if (br.error) {
        return -1;
    }
    out_sps->valid = 1;
    return unsupported ? -2 : 0;
}

int h264_parse_pps(const uint8_t *nal_data, size_t nal_size, uint8_t *rbsp_scratch, const H264SPS *sps_table, H264PPS *out_pps) {
    memset(out_pps, 0, sizeof(*out_pps));

    H264BitReader br;
    h264_br_init(&br, nal_data, nal_size, rbsp_scratch);
    h264_br_skip_bits(&br, 8); /* NAL header byte */

    int unsupported = 0;

    out_pps->pic_parameter_set_id = h264_br_ue(&br);
    out_pps->seq_parameter_set_id = h264_br_ue(&br);
    out_pps->entropy_coding_mode_flag = h264_br_flag(&br);
    out_pps->bottom_field_pic_order_in_frame_present_flag = h264_br_flag(&br);
    out_pps->num_slice_groups_minus1 = h264_br_ue(&br);

    if (out_pps->num_slice_groups_minus1 > 0) {
        /* FMO (multiple slice groups) is out of scope for this milestone;
         * correctly skipping slice_group_map_type-dependent syntax isn't
         * needed since we already know we'll reject this PPS. */
        out_pps->valid = br.error ? 0 : 1;
        return br.error ? -1 : -2;
    }

    out_pps->num_ref_idx_l0_default_active_minus1 = h264_br_ue(&br);
    out_pps->num_ref_idx_l1_default_active_minus1 = h264_br_ue(&br);
    out_pps->weighted_pred_flag = h264_br_flag(&br);
    out_pps->weighted_bipred_idc = h264_br_u(&br, 2);
    out_pps->pic_init_qp_minus26 = h264_br_se(&br);
    out_pps->pic_init_qs_minus26 = h264_br_se(&br);
    out_pps->chroma_qp_index_offset = h264_br_se(&br);
    out_pps->deblocking_filter_control_present_flag = h264_br_flag(&br);
    out_pps->constrained_intra_pred_flag = h264_br_flag(&br);
    out_pps->redundant_pic_cnt_present_flag = h264_br_flag(&br);

    out_pps->second_chroma_qp_index_offset = out_pps->chroma_qp_index_offset;

    if (h264_br_more_rbsp_data(&br)) {
        out_pps->transform_8x8_mode_flag = h264_br_flag(&br);
        int pic_scaling_matrix_present_flag = h264_br_flag(&br);
        if (pic_scaling_matrix_present_flag) {
            const H264SPS *sps = (out_pps->seq_parameter_set_id < H264_MAX_SPS_COUNT) ? &sps_table[out_pps->seq_parameter_set_id] : NULL;
            uint32_t chroma_format_idc = (sps && sps->valid) ? sps->chroma_format_idc : 1;
            int count = 6 + ((chroma_format_idc != 3) ? 2 : 6) * (out_pps->transform_8x8_mode_flag ? 1 : 0);
            for (int i = 0; i < count; i++) {
                int present = h264_br_flag(&br);
                if (present) {
                    out_pps->custom_scaling_matrices_present = 1;
                    skip_scaling_list(&br, (i < 6) ? 16 : 64);
                }
            }
        }
        out_pps->second_chroma_qp_index_offset = h264_br_se(&br);
    }

    if (out_pps->custom_scaling_matrices_present) {
        unsupported = 1;
    }

    if (br.error) {
        return -1;
    }
    out_pps->valid = 1;
    return unsupported ? -2 : 0;
}
