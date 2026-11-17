#include "h264_dxva_dpb.h"

#include <string.h>

void h264_dxva_dpb_init(H264DxvaDpb *dpb) {
    memset(dpb, 0, sizeof(*dpb));
}

void h264_dxva_dpb_reset(H264DxvaDpb *dpb) {
    for (uint32_t i = 0; i < H264_DXVA_DPB_MAX_ENTRIES; i++) {
        if (dpb->entries[i].in_use) {
            windows_video_frame_release((MKFF_VideoFrame *)dpb->entries[i].frame);
            dpb->entries[i].in_use = 0;
        }
    }
    for (uint32_t i = 0; i < dpb->output_count; i++) {
        windows_video_frame_release((MKFF_VideoFrame *)dpb->output_queue[i].frame);
    }
    dpb->output_count = 0;
}

uint32_t h264_dxva_dpb_fill_reference_frames(const H264DxvaDpb *dpb,
                                              DXVA_PicEntry_H264 *out_ref_frame_list,
                                              USHORT *out_frame_num_list,
                                              INT out_field_order_cnt_list[][2],
                                              uint32_t capacity,
                                              UINT *out_used_for_reference_flags) {
    uint32_t n = 0;
    UINT used_flags = 0;

    for (uint32_t i = 0; i < H264_DXVA_DPB_MAX_ENTRIES && n < capacity; i++) {
        if (!dpb->entries[i].in_use) continue;

        out_ref_frame_list[n].bPicEntry = 0;
        out_ref_frame_list[n].Index7Bits = (UCHAR)(i & 0x7F); /* surface pool index this ref occupies */
        out_ref_frame_list[n].AssociatedFlag = 0;              /* progressive: no bottom-field association */

        out_frame_num_list[n] = (USHORT)dpb->entries[i].frame_num;
        out_field_order_cnt_list[n][0] = dpb->entries[i].poc; /* TopFieldOrderCnt */
        out_field_order_cnt_list[n][1] = dpb->entries[i].poc; /* BottomFieldOrderCnt (progressive: same value) */

        used_flags |= (UINT)(0x3u << (2 * n)); /* both top/bottom "used for reference" bits, per spec convention */
        n++;
    }

    *out_used_for_reference_flags = used_flags;
    return n;
}

static int32_t frame_num_wrap(uint32_t entry_frame_num, uint32_t curr_frame_num) {
    /* Same relative-ordering trick used by the Linux DPB's sliding
     * window: exact MaxFrameNum wraparound doesn't matter for picking
     * the least-recently-decoded short-term reference, only consistent
     * relative order does. */
    return (entry_frame_num > curr_frame_num) ? (int32_t)entry_frame_num - (int32_t)0x40000000 : (int32_t)entry_frame_num;
}

void h264_dxva_dpb_add_reference(H264DxvaDpb *dpb, WindowsVideoFrame *frame, uint32_t frame_num, int32_t poc, uint32_t max_num_ref_frames, int is_reference) {
    if (!is_reference) {
        return;
    }
    if (max_num_ref_frames == 0) {
        max_num_ref_frames = 1;
    }

    uint32_t live_count = 0;
    for (uint32_t i = 0; i < H264_DXVA_DPB_MAX_ENTRIES; i++) {
        if (dpb->entries[i].in_use) live_count++;
    }

    if (live_count >= max_num_ref_frames) {
        int32_t oldest_index = -1;
        int32_t oldest_wrap = 0;
        for (uint32_t i = 0; i < H264_DXVA_DPB_MAX_ENTRIES; i++) {
            if (!dpb->entries[i].in_use) continue;
            int32_t wrap = frame_num_wrap(dpb->entries[i].frame_num, frame_num);
            if (oldest_index < 0 || wrap < oldest_wrap) {
                oldest_wrap = wrap;
                oldest_index = (int32_t)i;
            }
        }
        if (oldest_index >= 0) {
            windows_video_frame_release((MKFF_VideoFrame *)dpb->entries[oldest_index].frame);
            dpb->entries[oldest_index].in_use = 0;
        }
    }

    for (uint32_t i = 0; i < H264_DXVA_DPB_MAX_ENTRIES; i++) {
        if (!dpb->entries[i].in_use) {
            dpb->entries[i].in_use = 1;
            dpb->entries[i].frame_num = frame_num;
            dpb->entries[i].poc = poc;
            dpb->entries[i].frame = (WindowsVideoFrame *)windows_video_frame_retain((MKFF_VideoFrame *)frame);
            return;
        }
    }
    /* Pool exhausted despite eviction attempt: defensive, unreachable
     * given max_num_ref_frames <= H264_DXVA_DPB_MAX_ENTRIES. */
}

void h264_dxva_dpb_push_output(H264DxvaDpb *dpb, WindowsVideoFrame *frame, int32_t poc) {
    if (dpb->output_count >= H264_DXVA_OUTPUT_QUEUE_MAX) {
        return; /* defensive: should not happen given queue sizing vs DPB bound */
    }
    dpb->output_queue[dpb->output_count].frame = (WindowsVideoFrame *)windows_video_frame_retain((MKFF_VideoFrame *)frame);
    dpb->output_queue[dpb->output_count].poc = poc;
    dpb->output_count++;
}

static WindowsVideoFrame *pop_smallest_poc(H264DxvaDpb *dpb) {
    if (dpb->output_count == 0) {
        return NULL;
    }
    uint32_t min_index = 0;
    for (uint32_t i = 1; i < dpb->output_count; i++) {
        if (dpb->output_queue[i].poc < dpb->output_queue[min_index].poc) {
            min_index = i;
        }
    }
    WindowsVideoFrame *frame = dpb->output_queue[min_index].frame;
    for (uint32_t i = min_index; i + 1 < dpb->output_count; i++) {
        dpb->output_queue[i] = dpb->output_queue[i + 1];
    }
    dpb->output_count--;
    return frame; /* ownership (the queue's reference) transfers to the caller */
}

WindowsVideoFrame *h264_dxva_dpb_bump_if_needed(H264DxvaDpb *dpb, uint32_t max_reorder) {
    if (dpb->output_count <= max_reorder) {
        return NULL;
    }
    return pop_smallest_poc(dpb);
}

WindowsVideoFrame *h264_dxva_dpb_bump_one(H264DxvaDpb *dpb) {
    return pop_smallest_poc(dpb);
}
