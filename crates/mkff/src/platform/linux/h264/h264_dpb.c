#include "h264_dpb.h"

#include <stdlib.h>
#include <string.h>

void h264_dpb_init(H264Dpb *dpb) {
    memset(dpb, 0, sizeof(*dpb));
}

void h264_dpb_reset(H264Dpb *dpb) {
    for (uint32_t i = 0; i < H264_DPB_MAX_ENTRIES; i++) {
        if (dpb->entries[i].in_use) {
            linux_video_frame_release((MKFF_VideoFrame *)dpb->entries[i].frame);
            dpb->entries[i].in_use = 0;
        }
    }
    for (uint32_t i = 0; i < dpb->output_count; i++) {
        linux_video_frame_release((MKFF_VideoFrame *)dpb->output_queue[i].frame);
    }
    dpb->num_entries = 0;
    dpb->output_count = 0;
}

static int32_t frame_num_wrap(uint32_t entry_frame_num, uint32_t curr_frame_num, uint32_t max_frame_num) {
    return (entry_frame_num > curr_frame_num) ? (int32_t)entry_frame_num - (int32_t)max_frame_num : (int32_t)entry_frame_num;
}

static void fill_va_picture(VAPictureH264 *pic, const H264DpbEntry *e) {
    memset(pic, 0, sizeof(*pic));
    pic->picture_id = e->frame->surface;
    pic->frame_idx = e->frame_num;
    pic->flags = VA_PICTURE_H264_SHORT_TERM_REFERENCE;
    pic->TopFieldOrderCnt = e->poc;
    pic->BottomFieldOrderCnt = e->poc;
}

uint32_t h264_dpb_fill_reference_frames(const H264Dpb *dpb, VAPictureH264 *out_refs, uint32_t capacity) {
    uint32_t n = 0;
    for (uint32_t i = 0; i < H264_DPB_MAX_ENTRIES && n < capacity; i++) {
        if (dpb->entries[i].in_use) {
            fill_va_picture(&out_refs[n], &dpb->entries[i]);
            n++;
        }
    }
    return n;
}

typedef struct SortKey {
    uint32_t entry_index;
    int32_t  key;
} SortKey;

static int cmp_desc(const void *a, const void *b) {
    int32_t ka = ((const SortKey *)a)->key;
    int32_t kb = ((const SortKey *)b)->key;
    return (ka < kb) - (ka > kb);
}

static int cmp_asc(const void *a, const void *b) {
    int32_t ka = ((const SortKey *)a)->key;
    int32_t kb = ((const SortKey *)b)->key;
    return (ka > kb) - (ka < kb);
}

static uint32_t default_list_p(const H264Dpb *dpb, uint32_t curr_frame_num, uint32_t max_frame_num, VAPictureH264 *out, uint32_t capacity) {
    SortKey keys[H264_DPB_MAX_ENTRIES];
    uint32_t n = 0;
    for (uint32_t i = 0; i < H264_DPB_MAX_ENTRIES; i++) {
        if (dpb->entries[i].in_use) {
            keys[n].entry_index = i;
            keys[n].key = frame_num_wrap(dpb->entries[i].frame_num, curr_frame_num, max_frame_num);
            n++;
        }
    }
    qsort(keys, n, sizeof(SortKey), cmp_desc); /* descending PicNum */

    uint32_t written = n < capacity ? n : capacity;
    for (uint32_t i = 0; i < written; i++) {
        fill_va_picture(&out[i], &dpb->entries[keys[i].entry_index]);
    }
    return written;
}

static uint32_t default_list_b(const H264Dpb *dpb, int32_t current_poc, VAPictureH264 *out, uint32_t capacity, int ascending_first) {
    SortKey lower[H264_DPB_MAX_ENTRIES]; /* poc < current, want descending */
    SortKey higher[H264_DPB_MAX_ENTRIES]; /* poc > current, want ascending */
    uint32_t nl = 0, nh = 0;

    for (uint32_t i = 0; i < H264_DPB_MAX_ENTRIES; i++) {
        if (!dpb->entries[i].in_use) continue;
        int32_t poc = dpb->entries[i].poc;
        if (poc < current_poc) {
            lower[nl].entry_index = i;
            lower[nl].key = poc;
            nl++;
        } else if (poc > current_poc) {
            higher[nh].entry_index = i;
            higher[nh].key = poc;
            nh++;
        }
    }
    qsort(lower, nl, sizeof(SortKey), cmp_desc);
    qsort(higher, nh, sizeof(SortKey), cmp_asc);

    uint32_t written = 0;
    if (ascending_first) {
        /* RefPicList1: ascending (poc > current) first, then descending (poc < current) */
        for (uint32_t i = 0; i < nh && written < capacity; i++) fill_va_picture(&out[written++], &dpb->entries[higher[i].entry_index]);
        for (uint32_t i = 0; i < nl && written < capacity; i++) fill_va_picture(&out[written++], &dpb->entries[lower[i].entry_index]);
    } else {
        /* RefPicList0: descending (poc < current) first, then ascending (poc > current) */
        for (uint32_t i = 0; i < nl && written < capacity; i++) fill_va_picture(&out[written++], &dpb->entries[lower[i].entry_index]);
        for (uint32_t i = 0; i < nh && written < capacity; i++) fill_va_picture(&out[written++], &dpb->entries[higher[i].entry_index]);
    }
    return written;
}

