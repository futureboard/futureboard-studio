#include "video_frame.h"

#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <va/va_drmcommon.h>

#include "decoder_shared.h"
#include "platform_api_table.h"

LinuxVideoFrame *linux_video_frame_create(DecoderShared *shared,
                                           uint32_t pool_index,
                                           VASurfaceID surface,
                                           uint32_t width,
                                           uint32_t height,
                                           int64_t pts,
                                           int64_t dts,
                                           int is_key_frame,
                                           MKFF_PixelFormat format) {
    LinuxVideoFrame *frame = (LinuxVideoFrame *)calloc(1, sizeof(LinuxVideoFrame));
    if (!frame) return NULL;

    frame->common.api = mkff_linux_platform_api();
    frame->shared = decoder_shared_ref(shared);
    frame->pool_index = pool_index;
    frame->surface = surface;
    atomic_init(&frame->refcount, 1);

    MKFF_INIT_STRUCT_HEADER(&frame->info);
    frame->info.width = width;
    frame->info.height = height;
    frame->info.format = format;
    frame->info.pts = pts;
    frame->info.dts = dts;
    frame->info.is_key_frame = is_key_frame ? 1u : 0u;

    return frame;
}

MKFF_VideoFrame *linux_video_frame_retain(MKFF_VideoFrame *handle) {
    LinuxVideoFrame *frame = (LinuxVideoFrame *)handle;
    if (frame) {
        atomic_fetch_add_explicit(&frame->refcount, 1, memory_order_relaxed);
    }
    return handle;
}

static void linux_video_frame_release_cpu_map(LinuxVideoFrame *frame) {
    if (!frame || !frame->cpu_mapped || !frame->shared || !frame->shared->va_dpy) {
        return;
    }
    VADisplay dpy = frame->shared->va_dpy;
    (void)vaUnmapBuffer(dpy, frame->cpu_image.buf);
    (void)vaDestroyImage(dpy, frame->cpu_image.image_id);
    memset(&frame->cpu_image, 0, sizeof(frame->cpu_image));
    frame->cpu_mapped = 0;
}

void linux_video_frame_release(MKFF_VideoFrame *handle) {
    LinuxVideoFrame *frame = (LinuxVideoFrame *)handle;
    if (!frame) return;

    if (atomic_fetch_sub_explicit(&frame->refcount, 1, memory_order_acq_rel) == 1) {
        linux_video_frame_release_cpu_map(frame);
        /* Last reference: the VA surface can now return to the pool. */
        decoder_shared_pool_release(frame->shared, frame->pool_index);
        decoder_shared_unref(frame->shared);
        free(frame);
    }
}

MKFF_Result linux_video_frame_get_info(const MKFF_VideoFrame *handle, MKFF_VideoFrameInfo *out_info) {
    const LinuxVideoFrame *frame = (const LinuxVideoFrame *)handle;
    uint32_t requested_size = out_info->struct_size;
    MKFF_INIT_STRUCT_HEADER(out_info);
    *out_info = frame->info;
    out_info->struct_size = requested_size ? requested_size : (uint32_t)sizeof(*out_info);
    return MKFF_RESULT_OK;
}

MKFF_Result linux_video_frame_export_dmabuf(const MKFF_VideoFrame *handle, MKFF_LinuxDmaBufDesc *out_desc) {
    const LinuxVideoFrame *frame = (const LinuxVideoFrame *)handle;
    DecoderShared *shared = frame->shared;

    /* vaExportSurfaceHandle() is a synchronization point equivalent to
     * vaSyncSurface(), so no explicit sync is needed here even though
     * decode already completed synchronously before this frame was
     * handed to the caller. */
    VADRMPRIMESurfaceDescriptor va_desc;
    memset(&va_desc, 0, sizeof(va_desc));

    VAStatus status = vaExportSurfaceHandle(shared->va_dpy,
                                             frame->surface,
                                             VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
                                             VA_EXPORT_SURFACE_READ_ONLY | VA_EXPORT_SURFACE_COMPOSED_LAYERS,
                                             &va_desc);
    if (status != VA_STATUS_SUCCESS) {
        decoder_shared_set_error(shared, vaErrorStr(status));
        return MKFF_RESULT_ERROR_DEVICE;
    }

    uint32_t requested_size = out_desc->struct_size;
    memset(out_desc, 0, sizeof(*out_desc));
    MKFF_INIT_STRUCT_HEADER(out_desc);
    out_desc->struct_size = requested_size ? requested_size : (uint32_t)sizeof(*out_desc);

    out_desc->drm_fourcc = va_desc.fourcc;
    out_desc->width = va_desc.width;
    out_desc->height = va_desc.height;

    out_desc->num_objects = va_desc.num_objects;
    if (out_desc->num_objects > MKFF_LINUX_DMABUF_MAX_OBJECTS) {
        out_desc->num_objects = MKFF_LINUX_DMABUF_MAX_OBJECTS;
    }
    for (uint32_t i = 0; i < out_desc->num_objects; i++) {
        out_desc->objects[i].fd = va_desc.objects[i].fd; /* already a fresh, caller-owned dup per vaExportSurfaceHandle contract */
        out_desc->objects[i].size = va_desc.objects[i].size;
        out_desc->objects[i].modifier = va_desc.objects[i].drm_format_modifier;
    }

    uint32_t plane_count = 0;
    for (uint32_t l = 0; l < va_desc.num_layers && plane_count < MKFF_LINUX_DMABUF_MAX_PLANES; l++) {
        for (uint32_t p = 0; p < va_desc.layers[l].num_planes && plane_count < MKFF_LINUX_DMABUF_MAX_PLANES; p++) {
            out_desc->planes[plane_count].object_index = va_desc.layers[l].object_index[p];
            out_desc->planes[plane_count].offset = va_desc.layers[l].offset[p];
            out_desc->planes[plane_count].pitch = va_desc.layers[l].pitch[p];
            plane_count++;
        }
    }
    out_desc->num_planes = plane_count;

    return MKFF_RESULT_OK;
}

