#include "decoder.h"

#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "d3d11_device.h"
#include "decoder_shared.h"
#include "h264_dxva_dpb.h"
#include "hevc_dxva_dpb.h"
#include "platform_api_table.h"
#include "platform_context.h"
#include "src/codecs/h264/h264_bitstream.h"
#include "src/codecs/h264/h264_slice.h"
#include "src/codecs/h264/h264_sps_pps.h"
#include "src/codecs/hevc/hevc_bitstream.h"
#include "src/codecs/hevc/hevc_slice.h"
#include "src/codecs/hevc/hevc_vps_sps_pps.h"
#include "src/common/mkff_log_util.h"
#include "video_frame.h"
#include "win_common.h"

#define DEFAULT_POOL_MIN 6
#define DEFAULT_POOL_MAX 16
#define READY_QUEUE_CAPACITY H264_DXVA_OUTPUT_QUEUE_MAX
#define BITSTREAM_BUFFER_ALIGNMENT 128

typedef struct WindowsVideoDecoder {
    MKFF_HandleCommon common; /* must be first */

    MKFF_VideoCodec codec;
    DecoderShared *shared;
    int            initialized;
    GUID           decoder_profile;
    MKFF_VideoProfile mkff_profile;
    MKFF_PixelFormat  output_format;
    uint32_t          bit_depth;
    uint32_t          chroma_format_idc;

    H264SPS      sps_table[H264_MAX_SPS_COUNT];
    H264PPS      pps_table[H264_MAX_PPS_COUNT];
    H264PocState poc_state;
    H264DxvaDpb  dpb;

    HevcVPS      hevc_vps_table[HEVC_MAX_VPS_COUNT];
    HevcSPS      hevc_sps_table[HEVC_MAX_SPS_COUNT];
    HevcPPS      hevc_pps_table[HEVC_MAX_PPS_COUNT];
    HevcPocState hevc_poc_state;
    HevcDxvaDpb  hevc_dpb;

    uint32_t active_sps_id;
    uint32_t max_num_ref_frames;
    uint32_t coded_width, coded_height;
    uint32_t display_width, display_height;

    uint32_t requested_max_surfaces;

    WindowsVideoFrame *ready_queue[READY_QUEUE_CAPACITY];
    uint32_t            ready_count;
    int                 flushed;

    MKFF_LogCallback log_callback;
    void            *log_user_data;
    MKFF_LogLevel    log_min_level;

    char last_error[256];
} WindowsVideoDecoder;

#define DLOG(dec, lvl, ...) MKFF_LOG((dec)->log_callback, (dec)->log_user_data, (dec)->log_min_level, (lvl), \
    ((dec)->codec == MKFF_VIDEO_CODEC_HEVC) ? "mkff.platform.windows.hevc" : "mkff.platform.windows.h264", __VA_ARGS__)

static void set_last_error(WindowsVideoDecoder *dec, const char *msg) {
    snprintf(dec->last_error, sizeof(dec->last_error), "%s", msg);
    DLOG(dec, MKFF_LOG_LEVEL_ERROR, "%s", msg);
}

static MKFF_Result ensure_d3d11_initialized(WindowsVideoDecoder *dec, const H264SPS *sps) {
    uint32_t coded_w = sps->pic_width_in_mbs * 16;
    uint32_t coded_h = sps->pic_height_in_map_units * 16;

    if (dec->initialized) {
        if (coded_w != dec->coded_width || coded_h != dec->coded_height) {
            set_last_error(dec, "mid-stream resolution change is not supported in this milestone");
            return MKFF_RESULT_ERROR_NOT_SUPPORTED;
        }
        return MKFF_RESULT_OK;
    }

    char err[256] = {0};
    ID3D11Device *device = NULL;
    ID3D11DeviceContext *device_context = NULL;
    if (windows_create_d3d11_device(&device, &device_context, err, sizeof(err)) != MKFF_RESULT_OK) {
        set_last_error(dec, err[0] ? err : "failed to create a D3D11 device");
        return MKFF_RESULT_ERROR_DEVICE;
    }

    ID3D11VideoDevice *video_device = NULL;
    ID3D11VideoContext *video_context = NULL;
    if (FAILED(IUnknown_QueryInterface((IUnknown *)device, &IID_ID3D11VideoDevice, (void **)&video_device)) ||
        FAILED(IUnknown_QueryInterface((IUnknown *)device_context, &IID_ID3D11VideoContext, (void **)&video_context))) {
        set_last_error(dec, "this D3D11 device does not expose ID3D11VideoDevice/ID3D11VideoContext");
        if (video_device) IUnknown_Release((IUnknown *)video_device);
        if (video_context) IUnknown_Release((IUnknown *)video_context);
        IUnknown_Release((IUnknown *)device_context);
        IUnknown_Release((IUnknown *)device);
        return MKFF_RESULT_ERROR_DEVICE;
    }

    GUID profile;
    if (!windows_select_h264_decoder_guid(video_device, &profile)) {
        set_last_error(dec, "no D3D11 video decoder profile supports H.264 VLD (DXVA2_ModeH264_VLD_NoFGT) on this device");
        IUnknown_Release((IUnknown *)video_context);
        IUnknown_Release((IUnknown *)video_device);
        IUnknown_Release((IUnknown *)device_context);
        IUnknown_Release((IUnknown *)device);
        return MKFF_RESULT_ERROR_NOT_SUPPORTED;
    }

    if (!windows_check_nv12_supported(video_device, &profile)) {
        set_last_error(dec, "H.264 VLD decoder profile does not support NV12 output on this device");
        IUnknown_Release((IUnknown *)video_context);
        IUnknown_Release((IUnknown *)video_device);
        IUnknown_Release((IUnknown *)device_context);
        IUnknown_Release((IUnknown *)device);
        return MKFF_RESULT_ERROR_NOT_SUPPORTED;
    }

    D3D11_VIDEO_DECODER_DESC decoder_desc;
    memset(&decoder_desc, 0, sizeof(decoder_desc));
    decoder_desc.Guid = profile;
    decoder_desc.SampleWidth = coded_w;
    decoder_desc.SampleHeight = coded_h;
    decoder_desc.OutputFormat = DXGI_FORMAT_NV12;

    UINT config_count = 0;
    if (FAILED(ID3D11VideoDevice_GetVideoDecoderConfigCount(video_device, &decoder_desc, &config_count)) || config_count == 0) {
        set_last_error(dec, "no D3D11 video decoder configuration available for this stream");
        IUnknown_Release((IUnknown *)video_context);
        IUnknown_Release((IUnknown *)video_device);
        IUnknown_Release((IUnknown *)device_context);
        IUnknown_Release((IUnknown *)device);
        return MKFF_RESULT_ERROR_DEVICE;
    }

    /* Prefer ConfigBitstreamRaw == 2: "short" DXVA_Slice_H264_Short
     * slice control, where the hardware reparses each slice_header()
     * itself from the raw NAL bytes we submit — the well-established
     * path every real D3D11 H.264 decoder consumer uses (FFmpeg's
     * d3d11va_h264.c, LAV Filters, mpv). Falls back to the first
     * reported config if no ConfigBitstreamRaw == 2 entry exists. */
    D3D11_VIDEO_DECODER_CONFIG config;
    memset(&config, 0, sizeof(config));
    int found_short_format = 0;
    for (UINT i = 0; i < config_count; i++) {
        D3D11_VIDEO_DECODER_CONFIG candidate;
        if (FAILED(ID3D11VideoDevice_GetVideoDecoderConfig(video_device, &decoder_desc, i, &candidate))) {
            continue;
        }
        if (i == 0) {
            config = candidate;
        }
        if (candidate.ConfigBitstreamRaw == 2) {
            config = candidate;
            found_short_format = 1;
            break;
        }
    }
    if (!found_short_format) {
        DLOG(dec, MKFF_LOG_LEVEL_WARN, "%s", "no ConfigBitstreamRaw==2 (short slice control) decoder config reported; using the first available config, which this module has not validated against DXVA_Slice_H264_Long");
    }

    uint32_t pool_capacity = dec->requested_max_surfaces;
    if (pool_capacity == 0) {
        uint32_t base = sps->max_num_ref_frames + 4;
        pool_capacity = base < DEFAULT_POOL_MIN ? DEFAULT_POOL_MIN : (base > DEFAULT_POOL_MAX ? DEFAULT_POOL_MAX : base);
    }

    DecoderShared *shared = decoder_shared_create(pool_capacity, dec->log_callback, dec->log_user_data, dec->log_min_level);
    if (!shared) {
        IUnknown_Release((IUnknown *)video_context);
        IUnknown_Release((IUnknown *)video_device);
        IUnknown_Release((IUnknown *)device_context);
        IUnknown_Release((IUnknown *)device);
        set_last_error(dec, "out of memory allocating the surface pool");
        return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
    }
    shared->device = device;
    shared->device_context = device_context;
    shared->video_device = video_device;
    shared->video_context = video_context;
    shared->coded_width = coded_w;
    shared->coded_height = coded_h;
    shared->dxgi_format = DXGI_FORMAT_NV12;

    D3D11_TEXTURE2D_DESC tex_desc;
    memset(&tex_desc, 0, sizeof(tex_desc));
    tex_desc.Width = coded_w;
    tex_desc.Height = coded_h;
    tex_desc.MipLevels = 1;
    tex_desc.ArraySize = pool_capacity;
    tex_desc.Format = DXGI_FORMAT_NV12;
    tex_desc.SampleDesc.Count = 1;
    tex_desc.Usage = D3D11_USAGE_DEFAULT;
    tex_desc.BindFlags = D3D11_BIND_DECODER;
    /* SHARED + SHARED_NTHANDLE together: required for
     * IDXGIResource1::CreateSharedHandle() (see video_frame.c's
     * export). Not every driver accepts sharing on decode-bound
     * textures; if this fails, decode itself would still be usable
     * without cross-process/cross-API export, but we treat it as fatal
     * here to keep the "export always works if decode works" contract
     * this module documents. */
    tex_desc.MiscFlags = D3D11_RESOURCE_MISC_SHARED | D3D11_RESOURCE_MISC_SHARED_NTHANDLE;

    HRESULT hr = ID3D11Device_CreateTexture2D(device, &tex_desc, NULL, &shared->texture_array);
    if (FAILED(hr)) {
        set_last_error(dec, "ID3D11Device::CreateTexture2D failed for the decode surface pool");
        decoder_shared_unref(shared);
        return MKFF_RESULT_ERROR_DEVICE;
    }

    for (uint32_t i = 0; i < pool_capacity; i++) {
        D3D11_VIDEO_DECODER_OUTPUT_VIEW_DESC view_desc;
        memset(&view_desc, 0, sizeof(view_desc));
        view_desc.DecodeProfile = profile;
        view_desc.ViewDimension = D3D11_VDOV_DIMENSION_TEXTURE2D;
        view_desc.Texture2D.ArraySlice = i;

        hr = ID3D11VideoDevice_CreateVideoDecoderOutputView(video_device, (ID3D11Resource *)shared->texture_array, &view_desc, &shared->output_views[i]);
        if (FAILED(hr)) {
            set_last_error(dec, "ID3D11VideoDevice::CreateVideoDecoderOutputView failed");
            decoder_shared_unref(shared);
            return MKFF_RESULT_ERROR_DEVICE;
        }
    }

    hr = ID3D11VideoDevice_CreateVideoDecoder(video_device, &decoder_desc, &config, &shared->decoder);
    if (FAILED(hr)) {
        set_last_error(dec, "ID3D11VideoDevice::CreateVideoDecoder failed");
        decoder_shared_unref(shared);
        return MKFF_RESULT_ERROR_DEVICE;
    }

    shared->initialized = 1;
    dec->shared = shared;
    dec->initialized = 1;
    dec->decoder_profile = profile;
    dec->mkff_profile = MKFF_VIDEO_PROFILE_H264_HIGH; /* DXVA2_ModeH264_VLD_NoFGT covers Baseline/Main/High uniformly */
    dec->output_format = MKFF_PIXEL_FORMAT_NV12;
    dec->bit_depth = 8;
    dec->chroma_format_idc = 1;
    dec->coded_width = coded_w;
    dec->coded_height = coded_h;

    DLOG(dec, MKFF_LOG_LEVEL_INFO, "initialized D3D11 video decode: %ux%u pool_capacity=%u", coded_w, coded_h, pool_capacity);
    return MKFF_RESULT_OK;
}

