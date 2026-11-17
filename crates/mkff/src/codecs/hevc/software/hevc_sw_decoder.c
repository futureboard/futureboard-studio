#include "hevc_sw_decoder.h"

#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "ihevc_typedefs.h"
#include "iv.h"
#include "ivd.h"
#include "ihevcd_cxa.h"

#include "src/codecs/hevc/hevc_bitstream.h"
#include "src/codecs/hevc/hevc_vps_sps_pps.h"

#define MKFF_HEVC_SW_READY_CAP 8

struct HevcSwDecoder {
    iv_obj_t *codec;
    uint32_t width;
    uint32_t height;
    uint32_t bit_depth;
    MKFF_VideoProfile profile;
    MKFF_PixelFormat format;
    int header_done;
    int flush_mode;

    HevcSwFrame ready[MKFF_HEVC_SW_READY_CAP];
    uint32_t ready_count;

    uint8_t *out_y;
    uint8_t *out_uv;
    size_t out_y_size;
    size_t out_uv_size;
    ivd_out_bufdesc_t out_buf;
};

static void *mkff_aligned_alloc(void *ctxt, WORD32 alignment, WORD32 size) {
    (void)ctxt;
    if (alignment < (WORD32)sizeof(void *)) {
        alignment = (WORD32)sizeof(void *);
    }
#if defined(_MSC_VER)
    return _aligned_malloc((size_t)size, (size_t)alignment);
#else
    void *p = NULL;
    if (posix_memalign(&p, (size_t)alignment, (size_t)size) != 0) {
        return NULL;
    }
    return p;
#endif
}

static void mkff_aligned_free(void *ctxt, void *ptr) {
    (void)ctxt;
#if defined(_MSC_VER)
    _aligned_free(ptr);
#else
    free(ptr);
#endif
}

static IV_API_CALL_STATUS_T api(iv_obj_t *obj, void *ip, void *op) {
    return ihevcd_cxa_api_function(obj, ip, op);
}

static void push_ready(HevcSwDecoder *dec, HevcSwFrame *frame) {
    if (dec->ready_count >= MKFF_HEVC_SW_READY_CAP) {
        hevc_sw_frame_free(frame);
        return;
    }
    dec->ready[dec->ready_count++] = *frame;
    memset(frame, 0, sizeof(*frame));
}

void hevc_sw_frame_free(HevcSwFrame *frame) {
    if (!frame) {
        return;
    }
    free(frame->data[0]);
    free(frame->data[1]);
    memset(frame, 0, sizeof(*frame));
}

static MKFF_Result ensure_out_buffers(HevcSwDecoder *dec, uint32_t w, uint32_t h, int bytes_per_sample) {
    size_t y_size = (size_t)w * h * (size_t)bytes_per_sample;
    size_t uv_size = (size_t)w * (h / 2) * (size_t)bytes_per_sample;
    if (y_size > dec->out_y_size) {
        free(dec->out_y);
        dec->out_y = (uint8_t *)malloc(y_size);
        if (!dec->out_y) {
            return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
        }
        dec->out_y_size = y_size;
    }
    if (uv_size > dec->out_uv_size) {
        free(dec->out_uv);
        dec->out_uv = (uint8_t *)malloc(uv_size);
        if (!dec->out_uv) {
            return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
        }
        dec->out_uv_size = uv_size;
    }
    memset(&dec->out_buf, 0, sizeof(dec->out_buf));
    dec->out_buf.u4_num_bufs = 2;
    dec->out_buf.pu1_bufs[0] = dec->out_y;
    dec->out_buf.pu1_bufs[1] = dec->out_uv;
    dec->out_buf.u4_min_out_buf_size[0] = (UWORD32)y_size;
    dec->out_buf.u4_min_out_buf_size[1] = (UWORD32)uv_size;
    return MKFF_RESULT_OK;
}

