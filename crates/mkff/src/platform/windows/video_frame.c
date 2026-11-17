#include "video_frame.h"

#include <stdlib.h>
#include <string.h>
#include <unknwn.h>

#include "decoder_shared.h"
#include "platform_api_table.h"

WindowsVideoFrame *windows_video_frame_create(DecoderShared *shared,
                                               uint32_t array_slice,
                                               uint32_t width,
                                               uint32_t height,
                                               MKFF_PixelFormat format,
                                               int64_t pts,
                                               int64_t dts,
                                               int is_key_frame) {
    WindowsVideoFrame *frame = (WindowsVideoFrame *)calloc(1, sizeof(WindowsVideoFrame));
    if (!frame) return NULL;

    frame->common.api = mkff_windows_platform_api();
    frame->shared = decoder_shared_ref(shared);
    frame->array_slice = array_slice;
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

MKFF_VideoFrame *windows_video_frame_retain(MKFF_VideoFrame *handle) {
    WindowsVideoFrame *frame = (WindowsVideoFrame *)handle;
    if (frame) {
        atomic_fetch_add_explicit(&frame->refcount, 1, memory_order_relaxed);
    }
    return handle;
}

static void windows_video_frame_release_staging(WindowsVideoFrame *frame) {
    if (!frame || !frame->staging) {
        return;
    }
    if (frame->shared && frame->shared->device_context) {
        ID3D11DeviceContext_Unmap(frame->shared->device_context, (ID3D11Resource *)frame->staging, 0);
    }
    IUnknown_Release((IUnknown *)frame->staging);
    frame->staging = NULL;
}

void windows_video_frame_release(MKFF_VideoFrame *handle) {
    WindowsVideoFrame *frame = (WindowsVideoFrame *)handle;
    if (!frame) return;

    if (atomic_fetch_sub_explicit(&frame->refcount, 1, memory_order_acq_rel) == 1) {
        windows_video_frame_release_staging(frame);
        decoder_shared_pool_release(frame->shared, frame->array_slice);
        decoder_shared_unref(frame->shared);
        free(frame);
    }
}

MKFF_Result windows_video_frame_get_info(const MKFF_VideoFrame *handle, MKFF_VideoFrameInfo *out_info) {
    const WindowsVideoFrame *frame = (const WindowsVideoFrame *)handle;
    uint32_t requested_size = out_info->struct_size;
    MKFF_INIT_STRUCT_HEADER(out_info);
    *out_info = frame->info;
    out_info->struct_size = requested_size ? requested_size : (uint32_t)sizeof(*out_info);
    return MKFF_RESULT_OK;
}

MKFF_Result windows_video_frame_export_d3d11_texture(const MKFF_VideoFrame *handle, MKFF_WindowsD3D11TextureDesc *out_desc) {
    const WindowsVideoFrame *frame = (const WindowsVideoFrame *)handle;
    DecoderShared *shared = frame->shared;

    IDXGIResource1 *dxgi_resource = NULL;
    HRESULT hr = IUnknown_QueryInterface((IUnknown *)shared->texture_array, &IID_IDXGIResource1, (void **)&dxgi_resource);
    if (FAILED(hr)) {
        decoder_shared_set_error(shared, "QueryInterface(IDXGIResource1) failed on the decoder's texture array");
        return MKFF_RESULT_ERROR_DEVICE;
    }

    HANDLE shared_handle = NULL;
    hr = IDXGIResource1_CreateSharedHandle(dxgi_resource, NULL, DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE, NULL, &shared_handle);
    IUnknown_Release((IUnknown *)dxgi_resource);
    if (FAILED(hr)) {
        decoder_shared_set_error(shared, "IDXGIResource1::CreateSharedHandle failed");
        return MKFF_RESULT_ERROR_DEVICE;
    }

    IUnknown_AddRef((IUnknown *)shared->texture_array);

    uint32_t requested_size = out_desc->struct_size;
    memset(out_desc, 0, sizeof(*out_desc));
    MKFF_INIT_STRUCT_HEADER(out_desc);
    out_desc->struct_size = requested_size ? requested_size : (uint32_t)sizeof(*out_desc);

    out_desc->width = frame->info.width;
    out_desc->height = frame->info.height;
    out_desc->dxgi_format = shared->dxgi_format ? (uint32_t)shared->dxgi_format : (uint32_t)DXGI_FORMAT_NV12;
    out_desc->array_slice = frame->array_slice;
    out_desc->texture = shared->texture_array;
    out_desc->shared_handle = shared_handle;

    return MKFF_RESULT_OK;
}

MKFF_Result windows_video_frame_map_cpu_planes(const MKFF_VideoFrame *handle, MKFF_CpuPlaneDesc *out_planes) {
    WindowsVideoFrame *frame = (WindowsVideoFrame *)handle;
    if (!frame || !out_planes || !frame->shared) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }
    if (frame->staging) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT; /* already mapped */
    }

    DecoderShared *shared = frame->shared;
    if (!shared->device || !shared->device_context || !shared->texture_array) {
        return MKFF_RESULT_ERROR_DEVICE;
    }

    DXGI_FORMAT dxgi_format = shared->dxgi_format ? shared->dxgi_format : DXGI_FORMAT_NV12;
    if (dxgi_format != DXGI_FORMAT_NV12 && dxgi_format != DXGI_FORMAT_P010) {
        return MKFF_RESULT_ERROR_NOT_SUPPORTED;
    }
    if (frame->info.format != MKFF_PIXEL_FORMAT_NV12 && frame->info.format != MKFF_PIXEL_FORMAT_P010) {
        return MKFF_RESULT_ERROR_NOT_SUPPORTED;
    }

    uint32_t tex_w = shared->coded_width ? shared->coded_width : frame->info.width;
    uint32_t tex_h = shared->coded_height ? shared->coded_height : frame->info.height;
    if (tex_w == 0 || tex_h == 0) {
        return MKFF_RESULT_ERROR_DEVICE;
    }

    D3D11_TEXTURE2D_DESC staging_desc;
    memset(&staging_desc, 0, sizeof(staging_desc));
    staging_desc.Width = tex_w;
    staging_desc.Height = tex_h;
    staging_desc.MipLevels = 1;
    staging_desc.ArraySize = 1;
    staging_desc.Format = dxgi_format;
    staging_desc.SampleDesc.Count = 1;
    staging_desc.Usage = D3D11_USAGE_STAGING;
    staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;

    ID3D11Texture2D *staging = NULL;
    HRESULT hr = ID3D11Device_CreateTexture2D(shared->device, &staging_desc, NULL, &staging);
    if (FAILED(hr) || !staging) {
        decoder_shared_set_error(shared, "ID3D11Device::CreateTexture2D failed for CPU readback staging");
        return MKFF_RESULT_ERROR_DEVICE;
    }

    /* MipLevels == 1 on the decode texture array, so subresource == array slice. */
    UINT src_subresource = frame->array_slice;
    ID3D11DeviceContext_CopySubresourceRegion(shared->device_context,
                                              (ID3D11Resource *)staging,
                                              0,
                                              0,
                                              0,
                                              0,
                                              (ID3D11Resource *)shared->texture_array,
                                              src_subresource,
                                              NULL);
    /* Ensure the copy is submitted before Map; Map still synchronizes, but
     * Flush avoids stale/empty readback on some driver paths. */
    ID3D11DeviceContext_Flush(shared->device_context);

    D3D11_MAPPED_SUBRESOURCE mapped;
    memset(&mapped, 0, sizeof(mapped));
    hr = ID3D11DeviceContext_Map(shared->device_context,
                                 (ID3D11Resource *)staging,
                                 0,
                                 D3D11_MAP_READ,
                                 0,
                                 &mapped);
    if (FAILED(hr) || !mapped.pData || mapped.RowPitch == 0) {
        IUnknown_Release((IUnknown *)staging);
        decoder_shared_set_error(shared, "ID3D11DeviceContext::Map failed on CPU readback staging");
        return MKFF_RESULT_ERROR_DEVICE;
    }

    /* NV12/P010: one resource, Y then UV. UV starts at row `tex_h` with the
     * same RowPitch as Y (DepthPitch is typically the full Y+UV size — do
     * not use it as the UV base). */
    const uint32_t bytes_per_sample = (dxgi_format == DXGI_FORMAT_P010) ? 2u : 1u;
    const uint32_t min_row_bytes = frame->info.width * bytes_per_sample;
    if (mapped.RowPitch < min_row_bytes) {
        ID3D11DeviceContext_Unmap(shared->device_context, (ID3D11Resource *)staging, 0);
        IUnknown_Release((IUnknown *)staging);
        decoder_shared_set_error(shared, "D3D11 staging RowPitch smaller than display width");
        return MKFF_RESULT_ERROR_DEVICE;
    }

    frame->staging = staging;

    uint32_t requested_size = out_planes->struct_size;
    MKFF_INIT_STRUCT_HEADER(out_planes);
    if (requested_size) {
        out_planes->struct_size = requested_size;
    }
    out_planes->format = frame->info.format;
    out_planes->width = frame->info.width;
    out_planes->height = frame->info.height;
    out_planes->plane_count = 2;
    out_planes->data[0] = (const uint8_t *)mapped.pData;
    out_planes->data[1] = (const uint8_t *)mapped.pData + (size_t)mapped.RowPitch * (size_t)tex_h;
    out_planes->stride[0] = (uint32_t)mapped.RowPitch;
    out_planes->stride[1] = (uint32_t)mapped.RowPitch;
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

void windows_video_frame_unmap_cpu_planes(const MKFF_VideoFrame *handle, MKFF_CpuPlaneDesc *planes) {
    WindowsVideoFrame *frame = (WindowsVideoFrame *)handle;
    if (!frame) {
        return;
    }

    windows_video_frame_release_staging(frame);

    if (planes) {
        for (uint32_t i = 0; i < MKFF_CPU_PLANES_MAX; i++) {
            planes->data[i] = NULL;
            planes->stride[i] = 0;
            planes->height_lines[i] = 0;
        }
        planes->plane_count = 0;
    }
}
