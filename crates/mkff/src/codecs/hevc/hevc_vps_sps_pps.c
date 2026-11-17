#include "hevc_vps_sps_pps.h"

#include <string.h>

static void parse_profile_tier_level(HevcBitReader *br, int profile_present_flag,
                                     uint32_t max_sub_layers_minus1, HevcProfileTierLevel *out) {
    memset(out, 0, sizeof(*out));
    if (profile_present_flag) {
        out->general_profile_space = (uint8_t)hevc_br_u(br, 2);
        out->general_tier_flag = (uint8_t)hevc_br_flag(br);
        out->general_profile_idc = (uint8_t)hevc_br_u(br, 5);
        out->general_profile_compatibility_flags = hevc_br_u(br, 32);
        out->general_progressive_source_flag = (uint8_t)hevc_br_flag(br);
        out->general_interlaced_source_flag = (uint8_t)hevc_br_flag(br);
        out->general_non_packed_constraint_flag = (uint8_t)hevc_br_flag(br);
        out->general_frame_only_constraint_flag = (uint8_t)hevc_br_flag(br);
        hevc_br_skip_bits(br, 44);
    }
    out->general_level_idc = (uint8_t)hevc_br_u(br, 8);

    uint8_t sub_layer_profile_present_flag[HEVC_MAX_SUB_LAYERS];
    uint8_t sub_layer_level_present_flag[HEVC_MAX_SUB_LAYERS];
    for (uint32_t i = 0; i < max_sub_layers_minus1; i++) {
        sub_layer_profile_present_flag[i] = (uint8_t)hevc_br_flag(br);
        sub_layer_level_present_flag[i] = (uint8_t)hevc_br_flag(br);
    }
    if (max_sub_layers_minus1 > 0) {
        for (uint32_t i = max_sub_layers_minus1; i < 8; i++) {
            hevc_br_skip_bits(br, 2);
        }
    }
    for (uint32_t i = 0; i < max_sub_layers_minus1; i++) {
        if (sub_layer_profile_present_flag[i]) {
            hevc_br_skip_bits(br, 2 + 1 + 5 + 32 + 4 + 44);
        }
        if (sub_layer_level_present_flag[i]) {
            hevc_br_skip_bits(br, 8);
        }
    }
}

static void skip_scaling_list_data(HevcBitReader *br) {
    for (int size_id = 0; size_id < 4; size_id++) {
        for (int matrix_id = 0; matrix_id < ((size_id == 3) ? 2 : 6); matrix_id++) {
            int pred = hevc_br_flag(br);
            if (!pred) {
                uint32_t coef_num = (uint32_t)(1 << (4 + (size_id << 1)));
                if (coef_num > 64) {
                    coef_num = 64;
                }
                if (size_id > 1) {
                    (void)hevc_br_se(br);
                }
                int32_t next_coef = 8;
                for (uint32_t i = 0; i < coef_num; i++) {
                    int32_t delta = hevc_br_se(br);
                    next_coef = (next_coef + delta + 256) % 256;
                }
            } else {
                (void)hevc_br_ue(br);
            }
        }
    }
}

static int parse_st_rps_direct(HevcBitReader *br, HevcShortTermRps *rps) {
    memset(rps, 0, sizeof(*rps));
    rps->num_negative_pics = hevc_br_ue(br);
    rps->num_positive_pics = hevc_br_ue(br);
    if (rps->num_negative_pics > HEVC_MAX_DPB_SIZE || rps->num_positive_pics > HEVC_MAX_DPB_SIZE) {
        br->error = 1;
        return -1;
    }
    int32_t prev = 0;
    for (uint32_t i = 0; i < rps->num_negative_pics; i++) {
        uint32_t d = hevc_br_ue(br) + 1;
        prev -= (int32_t)d;
        rps->delta_poc_s0[i] = prev;
        rps->used_by_curr_pic_s0[i] = (uint8_t)hevc_br_flag(br);
    }
    prev = 0;
    for (uint32_t i = 0; i < rps->num_positive_pics; i++) {
        uint32_t d = hevc_br_ue(br) + 1;
        prev += (int32_t)d;
        rps->delta_poc_s1[i] = prev;
        rps->used_by_curr_pic_s1[i] = (uint8_t)hevc_br_flag(br);
    }
    rps->num_delta_pocs = rps->num_negative_pics + rps->num_positive_pics;
    return br->error ? -1 : 0;
}