static MKFF_Result ensure_d3d11_initialized_hevc(WindowsVideoDecoder *dec, const HevcSPS *sps) {
    uint32_t coded_w = sps->pic_width_in_luma_samples;
    uint32_t coded_h = sps->pic_height_in_luma_samples;
    uint32_t bit_depth = sps->bit_depth;
    DXGI_FORMAT dxgi_format = (bit_depth == 10) ? DXGI_FORMAT_P010 : DXGI_FORMAT_NV12;
    MKFF_PixelFormat pixel_format = (bit_depth == 10) ? MKFF_PIXEL_FORMAT_P010 : MKFF_PIXEL_FORMAT_NV12;

    if (dec->initialized) {
        if (coded_w != dec->coded_width || coded_h != dec->coded_height || bit_depth != dec->bit_depth) {
            set_last_error(dec, "mid-stream resolution/bit-depth change is not supported in this milestone");
            return MKFF_RESULT_ERROR_NOT_SUPPORTED;
        }
        return MKFF_RESULT_OK;
    }

    char err[256] = {0};
    ID3D11Device *device = NULL;
    ID3D11DeviceContext *device_context = NULL;
    if (windows_create_d3d11_device(&device, &device_context, err, sizeof(err)) != MKFF_RESULT_OK) {
        set_last_error(dec, err[0] ? err : "failed to create a D3D11 device");
        return MKFF_RESULT_ERROR_DEVICE;
    }

    ID3D11VideoDevice *video_device = NULL;
    ID3D11VideoContext *video_context = NULL;
    if (FAILED(IUnknown_QueryInterface((IUnknown *)device, &IID_ID3D11VideoDevice, (void **)&video_device)) ||
        FAILED(IUnknown_QueryInterface((IUnknown *)device_context, &IID_ID3D11VideoContext, (void **)&video_context))) {
        set_last_error(dec, "this D3D11 device does not expose ID3D11VideoDevice/ID3D11VideoContext");
        if (video_device) IUnknown_Release((IUnknown *)video_device);
        if (video_context) IUnknown_Release((IUnknown *)video_context);
        IUnknown_Release((IUnknown *)device_context);
        IUnknown_Release((IUnknown *)device);
        return MKFF_RESULT_ERROR_DEVICE;
    }

    GUID profile;
    if (!windows_select_hevc_decoder_guid(video_device, bit_depth, &profile)) {
        set_last_error(dec, bit_depth == 10
            ? "no D3D11 video decoder profile supports HEVC Main10 VLD (DXVA_ModeHEVC_VLD_Main10) on this device"
            : "no D3D11 video decoder profile supports HEVC Main VLD (DXVA_ModeHEVC_VLD_Main) on this device");
        IUnknown_Release((IUnknown *)video_context);
        IUnknown_Release((IUnknown *)video_device);
        IUnknown_Release((IUnknown *)device_context);
        IUnknown_Release((IUnknown *)device);
        return MKFF_RESULT_ERROR_NOT_SUPPORTED;
    }

    if (!windows_check_decoder_format_supported(video_device, &profile, dxgi_format)) {
        set_last_error(dec, bit_depth == 10
            ? "HEVC Main10 VLD decoder profile does not support P010 output on this device"
            : "HEVC Main VLD decoder profile does not support NV12 output on this device");
        IUnknown_Release((IUnknown *)video_context);
        IUnknown_Release((IUnknown *)video_device);
        IUnknown_Release((IUnknown *)device_context);
        IUnknown_Release((IUnknown *)device);
        return MKFF_RESULT_ERROR_NOT_SUPPORTED;
    }

    D3D11_VIDEO_DECODER_DESC decoder_desc;
    memset(&decoder_desc, 0, sizeof(decoder_desc));
    decoder_desc.Guid = profile;
    decoder_desc.SampleWidth = coded_w;
    decoder_desc.SampleHeight = coded_h;
    decoder_desc.OutputFormat = dxgi_format;

    UINT config_count = 0;
    if (FAILED(ID3D11VideoDevice_GetVideoDecoderConfigCount(video_device, &decoder_desc, &config_count)) || config_count == 0) {
        set_last_error(dec, "no D3D11 video decoder configuration available for this HEVC stream");
        IUnknown_Release((IUnknown *)video_context);
        IUnknown_Release((IUnknown *)video_device);
        IUnknown_Release((IUnknown *)device_context);
        IUnknown_Release((IUnknown *)device);
        return MKFF_RESULT_ERROR_DEVICE;
    }

    D3D11_VIDEO_DECODER_CONFIG config;
    memset(&config, 0, sizeof(config));
    int found_short_format = 0;
    for (UINT i = 0; i < config_count; i++) {
        D3D11_VIDEO_DECODER_CONFIG candidate;
        if (FAILED(ID3D11VideoDevice_GetVideoDecoderConfig(video_device, &decoder_desc, i, &candidate))) {
            continue;
        }
        if (i == 0) {
            config = candidate;
        }
        if (candidate.ConfigBitstreamRaw == 2) {
            config = candidate;
            found_short_format = 1;
            break;
        }
    }
    if (!found_short_format) {
        DLOG(dec, MKFF_LOG_LEVEL_WARN, "%s", "no ConfigBitstreamRaw==2 (short slice control) decoder config reported; using the first available config");
    }

    uint32_t pool_capacity = dec->requested_max_surfaces;
    if (pool_capacity == 0) {
        uint32_t max_dpb = sps->sps_max_dec_pic_buffering_minus1[sps->sps_max_sub_layers_minus1] + 1;
        uint32_t base = max_dpb + 3;
        pool_capacity = base < DEFAULT_POOL_MIN ? DEFAULT_POOL_MIN : (base > DEFAULT_POOL_MAX ? DEFAULT_POOL_MAX : base);
    }

    DecoderShared *shared = decoder_shared_create(pool_capacity, dec->log_callback, dec->log_user_data, dec->log_min_level);
    if (!shared) {
        IUnknown_Release((IUnknown *)video_context);
        IUnknown_Release((IUnknown *)video_device);
        IUnknown_Release((IUnknown *)device_context);
        IUnknown_Release((IUnknown *)device);
        set_last_error(dec, "out of memory allocating the surface pool");
        return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
    }
    shared->device = device;
    shared->device_context = device_context;
    shared->video_device = video_device;
    shared->video_context = video_context;
    shared->coded_width = coded_w;
    shared->coded_height = coded_h;
    shared->dxgi_format = dxgi_format;

    D3D11_TEXTURE2D_DESC tex_desc;
    memset(&tex_desc, 0, sizeof(tex_desc));
    tex_desc.Width = coded_w;
    tex_desc.Height = coded_h;
    tex_desc.MipLevels = 1;
    tex_desc.ArraySize = pool_capacity;
    tex_desc.Format = dxgi_format;
    tex_desc.SampleDesc.Count = 1;
    tex_desc.Usage = D3D11_USAGE_DEFAULT;
    tex_desc.BindFlags = D3D11_BIND_DECODER;
    tex_desc.MiscFlags = D3D11_RESOURCE_MISC_SHARED | D3D11_RESOURCE_MISC_SHARED_NTHANDLE;

    HRESULT hr = ID3D11Device_CreateTexture2D(device, &tex_desc, NULL, &shared->texture_array);
    if (FAILED(hr)) {
        set_last_error(dec, "ID3D11Device::CreateTexture2D failed for the HEVC decode surface pool");
        decoder_shared_unref(shared);
        return MKFF_RESULT_ERROR_DEVICE;
    }

    for (uint32_t i = 0; i < pool_capacity; i++) {
        D3D11_VIDEO_DECODER_OUTPUT_VIEW_DESC view_desc;
        memset(&view_desc, 0, sizeof(view_desc));
        view_desc.DecodeProfile = profile;
        view_desc.ViewDimension = D3D11_VDOV_DIMENSION_TEXTURE2D;
        view_desc.Texture2D.ArraySlice = i;

        hr = ID3D11VideoDevice_CreateVideoDecoderOutputView(video_device, (ID3D11Resource *)shared->texture_array, &view_desc, &shared->output_views[i]);
        if (FAILED(hr)) {
            set_last_error(dec, "ID3D11VideoDevice::CreateVideoDecoderOutputView failed");
            decoder_shared_unref(shared);
            return MKFF_RESULT_ERROR_DEVICE;
        }
    }

    hr = ID3D11VideoDevice_CreateVideoDecoder(video_device, &decoder_desc, &config, &shared->decoder);
    if (FAILED(hr)) {
        set_last_error(dec, "ID3D11VideoDevice::CreateVideoDecoder failed");
        decoder_shared_unref(shared);
        return MKFF_RESULT_ERROR_DEVICE;
    }

    shared->initialized = 1;
    dec->shared = shared;
    dec->initialized = 1;
    dec->decoder_profile = profile;
    dec->mkff_profile = (bit_depth == 10) ? MKFF_VIDEO_PROFILE_HEVC_MAIN10 : MKFF_VIDEO_PROFILE_HEVC_MAIN;
    dec->output_format = pixel_format;
    dec->bit_depth = bit_depth;
    dec->chroma_format_idc = 1;
    dec->coded_width = coded_w;
    dec->coded_height = coded_h;

    DLOG(dec, MKFF_LOG_LEVEL_INFO, "initialized D3D11 HEVC decode: %ux%u bit_depth=%u pool_capacity=%u",
         coded_w, coded_h, bit_depth, pool_capacity);
    return MKFF_RESULT_OK;
}

