#include "platform_loader.h"

#include <stdio.h>
#include <string.h>

#include "mkff/context.h"
#include "src/common/mkff_common.h"

/* Anchor symbol used purely so the platform loader can tell which file
 * libmkff itself was loaded from (dladdr() on POSIX, GetModuleHandleExA()
 * on Windows). */
extern MKFF_Result mkff_context_create(const MKFF_ContextDesc *desc, MKFF_Context **out_context);

#if defined(_WIN32)

#include <windows.h>

static MKFF_Result try_load(const char *path, MKFF_PlatformModule *out_module) {
    HMODULE handle = LoadLibraryA(path);
    if (!handle) {
        return MKFF_RESULT_ERROR_PLATFORM_LOAD;
    }

    FARPROC symbol = GetProcAddress(handle, MKFF_PLATFORM_GET_API_SYMBOL);
    if (!symbol) {
        FreeLibrary(handle);
        return MKFF_RESULT_ERROR_PLATFORM_LOAD;
    }

    MKFF_PlatformGetApiFn get_api = (MKFF_PlatformGetApiFn)(void *)symbol;
    const MKFF_PlatformAPI *api = get_api(MKFF_PLATFORM_ABI_VERSION);
    if (!api) {
        FreeLibrary(handle);
        return MKFF_RESULT_ERROR_ABI_MISMATCH;
    }

    out_module->dl_handle = (void *)handle;
    out_module->api = api;
    return MKFF_RESULT_OK;
}

static int build_path_next_to_self(char *out, size_t out_size, const char *filename) {
    HMODULE self_module = NULL;
    if (!GetModuleHandleExA(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                             (LPCSTR)(void *)&mkff_context_create, &self_module)) {
        return 0;
    }

    char self_path[MAX_PATH];
    DWORD len = GetModuleFileNameA(self_module, self_path, sizeof(self_path));
    if (len == 0 || len >= sizeof(self_path)) {
        return 0;
    }

    char *slash = strrchr(self_path, '\\');
    if (!slash) {
        return 0;
    }
    *slash = '\0';

    int written = snprintf(out, out_size, "%s\\%s", self_path, filename);
    return written > 0 && (size_t)written < out_size;
}

MKFF_Result mkff_platform_module_load(const char *override_path, MKFF_PlatformModule *out_module) {
    memset(out_module, 0, sizeof(*out_module));

    if (override_path && (override_path[1] == ':' || override_path[0] == '\\')) {
        return try_load(override_path, out_module);
    }

    char candidate[MAX_PATH];
    const char *filename = override_path ? override_path : MKFF_PLATFORM_MODULE_FILENAME;

    if (build_path_next_to_self(candidate, sizeof(candidate), filename)) {
        if (try_load(candidate, out_module) == MKFF_RESULT_OK) {
            return MKFF_RESULT_OK;
        }
    }

    /* Falls back to the OS loader's standard search rules (application
     * directory, system directories, PATH) — LoadLibraryA() with a bare
     * filename never implicitly searches the current working directory
     * on modern Windows (SafeDllSearchMode has been the default since
     * XP SP1 / Server 2003). */
    return try_load(filename, out_module);
}

void mkff_platform_module_unload(MKFF_PlatformModule *module) {
    /* Deliberately does NOT call FreeLibrary(): MKFF_VideoDecoder /
     * MKFF_VideoFrame objects carry a vtable pointer into this module
     * and are explicitly allowed to outlive the MKFF_Context that
     * created them (see the RTLD_NODELETE comment in the POSIX half of
     * this file for the full rationale — same tradeoff, no direct
     * FreeLibrary equivalent to RTLD_NODELETE on Windows, so we just
     * skip the call). */
    module->dl_handle = NULL;
    module->api = NULL;
}

#else /* POSIX: Linux, macOS */

#include <dlfcn.h>

static MKFF_Result try_load(const char *path, MKFF_PlatformModule *out_module) {
    /* RTLD_NODELETE: MKFF_VideoDecoder/MKFF_VideoFrame objects carry a
     * vtable pointer into this module (see MKFF_HandleCommon) and are
     * explicitly allowed to outlive the MKFF_Context that created them
     * (frames are reference-counted independently; see
     * mkff_video_frame_retain()). Without RTLD_NODELETE, a Context
     * destroyed while such objects are still alive would dlclose() the
     * module out from under them, leaving those vtable pointers
     * dangling. The module stays mapped for the life of the process
     * instead — a standard tradeoff for plugin-style loaders whose
     * loaded objects can outlive the loader handle. */
    void *handle = dlopen(path, RTLD_NOW | RTLD_LOCAL | RTLD_NODELETE);
    if (!handle) {
        return MKFF_RESULT_ERROR_PLATFORM_LOAD;
    }

    dlerror(); /* clear */
    void *symbol = dlsym(handle, MKFF_PLATFORM_GET_API_SYMBOL);
    if (!symbol) {
        dlclose(handle);
        return MKFF_RESULT_ERROR_PLATFORM_LOAD;
    }

    MKFF_PlatformGetApiFn get_api = (MKFF_PlatformGetApiFn)symbol;
    const MKFF_PlatformAPI *api = get_api(MKFF_PLATFORM_ABI_VERSION);
    if (!api) {
        dlclose(handle);
        return MKFF_RESULT_ERROR_ABI_MISMATCH;
    }

    out_module->dl_handle = handle;
    out_module->api = api;
    return MKFF_RESULT_OK;
}

static int build_path_next_to_self(char *out, size_t out_size, const char *filename) {
    Dl_info info;
    if (!dladdr((void *)&mkff_context_create, &info) || !info.dli_fname) {
        return 0;
    }

    char self_path[4096];
    size_t n = strlen(info.dli_fname);
    if (n >= sizeof(self_path)) {
        return 0;
    }
    memcpy(self_path, info.dli_fname, n + 1);

    /* Manual dirname: keep this allocation-free and independent of
     * libgen.h's in-place-mutation quirks. */
    char *slash = strrchr(self_path, '/');
    if (!slash) {
        return 0;
    }
    *slash = '\0';

    int written = snprintf(out, out_size, "%s/%s", self_path, filename);
    return written > 0 && (size_t)written < out_size;
}

MKFF_Result mkff_platform_module_load(const char *override_path, MKFF_PlatformModule *out_module) {
    memset(out_module, 0, sizeof(*out_module));

    if (override_path && override_path[0] == '/') {
        return try_load(override_path, out_module);
    }

    char candidate[4096];
    const char *filename = override_path ? override_path : MKFF_PLATFORM_MODULE_FILENAME;

    if (build_path_next_to_self(candidate, sizeof(candidate), filename)) {
        if (try_load(candidate, out_module) == MKFF_RESULT_OK) {
            return MKFF_RESULT_OK;
        }
    }

    /* Falls back to the dynamic linker's standard search rules. This is
     * NOT the same as consulting argv[0]'s directory or getcwd(): a bare
     * filename passed to dlopen() never implicitly includes "." unless
     * the embedding application put it in LD_LIBRARY_PATH itself. */
    return try_load(filename, out_module);
}

void mkff_platform_module_unload(MKFF_PlatformModule *module) {
    if (module->dl_handle) {
        dlclose(module->dl_handle);
    }
    module->dl_handle = NULL;
    module->api = NULL;
}

#endif
