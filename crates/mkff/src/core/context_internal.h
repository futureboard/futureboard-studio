#ifndef MKFF_CORE_CONTEXT_INTERNAL_H
#define MKFF_CORE_CONTEXT_INTERNAL_H

#include "mkff/context.h"
#include "mkff/platform.h"

const MKFF_PlatformAPI *mkff_context_internal_get_platform_api(MKFF_Context *context);
MKFF_PlatformContext   *mkff_context_internal_get_platform_context(MKFF_Context *context);

#endif /* MKFF_CORE_CONTEXT_INTERNAL_H */