MKFF_Result windows_video_decoder_create(MKFF_PlatformContext *pctx, const MKFF_VideoDecoderDesc *desc, void **out_decoder) {
    if (desc->codec != MKFF_VIDEO_CODEC_H264 && desc->codec != MKFF_VIDEO_CODEC_HEVC) {
        return MKFF_RESULT_ERROR_NOT_SUPPORTED;
    }

    if (desc->struct_size >= offsetof(MKFF_VideoDecoderDesc, backend) + sizeof(MKFF_VideoBackend)
        && desc->backend == MKFF_VIDEO_BACKEND_SOFTWARE_ONLY) {
        return MKFF_RESULT_ERROR_NOT_SUPPORTED;
    }

    WindowsVideoDecoder *dec = (WindowsVideoDecoder *)calloc(1, sizeof(WindowsVideoDecoder));
    if (!dec) {
        return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
    }

    dec->common.api = mkff_windows_platform_api();
    dec->codec = desc->codec;
    dec->log_callback = pctx->log_callback;
    dec->log_user_data = pctx->log_user_data;
    dec->log_min_level = pctx->log_min_level;
    dec->requested_max_surfaces = desc->max_surfaces;
    dec->chroma_format_idc = 1;
    h264_dxva_dpb_init(&dec->dpb);
    hevc_dxva_dpb_init(&dec->hevc_dpb);

    *out_decoder = dec;
    return MKFF_RESULT_OK;
}

void windows_video_decoder_destroy(void *decoder_v) {
    WindowsVideoDecoder *dec = (WindowsVideoDecoder *)decoder_v;
    if (!dec) return;

    h264_dxva_dpb_reset(&dec->dpb);
    hevc_dxva_dpb_reset(&dec->hevc_dpb);
    for (uint32_t i = 0; i < dec->ready_count; i++) {
        windows_video_frame_release((MKFF_VideoFrame *)dec->ready_queue[i]);
    }
    if (dec->shared) {
        decoder_shared_unref(dec->shared);
    }
    free(dec);
}

static void push_ready(WindowsVideoDecoder *dec, WindowsVideoFrame *frame) {
    if (!frame) return;
    if (dec->ready_count >= READY_QUEUE_CAPACITY) {
        windows_video_frame_release((MKFF_VideoFrame *)frame);
        return;
    }
    dec->ready_queue[dec->ready_count++] = frame;
}

static DXVA_PicEntry_H264 invalid_pic_entry(void) {
    DXVA_PicEntry_H264 e;
    e.bPicEntry = 0xFF; /* DXVA H.264 spec sentinel for "unused entry" */
    return e;
}

typedef struct BitstreamAccumulator {
    uint8_t *data;
    size_t   size;
    size_t   capacity;
} BitstreamAccumulator;

static void bitstream_acc_init(BitstreamAccumulator *acc, size_t capacity_hint) {
    acc->data = (uint8_t *)malloc(capacity_hint > 0 ? capacity_hint : 1);
    acc->size = 0;
    acc->capacity = acc->data ? capacity_hint : 0;
}

static int bitstream_acc_append(BitstreamAccumulator *acc, const uint8_t *data, size_t size, uint32_t *out_offset) {
    if (acc->size + size > acc->capacity) {
        return 0; /* pre-sized to the whole submit() buffer: should not happen */
    }
    memcpy(acc->data + acc->size, data, size);
    *out_offset = (uint32_t)acc->size;
    acc->size += size;
    return 1;
}

static void bitstream_acc_free(BitstreamAccumulator *acc) {
    free(acc->data);
    acc->data = NULL;
}

