#include "hevc_slice.h"

#include <string.h>

static void parse_pred_weight_table(HevcBitReader *br, const HevcSPS *sps, HevcSliceHeader *sh) {
    sh->has_pred_weight_table = 1;
    sh->luma_log2_weight_denom = hevc_br_ue(br);
    int32_t chroma_log2 = (int32_t)sh->luma_log2_weight_denom;
    if (sps->chroma_format_idc != 0) {
        chroma_log2 += hevc_br_se(br);
    }
    sh->delta_chroma_log2_weight_denom = chroma_log2 - (int32_t)sh->luma_log2_weight_denom;

    for (uint32_t i = 0; i <= sh->num_ref_idx_l0_active_minus1 && i < 15; i++) {
        int luma_weight_flag = hevc_br_flag(br);
        int chroma_weight_flag = 0;
        if (sps->chroma_format_idc != 0) {
            chroma_weight_flag = hevc_br_flag(br);
        }
        if (luma_weight_flag) {
            sh->delta_luma_weight_l0[i] = (int8_t)hevc_br_se(br);
            sh->luma_offset_l0[i] = (int8_t)hevc_br_se(br);
        }
        if (chroma_weight_flag) {
            for (int j = 0; j < 2; j++) {
                sh->delta_chroma_weight_l0[i][j] = (int8_t)hevc_br_se(br);
                sh->chroma_offset_l0[i][j] = (int8_t)hevc_br_se(br);
            }
        }
    }
    if (sh->slice_type == HEVC_SLICE_TYPE_B) {
        for (uint32_t i = 0; i <= sh->num_ref_idx_l1_active_minus1 && i < 15; i++) {
            int luma_weight_flag = hevc_br_flag(br);
            int chroma_weight_flag = 0;
            if (sps->chroma_format_idc != 0) {
                chroma_weight_flag = hevc_br_flag(br);
            }
            if (luma_weight_flag) {
                sh->delta_luma_weight_l1[i] = (int8_t)hevc_br_se(br);
                sh->luma_offset_l1[i] = (int8_t)hevc_br_se(br);
            }
            if (chroma_weight_flag) {
                for (int j = 0; j < 2; j++) {
                    sh->delta_chroma_weight_l1[i][j] = (int8_t)hevc_br_se(br);
                    sh->chroma_offset_l1[i][j] = (int8_t)hevc_br_se(br);
                }
            }
        }
    }
}

