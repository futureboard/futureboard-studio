/* MSVC-compatible stand-in for libhevc common/x86/ihevc_platform_macros.h */
#ifndef _IHEVC_PLATFORM_MACROS_H_
#define _IHEVC_PLATFORM_MACROS_H_

#include <intrin.h>

#ifndef CLIP3
#define CLIP3(x, minv, maxv) (((x) < (minv)) ? (minv) : (((x) > (maxv)) ? (maxv) : (x)))
#endif

#define CLIP_U8(x) CLIP3((x), 0,     255)
#define CLIP_S8(x) CLIP3((x), -128,  127)
#define CLIP_U10(x) CLIP3((x), 0,     1023)
#define CLIP_S10(x) CLIP3((x), -512,  511)
#define CLIP_U12(x) CLIP3((x), 0,     4095)
#define CLIP_S12(x) CLIP3((x), -2048,  2047)
#define CLIP_U14(x) CLIP3((x), 0,     16383)
#define CLIP_S14(x) CLIP3((x), -8192,  8191)
#define CLIP_U16(x) CLIP3((x), 0,        65535)
#define CLIP_S16(x) CLIP3((x), -32768,   32767)

#define SHL(x,y) (((y) < 32) ? ((x) << (y)) : 0)
#define SHR(x,y) (((y) < 32) ? ((x) >> (y)) : 0)
#define SHR_NEG(val,shift)  ((shift>0)?(val>>shift):(val<<(-shift)))
#define SHL_NEG(val,shift)  ((shift<0)?(val>>(-shift)):(val<<shift))

#define ITT_BIG_ENDIAN(x)       ((x << 24))                |   \
                            ((x & 0x0000ff00) << 8)    |   \
                            ((x & 0x00ff0000) >> 8)    |   \
                            ((UWORD32)x >> 24)

#define NOP(nop_cnt)    do { UWORD32 nop_i; for (nop_i = 0; nop_i < (UWORD32)(nop_cnt); nop_i++) { _mm_pause(); } } while (0)

static __forceinline unsigned int mkff_popcnt_u32(unsigned int x) {
    return (unsigned int)__popcnt(x);
}
#define POPCNT_U32(x) mkff_popcnt_u32((unsigned int)(x))

#define PLD(a)
#define INLINE __inline

static INLINE UWORD32 CLZ(UWORD32 u4_word)
{
    unsigned long index;
    if (u4_word && _BitScanReverse(&index, u4_word)) {
        return 31u - (UWORD32)index;
    }
    return 31;
}

static INLINE UWORD32 CLZNZ(UWORD32 u4_word)
{
    unsigned long index;
    _BitScanReverse(&index, u4_word);
    return 31u - (UWORD32)index;
}

static INLINE UWORD32 CTZ(UWORD32 u4_word)
{
    unsigned long index;
    if (0 == u4_word) {
        return 31;
    }
    _BitScanForward(&index, u4_word);
    return (UWORD32)index;
}

#define DATA_SYNC()  _mm_mfence()

#define GET_POS_MSB_32(r,word)                         \
{                                                      \
    if(word)                                           \
    {                                                  \
        r = 31 - (int)CLZ((UWORD32)(word));            \
    }                                                  \
    else                                               \
    {                                                  \
        r = -1;                                        \
    }                                                  \
}

#define GET_POS_MSB_64(r,word)                          \
{                                                       \
    if(word)                                            \
    {                                                   \
        unsigned long index;                            \
        if (_BitScanReverse64(&index, (unsigned __int64)(word))) { \
            r = 63 - (int)index;                        \
        } else { r = -1; }                              \
    }                                                   \
    else                                                \
    {                                                   \
        r = -1;                                         \
    }                                                   \
}

#define GETRANGE(r,word)                               \
{                                                      \
    if(word)                                           \
    {                                                  \
        r = 32 - (int)CLZ((UWORD32)(word));            \
    }                                                  \
    else                                               \
    {                                                  \
        r = 1;                                         \
    }                                                  \
}

#define GETRANGE64(r,llword)                             \
{                                                        \
    if(llword)                                           \
    {                                                    \
        unsigned long index;                             \
        if (_BitScanReverse64(&index, (unsigned __int64)(llword))) { \
            r = 64 - (int)index;                         \
        } else { r = 1; }                                \
    }                                                    \
    else                                                 \
    {                                                    \
        r = 1;                                           \
    }                                                    \
}

#define GCC_ENABLE 0
/* Call sites omit the trailing semicolon — include it in the macro. */
#undef PREFETCH
#define PREFETCH_ENABLE 0
#define PREFETCH(ptr, type) ((void)0);

#define MEM_ALIGN8  __declspec(align(8))
#define MEM_ALIGN16 __declspec(align(16))
#define MEM_ALIGN32 __declspec(align(32))

#endif /* _IHEVC_PLATFORM_MACROS_H_ */
