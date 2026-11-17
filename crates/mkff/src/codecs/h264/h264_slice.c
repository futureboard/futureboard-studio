#include "h264_slice.h"

#include <string.h>

static void parse_ref_pic_list_modification(H264BitReader *br, H264RefPicListMod *out_ops, int *out_count) {
    int count = 0;
    for (;;) {
        uint32_t idc = h264_br_ue(br);
        if (br->error || idc == 3 || count >= 32) {
            break;
        }
        uint32_t value = h264_br_ue(br); /* abs_diff_pic_num_minus1 (0/1) or long_term_pic_num (2) */
        out_ops[count].idc = idc;
        out_ops[count].value = value;
        count++;
    }
    *out_count = count;
}

static void parse_pred_weight_table(H264BitReader *br, H264SliceHeader *sh) {
    sh->has_pred_weight_table = 1;
    sh->luma_log2_weight_denom = h264_br_ue(br);
    sh->chroma_log2_weight_denom = h264_br_ue(br); /* chroma_array_type always 1 in this milestone's supported scope */

    int16_t default_luma_weight = (int16_t)(1 << sh->luma_log2_weight_denom);
    int16_t default_chroma_weight = (int16_t)(1 << sh->chroma_log2_weight_denom);

    uint32_t n0 = sh->num_ref_idx_l0_active_minus1 + 1;
    if (n0 > 32) n0 = 32;
    for (uint32_t i = 0; i < n0; i++) {
        sh->luma_weight_l0[i] = default_luma_weight;
        sh->luma_offset_l0[i] = 0;
        if (h264_br_flag(br)) {
            sh->luma_weight_l0[i] = (int16_t)h264_br_se(br);
            sh->luma_offset_l0[i] = (int16_t)h264_br_se(br);
        }
        sh->chroma_weight_l0[i][0] = sh->chroma_weight_l0[i][1] = default_chroma_weight;
        sh->chroma_offset_l0[i][0] = sh->chroma_offset_l0[i][1] = 0;
        if (h264_br_flag(br)) {
            for (int j = 0; j < 2; j++) {
                sh->chroma_weight_l0[i][j] = (int16_t)h264_br_se(br);
                sh->chroma_offset_l0[i][j] = (int16_t)h264_br_se(br);
            }
        }
    }

    if (sh->slice_type == H264_SLICE_TYPE_B) {
        uint32_t n1 = sh->num_ref_idx_l1_active_minus1 + 1;
        if (n1 > 32) n1 = 32;
        for (uint32_t i = 0; i < n1; i++) {
            sh->luma_weight_l1[i] = default_luma_weight;
            sh->luma_offset_l1[i] = 0;
            if (h264_br_flag(br)) {
                sh->luma_weight_l1[i] = (int16_t)h264_br_se(br);
                sh->luma_offset_l1[i] = (int16_t)h264_br_se(br);
            }
            sh->chroma_weight_l1[i][0] = sh->chroma_weight_l1[i][1] = default_chroma_weight;
            sh->chroma_offset_l1[i][0] = sh->chroma_offset_l1[i][1] = 0;
            if (h264_br_flag(br)) {
                for (int j = 0; j < 2; j++) {
                    sh->chroma_weight_l1[i][j] = (int16_t)h264_br_se(br);
                    sh->chroma_offset_l1[i][j] = (int16_t)h264_br_se(br);
                }
            }
        }
    }
}

static void parse_dec_ref_pic_marking(H264BitReader *br, H264SliceHeader *sh) {
    if (sh->is_idr) {
        sh->no_output_of_prior_pics_flag = h264_br_flag(br);
        sh->long_term_reference_flag = h264_br_flag(br);
        return;
    }

    sh->adaptive_ref_pic_marking_mode_flag = h264_br_flag(br);
    if (!sh->adaptive_ref_pic_marking_mode_flag) {
        return;
    }
    for (int guard = 0; guard < 64; guard++) {
        uint32_t op = h264_br_ue(br);
        if (br->error || op == 0) {
            break;
        }
        if (op == 1 || op == 3) {
            h264_br_ue(br); /* difference_of_pic_nums_minus1 */
        }
        if (op == 2) {
            h264_br_ue(br); /* long_term_pic_num */
        }
        if (op == 3 || op == 6) {
            h264_br_ue(br); /* long_term_frame_idx */
        }
        if (op == 4) {
            h264_br_ue(br); /* max_long_term_frame_idx_plus1 */
        }
    }
}