static MKFF_Result set_decode_mode(HevcSwDecoder *dec, IVD_VIDEO_DECODE_MODE_T mode) {
    ihevcd_cxa_ctl_set_config_ip_t ip;
    ihevcd_cxa_ctl_set_config_op_t op;
    memset(&ip, 0, sizeof(ip));
    memset(&op, 0, sizeof(op));
    ip.s_ivd_ctl_set_config_ip_t.e_cmd = IVD_CMD_VIDEO_CTL;
    ip.s_ivd_ctl_set_config_ip_t.e_sub_cmd = IVD_CMD_CTL_SETPARAMS;
    ip.s_ivd_ctl_set_config_ip_t.e_vid_dec_mode = mode;
    ip.s_ivd_ctl_set_config_ip_t.e_frm_skip_mode = IVD_SKIP_NONE;
    ip.s_ivd_ctl_set_config_ip_t.e_frm_out_mode = IVD_DISPLAY_FRAME_OUT;
    ip.s_ivd_ctl_set_config_ip_t.u4_size = sizeof(ip);
    op.s_ivd_ctl_set_config_op_t.u4_size = sizeof(op);
    if (api(dec->codec, &ip, &op) != IV_SUCCESS) {
        return MKFF_RESULT_ERROR_DECODE;
    }
    return MKFF_RESULT_OK;
}

MKFF_Result hevc_sw_decoder_create(uint32_t width_hint, uint32_t height_hint, HevcSwDecoder **out) {
    if (!out) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }
    *out = NULL;
    HevcSwDecoder *dec = (HevcSwDecoder *)calloc(1, sizeof(*dec));
    if (!dec) {
        return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
    }
    dec->width = width_hint;
    dec->height = height_hint;
    dec->bit_depth = 8;
    dec->profile = MKFF_VIDEO_PROFILE_HEVC_MAIN;
    dec->format = MKFF_PIXEL_FORMAT_NV12;

    ihevcd_cxa_create_ip_t create_ip;
    ihevcd_cxa_create_op_t create_op;
    memset(&create_ip, 0, sizeof(create_ip));
    memset(&create_op, 0, sizeof(create_op));
    create_ip.s_ivd_create_ip_t.e_cmd = IVD_CMD_CREATE;
    create_ip.s_ivd_create_ip_t.u4_share_disp_buf = 0;
    create_ip.s_ivd_create_ip_t.e_output_format = IV_YUV_420SP_UV;
    create_ip.s_ivd_create_ip_t.pf_aligned_alloc = mkff_aligned_alloc;
    create_ip.s_ivd_create_ip_t.pf_aligned_free = mkff_aligned_free;
    create_ip.s_ivd_create_ip_t.pv_mem_ctxt = NULL;
    create_ip.s_ivd_create_ip_t.u4_size = sizeof(create_ip);
    create_op.s_ivd_create_op_t.u4_size = sizeof(create_op);

    if (api(NULL, &create_ip, &create_op) != IV_SUCCESS) {
        free(dec);
        return MKFF_RESULT_ERROR_CODEC_UNAVAILABLE;
    }
    dec->codec = (iv_obj_t *)create_op.s_ivd_create_op_t.pv_handle;
    if (!dec->codec) {
        free(dec);
        return MKFF_RESULT_ERROR_CODEC_UNAVAILABLE;
    }
    dec->codec->pv_fxns = (void *)&ihevcd_cxa_api_function;
    dec->codec->u4_size = sizeof(iv_obj_t);

    /* Single-core for portability (Windows). */
    {
        ihevcd_cxa_ctl_set_num_cores_ip_t ip;
        ihevcd_cxa_ctl_set_num_cores_op_t op;
        memset(&ip, 0, sizeof(ip));
        memset(&op, 0, sizeof(op));
        ip.e_cmd = IVD_CMD_VIDEO_CTL;
        ip.e_sub_cmd = (IVD_CONTROL_API_COMMAND_TYPE_T)IHEVCD_CXA_CMD_CTL_SET_NUM_CORES;
        ip.u4_num_cores = 1;
        ip.u4_size = sizeof(ip);
        op.u4_size = sizeof(op);
        (void)api(dec->codec, &ip, &op);
    }
    {
        ihevcd_cxa_ctl_set_processor_ip_t ip;
        ihevcd_cxa_ctl_set_processor_op_t op;
        memset(&ip, 0, sizeof(ip));
        memset(&op, 0, sizeof(op));
        ip.e_cmd = IVD_CMD_VIDEO_CTL;
        ip.e_sub_cmd = (IVD_CONTROL_API_COMMAND_TYPE_T)IHEVCD_CXA_CMD_CTL_SET_PROCESSOR;
        ip.u4_arch = ARCH_X86_SSE42;
        ip.u4_soc = SOC_GENERIC;
        ip.u4_size = sizeof(ip);
        op.u4_size = sizeof(op);
        (void)api(dec->codec, &ip, &op);
    }

    if (set_decode_mode(dec, IVD_DECODE_HEADER) != MKFF_RESULT_OK) {
        hevc_sw_decoder_destroy(dec);
        return MKFF_RESULT_ERROR_DECODE;
    }

    *out = dec;
    return MKFF_RESULT_OK;
}

