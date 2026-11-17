#ifndef MKFF_WINDOWS_PLATFORM_API_TABLE_H
#define MKFF_WINDOWS_PLATFORM_API_TABLE_H

#include "mkff/platform.h"

/* Returns this module's own exported vtable instance, used to stamp
 * MKFF_HandleCommon.api on every decoder/frame object this module
 * creates. */
const MKFF_PlatformAPI *mkff_windows_platform_api(void);

#endif /* MKFF_WINDOWS_PLATFORM_API_TABLE_H */
