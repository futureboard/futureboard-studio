#ifndef MKFF_WINDOWS_D3D11_H
#define MKFF_WINDOWS_D3D11_H

#include <stdint.h>
#include "mkff/abi.h"
#include "mkff/error.h"
#include "mkff/export.h"
#include "mkff/types.h"

MKFF_BEGIN_DECLS

/*
 * Describes a decoded surface as a Direct3D 11 texture. D3D11 video
 * decode surfaces live as array slices of one texture array (the
 * "surface pool"), not individual textures, so `array_slice` identifies
 * which slice this frame occupies. Declared with plain C types rather
 * than pulling in <d3d11.h>: `texture`/`shared_handle` are
 * ID3D11Texture2D* / HANDLE under the hood, opaque here so this header
 * stays self-contained. Cast them back after including the real
 * Windows SDK headers.
 */
typedef struct MKFF_WindowsD3D11TextureDesc {
    MKFF_STRUCT_HEADER;

    uint32_t width;
    uint32_t height;
    uint32_t dxgi_format; /* DXGI_FORMAT, e.g. DXGI_FORMAT_NV12 */
    uint32_t array_slice; /* index into the texture array this surface occupies */

    /* ID3D11Texture2D*, AddRef'd once for the caller. The caller must
     * Release() it when done — this is the same underlying resource
     * the decoder's surface pool uses, not a copy. Only valid for
     * import into the same D3D11 device the decoder itself uses (see
     * `shared_handle` for cross-device import). */
    void *texture;

    /* OS-level shared handle from IDXGIResource1::CreateSharedHandle(),
     * for importing into a *different* D3D11/D3D12 device (or a
     * Vulkan/wgpu device via VK_KHR_external_memory_win32) than the one
     * the decoder used. NULL if the platform module could not create
     * one. The caller must CloseHandle() it when done. */
    void *shared_handle;
} MKFF_WindowsD3D11TextureDesc;

/* Exports the surface backing `frame` as a D3D11 texture. Mirrors
 * mkff_linux_video_frame_export_dmabuf(): no CPU pixel copy occurs. */
MKFF_API MKFF_Result mkff_windows_video_frame_export_d3d11_texture(const MKFF_VideoFrame *frame, MKFF_WindowsD3D11TextureDesc *out_desc);

/* Releases the reference/handle taken by export (Release()s `texture`,
 * CloseHandle()s `shared_handle` if non-NULL) and zeroes the
 * descriptor. Safe to call on an already-closed descriptor. */
MKFF_API void mkff_windows_d3d11_texture_desc_close(MKFF_WindowsD3D11TextureDesc *desc);

MKFF_END_DECLS

#endif /* MKFF_WINDOWS_D3D11_H */
