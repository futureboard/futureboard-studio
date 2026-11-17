#include "cpu_features.h"

#if (defined(__x86_64__) || defined(__i386__)) && !defined(_MSC_VER)

/* __builtin_cpu_supports is a GCC/Clang extension. There is no AVX2
 * assembly kernel for MSVC to dispatch to yet (see checksum.c's
 * MKFF_NO_AVX2_ASM path), so detection is only wired up for the
 * compilers that can actually use the result. */

uint32_t mkff_cpu_detect_features(void) {
    __builtin_cpu_init();
    uint32_t features = 0;
    if (__builtin_cpu_supports("avx2")) {
        features |= MKFF_CPU_FEATURE_AVX2;
    }
    return features;
}

#else

uint32_t mkff_cpu_detect_features(void) {
    return 0;
}

#endif
