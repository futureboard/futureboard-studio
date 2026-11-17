#include <assert.h>
#include <stdio.h>
#include <string.h>

#include "src/codecs/hevc/hevc_bitstream.h"

int main(void) {
    /* Annex-B: start code + VPS-like header + start code + SPS-like */
    const uint8_t annex_b[] = {
        0x00, 0x00, 0x00, 0x01,
        0x40, 0x01, 0x0C, 0x01, /* VPS type 32 */
        0x00, 0x00, 0x01,
        0x42, 0x01, 0x01, 0x01, /* SPS type 33 */
        0x00, 0x00, 0x01,
        0x44, 0x01,             /* PPS type 34 (tiny) */
    };

    HevcNalUnit units[8];
    size_t n = hevc_split_annex_b(annex_b, sizeof(annex_b), units, 8);
    assert(n == 3);
    assert(units[0].nal_unit_type == HEVC_NAL_VPS_NUT);
    assert(units[1].nal_unit_type == HEVC_NAL_SPS_NUT);
    assert(units[2].nal_unit_type == HEVC_NAL_PPS_NUT);
    assert(units[0].nuh_temporal_id_plus1 == 1);
    assert(hevc_nal_is_vcl(HEVC_NAL_TRAIL_R));
    assert(hevc_nal_is_idr(HEVC_NAL_IDR_W_RADL));
    assert(!hevc_nal_is_vcl(HEVC_NAL_SPS_NUT));

    /* Truncated / no start code */
    const uint8_t junk[] = {0x01, 0x02, 0x03};
    assert(hevc_split_annex_b(junk, sizeof(junk), units, 8) == 0);

    /* Malformed short NAL after start code ignored when size < 2 */
    const uint8_t short_nal[] = {0x00, 0x00, 0x01, 0x40};
    assert(hevc_split_annex_b(short_nal, sizeof(short_nal), units, 8) == 0);

    /* RBSP EPB removal */
    uint8_t scratch[32];
    const uint8_t with_epb[] = {0x42, 0x01, 0x00, 0x00, 0x03, 0x01};
    HevcBitReader br;
    hevc_br_init(&br, with_epb, sizeof(with_epb), scratch);
    assert(br.rbsp_size == 5);
    assert(br.rbsp[0] == 0x42);
    assert(br.rbsp[2] == 0x00);
    assert(br.rbsp[3] == 0x00);
    assert(br.rbsp[4] == 0x01);

    /* Truncated Exp-Golomb should set error */
    uint8_t tiny[] = {0x42, 0x01};
    hevc_br_init(&br, tiny, sizeof(tiny), scratch);
    hevc_br_skip_bits(&br, 16);
    (void)hevc_br_ue(&br);
    assert(br.error);

    puts("test_hevc_bitstream: ok");
    return 0;
}
