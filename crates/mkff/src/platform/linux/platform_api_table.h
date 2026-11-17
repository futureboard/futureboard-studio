#ifndef MKFF_LINUX_PLATFORM_API_TABLE_H
#define MKFF_LINUX_PLATFORM_API_TABLE_H

#include "mkff/platform.h"

/* Returns this module's own exported vtable instance, used to stamp
 * MKFF_HandleCommon.api on every decoder/frame object this module
 * creates. */
const MKFF_PlatformAPI *mkff_linux_platform_api(void);

#endif /* MKFF_LINUX_PLATFORM_API_TABLE_H */
