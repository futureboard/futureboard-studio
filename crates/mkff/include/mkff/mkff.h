#ifndef MKFF_H
#define MKFF_H

/*
 * MediaKit Foundation Framework (MKFF) — public C API umbrella header.
 *
 * Portable core surface. Linux-specific extensions (DRM device
 * enumeration, VA-API introspection, dma-buf export) live under
 * mkff/linux/ and are only meaningful when libmkff_platform_linux.so
 * is available at runtime.
 */

#include "mkff/abi.h"
#include "mkff/context.h"
#include "mkff/cpu_planes.h"
#include "mkff/error.h"
#include "mkff/export.h"
#include "mkff/log.h"
#include "mkff/mp4_demux.h"
#include "mkff/types.h"
#include "mkff/video_decoder.h"
#include "mkff/video_frame.h"

#endif /* MKFF_H */