void hevc_sw_decoder_destroy(HevcSwDecoder *dec) {
    if (!dec) {
        return;
    }
    for (uint32_t i = 0; i < dec->ready_count; i++) {
        hevc_sw_frame_free(&dec->ready[i]);
    }
    if (dec->codec) {
        ivd_delete_ip_t ip;
        ivd_delete_op_t op;
        memset(&ip, 0, sizeof(ip));
        memset(&op, 0, sizeof(op));
        ip.e_cmd = IVD_CMD_DELETE;
        ip.u4_size = sizeof(ip);
        op.u4_size = sizeof(op);
        (void)api(dec->codec, &ip, &op);
    }
    free(dec->out_y);
    free(dec->out_uv);
    free(dec);
}

static MKFF_Result decode_buffer(HevcSwDecoder *dec,
                                 const uint8_t *data,
                                 size_t size,
                                 int64_t pts,
                                 int64_t dts,
                                 int is_flush) {
    ihevcd_cxa_video_decode_ip_t dip;
    ihevcd_cxa_video_decode_op_t dop;
    memset(&dip, 0, sizeof(dip));
    memset(&dop, 0, sizeof(dop));

    if (dec->header_done) {
        int bps = (dec->bit_depth > 8) ? 2 : 1;
        uint32_t w = dec->width ? dec->width : 64;
        uint32_t h = dec->height ? dec->height : 64;
        MKFF_Result er = ensure_out_buffers(dec, w, h, bps);
        if (er != MKFF_RESULT_OK) {
            return er;
        }
        dip.s_ivd_video_decode_ip_t.s_out_buffer = dec->out_buf;
    }

    dip.s_ivd_video_decode_ip_t.e_cmd = IVD_CMD_VIDEO_DECODE;
    dip.s_ivd_video_decode_ip_t.u4_ts = (UWORD32)(pts == INT64_MIN ? 0 : (uint32_t)pts);
    dip.s_ivd_video_decode_ip_t.pv_stream_buffer = (void *)(uintptr_t)data;
    dip.s_ivd_video_decode_ip_t.u4_num_Bytes = (UWORD32)size;
    dip.s_ivd_video_decode_ip_t.u4_size = sizeof(dip);
    dop.s_ivd_video_decode_op_t.u4_size = sizeof(dop);

    IV_API_CALL_STATUS_T st = api(dec->codec, &dip, &dop);
    ivd_video_decode_op_t *op = &dop.s_ivd_video_decode_op_t;

    if (!dec->header_done && op->u4_pic_wd > 0 && op->u4_pic_ht > 0) {
        dec->width = op->u4_pic_wd;
        dec->height = op->u4_pic_ht;
        /* Heuristic: Main10 streams often report bit depth via residual; default 8 until known. */
        if (set_decode_mode(dec, IVD_DECODE_FRAME) != MKFF_RESULT_OK) {
            return MKFF_RESULT_ERROR_DECODE;
        }
        dec->header_done = 1;
        /* Re-submit same AU for frame decode if header-only consumed it. */
        if (size > 0 && !is_flush) {
            return decode_buffer(dec, data, size, pts, dts, 0);
        }
    }

    if (st != IV_SUCCESS && !op->u4_output_present) {
        if (is_flush) {
            return MKFF_RESULT_END_OF_STREAM;
        }
        /* Header / more data needed is not fatal. */
        return MKFF_RESULT_OK;
    }

    if (op->u4_output_present) {
        if (op->u4_pic_wd) {
            dec->width = op->u4_pic_wd;
        }
        if (op->u4_pic_ht) {
            dec->height = op->u4_pic_ht;
        }
        int bps = 1;
        /* 10-bit: libhevc may emit 16-bit samples in 420SP when Main10. */
        size_t y_bytes = (size_t)dec->out_buf.u4_min_out_buf_size[0];
        size_t expected8 = (size_t)dec->width * dec->height;
        if (y_bytes >= expected8 * 2) {
            bps = 2;
            dec->bit_depth = 10;
            dec->profile = MKFF_VIDEO_PROFILE_HEVC_MAIN10;
            dec->format = MKFF_PIXEL_FORMAT_P010;
        } else {
            dec->bit_depth = 8;
            dec->profile = MKFF_VIDEO_PROFILE_HEVC_MAIN;
            dec->format = MKFF_PIXEL_FORMAT_NV12;
        }

        HevcSwFrame frame;
        memset(&frame, 0, sizeof(frame));
        frame.width = dec->width;
        frame.height = dec->height;
        frame.format = dec->format;
        frame.bit_depth = dec->bit_depth;
        frame.pts = pts;
        frame.dts = dts;
        frame.is_key_frame = (op->e_pic_type == IV_IDR_FRAME || op->e_pic_type == IV_I_FRAME) ? 1 : 0;
        frame.stride[0] = dec->width * (uint32_t)bps;
        frame.stride[1] = dec->width * (uint32_t)bps;
        size_t y_size = (size_t)frame.stride[0] * frame.height;
        size_t uv_size = (size_t)frame.stride[1] * (frame.height / 2);
        frame.data[0] = (uint8_t *)malloc(y_size);
        frame.data[1] = (uint8_t *)malloc(uv_size);
        if (!frame.data[0] || !frame.data[1]) {
            hevc_sw_frame_free(&frame);
            return MKFF_RESULT_ERROR_OUT_OF_MEMORY;
        }
        memcpy(frame.data[0], dec->out_y, y_size);
        memcpy(frame.data[1], dec->out_uv, uv_size);

        /* Normalize 10-bit little-endian planar samples to P010 (MSB-aligned). */
        if (bps == 2) {
            uint16_t *y = (uint16_t *)frame.data[0];
            uint16_t *uv = (uint16_t *)frame.data[1];
            size_t y_count = (size_t)frame.width * frame.height;
            size_t uv_count = y_count / 2;
            for (size_t i = 0; i < y_count; i++) {
                /* libhevc typically stores LSB-aligned 10-bit in 16-bit; shift to MSB. */
                if ((y[i] & 0xFC00u) == 0) {
                    y[i] = (uint16_t)(y[i] << 6);
                }
            }
            for (size_t i = 0; i < uv_count; i++) {
                if ((uv[i] & 0xFC00u) == 0) {
                    uv[i] = (uint16_t)(uv[i] << 6);
                }
            }
        }
        push_ready(dec, &frame);
    }
    return MKFF_RESULT_OK;
}

