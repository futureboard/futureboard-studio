/*
 * Provides storage for GUID constants declared `EXTERN_GUID` in the
 * Windows SDK headers (win_common.h's <dxva.h>/<dxva2api.h>) that
 * dxguid.lib doesn't actually supply a definition for —
 * DXVA2_ModeH264_VLD_NoFGT among them (confirmed via a real link
 * failure: "unresolved external symbol DXVA2_ModeH264_..."). Defining
 * INITGUID before the *first* inclusion of these headers in a
 * translation unit switches EXTERN_GUID from an `extern const GUID`
 * declaration to an actual `DEFINE_GUID` definition. This must happen
 * in exactly one .c file in the whole link (here), or every other
 * translation unit — which all get plain `extern` declarations by
 * *not* defining INITGUID before their own inclusion of win_common.h —
 * would instead collide with duplicate-definition errors.
 */
#define INITGUID
#include "win_common.h"