static void submit_frame_to_hardware(WindowsVideoDecoder *dec,
                                      const H264SPS *sps,
                                      const H264PPS *pps,
                                      const H264SliceHeader *first_sh,
                                      int32_t poc,
                                      uint32_t curr_array_slice,
                                      DXVA_Slice_H264_Short *slices,
                                      uint32_t slice_count,
                                      BitstreamAccumulator *bitstream) {
    ID3D11VideoContext *vctx = dec->shared->video_context;
    ID3D11VideoDecoder *decoder = dec->shared->decoder;

    /* --- Picture parameters --- */
    DXVA_PicParams_H264 pp;
    memset(&pp, 0, sizeof(pp));

    pp.wFrameWidthInMbsMinus1 = (USHORT)(sps->pic_width_in_mbs - 1);
    pp.wFrameHeightInMbsMinus1 = (USHORT)(sps->pic_height_in_map_units - 1);
    pp.CurrPic.bPicEntry = 0;
    pp.CurrPic.Index7Bits = (UCHAR)(curr_array_slice & 0x7F);
    pp.CurrPic.AssociatedFlag = 0;
    pp.num_ref_frames = (UCHAR)sps->max_num_ref_frames;

    pp.field_pic_flag = 0;
    pp.MbaffFrameFlag = 0;
    pp.residual_colour_transform_flag = 0;
    pp.sp_for_switch_flag = 0;
    pp.chroma_format_idc = (USHORT)sps->chroma_format_idc;
    pp.RefPicFlag = (first_sh->nal_ref_idc != 0) ? 1 : 0;
    pp.constrained_intra_pred_flag = pps->constrained_intra_pred_flag;
    pp.weighted_pred_flag = pps->weighted_pred_flag;
    pp.weighted_bipred_idc = (USHORT)pps->weighted_bipred_idc;
    pp.MbsConsecutiveFlag = 1;
    pp.frame_mbs_only_flag = sps->frame_mbs_only_flag;
    pp.transform_8x8_mode_flag = pps->transform_8x8_mode_flag;
    pp.MinLumaBipredSize8x8Flag = (sps->profile_idc >= 100) ? 1 : 0;
    pp.IntraPicFlag = (first_sh->slice_type == H264_SLICE_TYPE_I) ? 1 : 0;

    pp.bit_depth_luma_minus8 = 0;
    pp.bit_depth_chroma_minus8 = 0;

    pp.CurrFieldOrderCnt[0] = poc;
    pp.CurrFieldOrderCnt[1] = poc;

    UINT used_for_reference_flags = 0;
    uint32_t nref = h264_dxva_dpb_fill_reference_frames(&dec->dpb, pp.RefFrameList, pp.FrameNumList, pp.FieldOrderCntList, 16, &used_for_reference_flags);
    for (uint32_t i = nref; i < 16; i++) {
        pp.RefFrameList[i] = invalid_pic_entry();
    }
    pp.UsedForReferenceFlags = used_for_reference_flags;

    pp.pic_init_qs_minus26 = (CHAR)pps->pic_init_qs_minus26;
    pp.chroma_qp_index_offset = (CHAR)pps->chroma_qp_index_offset;
    pp.second_chroma_qp_index_offset = (CHAR)pps->second_chroma_qp_index_offset;
    pp.pic_init_qp_minus26 = (CHAR)pps->pic_init_qp_minus26;
    pp.num_ref_idx_l0_active_minus1 = (UCHAR)pps->num_ref_idx_l0_default_active_minus1;
    pp.num_ref_idx_l1_active_minus1 = (UCHAR)pps->num_ref_idx_l1_default_active_minus1;

    pp.frame_num = (USHORT)first_sh->frame_num;
    pp.log2_max_frame_num_minus4 = (UCHAR)(sps->log2_max_frame_num - 4);
    pp.pic_order_cnt_type = (UCHAR)sps->pic_order_cnt_type;
    pp.log2_max_pic_order_cnt_lsb_minus4 = (sps->pic_order_cnt_type == 0) ? (UCHAR)(sps->log2_max_pic_order_cnt_lsb - 4) : 0;
    pp.delta_pic_order_always_zero_flag = (UCHAR)sps->delta_pic_order_always_zero_flag;
    pp.direct_8x8_inference_flag = (UCHAR)sps->direct_8x8_inference_flag;
    pp.entropy_coding_mode_flag = (UCHAR)pps->entropy_coding_mode_flag;
    pp.pic_order_present_flag = (UCHAR)pps->bottom_field_pic_order_in_frame_present_flag;
    pp.num_slice_groups_minus1 = (UCHAR)pps->num_slice_groups_minus1;

    pp.slice_group_map_type = 0;
    pp.deblocking_filter_control_present_flag = (UCHAR)pps->deblocking_filter_control_present_flag;
    pp.redundant_pic_cnt_present_flag = (UCHAR)pps->redundant_pic_cnt_present_flag;

    void *buf_ptr = NULL;
    UINT buf_size = 0;

    ID3D11VideoContext_GetDecoderBuffer(vctx, decoder, D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS, &buf_size, &buf_ptr);
    memcpy(buf_ptr, &pp, sizeof(pp) < buf_size ? sizeof(pp) : buf_size);
    ID3D11VideoContext_ReleaseDecoderBuffer(vctx, decoder, D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS);

    /* --- Inverse quantization matrix: flat (no custom scaling lists;
     * h264_parse_sps/pps already reject streams that request them). --- */
    DXVA_Qmatrix_H264 qm;
    memset(&qm, 16, sizeof(qm));
    ID3D11VideoContext_GetDecoderBuffer(vctx, decoder, D3D11_VIDEO_DECODER_BUFFER_INVERSE_QUANTIZATION_MATRIX, &buf_size, &buf_ptr);
    memcpy(buf_ptr, &qm, sizeof(qm) < buf_size ? sizeof(qm) : buf_size);
    ID3D11VideoContext_ReleaseDecoderBuffer(vctx, decoder, D3D11_VIDEO_DECODER_BUFFER_INVERSE_QUANTIZATION_MATRIX);

    /* --- Slice control (short form) --- */
    ID3D11VideoContext_GetDecoderBuffer(vctx, decoder, D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL, &buf_size, &buf_ptr);
    size_t slices_bytes = (size_t)slice_count * sizeof(DXVA_Slice_H264_Short);
    memcpy(buf_ptr, slices, slices_bytes < buf_size ? slices_bytes : buf_size);
    ID3D11VideoContext_ReleaseDecoderBuffer(vctx, decoder, D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL);

    /* --- Bitstream: padded to a 128-byte multiple per the DXVA H.264
     * spec; padding content is irrelevant since SliceBytesInBuffer
     * marks the true extent of each slice. --- */
    size_t padded_size = ((bitstream->size + BITSTREAM_BUFFER_ALIGNMENT - 1) / BITSTREAM_BUFFER_ALIGNMENT) * BITSTREAM_BUFFER_ALIGNMENT;
    ID3D11VideoContext_GetDecoderBuffer(vctx, decoder, D3D11_VIDEO_DECODER_BUFFER_BITSTREAM, &buf_size, &buf_ptr);
    memset(buf_ptr, 0, padded_size < buf_size ? padded_size : buf_size);
    memcpy(buf_ptr, bitstream->data, bitstream->size < buf_size ? bitstream->size : buf_size);
    ID3D11VideoContext_ReleaseDecoderBuffer(vctx, decoder, D3D11_VIDEO_DECODER_BUFFER_BITSTREAM);

    D3D11_VIDEO_DECODER_BUFFER_DESC buffer_descs[4];
    memset(buffer_descs, 0, sizeof(buffer_descs));

    buffer_descs[0].BufferType = D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS;
    buffer_descs[0].DataSize = (UINT)sizeof(pp);

    buffer_descs[1].BufferType = D3D11_VIDEO_DECODER_BUFFER_INVERSE_QUANTIZATION_MATRIX;
    buffer_descs[1].DataSize = (UINT)sizeof(qm);

    buffer_descs[2].BufferType = D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL;
    buffer_descs[2].DataSize = (UINT)slices_bytes;

    buffer_descs[3].BufferType = D3D11_VIDEO_DECODER_BUFFER_BITSTREAM;
    buffer_descs[3].DataSize = (UINT)padded_size;

    ID3D11VideoContext_SubmitDecoderBuffers(vctx, decoder, 4, buffer_descs);
}

static int hevc_nal_is_ref(uint8_t nal_unit_type) {
    if (nal_unit_type >= 16 && nal_unit_type <= 21) {
        return 1;
    }
    if (nal_unit_type <= 9) {
        return (nal_unit_type & 1) != 0;
    }
    return 0;
}

