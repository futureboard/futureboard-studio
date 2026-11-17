#ifndef MKFF_INTERNAL_COMMON_H
#define MKFF_INTERNAL_COMMON_H

/*
 * Private header shared between libmkff (core) and platform modules
 * (e.g. libmkff_platform_linux). Never installed, never seen by
 * application code — MKFF_VideoDecoder / MKFF_VideoFrame remain fully
 * opaque C handles from the public API's point of view.
 *
 * Every concrete object a platform module hands back to the core as an
 * MKFF_VideoDecoder* or MKFF_VideoFrame* must embed MKFF_HandleCommon as
 * its first struct member. That lets the core dispatch retain/release/
 * get_info/etc. generically without knowing the platform's real struct
 * layout: it reads the leading `api` pointer and calls through it.
 */

#include "mkff/platform.h"

typedef struct MKFF_HandleCommon {
    const MKFF_PlatformAPI *api;
} MKFF_HandleCommon;

#if defined(_WIN32)
#define MKFF_PLATFORM_MODULE_FILENAME "MKFF.Platform.Windows.dll"
#elif defined(__APPLE__)
#define MKFF_PLATFORM_MODULE_FILENAME "libmkff_platform_macos.dylib"
#else
#define MKFF_PLATFORM_MODULE_FILENAME "libmkff_platform_linux.so"
#endif

#endif /* MKFF_INTERNAL_COMMON_H */
