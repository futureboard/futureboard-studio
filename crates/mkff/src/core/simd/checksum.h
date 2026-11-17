#ifndef MKFF_CORE_SIMD_CHECKSUM_H
#define MKFF_CORE_SIMD_CHECKSUM_H

#include <stddef.h>
#include <stdint.h>

/* Dispatch seam for future SIMD kernels: scalar C is always correct and
 * always available; the x86_64 AVX2 path is used automatically when the
 * running CPU supports it. No decoded pixel data is ever routed through
 * the CPU in this milestone (decode stays GPU-native end to end) — this
 * exists as internal infrastructure (and its own test coverage) for the
 * SIMD dispatch mechanism itself. */
uint32_t mkff_internal_buffer_checksum(const void *data, size_t len);

/* Always available for tests to force the pure-scalar path independent
 * of what the host CPU supports. */
uint32_t mkff_internal_buffer_checksum_scalar(const void *data, size_t len);

#endif /* MKFF_CORE_SIMD_CHECKSUM_H */
