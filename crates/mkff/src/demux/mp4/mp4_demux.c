#include "mkff/mp4_demux.h"

#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* ---------------------------------------------------------------------------
 * Progressive ISOBMFF demuxer (moov + mdat).
 * First vide track; sample entries avc1 (+avcC) and hvc1/hev1 (+hvcC).
 * Emits Annex-B AUs with param sets prepended on first/sync samples.
 * ------------------------------------------------------------------------- */

/* Big-endian fourcc matching mkff_mp4_rb32() on on-disk type fields. */
#define MKFF_MP4_FOURCC(a, b, c, d) \
    (((uint32_t)(uint8_t)(a) << 24) | ((uint32_t)(uint8_t)(b) << 16) | \
     ((uint32_t)(uint8_t)(c) << 8) | (uint32_t)(uint8_t)(d))

#define MKFF_MP4_BOX_FTYP MKFF_MP4_FOURCC('f', 't', 'y', 'p')
#define MKFF_MP4_BOX_MOOV MKFF_MP4_FOURCC('m', 'o', 'o', 'v')
#define MKFF_MP4_BOX_MVHD MKFF_MP4_FOURCC('m', 'v', 'h', 'd')
#define MKFF_MP4_BOX_TRAK MKFF_MP4_FOURCC('t', 'r', 'a', 'k')
#define MKFF_MP4_BOX_TKHD MKFF_MP4_FOURCC('t', 'k', 'h', 'd')
#define MKFF_MP4_BOX_MDIA MKFF_MP4_FOURCC('m', 'd', 'i', 'a')
#define MKFF_MP4_BOX_MDHD MKFF_MP4_FOURCC('m', 'd', 'h', 'd')
#define MKFF_MP4_BOX_HDLR MKFF_MP4_FOURCC('h', 'd', 'l', 'r')
#define MKFF_MP4_BOX_MINF MKFF_MP4_FOURCC('m', 'i', 'n', 'f')
#define MKFF_MP4_BOX_STBL MKFF_MP4_FOURCC('s', 't', 'b', 'l')
#define MKFF_MP4_BOX_STSD MKFF_MP4_FOURCC('s', 't', 's', 'd')
#define MKFF_MP4_BOX_STTS MKFF_MP4_FOURCC('s', 't', 't', 's')
#define MKFF_MP4_BOX_CTTS MKFF_MP4_FOURCC('c', 't', 't', 's')
#define MKFF_MP4_BOX_STSC MKFF_MP4_FOURCC('s', 't', 's', 'c')
#define MKFF_MP4_BOX_STSZ MKFF_MP4_FOURCC('s', 't', 's', 'z')
#define MKFF_MP4_BOX_STZ2 MKFF_MP4_FOURCC('s', 't', 'z', '2')
#define MKFF_MP4_BOX_STCO MKFF_MP4_FOURCC('s', 't', 'c', 'o')
#define MKFF_MP4_BOX_CO64 MKFF_MP4_FOURCC('c', 'o', '6', '4')
#define MKFF_MP4_BOX_STSS MKFF_MP4_FOURCC('s', 't', 's', 's')
#define MKFF_MP4_BOX_AVC1 MKFF_MP4_FOURCC('a', 'v', 'c', '1')
#define MKFF_MP4_BOX_HVC1 MKFF_MP4_FOURCC('h', 'v', 'c', '1')
#define MKFF_MP4_BOX_HEV1 MKFF_MP4_FOURCC('h', 'e', 'v', '1')
#define MKFF_MP4_BOX_AVCC MKFF_MP4_FOURCC('a', 'v', 'c', 'C')
#define MKFF_MP4_BOX_HVCC MKFF_MP4_FOURCC('h', 'v', 'c', 'C')
#define MKFF_MP4_BOX_MDAT MKFF_MP4_FOURCC('m', 'd', 'a', 't')
#define MKFF_MP4_HDLR_VIDE MKFF_MP4_FOURCC('v', 'i', 'd', 'e')

typedef struct MKFF_Mp4SttsEntry {
    uint32_t sample_count;
    uint32_t sample_delta;
} MKFF_Mp4SttsEntry;

typedef struct MKFF_Mp4CttsEntry {
    uint32_t sample_count;
    int32_t sample_offset;
} MKFF_Mp4CttsEntry;

typedef struct MKFF_Mp4StscEntry {
    uint32_t first_chunk;          /* 1-based */
    uint32_t samples_per_chunk;
    uint32_t sample_description_index;
} MKFF_Mp4StscEntry;

typedef struct MKFF_Mp4NalSet {
    uint8_t *data;
    size_t size;
} MKFF_Mp4NalSet;

struct MKFF_Mp4Demux {
    uint8_t *file_data;
    size_t file_size;

    MKFF_VideoCodec codec;
    uint32_t width;
    uint32_t height;
    uint32_t timescale;
    uint64_t duration;
    uint32_t sample_count;

    uint8_t nal_length_size; /* 1..4 from avcC/hvcC */

    MKFF_Mp4NalSet param_sets; /* Annex-B VPS/SPS/PPS blob for prepend */

    MKFF_Mp4SttsEntry *stts;
    uint32_t stts_count;

    MKFF_Mp4CttsEntry *ctts;
    uint32_t ctts_count;

    MKFF_Mp4StscEntry *stsc;
    uint32_t stsc_count;

    uint32_t *sample_sizes; /* length sample_count; NULL if default_size */
    uint32_t default_sample_size;

    uint64_t *chunk_offsets;
    uint32_t chunk_count;

    uint32_t *sync_samples; /* 1-based sample numbers; NULL => all sync */
    uint32_t sync_count;

    /* Cached per-sample absolute file offsets (built once). */
    uint64_t *sample_offsets;

    uint32_t next_sample;

    uint8_t *au_buf;
    size_t au_buf_cap;
    size_t au_buf_size;
};

