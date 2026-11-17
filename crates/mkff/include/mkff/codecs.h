#ifndef MKFF_CODECS_H
#define MKFF_CODECS_H

/*
 * MKFF software codecs (libmkff_swcodecs) — pure-Rust demux + decode of
 * every supported container and codec, with no FFmpeg (or any other
 * native codec library) underneath. Built from bindings/rust/mkff-codecs-capi
 * with Cargo; independent of libmkff (link either or both).
 *
 *   Containers: MP4/MOV/3GP, Matroska/WebM, AVI, MXF, MPEG-TS/M2TS,
 *               MPEG-PS/VOB, Ogg, YUV4MPEG2, standalone audio files.
 *   Video:      H.264, HEVC Main/Main10, VP8, VP9, AV1, MPEG-1/2,
 *               ProRes, DNxHD/DNxHR, APV, Motion JPEG.
 *   Audio:      AAC, Opus, AC-3/E-AC-3, MPEG audio I/II/III, FLAC,
 *               ALAC, Vorbis, PCM.
 *
 * Two levels:
 *   - MKFF_SwMedia: open a file, query it, read frame N / audio ranges
 *     (GOP-aware random access).
 *   - MKFF_SwVideoDecoder / MKFF_SwAudioDecoder: packet decoders for
 *     elementary streams (e.g. Annex-B AUs from mkff_mp4_demux).
 *
 * All functions are thread-safe on distinct objects; an MKFF_SwMedia may
 * also be shared between threads. Strings are UTF-8. On failure,
 * mkff_sw_last_error() describes the most recent error on this thread.
 */

#include <stddef.h>
#include <stdint.h>

#include "mkff/abi.h"
#include "mkff/error.h"
#include "mkff/types.h"

#if defined(_WIN32) && !defined(MKFF_CODECS_STATIC)
#  define MKFF_CODECS_API __declspec(dllimport)
#else
#  define MKFF_CODECS_API
#endif

