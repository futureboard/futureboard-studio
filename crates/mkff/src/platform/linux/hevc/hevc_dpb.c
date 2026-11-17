#include "hevc_dpb.h"

#include <string.h>

void hevc_dpb_init(HevcDpb *dpb) {
    memset(dpb, 0, sizeof(*dpb));
}

void hevc_dpb_reset(HevcDpb *dpb) {
    hevc_dpb_clear_references(dpb, 1);
}

void hevc_dpb_clear_references(HevcDpb *dpb, int drop_output) {
    for (uint32_t i = 0; i < HEVC_DPB_MAX_ENTRIES; i++) {
        if (dpb->entries[i].in_use) {
            linux_video_frame_release((MKFF_VideoFrame *)dpb->entries[i].frame);
            dpb->entries[i].in_use = 0;
            dpb->entries[i].frame = NULL;
        }
    }
    if (drop_output) {
        for (uint32_t i = 0; i < dpb->output_count; i++) {
            linux_video_frame_release((MKFF_VideoFrame *)dpb->output_queue[i].frame);
        }
        dpb->output_count = 0;
    }
}

static void init_va_pic(VAPictureHEVC *pic) {
    memset(pic, 0, sizeof(*pic));
    pic->picture_id = VA_INVALID_ID;
    pic->flags = VA_PICTURE_HEVC_INVALID;
}

static int rps_flags_for_poc(int32_t curr_poc, int32_t ref_poc, const HevcShortTermRps *rps) {
    for (uint32_t i = 0; i < rps->num_negative_pics; i++) {
        if (curr_poc + rps->delta_poc_s0[i] == ref_poc) {
            return rps->used_by_curr_pic_s0[i] ? VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE : 0;
        }
    }
    for (uint32_t i = 0; i < rps->num_positive_pics; i++) {
        if (curr_poc + rps->delta_poc_s1[i] == ref_poc) {
            return rps->used_by_curr_pic_s1[i] ? VA_PICTURE_HEVC_RPS_ST_CURR_AFTER : 0;
        }
    }
    return 0;
}

static int poc_in_rps(int32_t curr_poc, int32_t ref_poc, const HevcShortTermRps *rps) {
    for (uint32_t i = 0; i < rps->num_negative_pics; i++) {
        if (curr_poc + rps->delta_poc_s0[i] == ref_poc) return 1;
    }
    for (uint32_t i = 0; i < rps->num_positive_pics; i++) {
        if (curr_poc + rps->delta_poc_s1[i] == ref_poc) return 1;
    }
    return 0;
}

uint32_t hevc_dpb_fill_reference_frames(const HevcDpb *dpb,
                                         int32_t curr_poc,
                                         const HevcShortTermRps *rps,
                                         VAPictureHEVC *out_refs) {
    uint32_t n = 0;
    for (uint32_t i = 0; i < 15; i++) {
        init_va_pic(&out_refs[i]);
    }

    for (uint32_t i = 0; i < HEVC_DPB_MAX_ENTRIES && n < 15; i++) {
        if (!dpb->entries[i].in_use) continue;
        VAPictureHEVC *pic = &out_refs[n];
        pic->picture_id = dpb->entries[i].frame->surface;
        pic->pic_order_cnt = dpb->entries[i].poc;
        pic->flags = rps_flags_for_poc(curr_poc, dpb->entries[i].poc, rps);
        if (dpb->entries[i].is_long_term) {
            pic->flags |= VA_PICTURE_HEVC_LONG_TERM_REFERENCE;
            if (pic->flags & VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE
                || pic->flags & VA_PICTURE_HEVC_RPS_ST_CURR_AFTER) {
                /* long-term used by current uses LT_CURR instead */
                pic->flags &= ~(uint32_t)(VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE | VA_PICTURE_HEVC_RPS_ST_CURR_AFTER);
                pic->flags |= VA_PICTURE_HEVC_RPS_LT_CURR;
            }
        }
        n++;
    }
    return n;
}

static int find_ref_index(const VAPictureHEVC *refs, int32_t poc) {
    for (int i = 0; i < 15; i++) {
        if (refs[i].picture_id != VA_INVALID_ID && refs[i].pic_order_cnt == poc) {
            return i;
        }
    }
    return -1;
}