/* ---- helpers ----------------------------------------------------------- */

static uint16_t mkff_mp4_rb16(const uint8_t *p) {
    return (uint16_t)(((uint16_t)p[0] << 8) | p[1]);
}

static uint32_t mkff_mp4_rb32(const uint8_t *p) {
    return ((uint32_t)p[0] << 24) | ((uint32_t)p[1] << 16) |
           ((uint32_t)p[2] << 8) | (uint32_t)p[3];
}

static uint64_t mkff_mp4_rb64(const uint8_t *p) {
    return ((uint64_t)mkff_mp4_rb32(p) << 32) | (uint64_t)mkff_mp4_rb32(p + 4);
}

static int mkff_mp4_in_range(size_t size, size_t off, size_t need) {
    return need <= size && off <= size - need;
}

static MKFF_Result mkff_mp4_grow(uint8_t **buf, size_t *cap, size_t need) {
    if (need <= *cap) return MKFF_RESULT_OK;
    size_t ncap = *cap ? *cap : 256;
    while (ncap < need) {
        if (ncap > (SIZE_MAX / 2)) return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
        ncap *= 2;
    }
    uint8_t *nbuf = (uint8_t *)realloc(*buf, ncap);
    if (!nbuf) return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
    *buf = nbuf;
    *cap = ncap;
    return MKFF_RESULT_OK;
}

static MKFF_Result mkff_mp4_append_annexb(uint8_t **buf, size_t *cap, size_t *len,
                                           const uint8_t *nal, size_t nal_size) {
    if (nal_size > SIZE_MAX - 4 || *len > SIZE_MAX - 4 - nal_size) {
        return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
    }
    MKFF_Result r = mkff_mp4_grow(buf, cap, *len + 4 + nal_size);
    if (r != MKFF_RESULT_OK) return r;
    (*buf)[(*len)++] = 0;
    (*buf)[(*len)++] = 0;
    (*buf)[(*len)++] = 0;
    (*buf)[(*len)++] = 1;
    memcpy(*buf + *len, nal, nal_size);
    *len += nal_size;
    return MKFF_RESULT_OK;
}

static int mkff_mp4_read_file(const char *path, uint8_t **out_data, size_t *out_size) {
    FILE *f = fopen(path, "rb");
    if (!f) return -1;
    if (fseek(f, 0, SEEK_END) != 0) {
        fclose(f);
        return -1;
    }
    long size = ftell(f);
    if (size < 0) {
        fclose(f);
        return -1;
    }
    rewind(f);
    uint8_t *buf = (uint8_t *)malloc((size_t)size > 0 ? (size_t)size : 1);
    if (!buf) {
        fclose(f);
        return -1;
    }
    size_t n = fread(buf, 1, (size_t)size, f);
    fclose(f);
    if (n != (size_t)size) {
        free(buf);
        return -1;
    }
    *out_data = buf;
    *out_size = (size_t)size;
    return 0;
}

/* ---- box walker -------------------------------------------------------- */

typedef struct MKFF_Mp4Box {
    uint32_t type;
    size_t header_size;
    size_t content_off;
    size_t content_size;
    size_t end; /* exclusive offset of box end in file */
} MKFF_Mp4Box;

static int mkff_mp4_parse_box_header(const uint8_t *data, size_t size, size_t off, MKFF_Mp4Box *out) {
    if (!mkff_mp4_in_range(size, off, 8)) return 0;
    uint32_t size32 = mkff_mp4_rb32(data + off);
    uint32_t type = mkff_mp4_rb32(data + off + 4);
    size_t header = 8;
    uint64_t box_size = size32;
    if (size32 == 1) {
        if (!mkff_mp4_in_range(size, off, 16)) return 0;
        box_size = mkff_mp4_rb64(data + off + 8);
        header = 16;
    } else if (size32 == 0) {
        box_size = (uint64_t)(size - off);
    }
    if (box_size < header) return 0;
    if (box_size > (uint64_t)(size - off)) return 0;
    out->type = type;
    out->header_size = header;
    out->content_off = off + header;
    out->content_size = (size_t)(box_size - header);
    out->end = off + (size_t)box_size;
    return 1;
}

/* ---- avcC / hvcC ------------------------------------------------------- */

static MKFF_Result mkff_mp4_parse_avcc(MKFF_Mp4Demux *d, const uint8_t *p, size_t size) {
    if (size < 7) return MKFF_RESULT_ERROR_BITSTREAM;
    d->nal_length_size = (uint8_t)((p[4] & 0x03u) + 1u);
    if (d->nal_length_size < 1 || d->nal_length_size > 4) return MKFF_RESULT_ERROR_BITSTREAM;

    uint8_t *ps = NULL;
    size_t ps_cap = 0;
    size_t ps_len = 0;
    size_t off = 5;
    uint8_t num_sps = (uint8_t)(p[off] & 0x1Fu);
    off++;
    for (uint8_t i = 0; i < num_sps; i++) {
        if (!mkff_mp4_in_range(size, off, 2)) {
            free(ps);
            return MKFF_RESULT_ERROR_BITSTREAM;
        }
        uint16_t n = mkff_mp4_rb16(p + off);
        off += 2;
        if (!mkff_mp4_in_range(size, off, n)) {
            free(ps);
            return MKFF_RESULT_ERROR_BITSTREAM;
        }
        MKFF_Result r = mkff_mp4_append_annexb(&ps, &ps_cap, &ps_len, p + off, n);
        if (r != MKFF_RESULT_OK) {
            free(ps);
            return r;
        }
        off += n;
    }
    if (!mkff_mp4_in_range(size, off, 1)) {
        free(ps);
        return MKFF_RESULT_ERROR_BITSTREAM;
    }
    uint8_t num_pps = p[off++];
    for (uint8_t i = 0; i < num_pps; i++) {
        if (!mkff_mp4_in_range(size, off, 2)) {
            free(ps);
            return MKFF_RESULT_ERROR_BITSTREAM;
        }
        uint16_t n = mkff_mp4_rb16(p + off);
        off += 2;
        if (!mkff_mp4_in_range(size, off, n)) {
            free(ps);
            return MKFF_RESULT_ERROR_BITSTREAM;
        }
        MKFF_Result r = mkff_mp4_append_annexb(&ps, &ps_cap, &ps_len, p + off, n);
        if (r != MKFF_RESULT_OK) {
            free(ps);
            return r;
        }
        off += n;
    }
    d->param_sets.data = ps;
    d->param_sets.size = ps_len;
    return MKFF_RESULT_OK;
}

