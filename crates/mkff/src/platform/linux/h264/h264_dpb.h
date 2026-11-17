#ifndef MKFF_LINUX_H264_DPB_H
#define MKFF_LINUX_H264_DPB_H

#include <stdint.h>
#include <va/va.h>

#include "src/codecs/h264/h264_slice.h"
#include "src/codecs/h264/h264_sps_pps.h"
#include "src/platform/linux/video_frame.h"

#define H264_DPB_MAX_ENTRIES 16
#define H264_OUTPUT_QUEUE_MAX 17

typedef struct H264DpbEntry {
    int              in_use;
    uint32_t         frame_num; /* PicNum/FrameNumWrap is recomputed relative to
                                    the current slice's frame_num at ref-list-
                                    construction time, per spec 8.2.4.1 */
    int32_t          poc;
    LinuxVideoFrame *frame;   /* one reference owned by this DPB slot */
} H264DpbEntry;

typedef struct H264OutputEntry {
    LinuxVideoFrame *frame; /* one reference owned by the output queue */
    int32_t          poc;
} H264OutputEntry;

typedef struct H264Dpb {
    H264DpbEntry entries[H264_DPB_MAX_ENTRIES];
    uint32_t     num_entries;

    H264OutputEntry output_queue[H264_OUTPUT_QUEUE_MAX];
    uint32_t        output_count;
} H264Dpb;

void h264_dpb_init(H264Dpb *dpb);

/* Releases every frame reference still held by the DPB and the pending
 * output queue. Call once on decoder teardown. */
void h264_dpb_reset(H264Dpb *dpb);

/* Builds VAPictureParameterBufferH264::ReferenceFrames[] from the
 * current short-term reference set. Returns the number of entries
 * written (<=16); the caller is responsible for marking the remainder
 * of a 16-entry array invalid. */
uint32_t h264_dpb_fill_reference_frames(const H264Dpb *dpb, VAPictureH264 *out_refs, uint32_t capacity);

/* Default reference list construction (spec 8.2.4.2) with explicit
 * reordering (8.2.4.3) applied on top. Long-term modification requests
 * (modification_of_pic_nums_idc == 2) are recognized but skipped, since
 * this DPB only ever holds short-term references (sliding window only,
 * no adaptive/long-term marking in this milestone) — *out_used_long_term
 * is set if any such request was seen, so the caller can log once.
 * Returns the number of entries written. */
uint32_t h264_dpb_build_ref_pic_list0(const H264Dpb *dpb,
                                       const H264SliceHeader *sh,
                                       int32_t current_poc,
                                       uint32_t max_frame_num,
                                       VAPictureH264 *out_list,
                                       uint32_t capacity,
                                       int *out_used_long_term);
uint32_t h264_dpb_build_ref_pic_list1(const H264Dpb *dpb,
                                       const H264SliceHeader *sh,
                                       int32_t current_poc,
                                       uint32_t max_frame_num,
                                       VAPictureH264 *out_list,
                                       uint32_t capacity,
                                       int *out_used_long_term);

/* Inserts a newly-decoded reference picture, evicting the oldest
 * short-term entry (sliding window, spec 8.2.5.3) if already at
 * `max_num_ref_frames` capacity. Retains `frame` itself. No-op if
 * nal_ref_idc == 0 (non-reference picture). */
void h264_dpb_add_reference(H264Dpb *dpb, LinuxVideoFrame *frame, uint32_t frame_num, int32_t poc, uint32_t max_num_ref_frames, int is_reference);

/* Pushes a decoded picture (retains it) into the output/reorder queue,
 * independent of reference status. */
void h264_dpb_push_output(H264Dpb *dpb, LinuxVideoFrame *frame, int32_t poc);

/* Pops the smallest-POC picture if the reorder buffer holds more than
 * `max_reorder` pictures, transferring the queue's reference to the
 * caller. Returns NULL if nothing is due yet. */
LinuxVideoFrame *h264_dpb_bump_if_needed(H264Dpb *dpb, uint32_t max_reorder);

/* Unconditionally pops the smallest-POC picture (used for flush /
 * end-of-stream draining). Returns NULL when the queue is empty. */
LinuxVideoFrame *h264_dpb_bump_one(H264Dpb *dpb);

#endif /* MKFF_LINUX_H264_DPB_H */