/* Applies spec 8.2.4.3.1 short-term reordering in place. `list`/`count`
 * describe the already-default-constructed list. */
static void apply_reordering(VAPictureH264 *list,
                              uint32_t *count,
                              uint32_t capacity,
                              const H264Dpb *dpb,
                              const H264RefPicListMod *ops,
                              int op_count,
                              uint32_t curr_frame_num,
                              uint32_t max_frame_num,
                              uint32_t num_ref_idx_active,
                              int *out_used_long_term) {
    int32_t pic_num_pred = (int32_t)curr_frame_num;
    uint32_t ref_idx = 0;

    for (int oi = 0; oi < op_count && ref_idx < num_ref_idx_active; oi++) {
        uint32_t idc = ops[oi].idc;
        if (idc == 2) {
            if (out_used_long_term) *out_used_long_term = 1;
            continue; /* long-term reordering not supported: no-op, per documented scope bound */
        }

        int32_t pic_num_no_wrap;
        int32_t abs_diff = (int32_t)ops[oi].value + 1;
        if (idc == 0) {
            pic_num_no_wrap = pic_num_pred - abs_diff;
            if (pic_num_no_wrap < 0) pic_num_no_wrap += (int32_t)max_frame_num;
        } else { /* idc == 1 */
            pic_num_no_wrap = pic_num_pred + abs_diff;
            if (pic_num_no_wrap >= (int32_t)max_frame_num) pic_num_no_wrap -= (int32_t)max_frame_num;
        }
        pic_num_pred = pic_num_no_wrap;

        int32_t pic_num = (pic_num_no_wrap > (int32_t)curr_frame_num) ? pic_num_no_wrap - (int32_t)max_frame_num : pic_num_no_wrap;

        /* Find the DPB entry with matching PicNum. */
        const H264DpbEntry *match = NULL;
        for (uint32_t i = 0; i < H264_DPB_MAX_ENTRIES; i++) {
            if (!dpb->entries[i].in_use) continue;
            if (frame_num_wrap(dpb->entries[i].frame_num, curr_frame_num, max_frame_num) == pic_num) {
                match = &dpb->entries[i];
                break;
            }
        }
        if (!match) {
            continue; /* referenced picture not in DPB: skip this command rather than fault */
        }

        VAPictureH264 moved;
        fill_va_picture(&moved, match);

        uint32_t shift_end = (*count < capacity) ? *count : capacity - 1;
        for (uint32_t i = shift_end; i > ref_idx; i--) {
            list[i] = list[i - 1];
        }
        list[ref_idx] = moved;
        if (*count < capacity) {
            (*count)++;
        }

        /* Remove any later duplicate of the same picture. */
        for (uint32_t i = ref_idx + 1; i < *count; i++) {
            if (list[i].picture_id == moved.picture_id) {
                for (uint32_t j = i; j + 1 < *count; j++) {
                    list[j] = list[j + 1];
                }
                (*count)--;
                break;
            }
        }

        ref_idx++;
    }
}

uint32_t h264_dpb_build_ref_pic_list0(const H264Dpb *dpb,
                                       const H264SliceHeader *sh,
                                       int32_t current_poc,
                                       uint32_t max_frame_num,
                                       VAPictureH264 *out_list,
                                       uint32_t capacity,
                                       int *out_used_long_term) {
    uint32_t count;
    if (sh->slice_type == H264_SLICE_TYPE_B) {
        count = default_list_b(dpb, current_poc, out_list, capacity, 0);
    } else {
        count = default_list_p(dpb, sh->frame_num, max_frame_num, out_list, capacity);
    }

    if (sh->rplm_l0_count > 0) {
        uint32_t num_active = sh->num_ref_idx_l0_active_minus1 + 1;
        apply_reordering(out_list, &count, capacity, dpb, sh->rplm_l0, sh->rplm_l0_count, sh->frame_num, max_frame_num, num_active, out_used_long_term);
    }
    return count;
}