int hevc_parse_short_term_ref_pic_set(HevcBitReader *br, HevcShortTermRps *table,
                                      uint32_t st_rps_idx, uint32_t num_st_rps, int from_slice) {
    HevcShortTermRps *rps = &table[st_rps_idx];
    memset(rps, 0, sizeof(*rps));

    int inter = 0;
    if (from_slice) {
        if (num_st_rps > 0) {
            inter = hevc_br_flag(br);
        }
    } else if (st_rps_idx != 0) {
        inter = hevc_br_flag(br);
    }
    rps->inter_ref_pic_set_prediction_flag = inter;

    if (!inter) {
        return parse_st_rps_direct(br, rps);
    }

    uint32_t delta_idx_minus1 = 0;
    if (from_slice) {
        delta_idx_minus1 = hevc_br_ue(br);
    }
    rps->delta_idx_minus1 = delta_idx_minus1;

    uint32_t RefRpsIdx;
    if (from_slice) {
        if (delta_idx_minus1 + 1 > num_st_rps) {
            br->error = 1;
            return -1;
        }
        RefRpsIdx = num_st_rps - (delta_idx_minus1 + 1);
    } else {
        if (delta_idx_minus1 + 1 > st_rps_idx) {
            br->error = 1;
            return -1;
        }
        RefRpsIdx = st_rps_idx - (delta_idx_minus1 + 1);
    }

    const HevcShortTermRps *ref = &table[RefRpsIdx];
    int delta_rps_sign = hevc_br_flag(br);
    uint32_t abs_delta_rps_minus1 = hevc_br_ue(br);
    int32_t delta_rps = (1 - 2 * delta_rps_sign) * (int32_t)(abs_delta_rps_minus1 + 1);

    uint32_t num_delta = ref->num_delta_pocs;
    if (num_delta > HEVC_MAX_DPB_SIZE * 2) {
        br->error = 1;
        return -1;
    }

    uint8_t used_by_curr[HEVC_MAX_DPB_SIZE * 2 + 1];
    uint8_t use_delta[HEVC_MAX_DPB_SIZE * 2 + 1];
    for (uint32_t j = 0; j <= num_delta; j++) {
        used_by_curr[j] = (uint8_t)hevc_br_flag(br);
        use_delta[j] = used_by_curr[j] ? 1 : (uint8_t)hevc_br_flag(br);
    }

    int32_t ref_delta_poc[HEVC_MAX_DPB_SIZE * 2];
    uint32_t ref_n = 0;
    for (uint32_t i = 0; i < ref->num_negative_pics; i++) {
        ref_delta_poc[ref_n++] = ref->delta_poc_s0[i];
    }
    for (uint32_t i = 0; i < ref->num_positive_pics; i++) {
        ref_delta_poc[ref_n++] = ref->delta_poc_s1[i];
    }

    int32_t neg[HEVC_MAX_DPB_SIZE];
    uint8_t neg_used[HEVC_MAX_DPB_SIZE];
    uint32_t n_neg = 0;
    for (int i = (int)num_delta - 1; i >= 0; i--) {
        int32_t d = ref_delta_poc[i] + delta_rps;
        if (d < 0 && use_delta[i]) {
            if (n_neg >= HEVC_MAX_DPB_SIZE) {
                br->error = 1;
                return -1;
            }
            neg[n_neg] = d;
            neg_used[n_neg] = used_by_curr[i];
            n_neg++;
        }
    }
    if (delta_rps < 0 && use_delta[num_delta]) {
        if (n_neg >= HEVC_MAX_DPB_SIZE) {
            br->error = 1;
            return -1;
        }
        neg[n_neg] = delta_rps;
        neg_used[n_neg] = used_by_curr[num_delta];
        n_neg++;
    }

    int32_t pos[HEVC_MAX_DPB_SIZE];
    uint8_t pos_used[HEVC_MAX_DPB_SIZE];
    uint32_t n_pos = 0;
    if (delta_rps > 0 && use_delta[num_delta]) {
        pos[n_pos] = delta_rps;
        pos_used[n_pos] = used_by_curr[num_delta];
        n_pos++;
    }
    for (uint32_t i = 0; i < num_delta; i++) {
        int32_t d = ref_delta_poc[i] + delta_rps;
        if (d > 0 && use_delta[i]) {
            if (n_pos >= HEVC_MAX_DPB_SIZE) {
                br->error = 1;
                return -1;
            }
            pos[n_pos] = d;
            pos_used[n_pos] = used_by_curr[i];
            n_pos++;
        }
    }

    rps->num_negative_pics = n_neg;
    rps->num_positive_pics = n_pos;
    for (uint32_t i = 0; i < n_neg; i++) {
        rps->delta_poc_s0[i] = neg[i];
        rps->used_by_curr_pic_s0[i] = neg_used[i];
    }
    for (uint32_t i = 0; i < n_pos; i++) {
        rps->delta_poc_s1[i] = pos[i];
        rps->used_by_curr_pic_s1[i] = pos_used[i];
    }
    rps->num_delta_pocs = n_neg + n_pos;
    return br->error ? -1 : 0;
}

