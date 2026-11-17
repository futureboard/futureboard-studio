#ifndef MKFF_WINDOWS_HEVC_DXVA_DPB_H
#define MKFF_WINDOWS_HEVC_DXVA_DPB_H

#include <stdint.h>

#include "src/codecs/hevc/hevc_vps_sps_pps.h"
#include "video_frame.h" /* pulls in win_common.h (and <dxva.h>) in the right order */

#define HEVC_DXVA_DPB_MAX_ENTRIES 16
#define HEVC_DXVA_OUTPUT_QUEUE_MAX 17
#define HEVC_DXVA_REF_PIC_LIST_SIZE 15
#define HEVC_DXVA_REF_SET_SIZE 8

/*
 * DXVA HEVC short-slice-control decode has the hardware reparse each
 * slice_segment_header() from the Annex-B NAL bytes we submit, but the
 * host must still fill DXVA_PicParams_HEVC::RefPicList /
 * PicOrderCntValList / RefPicSetStCurr{Before,After} / RefPicSetLtCurr
 * from the active short-term RPS (and any long-term refs). This DPB
 * tracks short-term reference frames by POC, applies RPS marking, and
 * does POC-based output reordering.
 */
typedef struct HevcDxvaDpbEntry {
    int                in_use;
    int                is_long_term;
    int32_t            poc;
    WindowsVideoFrame *frame; /* one reference owned by this DPB slot */
} HevcDxvaDpbEntry;

typedef struct HevcDxvaOutputEntry {
    WindowsVideoFrame *frame; /* one reference owned by the output queue */
    int32_t            poc;
} HevcDxvaOutputEntry;

typedef struct HevcDxvaDpb {
    HevcDxvaDpbEntry    entries[HEVC_DXVA_DPB_MAX_ENTRIES];
    HevcDxvaOutputEntry output_queue[HEVC_DXVA_OUTPUT_QUEUE_MAX];
    uint32_t            output_count;
    UINT                status_report_feedback;
} HevcDxvaDpb;

void hevc_dxva_dpb_init(HevcDxvaDpb *dpb);

/* Releases every frame reference still held by the DPB and the pending
 * output queue. Call once on decoder teardown. */
void hevc_dxva_dpb_reset(HevcDxvaDpb *dpb);

/* Clears reference and output queues (IDR / no_output_of_prior_pics). */
void hevc_dxva_dpb_clear(HevcDxvaDpb *dpb);

/* Applies the active short-term RPS: drops DPB entries whose POC is not
 * listed, keeping matches as short-term refs. Long-term entries are
 * cleared (this milestone does not parse LT POC lists into the DPB). */
void hevc_dxva_dpb_apply_st_rps(HevcDxvaDpb *dpb, int32_t curr_poc, const HevcShortTermRps *rps);

/* Inserts a newly-decoded short-term reference picture. Retains `frame`.
 * No-op when `is_reference` is 0. */
void hevc_dxva_dpb_add_reference(HevcDxvaDpb *dpb, WindowsVideoFrame *frame, int32_t poc, int is_reference);

/* Fills DXVA_PicParams_HEVC reference fields from the current DPB + RPS.
 * Unused RefPicList slots are set to bPicEntry=0xFF; unused RefPicSet*
 * slots to 0xFF. Returns the next StatusReportFeedbackNumber. */
UINT hevc_dxva_dpb_fill_pic_params_refs(HevcDxvaDpb *dpb,
                                         int32_t curr_poc,
                                         const HevcShortTermRps *rps,
                                         DXVA_PicEntry_HEVC *out_ref_pic_list,
                                         INT *out_poc_list,
                                         UCHAR *out_st_curr_before,
                                         UCHAR *out_st_curr_after,
                                         UCHAR *out_lt_curr);

void hevc_dxva_dpb_push_output(HevcDxvaDpb *dpb, WindowsVideoFrame *frame, int32_t poc);

WindowsVideoFrame *hevc_dxva_dpb_bump_if_needed(HevcDxvaDpb *dpb, uint32_t max_reorder);
WindowsVideoFrame *hevc_dxva_dpb_bump_one(HevcDxvaDpb *dpb);

#endif /* MKFF_WINDOWS_HEVC_DXVA_DPB_H */