static MKFF_Result mkff_mp4_parse_hvcc(MKFF_Mp4Demux *d, const uint8_t *p, size_t size) {
    /* ISO/IEC 14496-15 HEVCDecoderConfigurationRecord */
    if (size < 23) return MKFF_RESULT_ERROR_BITSTREAM;
    d->nal_length_size = (uint8_t)((p[21] & 0x03u) + 1u);
    if (d->nal_length_size < 1 || d->nal_length_size > 4) return MKFF_RESULT_ERROR_BITSTREAM;

    uint8_t num_arrays = p[22];
    size_t off = 23;

    uint8_t *ps = NULL;
    size_t ps_cap = 0;
    size_t ps_len = 0;

    for (uint8_t a = 0; a < num_arrays; a++) {
        if (!mkff_mp4_in_range(size, off, 3)) {
            free(ps);
            return MKFF_RESULT_ERROR_BITSTREAM;
        }
        uint8_t nal_type = (uint8_t)(p[off] & 0x3Fu);
        uint16_t num_nalus = mkff_mp4_rb16(p + off + 1);
        off += 3;
        for (uint16_t i = 0; i < num_nalus; i++) {
            if (!mkff_mp4_in_range(size, off, 2)) {
                free(ps);
                return MKFF_RESULT_ERROR_BITSTREAM;
            }
            uint16_t n = mkff_mp4_rb16(p + off);
            off += 2;
            if (!mkff_mp4_in_range(size, off, n)) {
                free(ps);
                return MKFF_RESULT_ERROR_BITSTREAM;
            }
            /* Keep VPS(32)/SPS(33)/PPS(34); ignore others for prepend. */
            if (nal_type == 32 || nal_type == 33 || nal_type == 34) {
                MKFF_Result r = mkff_mp4_append_annexb(&ps, &ps_cap, &ps_len, p + off, n);
                if (r != MKFF_RESULT_OK) {
                    free(ps);
                    return r;
                }
            }
            off += n;
        }
    }
    d->param_sets.data = ps;
    d->param_sets.size = ps_len;
    return MKFF_RESULT_OK;
}

/* ---- sample tables ----------------------------------------------------- */

static MKFF_Result mkff_mp4_parse_stts(MKFF_Mp4Demux *d, const uint8_t *p, size_t size) {
    if (size < 8) return MKFF_RESULT_ERROR_BITSTREAM;
    uint32_t entry_count = mkff_mp4_rb32(p + 4);
    if (entry_count > (size - 8) / 8) return MKFF_RESULT_ERROR_BITSTREAM;
    MKFF_Mp4SttsEntry *entries = NULL;
    if (entry_count > 0) {
        entries = (MKFF_Mp4SttsEntry *)calloc(entry_count, sizeof(*entries));
        if (!entries) return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
        for (uint32_t i = 0; i < entry_count; i++) {
            entries[i].sample_count = mkff_mp4_rb32(p + 8 + i * 8);
            entries[i].sample_delta = mkff_mp4_rb32(p + 8 + i * 8 + 4);
        }
    }
    free(d->stts);
    d->stts = entries;
    d->stts_count = entry_count;
    return MKFF_RESULT_OK;
}

static MKFF_Result mkff_mp4_parse_ctts(MKFF_Mp4Demux *d, const uint8_t *p, size_t size) {
    if (size < 8) return MKFF_RESULT_ERROR_BITSTREAM;
    uint32_t entry_count = mkff_mp4_rb32(p + 4);
    if (entry_count > (size - 8) / 8) return MKFF_RESULT_ERROR_BITSTREAM;
    MKFF_Mp4CttsEntry *entries = NULL;
    if (entry_count > 0) {
        entries = (MKFF_Mp4CttsEntry *)calloc(entry_count, sizeof(*entries));
        if (!entries) return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
        for (uint32_t i = 0; i < entry_count; i++) {
            entries[i].sample_count = mkff_mp4_rb32(p + 8 + i * 8);
            entries[i].sample_offset = (int32_t)mkff_mp4_rb32(p + 8 + i * 8 + 4);
        }
    }
    free(d->ctts);
    d->ctts = entries;
    d->ctts_count = entry_count;
    return MKFF_RESULT_OK;
}

static MKFF_Result mkff_mp4_parse_stsc(MKFF_Mp4Demux *d, const uint8_t *p, size_t size) {
    if (size < 8) return MKFF_RESULT_ERROR_BITSTREAM;
    uint32_t entry_count = mkff_mp4_rb32(p + 4);
    if (entry_count > (size - 8) / 12) return MKFF_RESULT_ERROR_BITSTREAM;
    MKFF_Mp4StscEntry *entries = NULL;
    if (entry_count > 0) {
        entries = (MKFF_Mp4StscEntry *)calloc(entry_count, sizeof(*entries));
        if (!entries) return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
        for (uint32_t i = 0; i < entry_count; i++) {
            entries[i].first_chunk = mkff_mp4_rb32(p + 8 + i * 12);
            entries[i].samples_per_chunk = mkff_mp4_rb32(p + 8 + i * 12 + 4);
            entries[i].sample_description_index = mkff_mp4_rb32(p + 8 + i * 12 + 8);
            if (entries[i].first_chunk == 0 || entries[i].samples_per_chunk == 0) {
                free(entries);
                return MKFF_RESULT_ERROR_BITSTREAM;
            }
        }
    }
    free(d->stsc);
    d->stsc = entries;
    d->stsc_count = entry_count;
    return MKFF_RESULT_OK;
}

