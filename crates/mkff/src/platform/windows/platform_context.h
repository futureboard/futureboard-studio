#ifndef MKFF_WINDOWS_PLATFORM_CONTEXT_H
#define MKFF_WINDOWS_PLATFORM_CONTEXT_H

#include "mkff/log.h"

/* Definition of the opaque MKFF_PlatformContext declared in mkff/platform.h. */
struct MKFF_PlatformContext {
    MKFF_LogCallback log_callback;
    void            *log_user_data;
    MKFF_LogLevel    log_min_level;
    char             last_error[256];
};

#endif /* MKFF_WINDOWS_PLATFORM_CONTEXT_H */
