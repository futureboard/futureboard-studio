#include <assert.h>
#include <stdio.h>
#include <string.h>

#include "h264_bitwriter.h"
#include "src/codecs/hevc/hevc_vps_sps_pps.h"

/* Synthetic Main 8-bit 4:2:0 SPS (64x64, minimal). */
static size_t build_main_sps(uint8_t *out, size_t out_capacity) {
    uint8_t rbsp[256];
    TestBitWriter w;
    tbw_init(&w, rbsp, sizeof(rbsp));

    /* NAL header: type=33 (SPS), layer=0, tid=1 → 0x4201 */
    tbw_put_bits(&w, 0x4201, 16);

    tbw_put_bits(&w, 0, 4); /* sps_video_parameter_set_id */
    tbw_put_bits(&w, 0, 3); /* sps_max_sub_layers_minus1 */
    tbw_put_bit(&w, 1);     /* sps_temporal_id_nesting_flag */

    /* profile_tier_level */
    tbw_put_bits(&w, 0, 2);  /* general_profile_space */
    tbw_put_bit(&w, 0);      /* general_tier_flag */
    tbw_put_bits(&w, 1, 5);  /* general_profile_idc = Main */
    tbw_put_bits(&w, (1u << 1), 32); /* compatibility: Main */
    tbw_put_bit(&w, 1);      /* progressive */
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 1);      /* frame_only */
    tbw_put_bits(&w, 0, 44);
    tbw_put_bits(&w, 120, 8); /* level 4.0 */

    tbw_put_ue(&w, 0); /* sps_seq_parameter_set_id */
    tbw_put_ue(&w, 1); /* chroma_format_idc = 4:2:0 */
    tbw_put_ue(&w, 64); /* width */
    tbw_put_ue(&w, 64); /* height */
    tbw_put_bit(&w, 0); /* conformance_window_flag */
    tbw_put_ue(&w, 0); /* bit_depth_luma_minus8 */
    tbw_put_ue(&w, 0); /* bit_depth_chroma_minus8 */
    tbw_put_ue(&w, 4); /* log2_max_pic_order_cnt_lsb_minus4 → 8 */
    tbw_put_bit(&w, 1); /* sps_sub_layer_ordering_info_present_flag */
    tbw_put_ue(&w, 1); /* max_dec_pic_buffering_minus1 */
    tbw_put_ue(&w, 0); /* max_num_reorder_pics */
    tbw_put_ue(&w, 0); /* max_latency_increase_plus1 */

    tbw_put_ue(&w, 0); /* log2_min_luma_coding_block_size_minus3 → 3 (8) */
    tbw_put_ue(&w, 3); /* log2_diff_max_min → CTB 64 */
    tbw_put_ue(&w, 0); /* log2_min_transform_block_size_minus2 */
    tbw_put_ue(&w, 3); /* log2_diff_max_min_transform */
    tbw_put_ue(&w, 0); /* max_transform_hierarchy_depth_inter */
    tbw_put_ue(&w, 0); /* max_transform_hierarchy_depth_intra */

    tbw_put_bit(&w, 0); /* scaling_list_enabled */
    tbw_put_bit(&w, 0); /* amp */
    tbw_put_bit(&w, 0); /* sao */
    tbw_put_bit(&w, 0); /* pcm */
    tbw_put_ue(&w, 0); /* num_short_term_ref_pic_sets */
    tbw_put_bit(&w, 0); /* long_term_ref_pics_present */
    tbw_put_bit(&w, 0); /* temporal_mvp */
    tbw_put_bit(&w, 0); /* strong_intra_smoothing */
    tbw_put_bit(&w, 0); /* vui */
    tbw_rbsp_trailing_bits(&w);

    return tbw_apply_emulation_prevention(rbsp, tbw_byte_size(&w), out, out_capacity);
}