void hevc_dpb_build_ref_pic_lists(const VAPictureHEVC *refs,
                                   int32_t curr_poc,
                                   const HevcShortTermRps *rps,
                                   uint32_t slice_type,
                                   uint32_t num_ref_idx_l0_active_minus1,
                                   uint32_t num_ref_idx_l1_active_minus1,
                                   uint8_t RefPicList[2][15]) {
    memset(RefPicList, 0xFF, 2 * 15);

    if (slice_type == HEVC_SLICE_TYPE_I) {
        return;
    }

    uint8_t before[15];
    uint8_t after[15];
    uint32_t nb = 0, na = 0;

    for (uint32_t i = 0; i < rps->num_negative_pics && nb < 15; i++) {
        if (!rps->used_by_curr_pic_s0[i]) continue;
        int idx = find_ref_index(refs, curr_poc + rps->delta_poc_s0[i]);
        if (idx >= 0) before[nb++] = (uint8_t)idx;
    }
    for (uint32_t i = 0; i < rps->num_positive_pics && na < 15; i++) {
        if (!rps->used_by_curr_pic_s1[i]) continue;
        int idx = find_ref_index(refs, curr_poc + rps->delta_poc_s1[i]);
        if (idx >= 0) after[na++] = (uint8_t)idx;
    }

    /* RefPicList0: ST_CURR_BEFORE then ST_CURR_AFTER */
    uint32_t n0 = 0;
    uint32_t want0 = num_ref_idx_l0_active_minus1 + 1;
    if (want0 > 15) want0 = 15;
    for (uint32_t i = 0; i < nb && n0 < want0; i++) RefPicList[0][n0++] = before[i];
    for (uint32_t i = 0; i < na && n0 < want0; i++) RefPicList[0][n0++] = after[i];

    if (slice_type != HEVC_SLICE_TYPE_B) {
        return;
    }

    /* RefPicList1: ST_CURR_AFTER then ST_CURR_BEFORE */
    uint32_t n1 = 0;
    uint32_t want1 = num_ref_idx_l1_active_minus1 + 1;
    if (want1 > 15) want1 = 15;
    for (uint32_t i = 0; i < na && n1 < want1; i++) RefPicList[1][n1++] = after[i];
    for (uint32_t i = 0; i < nb && n1 < want1; i++) RefPicList[1][n1++] = before[i];
}

void hevc_dpb_update_after_decode(HevcDpb *dpb,
                                   LinuxVideoFrame *frame,
                                   int32_t poc,
                                   const HevcShortTermRps *rps,
                                   int is_reference) {
    /* Drop short-term refs that are not in the current RPS. */
    for (uint32_t i = 0; i < HEVC_DPB_MAX_ENTRIES; i++) {
        if (!dpb->entries[i].in_use) continue;
        if (!poc_in_rps(poc, dpb->entries[i].poc, rps)) {
            linux_video_frame_release((MKFF_VideoFrame *)dpb->entries[i].frame);
            dpb->entries[i].in_use = 0;
            dpb->entries[i].frame = NULL;
        }
    }

    if (!is_reference || !frame) {
        return;
    }

    for (uint32_t i = 0; i < HEVC_DPB_MAX_ENTRIES; i++) {
        if (!dpb->entries[i].in_use) {
            dpb->entries[i].in_use = 1;
            dpb->entries[i].poc = poc;
            dpb->entries[i].is_long_term = 0;
            dpb->entries[i].frame = (LinuxVideoFrame *)linux_video_frame_retain((MKFF_VideoFrame *)frame);
            return;
        }
    }
}

void hevc_dpb_push_output(HevcDpb *dpb, LinuxVideoFrame *frame, int32_t poc) {
    if (dpb->output_count >= HEVC_OUTPUT_QUEUE_MAX) {
        return;
    }
    dpb->output_queue[dpb->output_count].frame = (LinuxVideoFrame *)linux_video_frame_retain((MKFF_VideoFrame *)frame);
    dpb->output_queue[dpb->output_count].poc = poc;
    dpb->output_count++;
}

static LinuxVideoFrame *pop_smallest_poc(HevcDpb *dpb) {
    if (dpb->output_count == 0) {
        return NULL;
    }
    uint32_t min_index = 0;
    for (uint32_t i = 1; i < dpb->output_count; i++) {
        if (dpb->output_queue[i].poc < dpb->output_queue[min_index].poc) {
            min_index = i;
        }
    }
    LinuxVideoFrame *frame = dpb->output_queue[min_index].frame;
    for (uint32_t i = min_index; i + 1 < dpb->output_count; i++) {
        dpb->output_queue[i] = dpb->output_queue[i + 1];
    }
    dpb->output_count--;
    return frame;
}

LinuxVideoFrame *hevc_dpb_bump_if_needed(HevcDpb *dpb, uint32_t max_reorder) {
    if (dpb->output_count <= max_reorder) {
        return NULL;
    }
    return pop_smallest_poc(dpb);
}

LinuxVideoFrame *hevc_dpb_bump_one(HevcDpb *dpb) {
    return pop_smallest_poc(dpb);
}