static MKFF_Result mkff_mp4_parse_stsz(MKFF_Mp4Demux *d, const uint8_t *p, size_t size) {
    if (size < 12) return MKFF_RESULT_ERROR_BITSTREAM;
    uint32_t sample_size = mkff_mp4_rb32(p + 4);
    uint32_t sample_count = mkff_mp4_rb32(p + 8);
    d->default_sample_size = sample_size;
    d->sample_count = sample_count;
    free(d->sample_sizes);
    d->sample_sizes = NULL;
    if (sample_size == 0) {
        if (sample_count > (size - 12) / 4) return MKFF_RESULT_ERROR_BITSTREAM;
        if (sample_count > 0) {
            d->sample_sizes = (uint32_t *)calloc(sample_count, sizeof(uint32_t));
            if (!d->sample_sizes) return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
            for (uint32_t i = 0; i < sample_count; i++) {
                d->sample_sizes[i] = mkff_mp4_rb32(p + 12 + i * 4);
            }
        }
    }
    return MKFF_RESULT_OK;
}

static MKFF_Result mkff_mp4_parse_stz2(MKFF_Mp4Demux *d, const uint8_t *p, size_t size) {
    if (size < 12) return MKFF_RESULT_ERROR_BITSTREAM;
    uint8_t field_size = p[7]; /* after version/flags(4) + reserved(3) */
    uint32_t sample_count = mkff_mp4_rb32(p + 8);
    d->default_sample_size = 0;
    d->sample_count = sample_count;
    free(d->sample_sizes);
    d->sample_sizes = NULL;
    if (sample_count == 0) return MKFF_RESULT_OK;
    d->sample_sizes = (uint32_t *)calloc(sample_count, sizeof(uint32_t));
    if (!d->sample_sizes) return MKFF_RESULT_ERROR_OUT_OF_MEMORY;

    size_t off = 12;
    if (field_size == 4) {
        for (uint32_t i = 0; i < sample_count; i++) {
            size_t byte_i = i / 2;
            if (!mkff_mp4_in_range(size, off + byte_i, 1)) return MKFF_RESULT_ERROR_BITSTREAM;
            uint8_t b = p[off + byte_i];
            d->sample_sizes[i] = (i & 1u) ? (uint32_t)(b & 0x0Fu) : (uint32_t)(b >> 4);
        }
    } else if (field_size == 8) {
        if (!mkff_mp4_in_range(size, off, sample_count)) return MKFF_RESULT_ERROR_BITSTREAM;
        for (uint32_t i = 0; i < sample_count; i++) d->sample_sizes[i] = p[off + i];
    } else if (field_size == 16) {
        if (sample_count > (size - off) / 2) return MKFF_RESULT_ERROR_BITSTREAM;
        for (uint32_t i = 0; i < sample_count; i++) d->sample_sizes[i] = mkff_mp4_rb16(p + off + i * 2);
    } else {
        return MKFF_RESULT_ERROR_NOT_SUPPORTED;
    }
    return MKFF_RESULT_OK;
}

static MKFF_Result mkff_mp4_parse_stco(MKFF_Mp4Demux *d, const uint8_t *p, size_t size, int is64) {
    if (size < 8) return MKFF_RESULT_ERROR_BITSTREAM;
    uint32_t entry_count = mkff_mp4_rb32(p + 4);
    size_t entry_bytes = is64 ? 8u : 4u;
    if (entry_count > (size - 8) / entry_bytes) return MKFF_RESULT_ERROR_BITSTREAM;
    uint64_t *offs = NULL;
    if (entry_count > 0) {
        offs = (uint64_t *)calloc(entry_count, sizeof(uint64_t));
        if (!offs) return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
        for (uint32_t i = 0; i < entry_count; i++) {
            offs[i] = is64 ? mkff_mp4_rb64(p + 8 + i * 8) : (uint64_t)mkff_mp4_rb32(p + 8 + i * 4);
        }
    }
    free(d->chunk_offsets);
    d->chunk_offsets = offs;
    d->chunk_count = entry_count;
    return MKFF_RESULT_OK;
}

static MKFF_Result mkff_mp4_parse_stss(MKFF_Mp4Demux *d, const uint8_t *p, size_t size) {
    if (size < 8) return MKFF_RESULT_ERROR_BITSTREAM;
    uint32_t entry_count = mkff_mp4_rb32(p + 4);
    if (entry_count > (size - 8) / 4) return MKFF_RESULT_ERROR_BITSTREAM;
    uint32_t *sync = NULL;
    if (entry_count > 0) {
        sync = (uint32_t *)calloc(entry_count, sizeof(uint32_t));
        if (!sync) return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
        for (uint32_t i = 0; i < entry_count; i++) {
            sync[i] = mkff_mp4_rb32(p + 8 + i * 4);
            if (sync[i] == 0) {
                free(sync);
                return MKFF_RESULT_ERROR_BITSTREAM;
            }
        }
    }
    free(d->sync_samples);
    d->sync_samples = sync;
    d->sync_count = entry_count;
    return MKFF_RESULT_OK;
}

static uint32_t mkff_mp4_sample_size(const MKFF_Mp4Demux *d, uint32_t index) {
    if (d->default_sample_size != 0) return d->default_sample_size;
    if (!d->sample_sizes || index >= d->sample_count) return 0;
    return d->sample_sizes[index];
}