static void submit_hevc_frame_to_hardware(WindowsVideoDecoder *dec,
                                           const HevcSPS *sps,
                                           const HevcPPS *pps,
                                           const HevcSliceHeader *first_sh,
                                           int32_t poc,
                                           uint32_t curr_array_slice,
                                           DXVA_Slice_HEVC_Short *slices,
                                           uint32_t slice_count,
                                           BitstreamAccumulator *bitstream) {
    ID3D11VideoContext *vctx = dec->shared->video_context;
    ID3D11VideoDecoder *decoder = dec->shared->decoder;

    DXVA_PicParams_HEVC pp;
    memset(&pp, 0, sizeof(pp));

    uint32_t min_cb_log2 = sps->log2_min_luma_coding_block_size_minus3 + 3;
    uint32_t min_cb_size = (uint32_t)1 << min_cb_log2;
    pp.PicWidthInMinCbsY = (USHORT)(sps->pic_width_in_luma_samples / min_cb_size);
    pp.PicHeightInMinCbsY = (USHORT)(sps->pic_height_in_luma_samples / min_cb_size);

    pp.chroma_format_idc = (USHORT)sps->chroma_format_idc;
    pp.separate_colour_plane_flag = (USHORT)sps->separate_colour_plane_flag;
    pp.bit_depth_luma_minus8 = (USHORT)sps->bit_depth_luma_minus8;
    pp.bit_depth_chroma_minus8 = (USHORT)sps->bit_depth_chroma_minus8;
    pp.log2_max_pic_order_cnt_lsb_minus4 = (USHORT)(sps->log2_max_pic_order_cnt_lsb - 4);

    pp.CurrPic.bPicEntry = 0;
    pp.CurrPic.Index7Bits = (UCHAR)(curr_array_slice & 0x7F);
    pp.CurrPic.AssociatedFlag = 0;

    pp.sps_max_dec_pic_buffering_minus1 =
        (UCHAR)sps->sps_max_dec_pic_buffering_minus1[sps->sps_max_sub_layers_minus1];
    pp.log2_min_luma_coding_block_size_minus3 = (UCHAR)sps->log2_min_luma_coding_block_size_minus3;
    pp.log2_diff_max_min_luma_coding_block_size = (UCHAR)sps->log2_diff_max_min_luma_coding_block_size;
    pp.log2_min_transform_block_size_minus2 = (UCHAR)sps->log2_min_luma_transform_block_size_minus2;
    pp.log2_diff_max_min_transform_block_size = (UCHAR)sps->log2_diff_max_min_luma_transform_block_size;
    pp.max_transform_hierarchy_depth_inter = (UCHAR)sps->max_transform_hierarchy_depth_inter;
    pp.max_transform_hierarchy_depth_intra = (UCHAR)sps->max_transform_hierarchy_depth_intra;
    pp.num_short_term_ref_pic_sets = (UCHAR)sps->num_short_term_ref_pic_sets;
    pp.num_long_term_ref_pics_sps = (UCHAR)sps->num_long_term_ref_pics_sps;

    pp.num_ref_idx_l0_default_active_minus1 = (UCHAR)pps->num_ref_idx_l0_default_active_minus1;
    pp.num_ref_idx_l1_default_active_minus1 = (UCHAR)pps->num_ref_idx_l1_default_active_minus1;
    pp.init_qp_minus26 = (CHAR)pps->init_qp_minus26;

    if (!first_sh->short_term_ref_pic_set_sps_flag) {
        pp.ucNumDeltaPocsOfRefRpsIdx = first_sh->num_delta_pocs_of_ref_rps_idx;
        pp.wNumBitsForShortTermRPSInSlice = first_sh->short_term_ref_pic_set_size;
    }

    pp.scaling_list_enabled_flag = (UINT32)sps->scaling_list_enabled_flag;
    pp.amp_enabled_flag = (UINT32)sps->amp_enabled_flag;
    pp.sample_adaptive_offset_enabled_flag = (UINT32)sps->sample_adaptive_offset_enabled_flag;
    pp.pcm_enabled_flag = 0;
    pp.long_term_ref_pics_present_flag = (UINT32)sps->long_term_ref_pics_present_flag;
    pp.sps_temporal_mvp_enabled_flag = (UINT32)sps->sps_temporal_mvp_enabled_flag;
    pp.strong_intra_smoothing_enabled_flag = (UINT32)sps->strong_intra_smoothing_enabled_flag;
    pp.dependent_slice_segments_enabled_flag = (UINT32)pps->dependent_slice_segments_enabled_flag;
    pp.output_flag_present_flag = (UINT32)pps->output_flag_present_flag;
    pp.num_extra_slice_header_bits = (UINT32)pps->num_extra_slice_header_bits;
    pp.sign_data_hiding_enabled_flag = (UINT32)pps->sign_data_hiding_enabled_flag;
    pp.cabac_init_present_flag = (UINT32)pps->cabac_init_present_flag;

    int irap = hevc_nal_is_irap(first_sh->nal_unit_type);
    int idr = hevc_nal_is_idr(first_sh->nal_unit_type);
    pp.constrained_intra_pred_flag = (UINT32)pps->constrained_intra_pred_flag;
    pp.transform_skip_enabled_flag = (UINT32)pps->transform_skip_enabled_flag;
    pp.cu_qp_delta_enabled_flag = (UINT32)pps->cu_qp_delta_enabled_flag;
    pp.pps_slice_chroma_qp_offsets_present_flag = (UINT32)pps->pps_slice_chroma_qp_offsets_present_flag;
    pp.weighted_pred_flag = (UINT32)pps->weighted_pred_flag;
    pp.weighted_bipred_flag = (UINT32)pps->weighted_bipred_flag;
    pp.transquant_bypass_enabled_flag = (UINT32)pps->transquant_bypass_enabled_flag;
    pp.tiles_enabled_flag = 0;
    pp.entropy_coding_sync_enabled_flag = (UINT32)pps->entropy_coding_sync_enabled_flag;
    pp.pps_loop_filter_across_slices_enabled_flag = (UINT32)pps->pps_loop_filter_across_slices_enabled_flag;
    pp.deblocking_filter_override_enabled_flag = (UINT32)pps->deblocking_filter_override_enabled_flag;
    pp.pps_deblocking_filter_disabled_flag = (UINT32)pps->pps_deblocking_filter_disabled_flag;
    pp.lists_modification_present_flag = (UINT32)pps->lists_modification_present_flag;
    pp.slice_segment_header_extension_present_flag = (UINT32)pps->slice_segment_header_extension_present_flag;
    pp.IrapPicFlag = irap ? 1u : 0u;
    pp.IdrPicFlag = idr ? 1u : 0u;
    pp.IntraPicFlag = (first_sh->slice_type == HEVC_SLICE_TYPE_I || irap) ? 1u : 0u;

    pp.pps_cb_qp_offset = (CHAR)pps->pps_cb_qp_offset;
    pp.pps_cr_qp_offset = (CHAR)pps->pps_cr_qp_offset;
    pp.diff_cu_qp_delta_depth = (UCHAR)pps->diff_cu_qp_delta_depth;
    pp.pps_beta_offset_div2 = (CHAR)pps->pps_beta_offset_div2;
    pp.pps_tc_offset_div2 = (CHAR)pps->pps_tc_offset_div2;
    pp.log2_parallel_merge_level_minus2 = (UCHAR)pps->log2_parallel_merge_level_minus2;
    pp.CurrPicOrderCntVal = poc;

    pp.StatusReportFeedbackNumber = hevc_dxva_dpb_fill_pic_params_refs(
        &dec->hevc_dpb, poc, &first_sh->st_rps, pp.RefPicList, pp.PicOrderCntValList,
        pp.RefPicSetStCurrBefore, pp.RefPicSetStCurrAfter, pp.RefPicSetLtCurr);

    void *buf_ptr = NULL;
    UINT buf_size = 0;

    ID3D11VideoContext_GetDecoderBuffer(vctx, decoder, D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS, &buf_size, &buf_ptr);
    memcpy(buf_ptr, &pp, sizeof(pp) < buf_size ? sizeof(pp) : buf_size);
    ID3D11VideoContext_ReleaseDecoderBuffer(vctx, decoder, D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS);

    UINT buffer_count = 3;
    D3D11_VIDEO_DECODER_BUFFER_DESC buffer_descs[4];
    memset(buffer_descs, 0, sizeof(buffer_descs));

    buffer_descs[0].BufferType = D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS;
    buffer_descs[0].DataSize = (UINT)sizeof(pp);

    if (sps->scaling_list_enabled_flag) {
        DXVA_Qmatrix_HEVC qm;
        memset(&qm, 16, sizeof(qm));
        ID3D11VideoContext_GetDecoderBuffer(vctx, decoder, D3D11_VIDEO_DECODER_BUFFER_INVERSE_QUANTIZATION_MATRIX, &buf_size, &buf_ptr);
        memcpy(buf_ptr, &qm, sizeof(qm) < buf_size ? sizeof(qm) : buf_size);
        ID3D11VideoContext_ReleaseDecoderBuffer(vctx, decoder, D3D11_VIDEO_DECODER_BUFFER_INVERSE_QUANTIZATION_MATRIX);
        buffer_descs[1].BufferType = D3D11_VIDEO_DECODER_BUFFER_INVERSE_QUANTIZATION_MATRIX;
        buffer_descs[1].DataSize = (UINT)sizeof(qm);
        buffer_count = 4;
    }

    UINT slice_idx = buffer_count - 2;
    UINT bs_idx = buffer_count - 1;

    ID3D11VideoContext_GetDecoderBuffer(vctx, decoder, D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL, &buf_size, &buf_ptr);
    size_t slices_bytes = (size_t)slice_count * sizeof(DXVA_Slice_HEVC_Short);
    memcpy(buf_ptr, slices, slices_bytes < buf_size ? slices_bytes : buf_size);
    ID3D11VideoContext_ReleaseDecoderBuffer(vctx, decoder, D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL);

    size_t padded_size = ((bitstream->size + BITSTREAM_BUFFER_ALIGNMENT - 1) / BITSTREAM_BUFFER_ALIGNMENT) * BITSTREAM_BUFFER_ALIGNMENT;
    ID3D11VideoContext_GetDecoderBuffer(vctx, decoder, D3D11_VIDEO_DECODER_BUFFER_BITSTREAM, &buf_size, &buf_ptr);
    memset(buf_ptr, 0, padded_size < buf_size ? padded_size : buf_size);
    memcpy(buf_ptr, bitstream->data, bitstream->size < buf_size ? bitstream->size : buf_size);
    ID3D11VideoContext_ReleaseDecoderBuffer(vctx, decoder, D3D11_VIDEO_DECODER_BUFFER_BITSTREAM);

    buffer_descs[slice_idx].BufferType = D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL;
    buffer_descs[slice_idx].DataSize = (UINT)slices_bytes;
    buffer_descs[bs_idx].BufferType = D3D11_VIDEO_DECODER_BUFFER_BITSTREAM;
    buffer_descs[bs_idx].DataSize = (UINT)padded_size;

    ID3D11VideoContext_SubmitDecoderBuffers(vctx, decoder, buffer_count, buffer_descs);
}