int hevc_parse_slice_header(const HevcNalUnit *nal,
                            uint8_t *rbsp_scratch,
                            const HevcSPS *sps_table,
                            const HevcPPS *pps_table,
                            HevcSliceHeader *out_sh) {
    memset(out_sh, 0, sizeof(*out_sh));
    if (!nal || nal->size < 2) {
        return -1;
    }

    HevcBitReader br;
    hevc_br_init(&br, nal->data, nal->size, rbsp_scratch);

    out_sh->nal_unit_type = nal->nal_unit_type;
    out_sh->nuh_temporal_id_plus1 = nal->nuh_temporal_id_plus1;
    hevc_br_skip_bits(&br, 16);

    out_sh->first_slice_segment_in_pic_flag = hevc_br_flag(&br);
    if (hevc_nal_is_irap(nal->nal_unit_type)) {
        out_sh->no_output_of_prior_pics_flag = hevc_br_flag(&br);
    }
    out_sh->slice_pic_parameter_set_id = hevc_br_ue(&br);
    if (out_sh->slice_pic_parameter_set_id >= HEVC_MAX_PPS_COUNT) {
        return -1;
    }
    const HevcPPS *pps = &pps_table[out_sh->slice_pic_parameter_set_id];
    if (!pps->valid || pps->pps_seq_parameter_set_id >= HEVC_MAX_SPS_COUNT) {
        return -1;
    }
    const HevcSPS *sps = &sps_table[pps->pps_seq_parameter_set_id];
    if (!sps->valid) {
        return -1;
    }

    if (!out_sh->first_slice_segment_in_pic_flag) {
        if (pps->dependent_slice_segments_enabled_flag) {
            out_sh->dependent_slice_segment_flag = hevc_br_flag(&br);
        }
        uint32_t bits = 0;
        uint32_t ctbs = sps->pic_width_in_ctbs_y * sps->pic_height_in_ctbs_y;
        while (((uint32_t)1 << bits) < ctbs) {
            bits++;
        }
        out_sh->slice_segment_address = hevc_br_u(&br, (int)bits);
    }

    out_sh->dependent_slice = out_sh->dependent_slice_segment_flag;
    if (out_sh->dependent_slice_segment_flag) {
        out_sh->header_bits = hevc_br_bits_consumed(&br);
        return br.error ? -1 : 0;
    }

    for (uint32_t i = 0; i < pps->num_extra_slice_header_bits; i++) {
        hevc_br_skip_bits(&br, 1);
    }

    out_sh->slice_type = hevc_br_ue(&br);
    if (out_sh->slice_type > 2) {
        return -2;
    }

    if (pps->output_flag_present_flag) {
        out_sh->pic_output_flag = hevc_br_flag(&br);
    } else {
        out_sh->pic_output_flag = 1;
    }
    if (sps->separate_colour_plane_flag) {
        out_sh->colour_plane_id = hevc_br_u(&br, 2);
    }

    if (!hevc_nal_is_idr(nal->nal_unit_type)) {
        out_sh->slice_pic_order_cnt_lsb =
            hevc_br_u(&br, (int)sps->log2_max_pic_order_cnt_lsb);
        out_sh->short_term_ref_pic_set_sps_flag = hevc_br_flag(&br);
        if (!out_sh->short_term_ref_pic_set_sps_flag) {
            size_t rps_start = hevc_br_bits_consumed(&br);
            HevcShortTermRps table[HEVC_MAX_SHORT_TERM_RPS_COUNT];
            memcpy(table, sps->short_term_ref_pic_set,
                   sizeof(HevcShortTermRps) * sps->num_short_term_ref_pic_sets);
            if (hevc_parse_short_term_ref_pic_set(&br, table, sps->num_short_term_ref_pic_sets,
                                                  sps->num_short_term_ref_pic_sets, 1) != 0) {
                return -1;
            }
            out_sh->st_rps = table[sps->num_short_term_ref_pic_sets];
            out_sh->st_rps_bits = (uint32_t)(hevc_br_bits_consumed(&br) - rps_start);
            out_sh->short_term_ref_pic_set_size = (uint16_t)out_sh->st_rps_bits;
            out_sh->num_delta_pocs_of_ref_rps_idx = (uint8_t)out_sh->st_rps.num_delta_pocs;
        } else if (sps->num_short_term_ref_pic_sets > 1) {
            uint32_t bits = 0;
            while (((uint32_t)1 << bits) < sps->num_short_term_ref_pic_sets) {
                bits++;
            }
            out_sh->short_term_ref_pic_set_idx = hevc_br_u(&br, (int)bits);
            if (out_sh->short_term_ref_pic_set_idx >= sps->num_short_term_ref_pic_sets) {
                return -1;
            }
            out_sh->st_rps = sps->short_term_ref_pic_set[out_sh->short_term_ref_pic_set_idx];
        } else if (sps->num_short_term_ref_pic_sets == 1) {
            out_sh->st_rps = sps->short_term_ref_pic_set[0];
        }

        if (sps->long_term_ref_pics_present_flag) {
            if (sps->num_long_term_ref_pics_sps > 0) {
                out_sh->num_long_term_sps = hevc_br_ue(&br);
            }
            out_sh->num_long_term_pics = hevc_br_ue(&br);
            uint32_t total = out_sh->num_long_term_sps + out_sh->num_long_term_pics;
            for (uint32_t i = 0; i < total; i++) {
                if (i < out_sh->num_long_term_sps) {
                    if (sps->num_long_term_ref_pics_sps > 1) {
                        uint32_t bits = 0;
                        while (((uint32_t)1 << bits) < sps->num_long_term_ref_pics_sps) {
                            bits++;
                        }
                        hevc_br_skip_bits(&br, (int)bits);
                    }
                } else {
                    hevc_br_skip_bits(&br, (int)sps->log2_max_pic_order_cnt_lsb);
                    hevc_br_skip_bits(&br, 1);
                }
                if (hevc_br_flag(&br)) {
                    (void)hevc_br_ue(&br);
                }
            }
        }

        if (sps->sps_temporal_mvp_enabled_flag) {
            out_sh->slice_temporal_mvp_enabled_flag = hevc_br_flag(&br);
        }
    }

    if (sps->sample_adaptive_offset_enabled_flag) {
        out_sh->slice_sao_luma_flag = hevc_br_flag(&br);
        if (sps->chroma_format_idc != 0) {
            out_sh->slice_sao_chroma_flag = hevc_br_flag(&br);
        }
    }

    if (out_sh->slice_type == HEVC_SLICE_TYPE_P || out_sh->slice_type == HEVC_SLICE_TYPE_B) {
        out_sh->num_ref_idx_active_override_flag = hevc_br_flag(&br);
        out_sh->num_ref_idx_l0_active_minus1 = pps->num_ref_idx_l0_default_active_minus1;
        out_sh->num_ref_idx_l1_active_minus1 = pps->num_ref_idx_l1_default_active_minus1;
        if (out_sh->num_ref_idx_active_override_flag) {
            out_sh->num_ref_idx_l0_active_minus1 = hevc_br_ue(&br);
            if (out_sh->slice_type == HEVC_SLICE_TYPE_B) {
                out_sh->num_ref_idx_l1_active_minus1 = hevc_br_ue(&br);
            }
        }

        uint32_t NumPicTotalCurr = 0;
        for (uint32_t i = 0; i < out_sh->st_rps.num_negative_pics; i++) {
            NumPicTotalCurr += out_sh->st_rps.used_by_curr_pic_s0[i];
        }
        for (uint32_t i = 0; i < out_sh->st_rps.num_positive_pics; i++) {
            NumPicTotalCurr += out_sh->st_rps.used_by_curr_pic_s1[i];
        }
        if (pps->lists_modification_present_flag && NumPicTotalCurr > 1) {
            if (hevc_br_flag(&br)) {
                uint32_t bits = 0;
                while (((uint32_t)1 << bits) < NumPicTotalCurr) {
                    bits++;
                }
                for (uint32_t i = 0; i <= out_sh->num_ref_idx_l0_active_minus1; i++) {
                    hevc_br_skip_bits(&br, (int)bits);
                }
            }
            if (out_sh->slice_type == HEVC_SLICE_TYPE_B && hevc_br_flag(&br)) {
                uint32_t bits = 0;
                while (((uint32_t)1 << bits) < NumPicTotalCurr) {
                    bits++;
                }
                for (uint32_t i = 0; i <= out_sh->num_ref_idx_l1_active_minus1; i++) {
                    hevc_br_skip_bits(&br, (int)bits);
                }
            }
        }

        if (out_sh->slice_type == HEVC_SLICE_TYPE_B) {
            out_sh->mvd_l1_zero_flag = hevc_br_flag(&br);
        }
        if (pps->cabac_init_present_flag) {
            out_sh->cabac_init_flag = hevc_br_flag(&br);
        }
        out_sh->collocated_ref_idx = 0;
        if (out_sh->slice_temporal_mvp_enabled_flag) {
            if (out_sh->slice_type == HEVC_SLICE_TYPE_B) {
                out_sh->collocated_from_l0_flag = (uint32_t)hevc_br_flag(&br);
            } else {
                out_sh->collocated_from_l0_flag = 1;
            }
            if ((out_sh->collocated_from_l0_flag && out_sh->num_ref_idx_l0_active_minus1 > 0)
                || (!out_sh->collocated_from_l0_flag && out_sh->num_ref_idx_l1_active_minus1 > 0)) {
                out_sh->collocated_ref_idx = hevc_br_ue(&br);
            }
        }
        if ((pps->weighted_pred_flag && out_sh->slice_type == HEVC_SLICE_TYPE_P)
            || (pps->weighted_bipred_flag && out_sh->slice_type == HEVC_SLICE_TYPE_B)) {
            parse_pred_weight_table(&br, sps, out_sh);
        }
        out_sh->five_minus_max_num_merge_cand = hevc_br_ue(&br);
    }

    out_sh->slice_qp_delta = hevc_br_se(&br);

    out_sh->slice_deblocking_filter_disabled_flag = pps->pps_deblocking_filter_disabled_flag;
    out_sh->slice_beta_offset_div2 = pps->pps_beta_offset_div2;
    out_sh->slice_tc_offset_div2 = pps->pps_tc_offset_div2;
    out_sh->slice_loop_filter_across_slices_enabled_flag = pps->pps_loop_filter_across_slices_enabled_flag;

    if (pps->pps_slice_chroma_qp_offsets_present_flag) {
        out_sh->slice_cb_qp_offset = hevc_br_se(&br);
        out_sh->slice_cr_qp_offset = hevc_br_se(&br);
    }

    if (pps->deblocking_filter_override_enabled_flag) {
        if (hevc_br_flag(&br)) {
            out_sh->slice_deblocking_filter_disabled_flag = hevc_br_flag(&br);
            if (!out_sh->slice_deblocking_filter_disabled_flag) {
                out_sh->slice_beta_offset_div2 = hevc_br_se(&br);
                out_sh->slice_tc_offset_div2 = hevc_br_se(&br);
            }
        }
    }

    if (pps->pps_loop_filter_across_slices_enabled_flag
        && (out_sh->slice_sao_luma_flag || out_sh->slice_sao_chroma_flag
            || !out_sh->slice_deblocking_filter_disabled_flag)) {
        out_sh->slice_loop_filter_across_slices_enabled_flag = hevc_br_flag(&br);
    }

    if (pps->tiles_enabled_flag || pps->entropy_coding_sync_enabled_flag) {
        out_sh->num_entry_point_offsets = hevc_br_ue(&br);
        if (out_sh->num_entry_point_offsets > 0) {
            uint32_t offset_len_minus1 = hevc_br_ue(&br);
            for (uint32_t i = 0; i < out_sh->num_entry_point_offsets; i++) {
                hevc_br_skip_bits(&br, (int)(offset_len_minus1 + 1));
            }
        }
    }

    if (pps->slice_segment_header_extension_present_flag) {
        uint32_t len = hevc_br_ue(&br);
        for (uint32_t i = 0; i < len; i++) {
            hevc_br_skip_bits(&br, 8);
        }
    }

    /* byte_alignment() */
    hevc_br_skip_bits(&br, 1);
    while (!hevc_br_byte_aligned(&br)) {
        hevc_br_skip_bits(&br, 1);
    }

    out_sh->header_bits = hevc_br_bits_consumed(&br);
    if (br.error) {
        return -1;
    }
    return 0;
}

