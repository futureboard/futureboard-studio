#ifndef MKFF_HEVC_SLICE_H
#define MKFF_HEVC_SLICE_H

#include <stdint.h>

#include "hevc_bitstream.h"
#include "hevc_vps_sps_pps.h"

#define HEVC_SLICE_TYPE_B 0
#define HEVC_SLICE_TYPE_P 1
#define HEVC_SLICE_TYPE_I 2

typedef struct HevcSliceHeader {
    int      first_slice_segment_in_pic_flag;
    int      no_output_of_prior_pics_flag;
    uint32_t slice_pic_parameter_set_id;
    int      dependent_slice_segment_flag;
    uint32_t slice_segment_address;

    uint32_t slice_type;
    int      pic_output_flag;
    uint32_t colour_plane_id;

    uint32_t slice_pic_order_cnt_lsb;
    int      short_term_ref_pic_set_sps_flag;
    uint32_t short_term_ref_pic_set_idx;
    HevcShortTermRps st_rps; /* active short-term RPS for this slice */

    /* Populated when short_term_ref_pic_set_sps_flag == 0 for DXVA
     * PicParams (wNumBitsForShortTermRPSInSlice /
     * ucNumDeltaPocsOfRefRpsIdx). */
    uint16_t short_term_ref_pic_set_size; /* bit count of short_term_ref_pic_set() */
    uint8_t  num_delta_pocs_of_ref_rps_idx;

    uint32_t num_long_term_sps;
    uint32_t num_long_term_pics;

    int      slice_temporal_mvp_enabled_flag;
    int      slice_sao_luma_flag;
    int      slice_sao_chroma_flag;

    int      num_ref_idx_active_override_flag;
    uint32_t num_ref_idx_l0_active_minus1;
    uint32_t num_ref_idx_l1_active_minus1;

    int      mvd_l1_zero_flag;
    int      cabac_init_flag;
    uint32_t collocated_from_l0_flag;
    uint32_t collocated_ref_idx;
    uint32_t five_minus_max_num_merge_cand;
    int32_t  slice_qp_delta;
    int32_t  slice_cb_qp_offset;
    int32_t  slice_cr_qp_offset;

    int      slice_deblocking_filter_disabled_flag;
    int32_t  slice_beta_offset_div2;
    int32_t  slice_tc_offset_div2;
    int      slice_loop_filter_across_slices_enabled_flag;

    uint32_t num_entry_point_offsets;
    uint32_t st_rps_bits; /* RBSP bits of embedded short_term_ref_pic_set, else 0 */

    int      has_pred_weight_table;
    uint32_t luma_log2_weight_denom;
    int32_t  delta_chroma_log2_weight_denom;
    int8_t   delta_luma_weight_l0[15];
    int8_t   luma_offset_l0[15];
    int8_t   delta_chroma_weight_l0[15][2];
    int8_t   chroma_offset_l0[15][2];
    int8_t   delta_luma_weight_l1[15];
    int8_t   luma_offset_l1[15];
    int8_t   delta_chroma_weight_l1[15][2];
    int8_t   chroma_offset_l1[15][2];

    int      dependent_slice;

    uint8_t  nal_unit_type;
    uint8_t  nuh_temporal_id_plus1;

    size_t   header_bits; /* RBSP bits through end of slice segment header (incl. NAL hdr) */
    int32_t  poc;
} HevcSliceHeader;

typedef struct HevcPocState {
    int32_t prev_tid0_pic_poc;
} HevcPocState;

int hevc_parse_slice_header(const HevcNalUnit *nal,
                            uint8_t *rbsp_scratch,
                            const HevcSPS *sps_table,
                            const HevcPPS *pps_table,
                            HevcSliceHeader *out_sh);

int32_t hevc_compute_poc(const HevcSPS *sps, const HevcSliceHeader *sh, HevcPocState *state);

#endif /* MKFF_HEVC_SLICE_H */
