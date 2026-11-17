#ifndef MKFF_LINUX_DMABUF_H
#define MKFF_LINUX_DMABUF_H

#include <stdint.h>
#include "mkff/abi.h"
#include "mkff/error.h"
#include "mkff/export.h"
#include "mkff/types.h"

MKFF_BEGIN_DECLS

#define MKFF_LINUX_DMABUF_MAX_OBJECTS 4
#define MKFF_LINUX_DMABUF_MAX_PLANES  4

/* One dma-buf file descriptor (one GEM/dma-buf object). A single NV12
 * surface exported by VA-API typically yields a single object shared by
 * both the luma and chroma planes. */
typedef struct MKFF_LinuxDmaBufObject {
    int      fd;       /* caller-owned; duplicated for the caller, must be closed */
    uint32_t size;      /* total allocation size backing this object, bytes */
    uint64_t modifier;  /* DRM format modifier (e.g. DRM_FORMAT_MOD_LINEAR) */
} MKFF_LinuxDmaBufObject;

typedef struct MKFF_LinuxDmaBufPlane {
    uint32_t object_index; /* index into objects[] */
    uint32_t offset;       /* byte offset of this plane within the object */
    uint32_t pitch;        /* row stride in bytes */
    uint32_t pad0;
} MKFF_LinuxDmaBufPlane;

/*
 * Describes a GPU-native decoded surface as an importable set of dma-buf
 * objects (à la VADRMPRIMESurfaceDescriptor / EGL_EXT_image_dma_buf_import).
 * No CPU-side pixel data is ever copied to produce this descriptor.
 */
typedef struct MKFF_LinuxDmaBufDesc {
    MKFF_STRUCT_HEADER;

    uint32_t drm_fourcc; /* DRM_FORMAT_* composite fourcc, e.g. NV12 */
    uint32_t width;
    uint32_t height;

    uint32_t num_objects;
    MKFF_LinuxDmaBufObject objects[MKFF_LINUX_DMABUF_MAX_OBJECTS];

    uint32_t num_planes;
    MKFF_LinuxDmaBufPlane planes[MKFF_LINUX_DMABUF_MAX_PLANES];
} MKFF_LinuxDmaBufDesc;

/* Exports the surface backing `frame` as dma-buf objects. The returned
 * file descriptors are duplicates owned by the caller: the caller must
 * release them with mkff_linux_dmabuf_desc_close() (or close(2) each fd
 * individually, being careful about objects shared by multiple planes).
 * The originating MKFF_VideoFrame reference is untouched and may be
 * released independently; the exported fds remain valid GPU buffer
 * references after the frame reference is dropped, per dma-buf semantics. */
MKFF_API MKFF_Result mkff_linux_video_frame_export_dmabuf(const MKFF_VideoFrame *frame,
                                                            MKFF_LinuxDmaBufDesc *out_desc);

/* Closes every unique fd referenced by `desc` and zeroes it. Safe to call
 * on an already-closed (zeroed) descriptor. */
MKFF_API void mkff_linux_dmabuf_desc_close(MKFF_LinuxDmaBufDesc *desc);

MKFF_END_DECLS

#endif /* MKFF_LINUX_DMABUF_H */
