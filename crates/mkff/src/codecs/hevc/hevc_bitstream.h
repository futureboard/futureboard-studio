#ifndef MKFF_HEVC_BITSTREAM_H
#define MKFF_HEVC_BITSTREAM_H

#include <stddef.h>
#include <stdint.h>

/* One NAL unit found in an Annex-B byte stream. `data` points at the
 * first byte of the 2-byte HEVC NAL header (forbidden_zero_bit ...). */
typedef struct HevcNalUnit {
    const uint8_t *data;
    size_t         size;
    uint8_t        nal_unit_type; /* 6-bit */
    uint8_t        nuh_layer_id;  /* 6-bit */
    uint8_t        nuh_temporal_id_plus1; /* 3-bit */
} HevcNalUnit;

#define HEVC_MAX_NAL_UNITS_PER_AU 64

/* HEVC NAL unit types (ITU-T H.265 Table 7-1), subset we care about. */
#define HEVC_NAL_TRAIL_N    0
#define HEVC_NAL_TRAIL_R    1
#define HEVC_NAL_TSA_N      2
#define HEVC_NAL_TSA_R      3
#define HEVC_NAL_STSA_N     4
#define HEVC_NAL_STSA_R     5
#define HEVC_NAL_RADL_N     6
#define HEVC_NAL_RADL_R     7
#define HEVC_NAL_RASL_N     8
#define HEVC_NAL_RASL_R     9
#define HEVC_NAL_BLA_W_LP   16
#define HEVC_NAL_BLA_W_RADL 17
#define HEVC_NAL_BLA_N_LP   18
#define HEVC_NAL_IDR_W_RADL 19
#define HEVC_NAL_IDR_N_LP   20
#define HEVC_NAL_CRA_NUT    21
#define HEVC_NAL_VPS_NUT    32
#define HEVC_NAL_SPS_NUT    33
#define HEVC_NAL_PPS_NUT    34
#define HEVC_NAL_AUD_NUT    35
#define HEVC_NAL_EOS_NUT    36
#define HEVC_NAL_EOB_NUT    37
#define HEVC_NAL_FD_NUT     38
#define HEVC_NAL_PREFIX_SEI 39
#define HEVC_NAL_SUFFIX_SEI 40

static inline int hevc_nal_is_vcl(uint8_t nal_unit_type) {
    return (nal_unit_type <= 9) || (nal_unit_type >= 16 && nal_unit_type <= 21);
}

static inline int hevc_nal_is_idr(uint8_t nal_unit_type) {
    return nal_unit_type == HEVC_NAL_IDR_W_RADL || nal_unit_type == HEVC_NAL_IDR_N_LP;
}

static inline int hevc_nal_is_irap(uint8_t nal_unit_type) {
    return nal_unit_type >= 16 && nal_unit_type <= 23;
}

size_t hevc_split_annex_b(const uint8_t *data, size_t size, HevcNalUnit *out_units, size_t capacity);

typedef struct HevcBitReader {
    const uint8_t *rbsp;
    size_t         rbsp_size;
    size_t         bit_pos;
    int            error;
} HevcBitReader;

/* `nal_data` includes the 2-byte NAL header. Bit position starts at 0. */
void hevc_br_init(HevcBitReader *br, const uint8_t *nal_data, size_t nal_size, uint8_t *rbsp_scratch);

uint32_t hevc_br_u(HevcBitReader *br, int num_bits);
uint32_t hevc_br_ue(HevcBitReader *br);
int32_t  hevc_br_se(HevcBitReader *br);
int      hevc_br_flag(HevcBitReader *br);
void     hevc_br_skip_bits(HevcBitReader *br, int num_bits);
size_t   hevc_br_bits_consumed(const HevcBitReader *br);
int      hevc_br_more_rbsp_data(const HevcBitReader *br);
int      hevc_br_byte_aligned(const HevcBitReader *br);

#endif /* MKFF_HEVC_BITSTREAM_H */
