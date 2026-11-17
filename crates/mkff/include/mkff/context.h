#ifndef MKFF_CONTEXT_H
#define MKFF_CONTEXT_H

#include <stdint.h>
#include "mkff/abi.h"
#include "mkff/error.h"
#include "mkff/export.h"
#include "mkff/log.h"
#include "mkff/types.h"

MKFF_BEGIN_DECLS

/*
 * MKFF_ContextDesc is an extensible struct: always zero it (or use
 * MKFF_INIT_STRUCT_HEADER) before setting fields so future fields added
 * after `reserved` default safely.
 */
typedef struct MKFF_ContextDesc {
    MKFF_STRUCT_HEADER;

    MKFF_LogCallback log_callback;   /* may be NULL: logs are dropped */
    void            *log_user_data;
    MKFF_LogLevel    log_min_level;  /* messages below this level are dropped */

    /* Overrides the platform module search path. NULL selects the
     * built-in default resolution (next to libmkff's own binary, then the
     * dynamic linker's standard search rules). Never resolved relative to
     * the process current working directory. */
    const char *platform_module_path;
} MKFF_ContextDesc;

MKFF_API MKFF_Result mkff_context_create(const MKFF_ContextDesc *desc, MKFF_Context **out_context);
MKFF_API void         mkff_context_destroy(MKFF_Context *context);

/* Returns a pointer to a human-readable description of the last error
 * that occurred on this context. Valid until the next call on the same
 * context from the same thread. Never NULL. */
MKFF_API const char *mkff_context_get_last_error(MKFF_Context *context);

MKFF_END_DECLS

#endif /* MKFF_CONTEXT_H */