#if defined(__cplusplus)
extern "C" {
#endif

typedef struct MKFF_SwMedia        MKFF_SwMedia;
typedef struct MKFF_SwFrame        MKFF_SwFrame;
typedef struct MKFF_SwAudio        MKFF_SwAudio;
typedef struct MKFF_SwVideoDecoder MKFF_SwVideoDecoder;
typedef struct MKFF_SwAudioDecoder MKFF_SwAudioDecoder;

typedef enum MKFF_AudioCodec {
    MKFF_AUDIO_CODEC_UNKNOWN    = 0,
    MKFF_AUDIO_CODEC_AAC        = 1,
    MKFF_AUDIO_CODEC_OPUS       = 2,
    MKFF_AUDIO_CODEC_AC3        = 3, /* AC-3 and E-AC-3 */
    MKFF_AUDIO_CODEC_MPEG_AUDIO = 4, /* layers I, II, III (MP3) */
    MKFF_AUDIO_CODEC_FLAC       = 5,
    MKFF_AUDIO_CODEC_ALAC       = 6,
    MKFF_AUDIO_CODEC_VORBIS     = 7
} MKFF_AudioCodec;

typedef enum MKFF_SwPixelLayout {
    MKFF_SW_LAYOUT_YUV      = 1, /* planar Y'CbCr: u8 planes at 8-bit, u16 (LSB-aligned) above */
    MKFF_SW_LAYOUT_RGBA8    = 2, /* straight-alpha sRGB RGBA, data[0] */
    MKFF_SW_LAYOUT_RGBA_F32 = 3  /* premultiplied linear RGBA float, data[0] */
} MKFF_SwPixelLayout;

#define MKFF_SW_MAX_PLANES 4

typedef struct MKFF_SwFrameInfo {
    MKFF_STRUCT_HEADER;

    uint32_t width;
    uint32_t height;
    MKFF_SwPixelLayout layout;
    uint32_t chroma_format_idc; /* 1 = 4:2:0, 2 = 4:2:2, 3 = 4:4:4 (YUV layout) */
    uint32_t bit_depth;         /* 8..16 (YUV layout) */
    uint32_t has_alpha;
    uint32_t full_range;
    uint32_t matrix;            /* ISO/IEC 23091-2: 1 = BT.709, 6 = BT.601, 9 = BT.2020 NCL */
    uint32_t par_num;           /* pixel aspect ratio */
    uint32_t par_den;
    int64_t pts;                /* packet pts (decoders) or media time in ns (MKFF_SwMedia) */

    /* Planes: Y, Cb, Cr, alpha (YUV) or the packed pixels in data[0].
     * Valid until mkff_sw_frame_destroy(). Strides are in bytes. */
    uint32_t plane_count;
    const void *data[MKFF_SW_MAX_PLANES];
    uint32_t stride[MKFF_SW_MAX_PLANES];
    uint32_t height_lines[MKFF_SW_MAX_PLANES];
} MKFF_SwFrameInfo;

typedef struct MKFF_SwAudioStreamInfo {
    MKFF_STRUCT_HEADER;

    uint32_t sample_rate;
    uint32_t channels;
    uint32_t bits_per_sample;   /* 0 when not applicable */
    char codec[48];
} MKFF_SwAudioStreamInfo;

typedef struct MKFF_SwMediaInfo {
    MKFF_STRUCT_HEADER;

    double duration_seconds;
    uint32_t has_video;
    uint32_t width;
    uint32_t height;
    uint32_t has_alpha;
    int64_t frame_rate_num;
    int64_t frame_rate_den;
    uint64_t frame_count;
    uint32_t audio_stream_count;
    char container[32];
    char video_codec[48];
    char pixel_format[32];
} MKFF_SwMediaInfo;

/* Description of the last error on the calling thread ("" if none). Valid
 * until the next mkff_sw_* call on this thread. */
MKFF_CODECS_API const char *mkff_sw_last_error(void);

/* Codec names ("h264", "prores", ...). */
MKFF_CODECS_API const char *mkff_sw_video_codec_name(MKFF_VideoCodec codec);
MKFF_CODECS_API const char *mkff_sw_audio_codec_name(MKFF_AudioCodec codec);

/* ---- Frames ---------------------------------------------------------- */

MKFF_CODECS_API MKFF_Result mkff_sw_frame_get_info(const MKFF_SwFrame *frame, MKFF_SwFrameInfo *out_info);

/* Converted copies. Sizes: NV12 = w*h + 2*ceil(w/2)*ceil(h/2) bytes;
 * P010 = the same number of uint16_t samples; RGBA8 = w*h*4 bytes.
 * Returns INVALID_ARGUMENT if dst is too small. */
MKFF_CODECS_API MKFF_Result mkff_sw_frame_copy_nv12(const MKFF_SwFrame *frame, uint8_t *dst, size_t dst_size);
MKFF_CODECS_API MKFF_Result mkff_sw_frame_copy_p010(const MKFF_SwFrame *frame, uint16_t *dst, size_t dst_count);
MKFF_CODECS_API MKFF_Result mkff_sw_frame_copy_rgba8(const MKFF_SwFrame *frame, uint8_t *dst, size_t dst_size);

MKFF_CODECS_API void mkff_sw_frame_destroy(MKFF_SwFrame *frame);

/* ---- Audio buffers ---------------------------------------------------- */

/* Interleaved float samples (frames * channels), valid until destroy. */
MKFF_CODECS_API MKFF_Result mkff_sw_audio_get(const MKFF_SwAudio *audio, uint32_t *out_sample_rate,
                                              uint32_t *out_channels, size_t *out_frames,
                                              const float **out_interleaved);
MKFF_CODECS_API void mkff_sw_audio_destroy(MKFF_SwAudio *audio);

/* ---- Media files ------------------------------------------------------ */

MKFF_CODECS_API MKFF_Result mkff_sw_media_open_path(const char *path, MKFF_SwMedia **out_media);
/* Copies `data`. `name_hint` (a file name, may be NULL) only breaks ties. */
MKFF_CODECS_API MKFF_Result mkff_sw_media_open_memory(const uint8_t *data, size_t size, const char *name_hint,
                                                      MKFF_SwMedia **out_media);
MKFF_CODECS_API void        mkff_sw_media_destroy(MKFF_SwMedia *media);

MKFF_CODECS_API MKFF_Result mkff_sw_media_get_info(const MKFF_SwMedia *media, MKFF_SwMediaInfo *out_info);
MKFF_CODECS_API MKFF_Result mkff_sw_media_get_audio_stream_info(const MKFF_SwMedia *media, uint32_t stream,
                                                                MKFF_SwAudioStreamInfo *out_info);

MKFF_CODECS_API MKFF_Result mkff_sw_media_read_video_frame(const MKFF_SwMedia *media, uint64_t index,
                                                           MKFF_SwFrame **out_frame);
MKFF_CODECS_API MKFF_Result mkff_sw_media_read_video_frame_at(const MKFF_SwMedia *media, double seconds,
                                                              MKFF_SwFrame **out_frame);

/* `frames` samples from sample index `start` of audio stream `stream`,
 * resampled to `sample_rate` (0 = the stream's rate). */
MKFF_CODECS_API MKFF_Result mkff_sw_media_read_audio(const MKFF_SwMedia *media, uint32_t stream, int64_t start,
                                                     uint32_t frames, uint32_t sample_rate, MKFF_SwAudio **out_audio);

/* ---- Elementary stream decoders --------------------------------------- */

/* `config` (may be NULL/0): avcC / hvcC / Annex-B parameter sets for
 * H.264 and HEVC, av1C or a sequence header OBU for AV1, a sequence
 * header for MPEG-1/2; unused by the other codecs. Without one, H.264 and
 * HEVC packets are Annex-B access units. */
MKFF_CODECS_API MKFF_Result mkff_sw_video_decoder_create(MKFF_VideoCodec codec, const uint8_t *config,
                                                         size_t config_size, MKFF_SwVideoDecoder **out_decoder);
MKFF_CODECS_API void        mkff_sw_video_decoder_destroy(MKFF_SwVideoDecoder *decoder);

/* Same submit / receive / flush contract as mkff_video_decoder_*:
 * receive() returns NOT_READY when it needs more input, and
 * END_OF_STREAM after flush() once drained. */
MKFF_CODECS_API MKFF_Result mkff_sw_video_decoder_submit(MKFF_SwVideoDecoder *decoder, const uint8_t *data,
                                                         size_t size, int64_t pts);
MKFF_CODECS_API MKFF_Result mkff_sw_video_decoder_receive(MKFF_SwVideoDecoder *decoder, MKFF_SwFrame **out_frame);
MKFF_CODECS_API MKFF_Result mkff_sw_video_decoder_flush(MKFF_SwVideoDecoder *decoder);
/* Drop queued frames and decoding state (after a seek). */
MKFF_CODECS_API MKFF_Result mkff_sw_video_decoder_reset(MKFF_SwVideoDecoder *decoder);

/* `sample_rate` may be 0 (read from config / frame headers). `config`:
 * AudioSpecificConfig (AAC; NULL = ADTS packets), OpusHead / dOps,
 * FLAC STREAMINFO, ALAC cookie, Vorbis headers; unused otherwise. */
MKFF_CODECS_API MKFF_Result mkff_sw_audio_decoder_create(MKFF_AudioCodec codec, uint32_t sample_rate,
                                                         const uint8_t *config, size_t config_size,
                                                         MKFF_SwAudioDecoder **out_decoder);
MKFF_CODECS_API void        mkff_sw_audio_decoder_destroy(MKFF_SwAudioDecoder *decoder);
MKFF_CODECS_API MKFF_Result mkff_sw_audio_decoder_decode(MKFF_SwAudioDecoder *decoder, const uint8_t *data,
                                                         size_t size, int64_t ts, MKFF_SwAudio **out_audio);

#if defined(__cplusplus)
}
#endif

#endif /* MKFF_CODECS_H */