int hevc_expand_short_term_rps(HevcShortTermRps *rps_table, uint32_t st_rps_idx,
                               uint32_t num_short_term_ref_pic_sets) {
    (void)rps_table;
    (void)st_rps_idx;
    (void)num_short_term_ref_pic_sets;
    return 0;
}

static int check_main_main10(const HevcProfileTierLevel *ptl, uint32_t bit_depth_minus8,
                             uint32_t chroma_format_idc) {
    if (chroma_format_idc != 1) {
        return -2;
    }
    if (bit_depth_minus8 != 0 && bit_depth_minus8 != 2) {
        return -2;
    }
    int main = (ptl->general_profile_idc == 1)
               || (ptl->general_profile_compatibility_flags & (1u << 1));
    int main10 = (ptl->general_profile_idc == 2)
                 || (ptl->general_profile_compatibility_flags & (1u << 2));
    if (bit_depth_minus8 == 0 && !main && !main10) {
        return -2;
    }
    if (bit_depth_minus8 == 2 && !main10) {
        return -2;
    }
    return 0;
}

int hevc_parse_vps(const uint8_t *nal_data, size_t nal_size, uint8_t *rbsp_scratch, HevcVPS *out_vps) {
    memset(out_vps, 0, sizeof(*out_vps));
    HevcBitReader br;
    hevc_br_init(&br, nal_data, nal_size, rbsp_scratch);
    hevc_br_skip_bits(&br, 16);

    out_vps->vps_video_parameter_set_id = hevc_br_u(&br, 4);
    hevc_br_skip_bits(&br, 1);
    hevc_br_skip_bits(&br, 1);
    out_vps->vps_max_layers_minus1 = hevc_br_u(&br, 6);
    out_vps->vps_max_sub_layers_minus1 = hevc_br_u(&br, 3);
    out_vps->vps_temporal_id_nesting_flag = hevc_br_flag(&br);
    hevc_br_skip_bits(&br, 16);

    if (out_vps->vps_max_sub_layers_minus1 >= HEVC_MAX_SUB_LAYERS) {
        return -1;
    }
    parse_profile_tier_level(&br, 1, out_vps->vps_max_sub_layers_minus1, &out_vps->ptl);

    if (br.error) {
        return -1;
    }
    out_vps->valid = 1;
    return 0;
}

