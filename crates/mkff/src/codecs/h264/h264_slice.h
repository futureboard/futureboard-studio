#ifndef MKFF_LINUX_H264_SLICE_H
#define MKFF_LINUX_H264_SLICE_H

#include <stdint.h>
#include "h264_bitstream.h"
#include "h264_sps_pps.h"

#define H264_SLICE_TYPE_P  0
#define H264_SLICE_TYPE_B  1
#define H264_SLICE_TYPE_I  2
#define H264_SLICE_TYPE_SP 3
#define H264_SLICE_TYPE_SI 4

typedef struct H264RefPicListMod {
    uint32_t idc;   /* modification_of_pic_nums_idc */
    uint32_t value; /* abs_diff_pic_num_minus1 or long_term_pic_num */
} H264RefPicListMod;

typedef struct H264SliceHeader {
    uint32_t first_mb_in_slice;
    uint32_t slice_type; /* already reduced modulo 5: one of H264_SLICE_TYPE_* */
    uint32_t pic_parameter_set_id;
    uint32_t frame_num;

    int      is_idr;
    uint32_t idr_pic_id;

    uint32_t pic_order_cnt_lsb;
    int32_t  delta_pic_order_cnt_bottom;
    int32_t  delta_pic_order_cnt[2];

    uint32_t redundant_pic_cnt;
    int      direct_spatial_mv_pred_flag;

    int      num_ref_idx_active_override_flag;
    uint32_t num_ref_idx_l0_active_minus1;
    uint32_t num_ref_idx_l1_active_minus1;

    H264RefPicListMod rplm_l0[32];
    int rplm_l0_count;
    H264RefPicListMod rplm_l1[32];
    int rplm_l1_count;

    int      has_pred_weight_table;
    uint32_t luma_log2_weight_denom;
    uint32_t chroma_log2_weight_denom;
    int16_t  luma_weight_l0[32], luma_offset_l0[32];
    int16_t  chroma_weight_l0[32][2], chroma_offset_l0[32][2];
    int16_t  luma_weight_l1[32], luma_offset_l1[32];
    int16_t  chroma_weight_l1[32][2], chroma_offset_l1[32][2];

    int      no_output_of_prior_pics_flag;
    int      long_term_reference_flag;
    int      adaptive_ref_pic_marking_mode_flag; /* MMCO commands are parsed for
                                                     bitstream alignment but not
                                                     fully honored: DPB management
                                                     uses sliding-window only. */

    int      cabac_init_idc;
    int32_t  slice_qp_delta;
    uint32_t disable_deblocking_filter_idc;
    int32_t  slice_alpha_c0_offset_div2;
    int32_t  slice_beta_offset_div2;

    uint8_t  nal_ref_idc;
    uint8_t  nal_unit_type;

    size_t   header_bits; /* bits consumed by NAL header + slice_header(), RBSP domain */

    int32_t  poc; /* filled in by h264_compute_poc(), not by the header parser itself */
} H264SliceHeader;

/* Returns 0 on success, -1 on malformed bitstream, -2 for a syntax
 * element outside this milestone's supported subset (SP/SI slices,
 * interlaced field pictures). `sps_table`/`pps_table` must be indexed by
 * id (H264_MAX_SPS_COUNT / H264_MAX_PPS_COUNT entries) and kept current
 * by the caller as SPS/PPS NALs are parsed. */
int h264_parse_slice_header(const H264NalUnit *nal,
                             uint8_t *rbsp_scratch,
                             const H264SPS *sps_table,
                             const H264PPS *pps_table,
                             H264SliceHeader *out_sh);

typedef struct H264PocState {
    int32_t  prev_poc_msb;
    int32_t  prev_poc_lsb;
    uint32_t prev_frame_num_offset;
    uint32_t prev_frame_num;
} H264PocState;

/* Implements the POC derivation processes of spec clause 8.2.1 for types
 * 0, 1 and 2, restricted to frame (non-field) pictures. Mutates `state`
 * per the reference-picture update rules. Returns the frame's picture
 * order count. */
int32_t h264_compute_poc(const H264SPS *sps, const H264SliceHeader *sh, H264PocState *state);

#endif /* MKFF_LINUX_H264_SLICE_H */
