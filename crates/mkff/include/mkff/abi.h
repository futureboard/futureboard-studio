#ifndef MKFF_ABI_H
#define MKFF_ABI_H

#include <string.h>
#include <stdint.h>

/*
 * MKFF ABI versioning.
 *
 * The core library (libmkff) and platform modules (libmkff_platform_*)
 * negotiate compatibility through a single monotonically increasing
 * integer. A platform module built against ABI version N is expected to
 * refuse to load (return NULL from mkff_platform_get_api) when asked for
 * an ABI version it does not implement.
 *
 * Every extensible public struct begins with the same three fields so
 * callers and callees can detect size/layout mismatches at runtime:
 *
 *   uint32_t struct_size;   // sizeof(ThisStruct) as compiled by the caller
 *   uint32_t abi_version;   // MKFF_ABI_VERSION as compiled by the caller
 *   uint32_t reserved[4];   // zero-initialized, reserved for future use
 */

#define MKFF_ABI_VERSION 1u
#define MKFF_PLATFORM_ABI_VERSION 1u

#define MKFF_STRUCT_HEADER \
    uint32_t struct_size; \
    uint32_t abi_version; \
    uint32_t reserved[4]

/*
 * Zeroes the entire struct first (so every field not explicitly set by
 * the caller — including any added in a future ABI revision — has a
 * safe default) and then stamps the struct_size/abi_version header.
 */
#define MKFF_INIT_STRUCT_HEADER(pstruct) \
    do { \
        memset((pstruct), 0, sizeof(*(pstruct))); \
        (pstruct)->struct_size = (uint32_t)sizeof(*(pstruct)); \
        (pstruct)->abi_version = MKFF_ABI_VERSION; \
    } while (0)

#endif /* MKFF_ABI_H */