static size_t build_main10_sps(uint8_t *out, size_t out_capacity) {
    uint8_t rbsp[256];
    TestBitWriter w;
    tbw_init(&w, rbsp, sizeof(rbsp));

    tbw_put_bits(&w, 0x4201, 16);
    tbw_put_bits(&w, 0, 4);
    tbw_put_bits(&w, 0, 3);
    tbw_put_bit(&w, 1);

    tbw_put_bits(&w, 0, 2);
    tbw_put_bit(&w, 0);
    tbw_put_bits(&w, 2, 5); /* Main10 */
    tbw_put_bits(&w, (1u << 2), 32);
    tbw_put_bit(&w, 1);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 1);
    tbw_put_bits(&w, 0, 44);
    tbw_put_bits(&w, 120, 8);

    tbw_put_ue(&w, 0);
    tbw_put_ue(&w, 1);
    tbw_put_ue(&w, 64);
    tbw_put_ue(&w, 64);
    tbw_put_bit(&w, 0);
    tbw_put_ue(&w, 2); /* 10-bit */
    tbw_put_ue(&w, 2);
    tbw_put_ue(&w, 4);
    tbw_put_bit(&w, 1);
    tbw_put_ue(&w, 1);
    tbw_put_ue(&w, 0);
    tbw_put_ue(&w, 0);
    tbw_put_ue(&w, 0);
    tbw_put_ue(&w, 3);
    tbw_put_ue(&w, 0);
    tbw_put_ue(&w, 3);
    tbw_put_ue(&w, 0);
    tbw_put_ue(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_ue(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_rbsp_trailing_bits(&w);

    return tbw_apply_emulation_prevention(rbsp, tbw_byte_size(&w), out, out_capacity);
}

static size_t build_422_sps(uint8_t *out, size_t out_capacity) {
    uint8_t rbsp[256];
    TestBitWriter w;
    tbw_init(&w, rbsp, sizeof(rbsp));

    tbw_put_bits(&w, 0x4201, 16);
    tbw_put_bits(&w, 0, 4);
    tbw_put_bits(&w, 0, 3);
    tbw_put_bit(&w, 1);
    tbw_put_bits(&w, 0, 2);
    tbw_put_bit(&w, 0);
    tbw_put_bits(&w, 1, 5);
    tbw_put_bits(&w, (1u << 1), 32);
    tbw_put_bit(&w, 1);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 1);
    tbw_put_bits(&w, 0, 44);
    tbw_put_bits(&w, 120, 8);
    tbw_put_ue(&w, 0);
    tbw_put_ue(&w, 2); /* 4:2:2 — rejected */
    tbw_put_ue(&w, 64);
    tbw_put_ue(&w, 64);
    tbw_put_bit(&w, 0);
    tbw_put_ue(&w, 0);
    tbw_put_ue(&w, 0);
    tbw_put_ue(&w, 4);
    tbw_put_bit(&w, 1);
    tbw_put_ue(&w, 1);
    tbw_put_ue(&w, 0);
    tbw_put_ue(&w, 0);
    tbw_put_ue(&w, 0);
    tbw_put_ue(&w, 3);
    tbw_put_ue(&w, 0);
    tbw_put_ue(&w, 3);
    tbw_put_ue(&w, 0);
    tbw_put_ue(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_ue(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_rbsp_trailing_bits(&w);

    return tbw_apply_emulation_prevention(rbsp, tbw_byte_size(&w), out, out_capacity);
}

static size_t build_vps(uint8_t *out, size_t out_capacity) {
    uint8_t rbsp[128];
    TestBitWriter w;
    tbw_init(&w, rbsp, sizeof(rbsp));
    tbw_put_bits(&w, 0x4001, 16); /* VPS NAL */
    tbw_put_bits(&w, 0, 4); /* vps_id */
    tbw_put_bit(&w, 1);
    tbw_put_bit(&w, 1);
    tbw_put_bits(&w, 0, 6);
    tbw_put_bits(&w, 0, 3);
    tbw_put_bit(&w, 1);
    tbw_put_bits(&w, 0xFFFF, 16);
    tbw_put_bits(&w, 0, 2);
    tbw_put_bit(&w, 0);
    tbw_put_bits(&w, 1, 5);
    tbw_put_bits(&w, (1u << 1), 32);
    tbw_put_bit(&w, 1);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 1);
    tbw_put_bits(&w, 0, 44);
    tbw_put_bits(&w, 120, 8);
    tbw_rbsp_trailing_bits(&w);
    return tbw_apply_emulation_prevention(rbsp, tbw_byte_size(&w), out, out_capacity);
}

static size_t build_pps(uint8_t *out, size_t out_capacity) {
    uint8_t rbsp[64];
    TestBitWriter w;
    tbw_init(&w, rbsp, sizeof(rbsp));
    tbw_put_bits(&w, 0x4401, 16); /* PPS */
    tbw_put_ue(&w, 0);
    tbw_put_ue(&w, 0);
    tbw_put_bit(&w, 0); /* dependent_slice */
    tbw_put_bit(&w, 0); /* output_flag_present */
    tbw_put_bits(&w, 0, 3);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_ue(&w, 0);
    tbw_put_ue(&w, 0);
    tbw_put_se(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_se(&w, 0);
    tbw_put_se(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 0);
    tbw_put_bit(&w, 1); /* loop_filter_across_slices */
    tbw_put_bit(&w, 0); /* deblocking_filter_control_present */
    tbw_put_bit(&w, 0); /* scaling_list */
    tbw_put_bit(&w, 0); /* lists_modification */
    tbw_put_ue(&w, 0); /* log2_parallel_merge_level_minus2 */
    tbw_put_bit(&w, 0); /* slice_segment_header_extension */
    tbw_rbsp_trailing_bits(&w);
    return tbw_apply_emulation_prevention(rbsp, tbw_byte_size(&w), out, out_capacity);
}

int main(void) {
    uint8_t nal[512];
    uint8_t scratch[512];

    size_t n = build_main_sps(nal, sizeof(nal));
    HevcSPS sps;
    int rc = hevc_parse_sps(nal, n, scratch, &sps);
    assert(rc == 0);
    assert(sps.valid);
    assert(sps.ptl.general_profile_idc == 1);
    assert(sps.chroma_format_idc == 1);
    assert(sps.bit_depth == 8);
    assert(sps.pic_width_in_luma_samples == 64);
    assert(sps.pic_height_in_luma_samples == 64);
    assert(sps.log2_max_pic_order_cnt_lsb == 8);

    n = build_main10_sps(nal, sizeof(nal));
    rc = hevc_parse_sps(nal, n, scratch, &sps);
    assert(rc == 0);
    assert(sps.bit_depth == 10);
    assert(sps.ptl.general_profile_idc == 2);

    n = build_422_sps(nal, sizeof(nal));
    rc = hevc_parse_sps(nal, n, scratch, &sps);
    assert(rc == -2);

    HevcVPS vps;
    n = build_vps(nal, sizeof(nal));
    rc = hevc_parse_vps(nal, n, scratch, &vps);
    assert(rc == 0);
    assert(vps.valid);

    HevcSPS sps_table[HEVC_MAX_SPS_COUNT];
    memset(sps_table, 0, sizeof(sps_table));
    n = build_main_sps(nal, sizeof(nal));
    assert(hevc_parse_sps(nal, n, scratch, &sps_table[0]) == 0);

    HevcPPS pps;
    n = build_pps(nal, sizeof(nal));
    rc = hevc_parse_pps(nal, n, scratch, sps_table, &pps);
    assert(rc == 0);
    assert(pps.valid);
    assert(pps.pps_pic_parameter_set_id == 0);

    puts("test_hevc_sps: ok");
    return 0;
}
