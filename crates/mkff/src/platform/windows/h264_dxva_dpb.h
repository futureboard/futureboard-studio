#ifndef MKFF_WINDOWS_H264_DXVA_DPB_H
#define MKFF_WINDOWS_H264_DXVA_DPB_H

#include <stdint.h>

#include "video_frame.h" /* pulls in win_common.h (and, transitively, <dxva.h>) in the right order */

#define H264_DXVA_DPB_MAX_ENTRIES 16
#define H264_DXVA_OUTPUT_QUEUE_MAX 17

/*
 * Unlike VA-API, DXVA's H.264 short-slice-control decode model has the
 * hardware itself reparse each slice_header() (ref_pic_list_modification,
 * pred_weight_table, ...) straight out of the raw NAL bytes we hand it —
 * see DXVA_Slice_H264_Short, which carries no per-slice reference-list
 * fields at all. So this DPB only needs to do sliding-window DPB
 * bookkeeping (to fill DXVA_PicParams_H264::RefFrameList/FrameNumList/
 * FieldOrderCntList/UsedForReferenceFlags) and POC-based output
 * reordering; it does NOT construct RefPicList0/1 the way
 * src/platform/linux/h264/h264_dpb.c does for VA-API.
 */
typedef struct H264DxvaDpbEntry {
    int                 in_use;
    uint32_t            frame_num;
    int32_t             poc;
    WindowsVideoFrame  *frame; /* one reference owned by this DPB slot */
} H264DxvaDpbEntry;

typedef struct H264DxvaOutputEntry {
    WindowsVideoFrame *frame; /* one reference owned by the output queue */
    int32_t            poc;
} H264DxvaOutputEntry;

typedef struct H264DxvaDpb {
    H264DxvaDpbEntry entries[H264_DXVA_DPB_MAX_ENTRIES];
    H264DxvaOutputEntry output_queue[H264_DXVA_OUTPUT_QUEUE_MAX];
    uint32_t output_count;
} H264DxvaDpb;

void h264_dxva_dpb_init(H264DxvaDpb *dpb);

/* Releases every frame reference still held by the DPB and the pending
 * output queue. Call once on decoder teardown. */
void h264_dxva_dpb_reset(H264DxvaDpb *dpb);

/* Fills DXVA_PicParams_H264::RefFrameList / FrameNumList /
 * FieldOrderCntList and returns the corresponding UsedForReferenceFlags
 * bitmask (2 bits per entry, both set for a used progressive-frame
 * short-term reference, matching the field/frame convention DXVA
 * reuses from the interlace-capable structure). Unused entries beyond
 * the returned count are the caller's responsibility to mark invalid
 * (bPicEntry = 0xFF, per the DXVA H.264 spec's sentinel convention). */
uint32_t h264_dxva_dpb_fill_reference_frames(const H264DxvaDpb *dpb,
                                              DXVA_PicEntry_H264 *out_ref_frame_list,
                                              USHORT *out_frame_num_list,
                                              INT out_field_order_cnt_list[][2],
                                              uint32_t capacity,
                                              UINT *out_used_for_reference_flags);

/* Inserts a newly-decoded reference picture, evicting the oldest
 * short-term entry (sliding window, spec 8.2.5.3) if already at
 * `max_num_ref_frames` capacity. Retains `frame`. No-op if
 * nal_ref_idc == 0 (non-reference picture). */
void h264_dxva_dpb_add_reference(H264DxvaDpb *dpb, WindowsVideoFrame *frame, uint32_t frame_num, int32_t poc, uint32_t max_num_ref_frames, int is_reference);

/* Pushes a decoded picture (retains it) into the output/reorder queue,
 * independent of reference status. */
void h264_dxva_dpb_push_output(H264DxvaDpb *dpb, WindowsVideoFrame *frame, int32_t poc);

/* Pops the smallest-POC picture if the reorder buffer holds more than
 * `max_reorder` pictures, transferring the queue's reference to the
 * caller. Returns NULL if nothing is due yet. */
WindowsVideoFrame *h264_dxva_dpb_bump_if_needed(H264DxvaDpb *dpb, uint32_t max_reorder);

/* Unconditionally pops the smallest-POC picture (flush / end-of-stream
 * draining). Returns NULL when the queue is empty. */
WindowsVideoFrame *h264_dxva_dpb_bump_one(H264DxvaDpb *dpb);

#endif /* MKFF_WINDOWS_H264_DXVA_DPB_H */
