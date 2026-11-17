#include "d3d11_device.h"

#include <stdio.h>

static void set_err(char *err_buf, size_t err_buf_size, const char *msg) {
    if (err_buf && err_buf_size > 0) {
        snprintf(err_buf, err_buf_size, "%s", msg);
    }
}

MKFF_Result windows_create_d3d11_device(ID3D11Device **out_device, ID3D11DeviceContext **out_device_context, char *err_buf, size_t err_buf_size) {
    static const D3D_FEATURE_LEVEL kFeatureLevels[] = {
        D3D_FEATURE_LEVEL_11_1,
        D3D_FEATURE_LEVEL_11_0,
    };
    D3D_FEATURE_LEVEL achieved_level;

    HRESULT hr = D3D11CreateDevice(NULL, /* default adapter */
                                    D3D_DRIVER_TYPE_HARDWARE,
                                    NULL,
                                    D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                                    kFeatureLevels,
                                    ARRAYSIZE(kFeatureLevels),
                                    D3D11_SDK_VERSION,
                                    out_device,
                                    &achieved_level,
                                    out_device_context);
    if (FAILED(hr)) {
        char msg[128];
        snprintf(msg, sizeof(msg), "D3D11CreateDevice failed: hr=0x%08lx", (unsigned long)hr);
        set_err(err_buf, err_buf_size, msg);
        return MKFF_RESULT_ERROR_DEVICE;
    }
    return MKFF_RESULT_OK;
}

int windows_select_h264_decoder_guid(ID3D11VideoDevice *video_device, GUID *out_guid) {
    UINT profile_count = ID3D11VideoDevice_GetVideoDecoderProfileCount(video_device);
    for (UINT i = 0; i < profile_count; i++) {
        GUID profile;
        if (FAILED(ID3D11VideoDevice_GetVideoDecoderProfile(video_device, i, &profile))) {
            continue;
        }
        if (IsEqualGUID(&profile, &DXVA2_ModeH264_VLD_NoFGT)) {
            *out_guid = profile;
            return 1;
        }
    }
    return 0;
}

int windows_select_hevc_decoder_guid(ID3D11VideoDevice *video_device, uint32_t bit_depth, GUID *out_guid) {
    const GUID *wanted = (bit_depth == 10) ? &DXVA_ModeHEVC_VLD_Main10 : &DXVA_ModeHEVC_VLD_Main;
    UINT profile_count = ID3D11VideoDevice_GetVideoDecoderProfileCount(video_device);
    for (UINT i = 0; i < profile_count; i++) {
        GUID profile;
        if (FAILED(ID3D11VideoDevice_GetVideoDecoderProfile(video_device, i, &profile))) {
            continue;
        }
        if (IsEqualGUID(&profile, wanted)) {
            *out_guid = profile;
            return 1;
        }
    }
    return 0;
}

int windows_check_nv12_supported(ID3D11VideoDevice *video_device, const GUID *profile) {
    return windows_check_decoder_format_supported(video_device, profile, DXGI_FORMAT_NV12);
}

int windows_check_decoder_format_supported(ID3D11VideoDevice *video_device, const GUID *profile, DXGI_FORMAT format) {
    BOOL supported = FALSE;
    HRESULT hr = ID3D11VideoDevice_CheckVideoDecoderFormat(video_device, profile, format, &supported);
    return SUCCEEDED(hr) && supported;
}
