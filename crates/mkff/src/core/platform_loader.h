#ifndef MKFF_CORE_PLATFORM_LOADER_H
#define MKFF_CORE_PLATFORM_LOADER_H

#include "mkff/error.h"
#include "mkff/platform.h"

typedef struct MKFF_PlatformModule {
    void *dl_handle;
    const MKFF_PlatformAPI *api;
} MKFF_PlatformModule;

/* Locates and dlopen()s the platform module, then resolves and calls its
 * mkff_platform_get_api() entry point at MKFF_PLATFORM_ABI_VERSION.
 *
 * Resolution order (never dependent on the process current working
 * directory):
 *   1. `override_path` if non-NULL: used verbatim if absolute, otherwise
 *      resolved next to libmkff's own binary.
 *   2. The directory containing libmkff's own loaded binary, discovered
 *      via dladdr() on a symbol inside libmkff.
 *   3. A bare dlopen() by filename, which defers to the dynamic linker's
 *      standard search rules (DT_RUNPATH/RPATH, LD_LIBRARY_PATH,
 *      /etc/ld.so.cache, default system paths) — none of which consult
 *      the current working directory.
 */
MKFF_Result mkff_platform_module_load(const char *override_path, MKFF_PlatformModule *out_module);
void        mkff_platform_module_unload(MKFF_PlatformModule *module);

#endif /* MKFF_CORE_PLATFORM_LOADER_H */
