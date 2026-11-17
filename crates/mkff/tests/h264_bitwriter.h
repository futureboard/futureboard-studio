#ifndef MKFF_TEST_H264_BITWRITER_H
#define MKFF_TEST_H264_BITWRITER_H

/* Minimal MSB-first bit writer + Exp-Golomb encoder used only by tests,
 * to construct synthetic H.264 RBSP/EBSP payloads without depending on
 * any external encoder. */

#include <stddef.h>
#include <stdint.h>
#include <string.h>

typedef struct TestBitWriter {
    uint8_t *buf;
    size_t   capacity;
    size_t   bit_pos;
} TestBitWriter;

static inline void tbw_init(TestBitWriter *w, uint8_t *buf, size_t capacity) {
    w->buf = buf;
    w->capacity = capacity;
    w->bit_pos = 0;
    memset(buf, 0, capacity);
}

static inline void tbw_put_bit(TestBitWriter *w, int bit) {
    size_t byte_i = w->bit_pos / 8;
    int bit_i = 7 - (int)(w->bit_pos % 8);
    if (byte_i < w->capacity && bit) {
        w->buf[byte_i] |= (uint8_t)(1u << bit_i);
    }
    w->bit_pos++;
}

static inline void tbw_put_bits(TestBitWriter *w, uint32_t value, int n) {
    for (int i = n - 1; i >= 0; i--) {
        tbw_put_bit(w, (int)((value >> i) & 1u));
    }
}

static inline int tbw_bit_length_for_codenum(uint32_t code_num) {
    uint32_t v = code_num + 1;
    int leading_zero_bits = 0;
    while (v > 1) {
        v >>= 1;
        leading_zero_bits++;
    }
    return 2 * leading_zero_bits + 1;
}

static inline void tbw_put_ue(TestBitWriter *w, uint32_t code_num) {
    uint32_t v = code_num + 1;
    int leading_zero_bits = 0;
    while ((v >> (leading_zero_bits + 1)) != 0) {
        leading_zero_bits++;
    }
    for (int i = 0; i < leading_zero_bits; i++) {
        tbw_put_bit(w, 0);
    }
    tbw_put_bits(w, v, leading_zero_bits + 1);
}

static inline void tbw_put_se(TestBitWriter *w, int32_t value) {
    uint32_t code_num;
    if (value <= 0) {
        code_num = (uint32_t)(-2 * value);
    } else {
        code_num = (uint32_t)(2 * value - 1);
    }
    tbw_put_ue(w, code_num);
}

static inline void tbw_rbsp_trailing_bits(TestBitWriter *w) {
    tbw_put_bit(w, 1);
    while (w->bit_pos % 8 != 0) {
        tbw_put_bit(w, 0);
    }
}

static inline size_t tbw_byte_size(const TestBitWriter *w) {
    return (w->bit_pos + 7) / 8;
}

/* Applies H.264 emulation prevention (0x000003 insertion before any
 * 0x00,0x01,0x02,0x03 that would otherwise follow two zero bytes),
 * turning an RBSP into a valid EBSP/NAL payload. `out` must be at least
 * `in_len` + in_len/2 + 4 bytes. */
static inline size_t tbw_apply_emulation_prevention(const uint8_t *in, size_t in_len, uint8_t *out, size_t out_capacity) {
    size_t zero_run = 0;
    size_t out_len = 0;
    for (size_t i = 0; i < in_len; i++) {
        if (zero_run >= 2 && in[i] <= 0x03) {
            if (out_len >= out_capacity) break;
            out[out_len++] = 0x03;
            zero_run = 0;
        }
        if (out_len >= out_capacity) break;
        out[out_len++] = in[i];
        zero_run = (in[i] == 0) ? zero_run + 1 : 0;
    }
    return out_len;
}

#endif /* MKFF_TEST_H264_BITWRITER_H */
