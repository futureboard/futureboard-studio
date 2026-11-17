#include "mkff/macos/iosurface.h"

#include <CoreFoundation/CoreFoundation.h>
#include <string.h>

#include "context_internal.h"
#include "src/common/mkff_common.h"

MKFF_Result mkff_macos_video_frame_export_iosurface(const MKFF_VideoFrame *frame, MKFF_MacosIOSurfaceDesc *out_desc) {
    if (!frame || !out_desc) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }
    const MKFF_HandleCommon *common = (const MKFF_HandleCommon *)frame;
    if (!common->api->video_frame_export_iosurface) {
        return MKFF_RESULT_ERROR_NOT_SUPPORTED;
    }
    return common->api->video_frame_export_iosurface(frame, out_desc);
}

void mkff_macos_iosurface_desc_close(MKFF_MacosIOSurfaceDesc *desc) {
    if (!desc) return;
    if (desc->surface) {
        CFRelease((CFTypeRef)desc->surface);
    }
    memset(desc, 0, sizeof(*desc));
}