static MKFF_Result mkff_mp4_build_sample_offsets(MKFF_Mp4Demux *d) {
    free(d->sample_offsets);
    d->sample_offsets = NULL;
    if (d->sample_count == 0) return MKFF_RESULT_OK;
    if (d->stsc_count == 0 || d->chunk_count == 0) return MKFF_RESULT_ERROR_BITSTREAM;

    d->sample_offsets = (uint64_t *)calloc(d->sample_count, sizeof(uint64_t));
    if (!d->sample_offsets) return MKFF_RESULT_ERROR_OUT_OF_MEMORY;

    uint32_t sample = 0;
    for (uint32_t chunk_idx = 0; chunk_idx < d->chunk_count && sample < d->sample_count; chunk_idx++) {
        uint32_t chunk_number = chunk_idx + 1; /* 1-based */
        uint32_t samples_per_chunk = 0;
        for (uint32_t e = 0; e < d->stsc_count; e++) {
            if (d->stsc[e].first_chunk > chunk_number) break;
            samples_per_chunk = d->stsc[e].samples_per_chunk;
        }
        if (samples_per_chunk == 0) {
            return MKFF_RESULT_ERROR_BITSTREAM;
        }
        uint64_t off = d->chunk_offsets[chunk_idx];
        for (uint32_t s = 0; s < samples_per_chunk && sample < d->sample_count; s++) {
            d->sample_offsets[sample] = off;
            off += (uint64_t)mkff_mp4_sample_size(d, sample);
            sample++;
        }
    }
    if (sample != d->sample_count) return MKFF_RESULT_ERROR_BITSTREAM;
    return MKFF_RESULT_OK;
}

static void mkff_mp4_sample_timing(const MKFF_Mp4Demux *d, uint32_t index,
                                   int64_t *out_dts, int64_t *out_pts) {
    int64_t dts = 0;
    uint32_t remaining = index;
    for (uint32_t i = 0; i < d->stts_count; i++) {
        uint32_t count = d->stts[i].sample_count;
        uint32_t delta = d->stts[i].sample_delta;
        if (remaining < count) {
            dts += (int64_t)remaining * (int64_t)delta;
            break;
        }
        dts += (int64_t)count * (int64_t)delta;
        remaining -= count;
    }

    int32_t cto = 0;
    if (d->ctts_count > 0) {
        remaining = index;
        for (uint32_t i = 0; i < d->ctts_count; i++) {
            if (remaining < d->ctts[i].sample_count) {
                cto = d->ctts[i].sample_offset;
                break;
            }
            remaining -= d->ctts[i].sample_count;
        }
    }
    *out_dts = dts;
    *out_pts = dts + (int64_t)cto;
}

static int mkff_mp4_is_sync(const MKFF_Mp4Demux *d, uint32_t index) {
    if (!d->sync_samples) return 1; /* no stss => all sync */
    uint32_t one_based = index + 1;
    for (uint32_t i = 0; i < d->sync_count; i++) {
        if (d->sync_samples[i] == one_based) return 1;
    }
    return 0;
}

/* ---- stsd / track ------------------------------------------------------ */

static MKFF_Result mkff_mp4_parse_sample_entry(MKFF_Mp4Demux *d, const uint8_t *data, size_t size,
                                                size_t entry_off, size_t entry_end) {
    if (entry_end < entry_off + 86) return MKFF_RESULT_ERROR_BITSTREAM;
    MKFF_Mp4Box entry;
    if (!mkff_mp4_parse_box_header(data, size, entry_off, &entry)) return MKFF_RESULT_ERROR_BITSTREAM;
    if (entry.end > entry_end) return MKFF_RESULT_ERROR_BITSTREAM;

    if (entry.type == MKFF_MP4_BOX_AVC1) {
        d->codec = MKFF_VIDEO_CODEC_H264;
    } else if (entry.type == MKFF_MP4_BOX_HVC1 || entry.type == MKFF_MP4_BOX_HEV1) {
        d->codec = MKFF_VIDEO_CODEC_HEVC;
    } else {
        return MKFF_RESULT_ERROR_NOT_SUPPORTED;
    }

    d->width = mkff_mp4_rb16(data + entry_off + 32);
    d->height = mkff_mp4_rb16(data + entry_off + 34);

    /* Walk child boxes after VisualSampleEntry fixed fields (86 bytes). */
    size_t child = entry_off + 86;
    int saw_cfg = 0;
    while (child + 8 <= entry.end) {
        MKFF_Mp4Box box;
        if (!mkff_mp4_parse_box_header(data, size, child, &box)) break;
        if (box.end > entry.end) break;
        if (box.type == MKFF_MP4_BOX_AVCC && d->codec == MKFF_VIDEO_CODEC_H264) {
            MKFF_Result r = mkff_mp4_parse_avcc(d, data + box.content_off, box.content_size);
            if (r != MKFF_RESULT_OK) return r;
            saw_cfg = 1;
        } else if (box.type == MKFF_MP4_BOX_HVCC && d->codec == MKFF_VIDEO_CODEC_HEVC) {
            MKFF_Result r = mkff_mp4_parse_hvcc(d, data + box.content_off, box.content_size);
            if (r != MKFF_RESULT_OK) return r;
            saw_cfg = 1;
        }
        child = box.end;
    }
    if (!saw_cfg) return MKFF_RESULT_ERROR_BITSTREAM;
    return MKFF_RESULT_OK;
}

