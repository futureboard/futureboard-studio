#include <assert.h>
#include <stdio.h>
#include <string.h>

#include "h264_bitwriter.h"
#include "src/codecs/hevc/hevc_slice.h"

static void make_minimal_sps_pps(HevcSPS *sps, HevcPPS *pps) {
    memset(sps, 0, sizeof(*sps));
    memset(pps, 0, sizeof(*pps));
    sps->valid = 1;
    sps->chroma_format_idc = 1;
    sps->bit_depth = 8;
    sps->pic_width_in_luma_samples = 64;
    sps->pic_height_in_luma_samples = 64;
    sps->log2_max_pic_order_cnt_lsb = 4; /* MaxPocLsb = 16 */
    sps->log2_min_luma_coding_block_size_minus3 = 0;
    sps->log2_diff_max_min_luma_coding_block_size = 3;
    sps->ctb_log2_size_y = 6;
    sps->pic_width_in_ctbs_y = 1;
    sps->pic_height_in_ctbs_y = 1;
    sps->num_short_term_ref_pic_sets = 0;

    pps->valid = 1;
    pps->pps_pic_parameter_set_id = 0;
    pps->pps_seq_parameter_set_id = 0;
}

static size_t build_idr_slice(uint8_t *out, size_t cap) {
    uint8_t rbsp[64];
    TestBitWriter w;
    tbw_init(&w, rbsp, sizeof(rbsp));
    /* IDR_W_RADL = 19 → nal header 0x2601 */
    tbw_put_bits(&w, 0x2601, 16);
    tbw_put_bit(&w, 1); /* first_slice_segment_in_pic */
    tbw_put_bit(&w, 0); /* no_output_of_prior_pics */
    tbw_put_ue(&w, 0);  /* pps_id */
    tbw_put_ue(&w, 2);  /* slice_type = I */
    tbw_put_se(&w, 0);  /* slice_qp_delta */
    tbw_rbsp_trailing_bits(&w);
    return tbw_apply_emulation_prevention(rbsp, tbw_byte_size(&w), out, cap);
}

static size_t build_trail_slice(uint8_t *out, size_t cap, uint32_t poc_lsb) {
    uint8_t rbsp[64];
    TestBitWriter w;
    tbw_init(&w, rbsp, sizeof(rbsp));
    /* TRAIL_R = 1 → 0x0201 */
    tbw_put_bits(&w, 0x0201, 16);
    tbw_put_bit(&w, 1);
    tbw_put_ue(&w, 0);
    tbw_put_ue(&w, 2); /* I slice for simplicity */
    tbw_put_bits(&w, poc_lsb, 4);
    tbw_put_bit(&w, 1); /* short_term_ref_pic_set_sps_flag; num_st_rps=0 → empty */
    tbw_put_se(&w, 0);
    tbw_rbsp_trailing_bits(&w);
    return tbw_apply_emulation_prevention(rbsp, tbw_byte_size(&w), out, cap);
}

int main(void) {
    HevcSPS sps_table[HEVC_MAX_SPS_COUNT];
    HevcPPS pps_table[HEVC_MAX_PPS_COUNT];
    memset(sps_table, 0, sizeof(sps_table));
    memset(pps_table, 0, sizeof(pps_table));
    make_minimal_sps_pps(&sps_table[0], &pps_table[0]);

    uint8_t nal[128];
    uint8_t scratch[128];
    HevcNalUnit unit;
    HevcSliceHeader sh;
    HevcPocState poc_state;
    memset(&poc_state, 0, sizeof(poc_state));

    size_t n = build_idr_slice(nal, sizeof(nal));
    unit.data = nal;
    unit.size = n;
    unit.nal_unit_type = HEVC_NAL_IDR_W_RADL;
    unit.nuh_layer_id = 0;
    unit.nuh_temporal_id_plus1 = 1;
    assert(hevc_parse_slice_header(&unit, scratch, sps_table, pps_table, &sh) == 0);
    assert(sh.slice_type == HEVC_SLICE_TYPE_I);
    int32_t poc = hevc_compute_poc(&sps_table[0], &sh, &poc_state);
    assert(poc == 0);

    n = build_trail_slice(nal, sizeof(nal), 1);
    unit.data = nal;
    unit.size = n;
    unit.nal_unit_type = HEVC_NAL_TRAIL_R;
    assert(hevc_parse_slice_header(&unit, scratch, sps_table, pps_table, &sh) == 0);
    sh.poc = hevc_compute_poc(&sps_table[0], &sh, &poc_state);
    assert(sh.poc == 1);

    n = build_trail_slice(nal, sizeof(nal), 2);
    unit.data = nal;
    unit.size = n;
    assert(hevc_parse_slice_header(&unit, scratch, sps_table, pps_table, &sh) == 0);
    sh.poc = hevc_compute_poc(&sps_table[0], &sh, &poc_state);
    assert(sh.poc == 2);

    /* Wrap-around edge: prev=15, next=0 → POC 16 */
    poc_state.prev_tid0_pic_poc = 15;
    n = build_trail_slice(nal, sizeof(nal), 0);
    unit.data = nal;
    unit.size = n;
    assert(hevc_parse_slice_header(&unit, scratch, sps_table, pps_table, &sh) == 0);
    sh.poc = hevc_compute_poc(&sps_table[0], &sh, &poc_state);
    assert(sh.poc == 16);

    puts("test_hevc_poc: ok");
    return 0;
}