/* Stock Ittiam libhevc rejects bit_depth_luma_minus8 != 0
 * (IHEVCD_UNSUPPORTED_BIT_DEPTH). Main10 SW is therefore unavailable;
 * callers should use hardware or fail with CODEC_UNAVAILABLE. */
static MKFF_Result reject_main10_if_present(const uint8_t *annex_b, size_t size) {
    if (!annex_b || size == 0) {
        return MKFF_RESULT_OK;
    }
    HevcNalUnit nals[HEVC_MAX_NAL_UNITS_PER_AU];
    size_t nal_count = hevc_split_annex_b(annex_b, size, nals, HEVC_MAX_NAL_UNITS_PER_AU);
    uint8_t rbsp_scratch[4096];
    for (size_t i = 0; i < nal_count; i++) {
        if (nals[i].nal_unit_type != HEVC_NAL_SPS_NUT) {
            continue;
        }
        HevcSPS sps;
        int rc = hevc_parse_sps(nals[i].data, nals[i].size, rbsp_scratch, &sps);
        if (rc == -1) {
            continue; /* truncated / unreadable */
        }
        /* bit_depth is filled even when check_main_main10 returns -2. */
        if (sps.bit_depth > 8) {
            return MKFF_RESULT_ERROR_CODEC_UNAVAILABLE;
        }
        break;
    }
    return MKFF_RESULT_OK;
}

