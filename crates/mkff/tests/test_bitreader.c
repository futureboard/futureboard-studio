#include <assert.h>
#include <stdio.h>

#include "h264_bitwriter.h"
#include "src/codecs/h264/h264_bitstream.h"

static void test_ue_known_values(void) {
    /* Table 9-2 examples: codeNum -> bit string. */
    struct {
        uint32_t code_num;
        int      expected_bits;
    } cases[] = {
        {0, 1}, {1, 3}, {2, 3}, {3, 5}, {4, 5}, {5, 5}, {6, 5}, {7, 7},
    };

    for (size_t i = 0; i < sizeof(cases) / sizeof(cases[0]); i++) {
        uint8_t raw[16];
        TestBitWriter w;
        tbw_init(&w, raw, sizeof(raw));
        tbw_put_ue(&w, cases[i].code_num);
        size_t bits_written = w.bit_pos;
        assert((int)bits_written == cases[i].expected_bits);

        uint8_t rbsp_scratch[16];
        H264BitReader br;
        h264_br_init(&br, raw, tbw_byte_size(&w), rbsp_scratch);
        uint32_t decoded = h264_br_ue(&br);
        assert(decoded == cases[i].code_num);
        assert((int)h264_br_bits_consumed(&br) == cases[i].expected_bits);
        assert(!br.error);
    }
}

static void test_ue_se_roundtrip(void) {
    uint8_t raw[256];
    TestBitWriter w;
    tbw_init(&w, raw, sizeof(raw));

    uint32_t ue_values[] = {0, 1, 5, 17, 42, 100, 255, 1000, 65535};
    int32_t se_values[] = {0, 1, -1, 2, -2, 100, -100, 32767, -32768};

    for (size_t i = 0; i < sizeof(ue_values) / sizeof(ue_values[0]); i++) {
        tbw_put_ue(&w, ue_values[i]);
    }
    for (size_t i = 0; i < sizeof(se_values) / sizeof(se_values[0]); i++) {
        tbw_put_se(&w, se_values[i]);
    }

    uint8_t rbsp_scratch[256];
    H264BitReader br;
    h264_br_init(&br, raw, tbw_byte_size(&w), rbsp_scratch);

    for (size_t i = 0; i < sizeof(ue_values) / sizeof(ue_values[0]); i++) {
        uint32_t decoded = h264_br_ue(&br);
        assert(decoded == ue_values[i]);
    }
    for (size_t i = 0; i < sizeof(se_values) / sizeof(se_values[0]); i++) {
        int32_t decoded = h264_br_se(&br);
        assert(decoded == se_values[i]);
    }
    assert(!br.error);
}

static void test_emulation_prevention_removal(void) {
    /* RBSP containing byte sequences that require emulation prevention:
     * 00 00 00 -> encoder inserts 0x03 after each 00 00 pair as needed. */
    uint8_t rbsp[] = {0xAA, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x02, 0xBB};
    uint8_t ebsp[32];
    size_t ebsp_len = tbw_apply_emulation_prevention(rbsp, sizeof(rbsp), ebsp, sizeof(ebsp));
    assert(ebsp_len > sizeof(rbsp)); /* stuffing bytes were inserted */

    uint8_t rbsp_scratch[32];
    H264BitReader br;
    h264_br_init(&br, ebsp, ebsp_len, rbsp_scratch);
    assert(br.rbsp_size == sizeof(rbsp));
    for (size_t i = 0; i < sizeof(rbsp); i++) {
        assert(br.rbsp[i] == rbsp[i]);
    }
}

static void test_more_rbsp_data(void) {
    uint8_t raw[8];
    TestBitWriter w;
    tbw_init(&w, raw, sizeof(raw));
    tbw_put_bits(&w, 5, 4);   /* some payload bits */
    tbw_rbsp_trailing_bits(&w);

    uint8_t rbsp_scratch[8];
    H264BitReader br;
    h264_br_init(&br, raw, tbw_byte_size(&w), rbsp_scratch);

    assert(h264_br_more_rbsp_data(&br));
    uint32_t payload = h264_br_u(&br, 4);
    assert(payload == 5);
    assert(!h264_br_more_rbsp_data(&br));
}

int main(void) {
    test_ue_known_values();
    test_ue_se_roundtrip();
    test_emulation_prevention_removal();
    test_more_rbsp_data();
    printf("test_bitreader: OK\n");
    return 0;
}
