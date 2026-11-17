#ifndef MKFF_LOG_H
#define MKFF_LOG_H

#include <stdint.h>
#include "mkff/export.h"

MKFF_BEGIN_DECLS

typedef enum MKFF_LogLevel {
    MKFF_LOG_LEVEL_TRACE = 0,
    MKFF_LOG_LEVEL_DEBUG = 1,
    MKFF_LOG_LEVEL_INFO  = 2,
    MKFF_LOG_LEVEL_WARN  = 3,
    MKFF_LOG_LEVEL_ERROR = 4,
    MKFF_LOG_LEVEL_NONE  = 5
} MKFF_LogLevel;

/* Called from arbitrary internal threads; must be reentrant. `message` is
 * only valid for the duration of the call. */
typedef void (*MKFF_LogCallback)(void *user_data,
                                  MKFF_LogLevel level,
                                  const char *component,
                                  const char *message);

MKFF_END_DECLS

#endif /* MKFF_LOG_H */