int hevc_parse_sps(const uint8_t *nal_data, size_t nal_size, uint8_t *rbsp_scratch, HevcSPS *out_sps) {
    memset(out_sps, 0, sizeof(*out_sps));
    HevcBitReader br;
    hevc_br_init(&br, nal_data, nal_size, rbsp_scratch);
    hevc_br_skip_bits(&br, 16);

    out_sps->sps_video_parameter_set_id = hevc_br_u(&br, 4);
    out_sps->sps_max_sub_layers_minus1 = hevc_br_u(&br, 3);
    out_sps->sps_temporal_id_nesting_flag = hevc_br_flag(&br);
    if (out_sps->sps_max_sub_layers_minus1 >= HEVC_MAX_SUB_LAYERS) {
        return -1;
    }
    parse_profile_tier_level(&br, 1, out_sps->sps_max_sub_layers_minus1, &out_sps->ptl);

    out_sps->sps_seq_parameter_set_id = hevc_br_ue(&br);
    out_sps->chroma_format_idc = hevc_br_ue(&br);
    if (out_sps->chroma_format_idc == 3) {
        out_sps->separate_colour_plane_flag = hevc_br_flag(&br);
    }

    out_sps->pic_width_in_luma_samples = hevc_br_ue(&br);
    out_sps->pic_height_in_luma_samples = hevc_br_ue(&br);

    out_sps->conformance_window_flag = hevc_br_flag(&br);
    if (out_sps->conformance_window_flag) {
        out_sps->conf_win_left_offset = hevc_br_ue(&br);
        out_sps->conf_win_right_offset = hevc_br_ue(&br);
        out_sps->conf_win_top_offset = hevc_br_ue(&br);
        out_sps->conf_win_bottom_offset = hevc_br_ue(&br);
    }

    out_sps->bit_depth_luma_minus8 = hevc_br_ue(&br);
    out_sps->bit_depth_chroma_minus8 = hevc_br_ue(&br);
    out_sps->bit_depth = out_sps->bit_depth_luma_minus8 + 8;

    uint32_t log2_max_pic_order_cnt_lsb_minus4 = hevc_br_ue(&br);
    out_sps->log2_max_pic_order_cnt_lsb = log2_max_pic_order_cnt_lsb_minus4 + 4;

    int sps_sub_layer_ordering_info_present_flag = hevc_br_flag(&br);
    for (uint32_t i = (sps_sub_layer_ordering_info_present_flag ? 0 : out_sps->sps_max_sub_layers_minus1);
         i <= out_sps->sps_max_sub_layers_minus1;
         i++) {
        out_sps->sps_max_dec_pic_buffering_minus1[i] = hevc_br_ue(&br);
        out_sps->sps_max_num_reorder_pics[i] = hevc_br_ue(&br);
        out_sps->sps_max_latency_increase_plus1[i] = hevc_br_ue(&br);
    }
    if (!sps_sub_layer_ordering_info_present_flag) {
        for (uint32_t i = 0; i < out_sps->sps_max_sub_layers_minus1; i++) {
            out_sps->sps_max_dec_pic_buffering_minus1[i] =
                out_sps->sps_max_dec_pic_buffering_minus1[out_sps->sps_max_sub_layers_minus1];
            out_sps->sps_max_num_reorder_pics[i] =
                out_sps->sps_max_num_reorder_pics[out_sps->sps_max_sub_layers_minus1];
            out_sps->sps_max_latency_increase_plus1[i] =
                out_sps->sps_max_latency_increase_plus1[out_sps->sps_max_sub_layers_minus1];
        }
    }

    out_sps->log2_min_luma_coding_block_size_minus3 = hevc_br_ue(&br);
    out_sps->log2_diff_max_min_luma_coding_block_size = hevc_br_ue(&br);
    out_sps->log2_min_luma_transform_block_size_minus2 = hevc_br_ue(&br);
    out_sps->log2_diff_max_min_luma_transform_block_size = hevc_br_ue(&br);
    out_sps->max_transform_hierarchy_depth_inter = hevc_br_ue(&br);
    out_sps->max_transform_hierarchy_depth_intra = hevc_br_ue(&br);

    out_sps->scaling_list_enabled_flag = hevc_br_flag(&br);
    if (out_sps->scaling_list_enabled_flag) {
        if (hevc_br_flag(&br)) {
            skip_scaling_list_data(&br);
        }
    }

    out_sps->amp_enabled_flag = hevc_br_flag(&br);
    out_sps->sample_adaptive_offset_enabled_flag = hevc_br_flag(&br);
    out_sps->pcm_enabled_flag = hevc_br_flag(&br);
    if (out_sps->pcm_enabled_flag) {
        hevc_br_skip_bits(&br, 4);
        hevc_br_skip_bits(&br, 4);
        (void)hevc_br_ue(&br);
        (void)hevc_br_ue(&br);
        hevc_br_skip_bits(&br, 1);
    }

    out_sps->num_short_term_ref_pic_sets = hevc_br_ue(&br);
    if (out_sps->num_short_term_ref_pic_sets > HEVC_MAX_SHORT_TERM_RPS_COUNT) {
        return -1;
    }
    for (uint32_t i = 0; i < out_sps->num_short_term_ref_pic_sets; i++) {
        if (hevc_parse_short_term_ref_pic_set(&br, out_sps->short_term_ref_pic_set, i,
                                              out_sps->num_short_term_ref_pic_sets, 0) != 0) {
            return -1;
        }
    }

    out_sps->long_term_ref_pics_present_flag = hevc_br_flag(&br);
    if (out_sps->long_term_ref_pics_present_flag) {
        out_sps->num_long_term_ref_pics_sps = hevc_br_ue(&br);
        if (out_sps->num_long_term_ref_pics_sps > HEVC_MAX_LONG_TERM_REFS) {
            return -1;
        }
        for (uint32_t i = 0; i < out_sps->num_long_term_ref_pics_sps; i++) {
            out_sps->lt_ref_pic_poc_lsb_sps[i] =
                hevc_br_u(&br, (int)out_sps->log2_max_pic_order_cnt_lsb);
            out_sps->used_by_curr_pic_lt_sps_flag[i] = (uint8_t)hevc_br_flag(&br);
        }
    }

    out_sps->sps_temporal_mvp_enabled_flag = hevc_br_flag(&br);
    out_sps->strong_intra_smoothing_enabled_flag = hevc_br_flag(&br);

    uint32_t min_cb_log2 = out_sps->log2_min_luma_coding_block_size_minus3 + 3;
    out_sps->ctb_log2_size_y = min_cb_log2 + out_sps->log2_diff_max_min_luma_coding_block_size;
    uint32_t ctb_size = (uint32_t)1 << out_sps->ctb_log2_size_y;
    if (ctb_size == 0) {
        return -1;
    }
    out_sps->pic_width_in_ctbs_y =
        (out_sps->pic_width_in_luma_samples + ctb_size - 1) / ctb_size;
    out_sps->pic_height_in_ctbs_y =
        (out_sps->pic_height_in_luma_samples + ctb_size - 1) / ctb_size;

    if (br.error) {
        return -1;
    }

    int unsupported = check_main_main10(&out_sps->ptl, out_sps->bit_depth_luma_minus8,
                                        out_sps->chroma_format_idc);
    if (out_sps->bit_depth_chroma_minus8 != out_sps->bit_depth_luma_minus8) {
        unsupported = -2;
    }

    out_sps->valid = 1;
    return unsupported;
}