static MKFF_Result windows_video_decoder_submit_hevc(WindowsVideoDecoder *dec,
                                                     const uint8_t *annex_b_data,
                                                     size_t annex_b_size,
                                                     int64_t pts,
                                                     int64_t dts) {
    HevcNalUnit nals[HEVC_MAX_NAL_UNITS_PER_AU];
    size_t nal_count = hevc_split_annex_b(annex_b_data, annex_b_size, nals, HEVC_MAX_NAL_UNITS_PER_AU);
    if (nal_count == 0) {
        set_last_error(dec, "no NAL units found (missing Annex-B start code)");
        return MKFF_RESULT_ERROR_BITSTREAM;
    }

    uint8_t *rbsp_scratch = (uint8_t *)malloc(annex_b_size);
    if (!rbsp_scratch) {
        return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
    }

    int picture_open = 0;
    uint32_t pic_array_slice = 0;
    WindowsVideoFrame *pic_frame = NULL;
    const HevcSPS *pic_sps = NULL;
    HevcSliceHeader pic_first_sh;
    int32_t pic_poc = 0;
    MKFF_Result result = MKFF_RESULT_OK;

    DXVA_Slice_HEVC_Short slices[HEVC_MAX_NAL_UNITS_PER_AU];
    uint32_t slice_count = 0;
    /* Room for a 3-byte start code before every VCL NAL (DXVA HEVC convention). */
    BitstreamAccumulator bitstream;
    bitstream_acc_init(&bitstream, annex_b_size + nal_count * 3);

    for (size_t i = 0; i < nal_count && result == MKFF_RESULT_OK; i++) {
        const HevcNalUnit *nal = &nals[i];

        if (nal->nal_unit_type == HEVC_NAL_VPS_NUT) {
            HevcVPS tmp;
            int rc = hevc_parse_vps(nal->data, nal->size, rbsp_scratch, &tmp);
            if (rc == 0 && tmp.vps_video_parameter_set_id < HEVC_MAX_VPS_COUNT) {
                dec->hevc_vps_table[tmp.vps_video_parameter_set_id] = tmp;
            }
            continue;
        }

        if (nal->nal_unit_type == HEVC_NAL_SPS_NUT) {
            HevcSPS tmp;
            int rc = hevc_parse_sps(nal->data, nal->size, rbsp_scratch, &tmp);
            if (rc == 0 && tmp.sps_seq_parameter_set_id < HEVC_MAX_SPS_COUNT) {
                dec->hevc_sps_table[tmp.sps_seq_parameter_set_id] = tmp;
            } else if (tmp.sps_seq_parameter_set_id < HEVC_MAX_SPS_COUNT) {
                DLOG(dec, MKFF_LOG_LEVEL_WARN, "rejecting HEVC SPS id=%u (rc=%d): outside supported Main/Main10 4:2:0 subset",
                     tmp.sps_seq_parameter_set_id, rc);
                dec->hevc_sps_table[tmp.sps_seq_parameter_set_id].valid = 0;
            }
            continue;
        }

        if (nal->nal_unit_type == HEVC_NAL_PPS_NUT) {
            HevcPPS tmp;
            int rc = hevc_parse_pps(nal->data, nal->size, rbsp_scratch, dec->hevc_sps_table, &tmp);
            if (rc == 0 && tmp.pps_pic_parameter_set_id < HEVC_MAX_PPS_COUNT) {
                if (tmp.tiles_enabled_flag || tmp.entropy_coding_sync_enabled_flag) {
                    DLOG(dec, MKFF_LOG_LEVEL_WARN, "rejecting HEVC PPS id=%u: tiles/WPP not supported in this milestone",
                         tmp.pps_pic_parameter_set_id);
                    dec->hevc_pps_table[tmp.pps_pic_parameter_set_id].valid = 0;
                } else {
                    dec->hevc_pps_table[tmp.pps_pic_parameter_set_id] = tmp;
                }
            } else if (tmp.pps_pic_parameter_set_id < HEVC_MAX_PPS_COUNT) {
                DLOG(dec, MKFF_LOG_LEVEL_WARN, "rejecting HEVC PPS id=%u (rc=%d)", tmp.pps_pic_parameter_set_id, rc);
                dec->hevc_pps_table[tmp.pps_pic_parameter_set_id].valid = 0;
            }
            continue;
        }

        if (!hevc_nal_is_vcl(nal->nal_unit_type)) {
            continue;
        }

        HevcSliceHeader sh;
        int rc = hevc_parse_slice_header(nal, rbsp_scratch, dec->hevc_sps_table, dec->hevc_pps_table, &sh);
        if (rc != 0) {
            DLOG(dec, MKFF_LOG_LEVEL_WARN, "dropping HEVC slice NAL (rc=%d)", rc);
            continue;
        }

        const HevcPPS *pps = &dec->hevc_pps_table[sh.slice_pic_parameter_set_id];
        const HevcSPS *sps = &dec->hevc_sps_table[pps->pps_seq_parameter_set_id];

        if (!picture_open) {
            if (!sh.first_slice_segment_in_pic_flag) {
                DLOG(dec, MKFF_LOG_LEVEL_DEBUG, "%s", "dropping dependent/continuation slice before picture open");
                continue;
            }

            if (!dec->initialized && !hevc_nal_is_irap(sh.nal_unit_type)) {
                DLOG(dec, MKFF_LOG_LEVEL_DEBUG, "%s", "dropping access unit before the first IRAP");
                break;
            }

            MKFF_Result init_result = ensure_d3d11_initialized_hevc(dec, sps);
            if (init_result != MKFF_RESULT_OK) {
                result = init_result;
                break;
            }

            dec->active_sps_id = pps->pps_seq_parameter_set_id;
            dec->max_num_ref_frames = sps->sps_max_dec_pic_buffering_minus1[sps->sps_max_sub_layers_minus1] + 1;

            dec->display_width = sps->pic_width_in_luma_samples;
            dec->display_height = sps->pic_height_in_luma_samples;
            if (sps->conformance_window_flag) {
                /* 4:2:0: SubWidthC = SubHeightC = 2 */
                dec->display_width -= (sps->conf_win_left_offset + sps->conf_win_right_offset) * 2;
                dec->display_height -= (sps->conf_win_top_offset + sps->conf_win_bottom_offset) * 2;
            }

            pic_poc = hevc_compute_poc(sps, &sh, &dec->hevc_poc_state);

            if (hevc_nal_is_idr(sh.nal_unit_type)
                || (hevc_nal_is_irap(sh.nal_unit_type) && sh.no_output_of_prior_pics_flag)) {
                hevc_dxva_dpb_clear(&dec->hevc_dpb);
            } else {
                hevc_dxva_dpb_apply_st_rps(&dec->hevc_dpb, pic_poc, &sh.st_rps);
            }

            if (decoder_shared_pool_checkout(dec->shared, &pic_array_slice) != 0) {
                set_last_error(dec, "surface pool exhausted: caller must release frames before submitting more");
                result = MKFF_RESULT_ERROR_POOL_EXHAUSTED;
                break;
            }

            pic_frame = windows_video_frame_create(dec->shared, pic_array_slice, dec->display_width, dec->display_height,
                                                   dec->output_format, pts, dts, hevc_nal_is_irap(sh.nal_unit_type));
            if (!pic_frame) {
                decoder_shared_pool_release(dec->shared, pic_array_slice);
                result = MKFF_RESULT_ERROR_OUT_OF_MEMORY;
                break;
            }

            pic_first_sh = sh;
            pic_sps = sps;
            picture_open = 1;
            slice_count = 0;

            HRESULT hr = ID3D11VideoContext_DecoderBeginFrame(dec->shared->video_context, dec->shared->decoder,
                                                              dec->shared->output_views[pic_array_slice], 0, NULL);
            if (FAILED(hr)) {
                set_last_error(dec, "ID3D11VideoContext::DecoderBeginFrame failed");
                decoder_shared_pool_release(dec->shared, pic_array_slice);
                windows_video_frame_release((MKFF_VideoFrame *)pic_frame);
                pic_frame = NULL;
                picture_open = 0;
                result = MKFF_RESULT_ERROR_DEVICE;
                break;
            }
        }

        if (slice_count < HEVC_MAX_NAL_UNITS_PER_AU) {
            static const uint8_t start_code[3] = {0, 0, 1};
            uint32_t sc_offset = 0;
            if (bitstream_acc_append(&bitstream, start_code, sizeof(start_code), &sc_offset)) {
                uint32_t nal_offset = 0;
                if (bitstream_acc_append(&bitstream, nal->data, nal->size, &nal_offset)) {
                    slices[slice_count].BSNALunitDataLocation = sc_offset;
                    slices[slice_count].SliceBytesInBuffer = (UINT)(sizeof(start_code) + nal->size);
                    slices[slice_count].wBadSliceChopping = 0;
                    slice_count++;
                }
            }
        }
    }

    if (picture_open && result == MKFF_RESULT_OK) {
        submit_hevc_frame_to_hardware(dec, pic_sps, &dec->hevc_pps_table[pic_first_sh.slice_pic_parameter_set_id],
                                      &pic_first_sh, pic_poc, pic_array_slice, slices, slice_count, &bitstream);

        HRESULT hr = ID3D11VideoContext_DecoderEndFrame(dec->shared->video_context, dec->shared->decoder);
        if (FAILED(hr)) {
            DLOG(dec, MKFF_LOG_LEVEL_ERROR, "%s", "ID3D11VideoContext::DecoderEndFrame failed");
        }

        hevc_dxva_dpb_add_reference(&dec->hevc_dpb, pic_frame, pic_poc, hevc_nal_is_ref(pic_first_sh.nal_unit_type));
        if (pic_first_sh.pic_output_flag) {
            hevc_dxva_dpb_push_output(&dec->hevc_dpb, pic_frame, pic_poc);
        }
        windows_video_frame_release((MKFF_VideoFrame *)pic_frame);

        uint32_t max_reorder = pic_sps->sps_max_num_reorder_pics[pic_sps->sps_max_sub_layers_minus1];
        if (max_reorder > 16) max_reorder = 16;
        push_ready(dec, hevc_dxva_dpb_bump_if_needed(&dec->hevc_dpb, max_reorder));
    } else if (picture_open) {
        ID3D11VideoContext_DecoderEndFrame(dec->shared->video_context, dec->shared->decoder);
        windows_video_frame_release((MKFF_VideoFrame *)pic_frame);
    }

    bitstream_acc_free(&bitstream);
    free(rbsp_scratch);
    return result;
}