int h264_parse_slice_header(const H264NalUnit *nal,
                             uint8_t *rbsp_scratch,
                             const H264SPS *sps_table,
                             const H264PPS *pps_table,
                             H264SliceHeader *out_sh) {
    memset(out_sh, 0, sizeof(*out_sh));

    H264BitReader br;
    h264_br_init(&br, nal->data, nal->size, rbsp_scratch);
    h264_br_skip_bits(&br, 8); /* NAL header byte */

    out_sh->nal_ref_idc = nal->nal_ref_idc;
    out_sh->nal_unit_type = nal->nal_unit_type;
    out_sh->is_idr = (nal->nal_unit_type == 5);

    out_sh->first_mb_in_slice = h264_br_ue(&br);
    uint32_t raw_slice_type = h264_br_ue(&br);
    out_sh->slice_type = raw_slice_type % 5;
    out_sh->pic_parameter_set_id = h264_br_ue(&br);

    if (out_sh->slice_type == H264_SLICE_TYPE_SP || out_sh->slice_type == H264_SLICE_TYPE_SI) {
        return -2; /* SP/SI slices out of scope for this milestone */
    }

    if (out_sh->pic_parameter_set_id >= H264_MAX_PPS_COUNT || !pps_table[out_sh->pic_parameter_set_id].valid) {
        return -1;
    }
    const H264PPS *pps = &pps_table[out_sh->pic_parameter_set_id];
    if (pps->seq_parameter_set_id >= H264_MAX_SPS_COUNT || !sps_table[pps->seq_parameter_set_id].valid) {
        return -1;
    }
    const H264SPS *sps = &sps_table[pps->seq_parameter_set_id];

    out_sh->frame_num = h264_br_u(&br, (int)sps->log2_max_frame_num);

    /* frame_mbs_only_flag is required to be 1 by h264_parse_sps() for any
     * SPS accepted into sps_table, so field_pic_flag is never signalled. */

    if (out_sh->is_idr) {
        out_sh->idr_pic_id = h264_br_ue(&br);
    }

    if (sps->pic_order_cnt_type == 0) {
        out_sh->pic_order_cnt_lsb = h264_br_u(&br, (int)sps->log2_max_pic_order_cnt_lsb);
        if (pps->bottom_field_pic_order_in_frame_present_flag) {
            out_sh->delta_pic_order_cnt_bottom = h264_br_se(&br);
        }
    } else if (sps->pic_order_cnt_type == 1 && !sps->delta_pic_order_always_zero_flag) {
        out_sh->delta_pic_order_cnt[0] = h264_br_se(&br);
        if (pps->bottom_field_pic_order_in_frame_present_flag) {
            out_sh->delta_pic_order_cnt[1] = h264_br_se(&br);
        }
    }

    if (pps->redundant_pic_cnt_present_flag) {
        out_sh->redundant_pic_cnt = h264_br_ue(&br);
    }

    if (out_sh->slice_type == H264_SLICE_TYPE_B) {
        out_sh->direct_spatial_mv_pred_flag = h264_br_flag(&br);
    }

    out_sh->num_ref_idx_l0_active_minus1 = pps->num_ref_idx_l0_default_active_minus1;
    out_sh->num_ref_idx_l1_active_minus1 = pps->num_ref_idx_l1_default_active_minus1;

    if (out_sh->slice_type == H264_SLICE_TYPE_P || out_sh->slice_type == H264_SLICE_TYPE_B) {
        out_sh->num_ref_idx_active_override_flag = h264_br_flag(&br);
        if (out_sh->num_ref_idx_active_override_flag) {
            out_sh->num_ref_idx_l0_active_minus1 = h264_br_ue(&br);
            if (out_sh->slice_type == H264_SLICE_TYPE_B) {
                out_sh->num_ref_idx_l1_active_minus1 = h264_br_ue(&br);
            }
        }
    }

    if (out_sh->slice_type != H264_SLICE_TYPE_I) {
        if (h264_br_flag(&br)) {
            parse_ref_pic_list_modification(&br, out_sh->rplm_l0, &out_sh->rplm_l0_count);
        }
    }
    if (out_sh->slice_type == H264_SLICE_TYPE_B) {
        if (h264_br_flag(&br)) {
            parse_ref_pic_list_modification(&br, out_sh->rplm_l1, &out_sh->rplm_l1_count);
        }
    }

    int use_weighted = (pps->weighted_pred_flag && (out_sh->slice_type == H264_SLICE_TYPE_P)) ||
                        (pps->weighted_bipred_idc == 1 && out_sh->slice_type == H264_SLICE_TYPE_B);
    if (use_weighted) {
        parse_pred_weight_table(&br, out_sh);
    }

    if (out_sh->nal_ref_idc != 0) {
        parse_dec_ref_pic_marking(&br, out_sh);
    }

    if (pps->entropy_coding_mode_flag && out_sh->slice_type != H264_SLICE_TYPE_I) {
        out_sh->cabac_init_idc = (int)h264_br_ue(&br);
    }

    out_sh->slice_qp_delta = h264_br_se(&br);

    if (pps->deblocking_filter_control_present_flag) {
        out_sh->disable_deblocking_filter_idc = h264_br_ue(&br);
        if (out_sh->disable_deblocking_filter_idc != 1) {
            out_sh->slice_alpha_c0_offset_div2 = h264_br_se(&br);
            out_sh->slice_beta_offset_div2 = h264_br_se(&br);
        }
    }

    /* num_slice_groups_minus1 > 0 (FMO) is rejected at PPS-parse time, so
     * slice_group_change_cycle is never signalled here. */

    if (br.error) {
        return -1;
    }

    out_sh->header_bits = h264_br_bits_consumed(&br);
    return 0;
}