int32_t hevc_compute_poc(const HevcSPS *sps, const HevcSliceHeader *sh, HevcPocState *state) {
    if (hevc_nal_is_idr(sh->nal_unit_type)) {
        state->prev_tid0_pic_poc = 0;
        return 0;
    }

    uint32_t max_poc_lsb = (uint32_t)1 << sps->log2_max_pic_order_cnt_lsb;
    int32_t prev_poc_lsb = state->prev_tid0_pic_poc & (int32_t)(max_poc_lsb - 1);
    int32_t prev_poc_msb = state->prev_tid0_pic_poc - prev_poc_lsb;
    int32_t poc_lsb = (int32_t)sh->slice_pic_order_cnt_lsb;
    int32_t poc_msb;

    if (poc_lsb < prev_poc_lsb && (prev_poc_lsb - poc_lsb) >= (int32_t)(max_poc_lsb / 2)) {
        poc_msb = prev_poc_msb + (int32_t)max_poc_lsb;
    } else if (poc_lsb > prev_poc_lsb && (poc_lsb - prev_poc_lsb) > (int32_t)(max_poc_lsb / 2)) {
        poc_msb = prev_poc_msb - (int32_t)max_poc_lsb;
    } else {
        poc_msb = prev_poc_msb;
    }

    if (sh->nal_unit_type == HEVC_NAL_BLA_W_LP || sh->nal_unit_type == HEVC_NAL_BLA_W_RADL
        || sh->nal_unit_type == HEVC_NAL_BLA_N_LP) {
        poc_msb = 0;
    }

    int32_t poc = poc_msb + poc_lsb;
    if (sh->nuh_temporal_id_plus1 == 1
        && sh->nal_unit_type != HEVC_NAL_RADL_N && sh->nal_unit_type != HEVC_NAL_RADL_R
        && sh->nal_unit_type != HEVC_NAL_RASL_N && sh->nal_unit_type != HEVC_NAL_RASL_R) {
        state->prev_tid0_pic_poc = poc;
    }
    return poc;
}