MKFF_Result hevc_sw_decoder_submit(HevcSwDecoder *dec,
                                   const uint8_t *annex_b,
                                   size_t size,
                                   int64_t pts,
                                   int64_t dts) {
    if (!dec || (!annex_b && size > 0)) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }
    MKFF_Result main10 = reject_main10_if_present(annex_b, size);
    if (main10 != MKFF_RESULT_OK) {
        return main10;
    }
    return decode_buffer(dec, annex_b, size, pts, dts, 0);
}

MKFF_Result hevc_sw_decoder_receive(HevcSwDecoder *dec, HevcSwFrame *out_frame) {
    if (!dec || !out_frame) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }
    memset(out_frame, 0, sizeof(*out_frame));
    if (dec->ready_count == 0) {
        return dec->flush_mode ? MKFF_RESULT_END_OF_STREAM : MKFF_RESULT_NOT_READY;
    }
    *out_frame = dec->ready[0];
    memmove(&dec->ready[0], &dec->ready[1], sizeof(HevcSwFrame) * (dec->ready_count - 1));
    dec->ready_count--;
    return MKFF_RESULT_OK;
}

MKFF_Result hevc_sw_decoder_flush(HevcSwDecoder *dec) {
    if (!dec) {
        return MKFF_RESULT_ERROR_INVALID_ARGUMENT;
    }
    dec->flush_mode = 1;
    ivd_ctl_flush_ip_t ip;
    ivd_ctl_flush_op_t op;
    memset(&ip, 0, sizeof(ip));
    memset(&op, 0, sizeof(op));
    ip.e_cmd = IVD_CMD_VIDEO_CTL;
    ip.e_sub_cmd = IVD_CMD_CTL_FLUSH;
    ip.u4_size = sizeof(ip);
    op.u4_size = sizeof(op);
    (void)api(dec->codec, &ip, &op);

    uint32_t prev_ready = dec->ready_count;
    for (int i = 0; i < 32; i++) {
        MKFF_Result r = decode_buffer(dec, NULL, 0, INT64_MIN, INT64_MIN, 1);
        if (r == MKFF_RESULT_END_OF_STREAM) {
            break;
        }
        if (r != MKFF_RESULT_OK) {
            return r;
        }
        if (dec->ready_count == prev_ready) {
            break;
        }
        prev_ready = dec->ready_count;
        if (dec->ready_count >= MKFF_HEVC_SW_READY_CAP) {
            break;
        }
    }
    return MKFF_RESULT_OK;
}

uint32_t hevc_sw_decoder_width(const HevcSwDecoder *dec) { return dec ? dec->width : 0; }
uint32_t hevc_sw_decoder_height(const HevcSwDecoder *dec) { return dec ? dec->height : 0; }
uint32_t hevc_sw_decoder_bit_depth(const HevcSwDecoder *dec) { return dec ? dec->bit_depth : 0; }
MKFF_VideoProfile hevc_sw_decoder_profile(const HevcSwDecoder *dec) {
    return dec ? dec->profile : MKFF_VIDEO_PROFILE_UNKNOWN;
}
MKFF_PixelFormat hevc_sw_decoder_format(const HevcSwDecoder *dec) {
    return dec ? dec->format : MKFF_PIXEL_FORMAT_UNKNOWN;
}
