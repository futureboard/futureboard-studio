#ifndef MKFF_MACOS_IOSURFACE_H
#define MKFF_MACOS_IOSURFACE_H

#include <stdint.h>
#include "mkff/abi.h"
#include "mkff/error.h"
#include "mkff/export.h"
#include "mkff/types.h"

MKFF_BEGIN_DECLS

/*
 * Describes a decoded surface as an IOSurface, macOS's zero-copy GPU
 * buffer sharing primitive (the IOSurface/IOSurfaceRef analogue of
 * Linux's dma-buf). Deliberately declared with plain C types rather
 * than pulling in <IOSurface/IOSurface.h>: `surface` is an
 * IOSurfaceRef under the hood, opaque here so this header stays
 * self-contained. Cast it back after including the real Apple header.
 */
typedef struct MKFF_MacosIOSurfaceDesc {
    MKFF_STRUCT_HEADER;

    uint32_t width;
    uint32_t height;
    uint32_t pixel_format; /* OSType, e.g. kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange */

    uint32_t io_surface_id; /* global IOSurfaceID, valid for IOSurfaceLookup() across processes */

    /* IOSurfaceRef, CFRetain'd once for the caller. The caller must
     * CFRelease() (equivalently, IOSurfaceRelease-via-CFRelease) it
     * when done — this is the same underlying GPU buffer the decoder's
     * surface pool uses, not a copy. */
    void *surface;
} MKFF_MacosIOSurfaceDesc;

/* Exports the surface backing `frame` as an IOSurface. Mirrors
 * mkff_linux_video_frame_export_dmabuf(): no CPU pixel copy occurs. */
MKFF_API MKFF_Result mkff_macos_video_frame_export_iosurface(const MKFF_VideoFrame *frame, MKFF_MacosIOSurfaceDesc *out_desc);

/* Releases the CFRetain taken by export (CFRelease(desc->surface)) and
 * zeroes the descriptor. Safe to call on an already-closed descriptor. */
MKFF_API void mkff_macos_iosurface_desc_close(MKFF_MacosIOSurfaceDesc *desc);

MKFF_END_DECLS

#endif /* MKFF_MACOS_IOSURFACE_H */