uint32_t h264_dpb_build_ref_pic_list1(const H264Dpb *dpb,
                                       const H264SliceHeader *sh,
                                       int32_t current_poc,
                                       uint32_t max_frame_num,
                                       VAPictureH264 *out_list,
                                       uint32_t capacity,
                                       int *out_used_long_term) {
    if (sh->slice_type != H264_SLICE_TYPE_B) {
        return 0;
    }
    uint32_t count = default_list_b(dpb, current_poc, out_list, capacity, 1);

    if (sh->rplm_l1_count > 0) {
        uint32_t num_active = sh->num_ref_idx_l1_active_minus1 + 1;
        apply_reordering(out_list, &count, capacity, dpb, sh->rplm_l1, sh->rplm_l1_count, sh->frame_num, max_frame_num, num_active, out_used_long_term);
    }
    return count;
}

void h264_dpb_add_reference(H264Dpb *dpb, LinuxVideoFrame *frame, uint32_t frame_num, int32_t poc, uint32_t max_num_ref_frames, int is_reference) {
    if (!is_reference) {
        return;
    }
    if (max_num_ref_frames == 0) {
        max_num_ref_frames = 1;
    }

    /* Sliding window (spec 8.2.5.3): evict the entry with the smallest
     * FrameNumWrap relative to the incoming picture when at capacity. */
    uint32_t live_count = 0;
    for (uint32_t i = 0; i < H264_DPB_MAX_ENTRIES; i++) {
        if (dpb->entries[i].in_use) live_count++;
    }

    if (live_count >= max_num_ref_frames) {
        int32_t oldest_index = -1;
        int32_t oldest_wrap = 0;
        for (uint32_t i = 0; i < H264_DPB_MAX_ENTRIES; i++) {
            if (!dpb->entries[i].in_use) continue;
            /* frame_num values were all inserted under the same
             * MaxFrameNum epoch, so ordering relative to the incoming
             * picture's frame_num via a large modulus is sufficient to
             * pick the least-recent short-term reference. */
            int32_t wrap = (dpb->entries[i].frame_num > frame_num)
                                ? (int32_t)dpb->entries[i].frame_num - (int32_t)0x40000000
                                : (int32_t)dpb->entries[i].frame_num;
            if (oldest_index < 0 || wrap < oldest_wrap) {
                oldest_wrap = wrap;
                oldest_index = (int32_t)i;
            }
        }
        if (oldest_index >= 0) {
            linux_video_frame_release((MKFF_VideoFrame *)dpb->entries[oldest_index].frame);
            dpb->entries[oldest_index].in_use = 0;
        }
    }

    for (uint32_t i = 0; i < H264_DPB_MAX_ENTRIES; i++) {
        if (!dpb->entries[i].in_use) {
            dpb->entries[i].in_use = 1;
            dpb->entries[i].frame_num = frame_num;
            dpb->entries[i].poc = poc;
            dpb->entries[i].frame = (LinuxVideoFrame *)linux_video_frame_retain((MKFF_VideoFrame *)frame);
            return;
        }
    }
    /* Pool exhausted despite eviction attempt: extremely defensive path,
     * should be unreachable given max_num_ref_frames <= H264_DPB_MAX_ENTRIES. */
}

void h264_dpb_push_output(H264Dpb *dpb, LinuxVideoFrame *frame, int32_t poc) {
    if (dpb->output_count >= H264_OUTPUT_QUEUE_MAX) {
        return; /* defensive: should not happen given queue sizing vs DPB bound */
    }
    dpb->output_queue[dpb->output_count].frame = (LinuxVideoFrame *)linux_video_frame_retain((MKFF_VideoFrame *)frame);
    dpb->output_queue[dpb->output_count].poc = poc;
    dpb->output_count++;
}

static LinuxVideoFrame *pop_smallest_poc(H264Dpb *dpb) {
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
    return frame; /* ownership (the queue's reference) transfers to the caller */
}

LinuxVideoFrame *h264_dpb_bump_if_needed(H264Dpb *dpb, uint32_t max_reorder) {
    if (dpb->output_count <= max_reorder) {
        return NULL;
    }
    return pop_smallest_poc(dpb);
}

LinuxVideoFrame *h264_dpb_bump_one(H264Dpb *dpb) {
    return pop_smallest_poc(dpb);
}
