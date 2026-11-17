#ifndef MKFF_INTERNAL_LOG_UTIL_H
#define MKFF_INTERNAL_LOG_UTIL_H

#include <stdarg.h>
#include <stdio.h>

#include "mkff/log.h"

/* Shared formatting helper used by both the core and platform modules so
 * every component logs through the same MKFF_LogCallback contract. */
static inline void mkff_log_emit(MKFF_LogCallback cb,
                                  void *user_data,
                                  MKFF_LogLevel min_level,
                                  MKFF_LogLevel level,
                                  const char *component,
                                  const char *fmt,
                                  ...) {
    if (!cb || level < min_level) {
        return;
    }
    char buffer[512];
    va_list args;
    va_start(args, fmt);
    vsnprintf(buffer, sizeof(buffer), fmt, args);
    va_end(args);
    cb(user_data, level, component, buffer);
}

#define MKFF_LOG(cb, ud, minlvl, lvl, component, ...) \
    mkff_log_emit((cb), (ud), (minlvl), (lvl), (component), __VA_ARGS__)

#endif /* MKFF_INTERNAL_LOG_UTIL_H */