int hevc_parse_pps(const uint8_t *nal_data, size_t nal_size, uint8_t *rbsp_scratch,
                   const HevcSPS *sps_table, HevcPPS *out_pps) {
    (void)sps_table;
    memset(out_pps, 0, sizeof(*out_pps));
    HevcBitReader br;
    hevc_br_init(&br, nal_data, nal_size, rbsp_scratch);
    hevc_br_skip_bits(&br, 16);

    out_pps->pps_pic_parameter_set_id = hevc_br_ue(&br);
    out_pps->pps_seq_parameter_set_id = hevc_br_ue(&br);
    out_pps->dependent_slice_segments_enabled_flag = hevc_br_flag(&br);
    out_pps->output_flag_present_flag = hevc_br_flag(&br);
    out_pps->num_extra_slice_header_bits = hevc_br_u(&br, 3);
    out_pps->sign_data_hiding_enabled_flag = hevc_br_flag(&br);
    out_pps->cabac_init_present_flag = hevc_br_flag(&br);
    out_pps->num_ref_idx_l0_default_active_minus1 = hevc_br_ue(&br);
    out_pps->num_ref_idx_l1_default_active_minus1 = hevc_br_ue(&br);
    out_pps->init_qp_minus26 = hevc_br_se(&br);
    out_pps->constrained_intra_pred_flag = hevc_br_flag(&br);
    out_pps->transform_skip_enabled_flag = hevc_br_flag(&br);
    out_pps->cu_qp_delta_enabled_flag = hevc_br_flag(&br);
    if (out_pps->cu_qp_delta_enabled_flag) {
        out_pps->diff_cu_qp_delta_depth = hevc_br_ue(&br);
    }
    out_pps->pps_cb_qp_offset = hevc_br_se(&br);
    out_pps->pps_cr_qp_offset = hevc_br_se(&br);
    out_pps->pps_slice_chroma_qp_offsets_present_flag = hevc_br_flag(&br);
    out_pps->weighted_pred_flag = hevc_br_flag(&br);
    out_pps->weighted_bipred_flag = hevc_br_flag(&br);
    out_pps->transquant_bypass_enabled_flag = hevc_br_flag(&br);
    out_pps->tiles_enabled_flag = hevc_br_flag(&br);
    out_pps->entropy_coding_sync_enabled_flag = hevc_br_flag(&br);

    if (out_pps->tiles_enabled_flag) {
        uint32_t num_tile_columns_minus1 = hevc_br_ue(&br);
        uint32_t num_tile_rows_minus1 = hevc_br_ue(&br);
        int uniform = hevc_br_flag(&br);
        if (!uniform) {
            for (uint32_t i = 0; i < num_tile_columns_minus1; i++) {
                (void)hevc_br_ue(&br);
            }
            for (uint32_t i = 0; i < num_tile_rows_minus1; i++) {
                (void)hevc_br_ue(&br);
            }
        }
        hevc_br_skip_bits(&br, 1);
    }

    out_pps->pps_loop_filter_across_slices_enabled_flag = hevc_br_flag(&br);
    out_pps->deblocking_filter_control_present_flag = hevc_br_flag(&br);
    if (out_pps->deblocking_filter_control_present_flag) {
        out_pps->deblocking_filter_override_enabled_flag = hevc_br_flag(&br);
        out_pps->pps_deblocking_filter_disabled_flag = hevc_br_flag(&br);
        if (!out_pps->pps_deblocking_filter_disabled_flag) {
            out_pps->pps_beta_offset_div2 = hevc_br_se(&br);
            out_pps->pps_tc_offset_div2 = hevc_br_se(&br);
        }
    }

    if (hevc_br_flag(&br)) {
        skip_scaling_list_data(&br);
    }

    out_pps->lists_modification_present_flag = hevc_br_flag(&br);
    out_pps->log2_parallel_merge_level_minus2 = hevc_br_ue(&br);
    out_pps->slice_segment_header_extension_present_flag = hevc_br_flag(&br);

    if (br.error) {
        return -1;
    }
    out_pps->valid = 1;
    return 0;
}
