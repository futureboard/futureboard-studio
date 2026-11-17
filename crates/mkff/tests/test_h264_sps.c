#include <assert.h>
#include <stdio.h>

#include "h264_bitwriter.h"
#include "src/codecs/h264/h264_sps_pps.h"

static size_t build_baseline_sps(uint8_t *out, size_t out_capacity) {
    uint8_t rbsp[64];
    TestBitWriter w;
    tbw_init(&w, rbsp, sizeof(rbsp));

    tbw_put_bits(&w, 0x67, 8); /* NAL header: nal_ref_idc=3, nal_unit_type=7 (SPS) */

    tbw_put_bits(&w, 66, 8);  /* profile_idc: Baseline */
    tbw_put_bits(&w, 0, 8);   /* constraint_set flags + reserved */
    tbw_put_bits(&w, 30, 8);  /* level_idc: 3.0 */
    tbw_put_ue(&w, 0);        /* seq_parameter_set_id */
    /* profile 66 has no chroma_format_idc/bit_depth extension */
    tbw_put_ue(&w, 0);        /* log2_max_frame_num_minus4 -> 4 */
    tbw_put_ue(&w, 0);        /* pic_order_cnt_type = 0 */
    tbw_put_ue(&w, 2);        /* log2_max_pic_order_cnt_lsb_minus4 -> 6 */
    tbw_put_ue(&w, 2);        /* max_num_ref_frames */
    tbw_put_bit(&w, 0);       /* gaps_in_frame_num_value_allowed_flag */
    tbw_put_ue(&w, 3);        /* pic_width_in_mbs_minus1 -> 4 (64px) */
    tbw_put_ue(&w, 3);        /* pic_height_in_map_units_minus1 -> 4 (64px) */
    tbw_put_bit(&w, 1);       /* frame_mbs_only_flag */
    tbw_put_bit(&w, 1);       /* direct_8x8_inference_flag */
    tbw_put_bit(&w, 0);       /* frame_cropping_flag */
    tbw_put_bit(&w, 0);       /* vui_parameters_present_flag */
    tbw_rbsp_trailing_bits(&w);

    return tbw_apply_emulation_prevention(rbsp, tbw_byte_size(&w), out, out_capacity);
}

int main(void) {
    uint8_t nal[128];
    size_t nal_size = build_baseline_sps(nal, sizeof(nal));

    uint8_t rbsp_scratch[128];
    H264SPS sps;
    int rc = h264_parse_sps(nal, nal_size, rbsp_scratch, &sps);

    assert(rc == 0);
    assert(sps.valid);
    assert(sps.profile_idc == 66);
    assert(sps.level_idc == 30);
    assert(sps.seq_parameter_set_id == 0);
    assert(sps.chroma_format_idc == 1);
    assert(sps.bit_depth_luma_minus8 == 0);
    assert(sps.bit_depth_chroma_minus8 == 0);
    assert(sps.log2_max_frame_num == 4);
    assert(sps.pic_order_cnt_type == 0);
    assert(sps.log2_max_pic_order_cnt_lsb == 6);
    assert(sps.max_num_ref_frames == 2);
    assert(sps.pic_width_in_mbs == 4);
    assert(sps.pic_height_in_map_units == 4);
    assert(sps.frame_mbs_only_flag == 1);
    assert(sps.direct_8x8_inference_flag == 1);
    assert(sps.frame_cropping_flag == 0);
    assert(!sps.custom_scaling_matrices_present);

    /* A PPS referencing this SPS should parse and link correctly. */
    uint8_t pps_rbsp[32];
    TestBitWriter w;
    tbw_init(&w, pps_rbsp, sizeof(pps_rbsp));
    tbw_put_bits(&w, 0x68, 8); /* NAL header: nal_ref_idc=3, nal_unit_type=8 (PPS) */
    tbw_put_ue(&w, 0); /* pic_parameter_set_id */
    tbw_put_ue(&w, 0); /* seq_parameter_set_id */
    tbw_put_bit(&w, 0); /* entropy_coding_mode_flag: CAVLC */
    tbw_put_bit(&w, 0); /* bottom_field_pic_order_in_frame_present_flag */
    tbw_put_ue(&w, 0); /* num_slice_groups_minus1 */
    tbw_put_ue(&w, 0); /* num_ref_idx_l0_default_active_minus1 */
    tbw_put_ue(&w, 0); /* num_ref_idx_l1_default_active_minus1 */
    tbw_put_bit(&w, 0); /* weighted_pred_flag */
    tbw_put_bits(&w, 0, 2); /* weighted_bipred_idc */
    tbw_put_se(&w, 0); /* pic_init_qp_minus26 */
    tbw_put_se(&w, 0); /* pic_init_qs_minus26 */
    tbw_put_se(&w, 0); /* chroma_qp_index_offset */
    tbw_put_bit(&w, 0); /* deblocking_filter_control_present_flag */
    tbw_put_bit(&w, 0); /* constrained_intra_pred_flag */
    tbw_put_bit(&w, 0); /* redundant_pic_cnt_present_flag */
    tbw_rbsp_trailing_bits(&w);

    uint8_t pps_nal[64];
    size_t pps_nal_size = tbw_apply_emulation_prevention(pps_rbsp, tbw_byte_size(&w), pps_nal, sizeof(pps_nal));

    H264SPS sps_table[H264_MAX_SPS_COUNT];
    memset(sps_table, 0, sizeof(sps_table));
    sps_table[0] = sps;

    H264PPS pps;
    uint8_t pps_scratch[64];
    rc = h264_parse_pps(pps_nal, pps_nal_size, pps_scratch, sps_table, &pps);
    assert(rc == 0);
    assert(pps.valid);
    assert(pps.pic_parameter_set_id == 0);
    assert(pps.seq_parameter_set_id == 0);
    assert(pps.entropy_coding_mode_flag == 0);
    assert(pps.num_slice_groups_minus1 == 0);

    printf("test_h264_sps: OK\n");
    return 0;
}
