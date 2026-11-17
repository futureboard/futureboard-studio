#ifndef MKFF_ERROR_H
#define MKFF_ERROR_H

#include "mkff/export.h"

MKFF_BEGIN_DECLS

typedef enum MKFF_Result {
    MKFF_RESULT_OK              = 0,
    MKFF_RESULT_NOT_READY       = 1,  /* e.g. decoder has no frame to receive yet */
    MKFF_RESULT_END_OF_STREAM   = 2,

    MKFF_RESULT_ERROR_INVALID_ARGUMENT   = -1,
    MKFF_RESULT_ERROR_OUT_OF_MEMORY      = -2,
    MKFF_RESULT_ERROR_ABI_MISMATCH       = -3,
    MKFF_RESULT_ERROR_PLATFORM_LOAD      = -4,
    MKFF_RESULT_ERROR_NOT_SUPPORTED      = -5,
    MKFF_RESULT_ERROR_DEVICE             = -6,
    MKFF_RESULT_ERROR_BITSTREAM          = -7,
    MKFF_RESULT_ERROR_DECODE             = -8,
    MKFF_RESULT_ERROR_POOL_EXHAUSTED     = -9,
    MKFF_RESULT_ERROR_INTERNAL           = -10,
    MKFF_RESULT_ERROR_CODEC_UNAVAILABLE  = -11  /* requested codec/backend not built or not present */
} MKFF_Result;

MKFF_API const char *mkff_result_to_string(MKFF_Result result);

MKFF_END_DECLS

#endif /* MKFF_ERROR_H */