static MKFF_Result windows_video_decoder_submit_h264(WindowsVideoDecoder *dec,
                                                     const uint8_t *annex_b_data,
                                                     size_t annex_b_size,
                                                     int64_t pts,
                                                     int64_t dts) {
    H264NalUnit nals[H264_MAX_NAL_UNITS_PER_AU];
    size_t nal_count = h264_split_annex_b(annex_b_data, annex_b_size, nals, H264_MAX_NAL_UNITS_PER_AU);
    if (nal_count == 0) {
        set_last_error(dec, "no NAL units found (missing Annex-B start code)");
        return MKFF_RESULT_ERROR_BITSTREAM;
    }

    uint8_t *rbsp_scratch = (uint8_t *)malloc(annex_b_size);
    if (!rbsp_scratch) {
        return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
    }

    int picture_open = 0;
    uint32_t pic_array_slice = 0;
    WindowsVideoFrame *pic_frame = NULL;
    const H264SPS *pic_sps = NULL;
    H264SliceHeader pic_first_sh;
    int32_t pic_poc = 0;
    MKFF_Result result = MKFF_RESULT_OK;

    DXVA_Slice_H264_Short slices[H264_MAX_NAL_UNITS_PER_AU];
    uint32_t slice_count = 0;
    BitstreamAccumulator bitstream;
    bitstream_acc_init(&bitstream, annex_b_size);

    for (size_t i = 0; i < nal_count && result == MKFF_RESULT_OK; i++) {
        const H264NalUnit *nal = &nals[i];

        if (nal->nal_unit_type == 7) { /* SPS */
            H264SPS tmp;
            int rc = h264_parse_sps(nal->data, nal->size, rbsp_scratch, &tmp);
            if (rc == 0 && tmp.seq_parameter_set_id < H264_MAX_SPS_COUNT) {
                dec->sps_table[tmp.seq_parameter_set_id] = tmp;
            } else if (tmp.seq_parameter_set_id < H264_MAX_SPS_COUNT) {
                DLOG(dec, MKFF_LOG_LEVEL_WARN, "rejecting SPS id=%u (rc=%d): outside this milestone's supported subset", tmp.seq_parameter_set_id, rc);
                dec->sps_table[tmp.seq_parameter_set_id].valid = 0;
            }
            continue;
        }

        if (nal->nal_unit_type == 8) { /* PPS */
            H264PPS tmp;
            int rc = h264_parse_pps(nal->data, nal->size, rbsp_scratch, dec->sps_table, &tmp);
            if (rc == 0 && tmp.pic_parameter_set_id < H264_MAX_PPS_COUNT) {
                dec->pps_table[tmp.pic_parameter_set_id] = tmp;
            } else if (tmp.pic_parameter_set_id < H264_MAX_PPS_COUNT) {
                DLOG(dec, MKFF_LOG_LEVEL_WARN, "rejecting PPS id=%u (rc=%d): outside this milestone's supported subset", tmp.pic_parameter_set_id, rc);
                dec->pps_table[tmp.pic_parameter_set_id].valid = 0;
            }
            continue;
        }

        if (nal->nal_unit_type != 1 && nal->nal_unit_type != 5) {
            continue;
        }

        H264SliceHeader sh;
        int rc = h264_parse_slice_header(nal, rbsp_scratch, dec->sps_table, dec->pps_table, &sh);
        if (rc != 0) {
            DLOG(dec, MKFF_LOG_LEVEL_WARN, "dropping slice NAL (rc=%d)", rc);
            continue;
        }

        const H264PPS *pps = &dec->pps_table[sh.pic_parameter_set_id];
        const H264SPS *sps = &dec->sps_table[pps->seq_parameter_set_id];

        if (!picture_open) {
            if (!dec->initialized && !sh.is_idr) {
                DLOG(dec, MKFF_LOG_LEVEL_DEBUG, "%s", "dropping access unit before the first IDR");
                break;
            }

            MKFF_Result init_result = ensure_d3d11_initialized(dec, sps);
            if (init_result != MKFF_RESULT_OK) {
                result = init_result;
                break;
            }

            dec->active_sps_id = pps->seq_parameter_set_id;
            dec->max_num_ref_frames = sps->max_num_ref_frames;

            uint32_t crop_unit_x = 2;
            uint32_t crop_unit_y = 2;
            dec->display_width = dec->coded_width;
            dec->display_height = dec->coded_height;
            if (sps->frame_cropping_flag) {
                dec->display_width -= (sps->crop_left + sps->crop_right) * crop_unit_x;
                dec->display_height -= (sps->crop_top + sps->crop_bottom) * crop_unit_y;
            }

            if (decoder_shared_pool_checkout(dec->shared, &pic_array_slice) != 0) {
                set_last_error(dec, "surface pool exhausted: caller must release frames before submitting more");
                result = MKFF_RESULT_ERROR_POOL_EXHAUSTED;
                break;
            }

            pic_frame = windows_video_frame_create(dec->shared, pic_array_slice, dec->display_width, dec->display_height,
                                                   MKFF_PIXEL_FORMAT_NV12, pts, dts, sh.is_idr);
            if (!pic_frame) {
                decoder_shared_pool_release(dec->shared, pic_array_slice);
                result = MKFF_RESULT_ERROR_OUT_OF_MEMORY;
                break;
            }

            pic_poc = h264_compute_poc(sps, &sh, &dec->poc_state);
            pic_first_sh = sh;
            pic_sps = sps;
            picture_open = 1;
            slice_count = 0;

            HRESULT hr = ID3D11VideoContext_DecoderBeginFrame(dec->shared->video_context, dec->shared->decoder, dec->shared->output_views[pic_array_slice], 0, NULL);
            if (FAILED(hr)) {
                set_last_error(dec, "ID3D11VideoContext::DecoderBeginFrame failed");
                decoder_shared_pool_release(dec->shared, pic_array_slice);
                windows_video_frame_release((MKFF_VideoFrame *)pic_frame);
                pic_frame = NULL;
                picture_open = 0;
                result = MKFF_RESULT_ERROR_DEVICE;
                break;
            }
        }

        if (slice_count < H264_MAX_NAL_UNITS_PER_AU) {
            /* ConfigBitstreamRaw == 2 expects Annex-B start codes in the
             * DXVA bitstream buffer (same as the HEVC path below). nal->data
             * from h264_split_annex_b points past the start code. */
            static const uint8_t start_code[3] = {0, 0, 1};
            uint32_t sc_offset = 0;
            if (bitstream_acc_append(&bitstream, start_code, sizeof(start_code), &sc_offset)) {
                uint32_t nal_offset = 0;
                if (bitstream_acc_append(&bitstream, nal->data, nal->size, &nal_offset)) {
                    slices[slice_count].BSNALunitDataLocation = sc_offset;
                    slices[slice_count].SliceBytesInBuffer = (UINT)(sizeof(start_code) + nal->size);
                    slices[slice_count].wBadSliceChopping = 0;
                    slice_count++;
                }
            }
        }
    }

    if (picture_open) {
        submit_frame_to_hardware(dec, pic_sps, &dec->pps_table[pic_first_sh.pic_parameter_set_id], &pic_first_sh, pic_poc, pic_array_slice, slices, slice_count, &bitstream);

        HRESULT hr = ID3D11VideoContext_DecoderEndFrame(dec->shared->video_context, dec->shared->decoder);
        if (FAILED(hr)) {
            DLOG(dec, MKFF_LOG_LEVEL_ERROR, "%s", "ID3D11VideoContext::DecoderEndFrame failed");
        }

        h264_dxva_dpb_add_reference(&dec->dpb, pic_frame, pic_first_sh.frame_num, pic_poc, pic_sps->max_num_ref_frames, pic_first_sh.nal_ref_idc != 0);
        h264_dxva_dpb_push_output(&dec->dpb, pic_frame, pic_poc);
        windows_video_frame_release((MKFF_VideoFrame *)pic_frame);

        uint32_t max_reorder = pic_sps->max_num_ref_frames;
        if (max_reorder > 16) max_reorder = 16;
        push_ready(dec, h264_dxva_dpb_bump_if_needed(&dec->dpb, max_reorder));
    }

    bitstream_acc_free(&bitstream);
    free(rbsp_scratch);
    return result;
}