#ifndef VA_FOURCC_P010
#define VA_FOURCC_P010 0x30313050 /* 'P010' */
#endif

MKFF_Result linux_video_frame_map_cpu_planes(const MKFF_VideoFrame *handle, MKFF_CpuPlaneDesc *out_planes) {
    LinuxVideoFrame *frame = (LinuxVideoFrame *)handle;
    if (!frame || !out_planes || !frame->shared) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }
    if (frame->cpu_mapped) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT; /* already mapped */
    }

    DecoderShared *shared = frame->shared;
    if (!shared->va_dpy) {
        return MKFF_RESULT_ERROR_DEVICE;
    }
    if (frame->info.format != MKFF_PIXEL_FORMAT_NV12 && frame->info.format != MKFF_PIXEL_FORMAT_P010) {
        return MKFF_RESULT_ERROR_NOT_SUPPORTED;
    }

    /* Decode already syncs before handing out frames, but re-sync so a
     * late map after more submit/receive work still sees completed pixels. */
    VAStatus status = vaSyncSurface(shared->va_dpy, frame->surface);
    if (status != VA_STATUS_SUCCESS) {
        decoder_shared_set_error(shared, vaErrorStr(status));
        return MKFF_RESULT_ERROR_DEVICE;
    }

    VAImage image;
    memset(&image, 0, sizeof(image));
    status = vaDeriveImage(shared->va_dpy, frame->surface, &image);
    if (status != VA_STATUS_SUCCESS) {
        /* Surface is not CPU-mappable via derive (common with some
         * tiled/compressed backends). Do not invent a second download path. */
        return MKFF_RESULT_ERROR_NOT_SUPPORTED;
    }

    if ((image.format.fourcc != VA_FOURCC_NV12 && image.format.fourcc != VA_FOURCC_P010) ||
        image.num_planes < 2) {
        (void)vaDestroyImage(shared->va_dpy, image.image_id);
        return MKFF_RESULT_ERROR_NOT_SUPPORTED;
    }

    void *buf = NULL;
    status = vaMapBuffer(shared->va_dpy, image.buf, &buf);
    if (status != VA_STATUS_SUCCESS || !buf) {
        (void)vaDestroyImage(shared->va_dpy, image.image_id);
        return MKFF_RESULT_ERROR_NOT_SUPPORTED;
    }

    frame->cpu_image = image;
    frame->cpu_mapped = 1;

    const uint8_t *base = (const uint8_t *)buf;

    uint32_t requested_size = out_planes->struct_size;
    MKFF_INIT_STRUCT_HEADER(out_planes);
    if (requested_size) {
        out_planes->struct_size = requested_size;
    }
    out_planes->format = frame->info.format;
    out_planes->width = frame->info.width;
    out_planes->height = frame->info.height;
    out_planes->plane_count = 2;
    out_planes->data[0] = base + image.offsets[0];
    out_planes->data[1] = base + image.offsets[1];
    out_planes->stride[0] = image.pitches[0];
    out_planes->stride[1] = image.pitches[1];
    out_planes->height_lines[0] = frame->info.height;
    out_planes->height_lines[1] = frame->info.height / 2u;
    out_planes->data[2] = NULL;
    out_planes->data[3] = NULL;
    out_planes->stride[2] = 0;
    out_planes->stride[3] = 0;
    out_planes->height_lines[2] = 0;
    out_planes->height_lines[3] = 0;

    return MKFF_RESULT_OK;
}

void linux_video_frame_unmap_cpu_planes(const MKFF_VideoFrame *handle, MKFF_CpuPlaneDesc *planes) {
    LinuxVideoFrame *frame = (LinuxVideoFrame *)handle;
    if (!frame) {
        return;
    }

    linux_video_frame_release_cpu_map(frame);

    if (planes) {
        for (uint32_t i = 0; i < MKFF_CPU_PLANES_MAX; i++) {
            planes->data[i] = NULL;
            planes->stride[i] = 0;
            planes->height_lines[i] = 0;
        }
        planes->plane_count = 0;
    }
}
