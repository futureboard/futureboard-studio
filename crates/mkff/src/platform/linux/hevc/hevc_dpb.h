#ifndef MKFF_LINUX_HEVC_DPB_H
#define MKFF_LINUX_HEVC_DPB_H

#include <stdint.h>
#include <va/va.h>

#include "src/codecs/hevc/hevc_slice.h"
#include "src/codecs/hevc/hevc_vps_sps_pps.h"
#include "src/platform/linux/video_frame.h"

#define HEVC_DPB_MAX_ENTRIES 16
#define HEVC_OUTPUT_QUEUE_MAX 17

typedef struct HevcDpbEntry {
    int              in_use;
    int32_t          poc;
    int              is_long_term;
    LinuxVideoFrame *frame; /* one reference owned by this DPB slot */
} HevcDpbEntry;

typedef struct HevcOutputEntry {
    LinuxVideoFrame *frame; /* one reference owned by the output queue */
    int32_t          poc;
} HevcOutputEntry;

typedef struct HevcDpb {
    HevcDpbEntry    entries[HEVC_DPB_MAX_ENTRIES];
    HevcOutputEntry output_queue[HEVC_OUTPUT_QUEUE_MAX];
    uint32_t        output_count;
} HevcDpb;

void hevc_dpb_init(HevcDpb *dpb);
void hevc_dpb_reset(HevcDpb *dpb);

/* Clears every reference slot (IDR / BLA with no_output). Optionally
 * also drops the pending output queue when no_output_of_prior_pics. */
void hevc_dpb_clear_references(HevcDpb *dpb, int drop_output);

/* Fills VAPictureParameterBufferHEVC::ReferenceFrames[15] from the DPB
 * using the active short-term RPS to set RPS_* flags. Unused slots are
 * marked VA_PICTURE_HEVC_INVALID. Returns number of valid entries. */
uint32_t hevc_dpb_fill_reference_frames(const HevcDpb *dpb,
                                         int32_t curr_poc,
                                         const HevcShortTermRps *rps,
                                         VAPictureHEVC *out_refs);

/* Builds RefPicList[0/1] indices into the 15-entry ReferenceFrames array
 * already filled by hevc_dpb_fill_reference_frames (default construction
 * per H.265 8.3.4; list modification is not applied). */
void hevc_dpb_build_ref_pic_lists(const VAPictureHEVC *refs,
                                   int32_t curr_poc,
                                   const HevcShortTermRps *rps,
                                   uint32_t slice_type,
                                   uint32_t num_ref_idx_l0_active_minus1,
                                   uint32_t num_ref_idx_l1_active_minus1,
                                   uint8_t RefPicList[2][15]);

/* After a picture is decoded: drop DPB refs that are not in the current
 * RPS, then insert the new picture when it is a reference NAL. */
void hevc_dpb_update_after_decode(HevcDpb *dpb,
                                   LinuxVideoFrame *frame,
                                   int32_t poc,
                                   const HevcShortTermRps *rps,
                                   int is_reference);

void hevc_dpb_push_output(HevcDpb *dpb, LinuxVideoFrame *frame, int32_t poc);
LinuxVideoFrame *hevc_dpb_bump_if_needed(HevcDpb *dpb, uint32_t max_reorder);
LinuxVideoFrame *hevc_dpb_bump_one(HevcDpb *dpb);

#endif /* MKFF_LINUX_HEVC_DPB_H */
