#include "hevc_dxva_dpb.h"

#include <string.h>

void hevc_dxva_dpb_init(HevcDxvaDpb *dpb) {
    memset(dpb, 0, sizeof(*dpb));
}

void hevc_dxva_dpb_reset(HevcDxvaDpb *dpb) {
    hevc_dxva_dpb_clear(dpb);
}

void hevc_dxva_dpb_clear(HevcDxvaDpb *dpb) {
    for (uint32_t i = 0; i < HEVC_DXVA_DPB_MAX_ENTRIES; i++) {
        if (dpb->entries[i].in_use) {
            windows_video_frame_release((MKFF_VideoFrame *)dpb->entries[i].frame);
            dpb->entries[i].in_use = 0;
            dpb->entries[i].frame = NULL;
        }
    }
    for (uint32_t i = 0; i < dpb->output_count; i++) {
        windows_video_frame_release((MKFF_VideoFrame *)dpb->output_queue[i].frame);
    }
    dpb->output_count = 0;
}

static int poc_in_st_rps(int32_t curr_poc, const HevcShortTermRps *rps, int32_t poc) {
    for (uint32_t i = 0; i < rps->num_negative_pics; i++) {
        if (curr_poc + rps->delta_poc_s0[i] == poc) {
            return 1;
        }
    }
    for (uint32_t i = 0; i < rps->num_positive_pics; i++) {
        if (curr_poc + rps->delta_poc_s1[i] == poc) {
            return 1;
        }
    }
    return 0;
}

void hevc_dxva_dpb_apply_st_rps(HevcDxvaDpb *dpb, int32_t curr_poc, const HevcShortTermRps *rps) {
    for (uint32_t i = 0; i < HEVC_DXVA_DPB_MAX_ENTRIES; i++) {
        if (!dpb->entries[i].in_use) {
            continue;
        }
        if (dpb->entries[i].is_long_term || !poc_in_st_rps(curr_poc, rps, dpb->entries[i].poc)) {
            windows_video_frame_release((MKFF_VideoFrame *)dpb->entries[i].frame);
            dpb->entries[i].in_use = 0;
            dpb->entries[i].frame = NULL;
        } else {
            dpb->entries[i].is_long_term = 0;
        }
    }
}

void hevc_dxva_dpb_add_reference(HevcDxvaDpb *dpb, WindowsVideoFrame *frame, int32_t poc, int is_reference) {
    if (!is_reference || !frame) {
        return;
    }

    for (uint32_t i = 0; i < HEVC_DXVA_DPB_MAX_ENTRIES; i++) {
        if (dpb->entries[i].in_use && dpb->entries[i].poc == poc) {
            windows_video_frame_release((MKFF_VideoFrame *)dpb->entries[i].frame);
            dpb->entries[i].frame = (WindowsVideoFrame *)windows_video_frame_retain((MKFF_VideoFrame *)frame);
            dpb->entries[i].is_long_term = 0;
            return;
        }
    }

    for (uint32_t i = 0; i < HEVC_DXVA_DPB_MAX_ENTRIES; i++) {
        if (!dpb->entries[i].in_use) {
            dpb->entries[i].in_use = 1;
            dpb->entries[i].is_long_term = 0;
            dpb->entries[i].poc = poc;
            dpb->entries[i].frame = (WindowsVideoFrame *)windows_video_frame_retain((MKFF_VideoFrame *)frame);
            return;
        }
    }
}

static int get_refpic_index(const DXVA_PicEntry_HEVC *ref_pic_list, uint32_t array_slice) {
    for (uint32_t i = 0; i < HEVC_DXVA_REF_PIC_LIST_SIZE; i++) {
        if ((ref_pic_list[i].bPicEntry & 0x7Fu) == (array_slice & 0x7Fu)
            && ref_pic_list[i].bPicEntry != 0xFF) {
            return (int)i;
        }
    }
    return 0xFF;
}

static WindowsVideoFrame *find_by_poc(const HevcDxvaDpb *dpb, int32_t poc, uint32_t *out_array_slice) {
    for (uint32_t i = 0; i < HEVC_DXVA_DPB_MAX_ENTRIES; i++) {
        if (dpb->entries[i].in_use && dpb->entries[i].poc == poc && dpb->entries[i].frame) {
            *out_array_slice = dpb->entries[i].frame->array_slice;
            return dpb->entries[i].frame;
        }
    }
    return NULL;
}

