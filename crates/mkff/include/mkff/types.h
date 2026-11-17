#ifndef MKFF_TYPES_H
#define MKFF_TYPES_H

#include <stdint.h>

/* Opaque handles. Never dereferenced by API consumers. */
typedef struct MKFF_Context      MKFF_Context;
typedef struct MKFF_VideoDecoder MKFF_VideoDecoder;
typedef struct MKFF_VideoFrame   MKFF_VideoFrame;
typedef struct MKFF_Mp4Demux     MKFF_Mp4Demux;

typedef enum MKFF_PixelFormat {
    MKFF_PIXEL_FORMAT_UNKNOWN = 0,
    MKFF_PIXEL_FORMAT_NV12    = 1,
    MKFF_PIXEL_FORMAT_P010    = 2  /* 10-bit biplanar 4:2:0 (MSB-aligned in 16-bit samples) */
} MKFF_PixelFormat;

/* Decoder backend preference. Appended to MKFF_VideoDecoderDesc; older
 * callers with a smaller struct_size default to AUTO. */
typedef enum MKFF_VideoBackend {
    MKFF_VIDEO_BACKEND_AUTO           = 0, /* try hardware, then software if built */
    MKFF_VIDEO_BACKEND_HARDWARE_ONLY  = 1,
    MKFF_VIDEO_BACKEND_SOFTWARE_ONLY  = 2
} MKFF_VideoBackend;

typedef enum MKFF_VideoCodec {
    MKFF_VIDEO_CODEC_UNKNOWN = 0,
    MKFF_VIDEO_CODEC_H264    = 1,
    MKFF_VIDEO_CODEC_HEVC    = 2,
    MKFF_VIDEO_CODEC_VP9     = 3,
    MKFF_VIDEO_CODEC_AV1     = 4,
    /* Software-only (libmkff_codecs, see mkff/codecs.h); libmkff's
     * hardware decoders reject these with CODEC_UNAVAILABLE. */
    MKFF_VIDEO_CODEC_VP8     = 5,
    MKFF_VIDEO_CODEC_MPEG1   = 6,
    MKFF_VIDEO_CODEC_MPEG2   = 7,
    MKFF_VIDEO_CODEC_PRORES  = 8,
    MKFF_VIDEO_CODEC_DNXHD   = 9,  /* DNxHD / DNxHR (SMPTE VC-3) */
    MKFF_VIDEO_CODEC_APV     = 10,
    MKFF_VIDEO_CODEC_MJPEG   = 11
} MKFF_VideoCodec;

typedef enum MKFF_VideoProfile {
    MKFF_VIDEO_PROFILE_UNKNOWN          = 0,

    MKFF_VIDEO_PROFILE_H264_BASELINE    = 100,
    MKFF_VIDEO_PROFILE_H264_MAIN        = 101,
    MKFF_VIDEO_PROFILE_H264_HIGH        = 102,
    MKFF_VIDEO_PROFILE_H264_CONSTRAINED_BASELINE = 103,

    MKFF_VIDEO_PROFILE_HEVC_MAIN        = 200,
    MKFF_VIDEO_PROFILE_HEVC_MAIN10      = 201,

    MKFF_VIDEO_PROFILE_VP9_PROFILE0     = 300,
    MKFF_VIDEO_PROFILE_VP9_PROFILE2     = 301,

    MKFF_VIDEO_PROFILE_AV1_MAIN         = 400
} MKFF_VideoProfile;

typedef enum MKFF_VideoEntrypoint {
    MKFF_VIDEO_ENTRYPOINT_UNKNOWN = 0,
    MKFF_VIDEO_ENTRYPOINT_VLD     = 1  /* Variable Length Decoding (hw slice/entropy decode) */
} MKFF_VideoEntrypoint;

#endif /* MKFF_TYPES_H */
