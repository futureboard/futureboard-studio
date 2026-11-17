#include "hevc_bitstream.h"

static long find_start_code(const uint8_t *data, size_t size, size_t from) {
    if (size < 3) {
        return -1;
    }
    for (size_t i = from; i + 3 <= size; i++) {
        if (data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1) {
            return (long)i;
        }
    }
    return -1;
}

size_t hevc_split_annex_b(const uint8_t *data, size_t size, HevcNalUnit *out_units, size_t capacity) {
    size_t count = 0;
    long first = find_start_code(data, size, 0);
    if (first < 0) {
        return 0;
    }
    size_t pos = (size_t)first + 3;

    while (pos <= size && count < capacity) {
        long next = find_start_code(data, size, pos);
        size_t end = (next < 0) ? size : (size_t)next;

        /* Skip leading zero bytes that form a 4-byte start code prefix. */
        if (end > pos && data[pos] == 0 && (end - pos) >= 1) {
            /* NAL payload begins at pos; for 0x00000001 the find already
             * landed on the last three zeros+1 of a prior start, so pos
             * points at NAL header. */
        }

        if (end > pos + 1) {
            HevcNalUnit *u = &out_units[count];
            u->data = data + pos;
            u->size = end - pos;
            uint16_t hdr = (uint16_t)((data[pos] << 8) | data[pos + 1]);
            u->nal_unit_type = (uint8_t)((hdr >> 9) & 0x3F);
            u->nuh_layer_id = (uint8_t)((hdr >> 3) & 0x3F);
            u->nuh_temporal_id_plus1 = (uint8_t)(hdr & 0x7);
            count++;
        }

        if (next < 0) {
            break;
        }
        pos = (size_t)next + 3;
    }

    return count;
}

void hevc_br_init(HevcBitReader *br, const uint8_t *nal_data, size_t nal_size, uint8_t *rbsp_scratch) {
    size_t out = 0;
    size_t zero_run = 0;

    for (size_t i = 0; i < nal_size; i++) {
        uint8_t b = nal_data[i];
        if (zero_run >= 2 && b == 0x03) {
            zero_run = 0;
            continue;
        }
        rbsp_scratch[out++] = b;
        zero_run = (b == 0) ? zero_run + 1 : 0;
    }

    br->rbsp = rbsp_scratch;
    br->rbsp_size = out;
    br->bit_pos = 0;
    br->error = 0;
}

uint32_t hevc_br_u(HevcBitReader *br, int num_bits) {
    uint32_t value = 0;
    for (int i = 0; i < num_bits; i++) {
        size_t byte_i = br->bit_pos / 8;
        int bit_i = 7 - (int)(br->bit_pos % 8);
        uint32_t bit;
        if (byte_i >= br->rbsp_size) {
            br->error = 1;
            bit = 0;
        } else {
            bit = (uint32_t)((br->rbsp[byte_i] >> bit_i) & 1);
        }
        value = (value << 1) | bit;
        br->bit_pos++;
    }
    return value;
}

uint32_t hevc_br_ue(HevcBitReader *br) {
    int leading_zero_bits = 0;
    while (hevc_br_u(br, 1) == 0) {
        if (br->error) {
            return 0;
        }
        leading_zero_bits++;
        if (leading_zero_bits > 31) {
            br->error = 1;
            return 0;
        }
    }
    if (leading_zero_bits == 0) {
        return 0;
    }
    uint32_t suffix = hevc_br_u(br, leading_zero_bits);
    return ((uint32_t)1 << leading_zero_bits) - 1 + suffix;
}

int32_t hevc_br_se(HevcBitReader *br) {
    uint32_t code = hevc_br_ue(br);
    int32_t magnitude = (int32_t)((code + 1) / 2);
    return (code % 2 == 0) ? -magnitude : magnitude;
}

int hevc_br_flag(HevcBitReader *br) {
    return (int)hevc_br_u(br, 1);
}

void hevc_br_skip_bits(HevcBitReader *br, int num_bits) {
    (void)hevc_br_u(br, num_bits);
}

size_t hevc_br_bits_consumed(const HevcBitReader *br) {
    return br->bit_pos;
}

int hevc_br_byte_aligned(const HevcBitReader *br) {
    return (br->bit_pos % 8) == 0;
}

int hevc_br_more_rbsp_data(const HevcBitReader *br) {
    if (br->error) {
        return 0;
    }
    size_t total_bits = br->rbsp_size * 8;
    if (br->bit_pos >= total_bits) {
        return 0;
    }

    size_t last_one_bit = total_bits;
    for (size_t byte_i = br->rbsp_size; byte_i > 0; byte_i--) {
        uint8_t b = br->rbsp[byte_i - 1];
        if (b != 0) {
            int bit_in_byte = 0;
            for (int k = 0; k < 8; k++) {
                if (b & (1 << k)) {
                    bit_in_byte = k;
                    break;
                }
            }
            last_one_bit = (byte_i - 1) * 8 + (size_t)(7 - bit_in_byte);
            break;
        }
    }

    if (last_one_bit == total_bits) {
        return 0;
    }
    return br->bit_pos < last_one_bit;
}
