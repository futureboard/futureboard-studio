#ifndef MKFF_CORE_CPU_FEATURES_H
#define MKFF_CORE_CPU_FEATURES_H

#include <stdint.h>

#define MKFF_CPU_FEATURE_AVX2 (1u << 0)

/* Runtime CPU feature bitmask. Always 0 on non-x86 targets. */
uint32_t mkff_cpu_detect_features(void);

#endif /* MKFF_CORE_CPU_FEATURES_H */
