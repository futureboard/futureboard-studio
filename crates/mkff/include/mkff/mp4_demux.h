#ifndef MKFF_MP4_DEMUX_H
#define MKFF_MP4_DEMUX_H

#include <stddef.h>
#include <stdint.h>

#include "mkff/abi.h"
#include "mkff/error.h"
#include "mkff/export.h"
#include "mkff/types.h"

MKFF_BEGIN_DECLS

/*
 * First video track summary. Times are in the track timescale
 * (duration / timescale = seconds). sample_count is the number of
 * coded samples available via read_access_unit / seek_sample.
 */
typedef struct MKFF_Mp4VideoTrackInfo {
    MKFF_STRUCT_HEADER;

    MKFF_VideoCodec codec;
    uint32_t width;
    uint32_t height;
    uint32_t timescale;
    uint64_t duration;     /* track timescale ticks */
    uint32_t sample_count;
} MKFF_Mp4VideoTrackInfo;

/*
 * One Annex-B access unit produced from a length-prefixed MP4 sample.
 * `data` is owned by the demuxer and remains valid until the next
 * read_access_unit, seek_sample, or destroy on the same demuxer.
 * pts/dts are track-timescale ticks.
 */
typedef struct MKFF_Mp4AccessUnit {
    MKFF_STRUCT_HEADER;

    const uint8_t *data;
    size_t size;
    int64_t pts;
    int64_t dts;
    uint32_t sync;          /* 1 if sync / random-access sample */
    uint32_t sample_index;  /* 0-based index of the source sample */
} MKFF_Mp4AccessUnit;

/* Opens a progressive MP4/MOV file (moov + mdat). Copies file bytes. */
MKFF_API MKFF_Result mkff_mp4_demux_open_path(const char *path, MKFF_Mp4Demux **out_demux);

/* Opens from a memory buffer. Copies `data` (caller may free immediately). */
MKFF_API MKFF_Result mkff_mp4_demux_open_memory(const uint8_t *data,
                                                 size_t size,
                                                 MKFF_Mp4Demux **out_demux);

MKFF_API void mkff_mp4_demux_destroy(MKFF_Mp4Demux *demux);

/* Fills info for the first `vide` track (H.264 avc1 or HEVC hvc1/hev1). */
MKFF_API MKFF_Result mkff_mp4_demux_get_video_track(const MKFF_Mp4Demux *demux,
                                                     MKFF_Mp4VideoTrackInfo *out_info);

/*
 * Reads the next sample as an Annex-B AU. Parameter sets from avcC/hvcC
 * are prepended on the first sample and on subsequent sync samples.
 * Returns MKFF_RESULT_END_OF_STREAM when no samples remain.
 */
MKFF_API MKFF_Result mkff_mp4_demux_read_access_unit(MKFF_Mp4Demux *demux,
                                                      MKFF_Mp4AccessUnit *out_au);

/* Seeks so the next read_access_unit returns sample `sample_index` (0-based). */
MKFF_API MKFF_Result mkff_mp4_demux_seek_sample(MKFF_Mp4Demux *demux, uint32_t sample_index);

MKFF_END_DECLS

#endif /* MKFF_MP4_DEMUX_H */