static MKFF_Result mkff_mp4_parse_stsd(MKFF_Mp4Demux *d, const uint8_t *data, size_t size,
                                        size_t content_off, size_t content_size) {
    if (content_size < 8) return MKFF_RESULT_ERROR_BITSTREAM;
    const uint8_t *p = data + content_off;
    uint32_t entry_count = mkff_mp4_rb32(p + 4);
    size_t off = content_off + 8;
    size_t end = content_off + content_size;
    for (uint32_t i = 0; i < entry_count; i++) {
        MKFF_Mp4Box entry;
        if (!mkff_mp4_parse_box_header(data, size, off, &entry)) return MKFF_RESULT_ERROR_BITSTREAM;
        if (entry.end > end) return MKFF_RESULT_ERROR_BITSTREAM;
        if (entry.type == MKFF_MP4_BOX_AVC1 || entry.type == MKFF_MP4_BOX_HVC1 ||
            entry.type == MKFF_MP4_BOX_HEV1) {
            return mkff_mp4_parse_sample_entry(d, data, size, off, entry.end);
        }
        off = entry.end;
    }
    return MKFF_RESULT_ERROR_NOT_SUPPORTED;
}

static MKFF_Result mkff_mp4_parse_stbl(MKFF_Mp4Demux *d, const uint8_t *data, size_t size,
                                        size_t content_off, size_t content_size) {
    size_t off = content_off;
    size_t end = content_off + content_size;
    int have_stsd = 0, have_stts = 0, have_stsc = 0, have_size = 0, have_chunk = 0;

    while (off + 8 <= end) {
        MKFF_Mp4Box box;
        if (!mkff_mp4_parse_box_header(data, size, off, &box)) return MKFF_RESULT_ERROR_BITSTREAM;
        if (box.end > end) return MKFF_RESULT_ERROR_BITSTREAM;
        MKFF_Result r = MKFF_RESULT_OK;
        if (box.type == MKFF_MP4_BOX_STSD) {
            r = mkff_mp4_parse_stsd(d, data, size, box.content_off, box.content_size);
            have_stsd = 1;
        } else if (box.type == MKFF_MP4_BOX_STTS) {
            r = mkff_mp4_parse_stts(d, data + box.content_off, box.content_size);
            have_stts = 1;
        } else if (box.type == MKFF_MP4_BOX_CTTS) {
            r = mkff_mp4_parse_ctts(d, data + box.content_off, box.content_size);
        } else if (box.type == MKFF_MP4_BOX_STSC) {
            r = mkff_mp4_parse_stsc(d, data + box.content_off, box.content_size);
            have_stsc = 1;
        } else if (box.type == MKFF_MP4_BOX_STSZ) {
            r = mkff_mp4_parse_stsz(d, data + box.content_off, box.content_size);
            have_size = 1;
        } else if (box.type == MKFF_MP4_BOX_STZ2) {
            r = mkff_mp4_parse_stz2(d, data + box.content_off, box.content_size);
            have_size = 1;
        } else if (box.type == MKFF_MP4_BOX_STCO) {
            r = mkff_mp4_parse_stco(d, data + box.content_off, box.content_size, 0);
            have_chunk = 1;
        } else if (box.type == MKFF_MP4_BOX_CO64) {
            r = mkff_mp4_parse_stco(d, data + box.content_off, box.content_size, 1);
            have_chunk = 1;
        } else if (box.type == MKFF_MP4_BOX_STSS) {
            r = mkff_mp4_parse_stss(d, data + box.content_off, box.content_size);
        }
        if (r != MKFF_RESULT_OK) return r;
        off = box.end;
    }

    if (!have_stsd || !have_stts || !have_stsc || !have_size || !have_chunk) {
        return MKFF_RESULT_ERROR_BITSTREAM;
    }
    return mkff_mp4_build_sample_offsets(d);
}

static int mkff_mp4_hdlr_is_vide(const uint8_t *p, size_t size) {
    /* FullBox(version/flags) + pre_defined + handler_type */
    if (size < 12) return 0;
    return mkff_mp4_rb32(p + 8) == MKFF_MP4_HDLR_VIDE;
}

static MKFF_Result mkff_mp4_parse_mdhd(MKFF_Mp4Demux *d, const uint8_t *p, size_t size) {
    if (size < 4) return MKFF_RESULT_ERROR_BITSTREAM;
    uint8_t version = p[0];
    if (version == 1) {
        if (size < 32) return MKFF_RESULT_ERROR_BITSTREAM;
        d->timescale = mkff_mp4_rb32(p + 20);
        d->duration = mkff_mp4_rb64(p + 24);
    } else {
        if (size < 20) return MKFF_RESULT_ERROR_BITSTREAM;
        d->timescale = mkff_mp4_rb32(p + 12);
        d->duration = mkff_mp4_rb32(p + 16);
    }
    if (d->timescale == 0) return MKFF_RESULT_ERROR_BITSTREAM;
    return MKFF_RESULT_OK;
}