MKFF_Result windows_video_decoder_submit(void *decoder_v, const uint8_t *annex_b_data, size_t annex_b_size, int64_t pts, int64_t dts) {
    WindowsVideoDecoder *dec = (WindowsVideoDecoder *)decoder_v;

    if (annex_b_size == 0) {
        return MKFF_RESULT_OK;
    }

    if (dec->codec == MKFF_VIDEO_CODEC_HEVC) {
        return windows_video_decoder_submit_hevc(dec, annex_b_data, annex_b_size, pts, dts);
    }
    return windows_video_decoder_submit_h264(dec, annex_b_data, annex_b_size, pts, dts);
}

MKFF_Result windows_video_decoder_receive(void *decoder_v, MKFF_VideoFrame **out_frame) {
    WindowsVideoDecoder *dec = (WindowsVideoDecoder *)decoder_v;

    if (dec->ready_count > 0) {
        *out_frame = (MKFF_VideoFrame *)dec->ready_queue[0];
        for (uint32_t i = 1; i < dec->ready_count; i++) {
            dec->ready_queue[i - 1] = dec->ready_queue[i];
        }
        dec->ready_count--;
        return MKFF_RESULT_OK;
    }

    return dec->flushed ? MKFF_RESULT_END_OF_STREAM : MKFF_RESULT_NOT_READY;
}

MKFF_Result windows_video_decoder_flush(void *decoder_v) {
    WindowsVideoDecoder *dec = (WindowsVideoDecoder *)decoder_v;

    WindowsVideoFrame *f;
    if (dec->codec == MKFF_VIDEO_CODEC_HEVC) {
        while ((f = hevc_dxva_dpb_bump_one(&dec->hevc_dpb)) != NULL) {
            push_ready(dec, f);
        }
    } else {
        while ((f = h264_dxva_dpb_bump_one(&dec->dpb)) != NULL) {
            push_ready(dec, f);
        }
    }
    dec->flushed = 1;
    return MKFF_RESULT_OK;
}

MKFF_Result windows_video_decoder_get_info(const void *decoder_v, MKFF_VideoDecoderInfo *out_info) {
    const WindowsVideoDecoder *dec = (const WindowsVideoDecoder *)decoder_v;

    uint32_t requested_size = out_info->struct_size;
    memset(out_info, 0, sizeof(*out_info));
    MKFF_INIT_STRUCT_HEADER(out_info);
    out_info->struct_size = requested_size ? requested_size : (uint32_t)sizeof(*out_info);

    out_info->width = dec->display_width;
    out_info->height = dec->display_height;
    out_info->profile = dec->mkff_profile;
    out_info->entrypoint = dec->initialized ? MKFF_VIDEO_ENTRYPOINT_VLD : MKFF_VIDEO_ENTRYPOINT_UNKNOWN;
    out_info->output_format = dec->output_format ? dec->output_format : MKFF_PIXEL_FORMAT_NV12;

    if (dec->shared) {
        uint32_t in_use = 0;
        EnterCriticalSection(&dec->shared->pool_lock);
        for (uint32_t i = 0; i < dec->shared->pool_capacity; i++) {
            if (dec->shared->pool_in_use[i]) in_use++;
        }
        LeaveCriticalSection(&dec->shared->pool_lock);
        out_info->surface_pool_size = in_use;
        out_info->surface_pool_capacity = dec->shared->pool_capacity;
    }

    if (out_info->struct_size >= offsetof(MKFF_VideoDecoderInfo, hardware) + sizeof(uint32_t)) {
        out_info->backend = MKFF_VIDEO_BACKEND_HARDWARE_ONLY;
        out_info->bit_depth = dec->bit_depth ? dec->bit_depth : (dec->initialized ? 8u : 0u);
        out_info->chroma_format_idc = dec->initialized ? 1u : 0u;
        out_info->hardware = dec->initialized ? 1u : 0u;
    }

    return MKFF_RESULT_OK;
}
