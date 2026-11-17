#include "mkff/error.h"

const char *mkff_result_to_string(MKFF_Result result) {
    switch (result) {
        case MKFF_RESULT_OK:                      return "ok";
        case MKFF_RESULT_NOT_READY:                return "not ready";
        case MKFF_RESULT_END_OF_STREAM:            return "end of stream";
        case MKFF_RESULT_ERROR_INVALID_ARGUMENT:   return "invalid argument";
        case MKFF_RESULT_ERROR_OUT_OF_MEMORY:      return "out of memory";
        case MKFF_RESULT_ERROR_ABI_MISMATCH:       return "ABI mismatch";
        case MKFF_RESULT_ERROR_PLATFORM_LOAD:      return "platform module load failure";
        case MKFF_RESULT_ERROR_NOT_SUPPORTED:      return "not supported";
        case MKFF_RESULT_ERROR_DEVICE:              return "device error";
        case MKFF_RESULT_ERROR_BITSTREAM:           return "bitstream error";
        case MKFF_RESULT_ERROR_DECODE:              return "decode error";
        case MKFF_RESULT_ERROR_POOL_EXHAUSTED:      return "surface pool exhausted";
        case MKFF_RESULT_ERROR_INTERNAL:            return "internal error";
        case MKFF_RESULT_ERROR_CODEC_UNAVAILABLE:   return "codec unavailable";
        default:                                    return "unknown result";
    }
}
