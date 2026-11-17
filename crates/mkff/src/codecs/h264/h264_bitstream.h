#ifndef MKFF_LINUX_H264_BITSTREAM_H
#define MKFF_LINUX_H264_BITSTREAM_H

#include <stddef.h>
#include <stdint.h>

/* One NAL unit found in an Annex-B byte stream, still in its original
 * (emulation-prevention-byte-intact) form. `data` points into the
 * caller-owned Annex-B buffer; it is not copied. */
typedef struct H264NalUnit {
    const uint8_t *data; /* points at the NAL header byte (forbidden_zero_bit ... ) */
    size_t         size; /* length of `data`, NAL header included, next start code excluded */
    uint8_t        nal_ref_idc;
    uint8_t        nal_unit_type;
} H264NalUnit;

#define H264_MAX_NAL_UNITS_PER_AU 32

/* Splits an Annex-B buffer (0x000001 / 0x00000001 start codes) into NAL
 * units. Returns the number of NAL units found (possibly 0), truncated
 * to `capacity`. */
size_t h264_split_annex_b(const uint8_t *data, size_t size, H264NalUnit *out_units, size_t capacity);

/*
 * Bit reader operating over the RBSP (emulation-prevention-byte-removed)
 * form of a NAL unit. Construction removes 0x000003 escape sequences
 * once, up front, into an internal scratch buffer.
 *
 * bits_consumed() tracks position in the RBSP domain, which is exactly
 * what VASliceParameterBufferH264::slice_data_bit_offset expects per the
 * VA-API contract (counted after emulation-prevention removal, even
 * though the buffer handed to hardware still contains the original,
 * un-escaped bytes).
 */
typedef struct H264BitReader {
    const uint8_t *rbsp;
    size_t         rbsp_size; /* bytes */
    size_t         bit_pos;   /* next bit to read, MSB-first within each byte */
    int            error;     /* set once a read runs past the end */
} H264BitReader;

/* `rbsp_scratch` must be at least `nal_size` bytes; the reader will use
 * at most that many bytes of it. `nal_data` includes the NAL header
 * byte. The reader's bit position starts at 0 (start of the NAL header
 * byte) so callers that want to skip the NAL header call
 * h264_br_skip_bits(br, 8) first (slice_data_bit_offset is defined
 * relative to and including the NAL header byte). */
void h264_br_init(H264BitReader *br, const uint8_t *nal_data, size_t nal_size, uint8_t *rbsp_scratch);

uint32_t h264_br_u(H264BitReader *br, int num_bits);
uint32_t h264_br_ue(H264BitReader *br); /* unsigned Exp-Golomb */
int32_t  h264_br_se(H264BitReader *br); /* signed Exp-Golomb */
int      h264_br_flag(H264BitReader *br);
void     h264_br_skip_bits(H264BitReader *br, int num_bits);
size_t   h264_br_bits_consumed(const H264BitReader *br);
int      h264_br_more_rbsp_data(const H264BitReader *br);
int      h264_br_byte_aligned(const H264BitReader *br);

#endif /* MKFF_LINUX_H264_BITSTREAM_H */
