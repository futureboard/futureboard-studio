#include "checksum.h"

#include "../cpu_features.h"

#if defined(__x86_64__) && !defined(MKFF_NO_AVX2_ASM)
/* Implemented in checksum_avx2.S. Processes exactly `aligned_len` bytes
 * (caller guarantees aligned_len % 32 == 0) and returns the byte sum
 * truncated to 32 bits via VPSADBW-based horizontal summation. */
extern uint32_t mkff_checksum_avx2_aligned32(const uint8_t *data, size_t aligned_len);
#endif

uint32_t mkff_internal_buffer_checksum_scalar(const void *data, size_t len) {
    const uint8_t *bytes = (const uint8_t *)data;
    uint32_t sum = 0;
    for (size_t i = 0; i < len; i++) {
        sum += bytes[i];
    }
    return sum;
}

uint32_t mkff_internal_buffer_checksum(const void *data, size_t len) {
    const uint8_t *bytes = (const uint8_t *)data;

#if defined(__x86_64__) && !defined(MKFF_NO_AVX2_ASM)
    if (len >= 32 && (mkff_cpu_detect_features() & MKFF_CPU_FEATURE_AVX2)) {
        size_t aligned_len = len - (len % 32);
        uint32_t sum = mkff_checksum_avx2_aligned32(bytes, aligned_len);
        sum += mkff_internal_buffer_checksum_scalar(bytes + aligned_len, len - aligned_len);
        return sum;
    }
#endif

    return mkff_internal_buffer_checksum_scalar(bytes, len);
}