static MKFF_Result mkff_mp4_try_parse_vide_trak(MKFF_Mp4Demux *d, const uint8_t *data, size_t size,
                                                 size_t trak_content_off, size_t trak_content_size) {
    size_t off = trak_content_off;
    size_t end = trak_content_off + trak_content_size;
    size_t mdia_off = 0, mdia_size = 0;
    int have_mdia = 0;

    while (off + 8 <= end) {
        MKFF_Mp4Box box;
        if (!mkff_mp4_parse_box_header(data, size, off, &box)) return MKFF_RESULT_ERROR_BITSTREAM;
        if (box.end > end) return MKFF_RESULT_ERROR_BITSTREAM;
        if (box.type == MKFF_MP4_BOX_MDIA) {
            mdia_off = box.content_off;
            mdia_size = box.content_size;
            have_mdia = 1;
            break;
        }
        off = box.end;
    }
    if (!have_mdia) return MKFF_RESULT_ERROR_NOT_SUPPORTED;

    int is_vide = 0;
    size_t minf_off = 0, minf_size = 0;
    int have_minf = 0;
    off = mdia_off;
    end = mdia_off + mdia_size;
    while (off + 8 <= end) {
        MKFF_Mp4Box box;
        if (!mkff_mp4_parse_box_header(data, size, off, &box)) return MKFF_RESULT_ERROR_BITSTREAM;
        if (box.end > end) return MKFF_RESULT_ERROR_BITSTREAM;
        if (box.type == MKFF_MP4_BOX_MDHD) {
            MKFF_Result r = mkff_mp4_parse_mdhd(d, data + box.content_off, box.content_size);
            if (r != MKFF_RESULT_OK) return r;
        } else if (box.type == MKFF_MP4_BOX_HDLR) {
            is_vide = mkff_mp4_hdlr_is_vide(data + box.content_off, box.content_size);
        } else if (box.type == MKFF_MP4_BOX_MINF) {
            minf_off = box.content_off;
            minf_size = box.content_size;
            have_minf = 1;
        }
        off = box.end;
    }
    if (!is_vide || !have_minf) return MKFF_RESULT_ERROR_NOT_SUPPORTED;

    off = minf_off;
    end = minf_off + minf_size;
    while (off + 8 <= end) {
        MKFF_Mp4Box box;
        if (!mkff_mp4_parse_box_header(data, size, off, &box)) return MKFF_RESULT_ERROR_BITSTREAM;
        if (box.end > end) return MKFF_RESULT_ERROR_BITSTREAM;
        if (box.type == MKFF_MP4_BOX_STBL) {
            return mkff_mp4_parse_stbl(d, data, size, box.content_off, box.content_size);
        }
        off = box.end;
    }
    return MKFF_RESULT_ERROR_BITSTREAM;
}

static MKFF_Result mkff_mp4_parse_moov(MKFF_Mp4Demux *d, const uint8_t *data, size_t size,
                                        size_t content_off, size_t content_size) {
    size_t off = content_off;
    size_t end = content_off + content_size;
    while (off + 8 <= end) {
        MKFF_Mp4Box box;
        if (!mkff_mp4_parse_box_header(data, size, off, &box)) return MKFF_RESULT_ERROR_BITSTREAM;
        if (box.end > end) return MKFF_RESULT_ERROR_BITSTREAM;
        if (box.type == MKFF_MP4_BOX_TRAK) {
            /* Snapshot fields we may have partially filled on a non-vide trak. */
            MKFF_VideoCodec saved_codec = d->codec;
            uint32_t saved_w = d->width, saved_h = d->height;
            uint32_t saved_ts = d->timescale;
            uint64_t saved_dur = d->duration;
            uint32_t saved_sc = d->sample_count;

            MKFF_Result r = mkff_mp4_try_parse_vide_trak(d, data, size, box.content_off, box.content_size);
            if (r == MKFF_RESULT_OK) {
                return MKFF_RESULT_OK;
            }
            /* Non-vide / unsupported sample entry: roll back and keep scanning. */
            d->codec = saved_codec;
            d->width = saved_w;
            d->height = saved_h;
            d->timescale = saved_ts;
            d->duration = saved_dur;
            d->sample_count = saved_sc;
            if (r != MKFF_RESULT_ERROR_NOT_SUPPORTED) {
                return r;
            }
        }
        off = box.end;
    }
    return MKFF_RESULT_ERROR_NOT_SUPPORTED;
}

static MKFF_Result mkff_mp4_parse_file(MKFF_Mp4Demux *d) {
    size_t off = 0;
    int saw_moov = 0;
    int saw_mdat = 0;
    while (off + 8 <= d->file_size) {
        MKFF_Mp4Box box;
        if (!mkff_mp4_parse_box_header(d->file_data, d->file_size, off, &box)) {
            return MKFF_RESULT_ERROR_BITSTREAM;
        }
        if (box.type == MKFF_MP4_BOX_MOOV) {
            MKFF_Result r = mkff_mp4_parse_moov(d, d->file_data, d->file_size, box.content_off, box.content_size);
            if (r != MKFF_RESULT_OK) return r;
            saw_moov = 1;
        } else if (box.type == MKFF_MP4_BOX_MDAT) {
            saw_mdat = 1;
        }
        /* ftyp and other top-level boxes are ignored. */
        off = box.end;
    }
    if (!saw_moov) return MKFF_RESULT_ERROR_BITSTREAM;
    if (!saw_mdat) return MKFF_RESULT_ERROR_BITSTREAM;
    if (d->codec == MKFF_VIDEO_CODEC_UNKNOWN) return MKFF_RESULT_ERROR_NOT_SUPPORTED;
    return MKFF_RESULT_OK;
}

/* ---- AU conversion ----------------------------------------------------- */

static MKFF_Result mkff_mp4_convert_sample(MKFF_Mp4Demux *d, uint32_t index) {
    uint32_t sample_size = mkff_mp4_sample_size(d, index);
    uint64_t sample_off = d->sample_offsets[index];
    if (sample_off > (uint64_t)d->file_size ||
        (uint64_t)sample_size > (uint64_t)d->file_size - sample_off) {
        return MKFF_RESULT_ERROR_BITSTREAM;
    }

    d->au_buf_size = 0;
    int prepend = (index == 0) || mkff_mp4_is_sync(d, index);
    if (prepend && d->param_sets.size > 0) {
        MKFF_Result r = mkff_mp4_grow(&d->au_buf, &d->au_buf_cap, d->param_sets.size);
        if (r != MKFF_RESULT_OK) return r;
        memcpy(d->au_buf, d->param_sets.data, d->param_sets.size);
        d->au_buf_size = d->param_sets.size;
    }

    const uint8_t *p = d->file_data + (size_t)sample_off;
    size_t remain = sample_size;
    uint8_t llen = d->nal_length_size;
    while (remain > 0) {
        if (remain < llen) return MKFF_RESULT_ERROR_BITSTREAM;
        uint32_t nal_size = 0;
        for (uint8_t i = 0; i < llen; i++) {
            nal_size = (nal_size << 8) | p[i];
        }
        p += llen;
        remain -= llen;
        if (nal_size > remain) return MKFF_RESULT_ERROR_BITSTREAM;
        MKFF_Result r = mkff_mp4_append_annexb(&d->au_buf, &d->au_buf_cap, &d->au_buf_size, p, nal_size);
        if (r != MKFF_RESULT_OK) return r;
        p += nal_size;
        remain -= nal_size;
    }
    return MKFF_RESULT_OK;
}