int32_t h264_compute_poc(const H264SPS *sps, const H264SliceHeader *sh, H264PocState *state) {
    int is_ref = (sh->nal_ref_idc != 0);

    if (sps->pic_order_cnt_type == 0) {
        int32_t prev_msb = sh->is_idr ? 0 : state->prev_poc_msb;
        int32_t prev_lsb = sh->is_idr ? 0 : state->prev_poc_lsb;
        int32_t max_lsb = (int32_t)1 << sps->log2_max_pic_order_cnt_lsb;

        int32_t poc_msb;
        if ((int32_t)sh->pic_order_cnt_lsb < prev_lsb && (prev_lsb - (int32_t)sh->pic_order_cnt_lsb) >= max_lsb / 2) {
            poc_msb = prev_msb + max_lsb;
        } else if ((int32_t)sh->pic_order_cnt_lsb > prev_lsb && ((int32_t)sh->pic_order_cnt_lsb - prev_lsb) > max_lsb / 2) {
            poc_msb = prev_msb - max_lsb;
        } else {
            poc_msb = prev_msb;
        }

        int32_t top_poc = poc_msb + (int32_t)sh->pic_order_cnt_lsb;
        int32_t bottom_poc = top_poc + sh->delta_pic_order_cnt_bottom;

        if (is_ref) {
            state->prev_poc_msb = poc_msb;
            state->prev_poc_lsb = (int32_t)sh->pic_order_cnt_lsb;
        }
        return top_poc < bottom_poc ? top_poc : bottom_poc;
    }

    uint32_t max_frame_num = (uint32_t)1 << sps->log2_max_frame_num;
    uint32_t frame_num_offset;
    if (sh->is_idr) {
        frame_num_offset = 0;
    } else if (state->prev_frame_num > sh->frame_num) {
        frame_num_offset = state->prev_frame_num_offset + max_frame_num;
    } else {
        frame_num_offset = state->prev_frame_num_offset;
    }

    int32_t poc;
    if (sps->pic_order_cnt_type == 2) {
        int64_t temp_poc;
        if (sh->is_idr) {
            temp_poc = 0;
        } else if (!is_ref) {
            temp_poc = 2 * (int64_t)(frame_num_offset + sh->frame_num) - 1;
        } else {
            temp_poc = 2 * (int64_t)(frame_num_offset + sh->frame_num);
        }
        poc = (int32_t)temp_poc;
    } else {
        /* pic_order_cnt_type == 1 */
        uint32_t abs_frame_num = 0;
        if (sps->num_ref_frames_in_pic_order_cnt_cycle != 0) {
            abs_frame_num = frame_num_offset + sh->frame_num;
        }
        if (!is_ref && abs_frame_num > 0) {
            abs_frame_num -= 1;
        }

        int32_t expected_delta_per_cycle = 0;
        for (uint32_t i = 0; i < sps->num_ref_frames_in_pic_order_cnt_cycle; i++) {
            expected_delta_per_cycle += sps->offset_for_ref_frame[i];
        }

        int32_t expected_poc = 0;
        if (abs_frame_num > 0 && sps->num_ref_frames_in_pic_order_cnt_cycle != 0) {
            uint32_t cycle_cnt = (abs_frame_num - 1) / sps->num_ref_frames_in_pic_order_cnt_cycle;
            uint32_t in_cycle = (abs_frame_num - 1) % sps->num_ref_frames_in_pic_order_cnt_cycle;
            expected_poc = (int32_t)cycle_cnt * expected_delta_per_cycle;
            for (uint32_t i = 0; i <= in_cycle; i++) {
                expected_poc += sps->offset_for_ref_frame[i];
            }
        }
        if (!is_ref) {
            expected_poc += sps->offset_for_non_ref_pic;
        }

        int32_t top_poc = expected_poc + sh->delta_pic_order_cnt[0];
        int32_t bottom_poc = top_poc + sps->offset_for_top_to_bottom_field + sh->delta_pic_order_cnt[1];
        poc = top_poc < bottom_poc ? top_poc : bottom_poc;
    }

    state->prev_frame_num_offset = frame_num_offset;
    state->prev_frame_num = sh->frame_num;
    return poc;
}