UINT hevc_dxva_dpb_fill_pic_params_refs(HevcDxvaDpb *dpb,
                                         int32_t curr_poc,
                                         const HevcShortTermRps *rps,
                                         DXVA_PicEntry_HEVC *out_ref_pic_list,
                                         INT *out_poc_list,
                                         UCHAR *out_st_curr_before,
                                         UCHAR *out_st_curr_after,
                                         UCHAR *out_lt_curr) {
    for (uint32_t i = 0; i < HEVC_DXVA_REF_PIC_LIST_SIZE; i++) {
        out_ref_pic_list[i].bPicEntry = 0xFF;
        out_poc_list[i] = 0;
    }
    for (uint32_t i = 0; i < HEVC_DXVA_REF_SET_SIZE; i++) {
        out_st_curr_before[i] = 0xFF;
        out_st_curr_after[i] = 0xFF;
        out_lt_curr[i] = 0xFF;
    }

    uint32_t n = 0;
    for (uint32_t i = 0; i < HEVC_DXVA_DPB_MAX_ENTRIES && n < HEVC_DXVA_REF_PIC_LIST_SIZE; i++) {
        if (!dpb->entries[i].in_use || !dpb->entries[i].frame) {
            continue;
        }
        out_ref_pic_list[n].bPicEntry = 0;
        out_ref_pic_list[n].Index7Bits = (UCHAR)(dpb->entries[i].frame->array_slice & 0x7F);
        out_ref_pic_list[n].AssociatedFlag = dpb->entries[i].is_long_term ? 1 : 0;
        out_poc_list[n] = dpb->entries[i].poc;
        n++;
    }

    uint32_t before = 0;
    for (uint32_t i = 0; i < rps->num_negative_pics && before < HEVC_DXVA_REF_SET_SIZE; i++) {
        if (!rps->used_by_curr_pic_s0[i]) {
            continue;
        }
        uint32_t slice = 0;
        if (find_by_poc(dpb, curr_poc + rps->delta_poc_s0[i], &slice)) {
            out_st_curr_before[before++] = (UCHAR)get_refpic_index(out_ref_pic_list, slice);
        }
    }

    uint32_t after = 0;
    for (uint32_t i = 0; i < rps->num_positive_pics && after < HEVC_DXVA_REF_SET_SIZE; i++) {
        if (!rps->used_by_curr_pic_s1[i]) {
            continue;
        }
        uint32_t slice = 0;
        if (find_by_poc(dpb, curr_poc + rps->delta_poc_s1[i], &slice)) {
            out_st_curr_after[after++] = (UCHAR)get_refpic_index(out_ref_pic_list, slice);
        }
    }

    dpb->status_report_feedback++;
    if (dpb->status_report_feedback == 0) {
        dpb->status_report_feedback = 1;
    }
    return dpb->status_report_feedback;
}

void hevc_dxva_dpb_push_output(HevcDxvaDpb *dpb, WindowsVideoFrame *frame, int32_t poc) {
    if (dpb->output_count >= HEVC_DXVA_OUTPUT_QUEUE_MAX) {
        return;
    }
    dpb->output_queue[dpb->output_count].frame =
        (WindowsVideoFrame *)windows_video_frame_retain((MKFF_VideoFrame *)frame);
    dpb->output_queue[dpb->output_count].poc = poc;
    dpb->output_count++;
}

static WindowsVideoFrame *pop_smallest_poc(HevcDxvaDpb *dpb) {
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
    return frame;
}

WindowsVideoFrame *hevc_dxva_dpb_bump_if_needed(HevcDxvaDpb *dpb, uint32_t max_reorder) {
    if (dpb->output_count <= max_reorder) {
        return NULL;
    }
    return pop_smallest_poc(dpb);
}

WindowsVideoFrame *hevc_dxva_dpb_bump_one(HevcDxvaDpb *dpb) {
    return pop_smallest_poc(dpb);
}