/* ---- lifecycle --------------------------------------------------------- */

static void mkff_mp4_demux_free_tables(MKFF_Mp4Demux *d) {
    free(d->param_sets.data);
    d->param_sets.data = NULL;
    d->param_sets.size = 0;
    free(d->stts);
    d->stts = NULL;
    free(d->ctts);
    d->ctts = NULL;
    free(d->stsc);
    d->stsc = NULL;
    free(d->sample_sizes);
    d->sample_sizes = NULL;
    free(d->chunk_offsets);
    d->chunk_offsets = NULL;
    free(d->sync_samples);
    d->sync_samples = NULL;
    free(d->sample_offsets);
    d->sample_offsets = NULL;
    free(d->au_buf);
    d->au_buf = NULL;
    d->au_buf_cap = 0;
    d->au_buf_size = 0;
}

static MKFF_Result mkff_mp4_demux_from_owned(uint8_t *data, size_t size, MKFF_Mp4Demux **out_demux) {
    MKFF_Mp4Demux *d = (MKFF_Mp4Demux *)calloc(1, sizeof(*d));
    if (!d) {
        free(data);
        return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
    }
    d->file_data = data;
    d->file_size = size;
    d->nal_length_size = 4;

    MKFF_Result r = mkff_mp4_parse_file(d);
    if (r != MKFF_RESULT_OK) {
        mkff_mp4_demux_destroy(d);
        return r;
    }
    *out_demux = d;
    return MKFF_RESULT_OK;
}

MKFF_Result mkff_mp4_demux_open_path(const char *path, MKFF_Mp4Demux **out_demux) {
    if (!path || !out_demux) return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    *out_demux = NULL;
    uint8_t *data = NULL;
    size_t size = 0;
    if (mkff_mp4_read_file(path, &data, &size) != 0) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }
    return mkff_mp4_demux_from_owned(data, size, out_demux);
}

MKFF_Result mkff_mp4_demux_open_memory(const uint8_t *data, size_t size, MKFF_Mp4Demux **out_demux) {
    if (!data || !out_demux) return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    *out_demux = NULL;
    uint8_t *copy = (uint8_t *)malloc(size > 0 ? size : 1);
    if (!copy) return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
    if (size > 0) memcpy(copy, data, size);
    return mkff_mp4_demux_from_owned(copy, size, out_demux);
}

void mkff_mp4_demux_destroy(MKFF_Mp4Demux *demux) {
    if (!demux) return;
    mkff_mp4_demux_free_tables(demux);
    free(demux->file_data);
    free(demux);
}

MKFF_Result mkff_mp4_demux_get_video_track(const MKFF_Mp4Demux *demux, MKFF_Mp4VideoTrackInfo *out_info) {
    if (!demux || !out_info) return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    if (out_info->struct_size < sizeof(uint32_t) * 2) return MKFF_RESULT_ERROR_ABI_MISMATCH;

    uint32_t caller_size = out_info->struct_size;
    uint32_t caller_abi = out_info->abi_version;
    memset(out_info, 0, caller_size < sizeof(*out_info) ? caller_size : sizeof(*out_info));
    out_info->struct_size = caller_size;
    out_info->abi_version = caller_abi;

    if (caller_size >= offsetof(MKFF_Mp4VideoTrackInfo, sample_count) + sizeof(uint32_t)) {
        out_info->codec = demux->codec;
        out_info->width = demux->width;
        out_info->height = demux->height;
        out_info->timescale = demux->timescale;
        out_info->duration = demux->duration;
        out_info->sample_count = demux->sample_count;
    }
    return MKFF_RESULT_OK;
}

MKFF_Result mkff_mp4_demux_seek_sample(MKFF_Mp4Demux *demux, uint32_t sample_index) {
    if (!demux) return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    if (sample_index > demux->sample_count) return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    demux->next_sample = sample_index;
    return MKFF_RESULT_OK;
}

MKFF_Result mkff_mp4_demux_read_access_unit(MKFF_Mp4Demux *demux, MKFF_Mp4AccessUnit *out_au) {
    if (!demux || !out_au) return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    if (out_au->struct_size < sizeof(uint32_t) * 2) return MKFF_RESULT_ERROR_ABI_MISMATCH;
    if (demux->next_sample >= demux->sample_count) return MKFF_RESULT_END_OF_STREAM;

    uint32_t index = demux->next_sample;
    MKFF_Result r = mkff_mp4_convert_sample(demux, index);
    if (r != MKFF_RESULT_OK) return r;

    int64_t dts = 0, pts = 0;
    mkff_mp4_sample_timing(demux, index, &dts, &pts);

    uint32_t caller_size = out_au->struct_size;
    uint32_t caller_abi = out_au->abi_version;
    memset(out_au, 0, caller_size < sizeof(*out_au) ? caller_size : sizeof(*out_au));
    out_au->struct_size = caller_size;
    out_au->abi_version = caller_abi;

    if (caller_size >= offsetof(MKFF_Mp4AccessUnit, sample_index) + sizeof(uint32_t)) {
        out_au->data = demux->au_buf;
        out_au->size = demux->au_buf_size;
        out_au->pts = pts;
        out_au->dts = dts;
        out_au->sync = mkff_mp4_is_sync(demux, index) ? 1u : 0u;
        out_au->sample_index = index;
    }

    demux->next_sample = index + 1;
    return MKFF_RESULT_OK;
}
