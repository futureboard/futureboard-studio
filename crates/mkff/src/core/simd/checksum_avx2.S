#if defined(__x86_64__)

/*
 * uint32_t mkff_checksum_avx2_aligned32(const uint8_t *data, size_t aligned_len);
 *
 * SysV x86-64: rdi = data, rsi = aligned_len (multiple of 32, may be 0).
 * Returns the sum of all bytes truncated to 32 bits in eax (the SysV ABI
 * defines a uint32_t return as living in eax; the caller ignores the
 * upper 32 bits of rax, which is exactly "sum mod 2^32").
 *
 * Uses VPSADBW against an all-zero comparand, which sums the 8 bytes in
 * each 8-byte lane into that lane's low 16 bits at zero cost versus a
 * generic horizontal add — the standard trick for SIMD byte-sum kernels.
 */

.text
.globl mkff_checksum_avx2_aligned32
.type mkff_checksum_avx2_aligned32,@function
.align 16
mkff_checksum_avx2_aligned32:
    vpxor   %ymm3, %ymm3, %ymm3        # 64-bit lane accumulator = 0
    vpxor   %ymm4, %ymm4, %ymm4        # zero comparand for vpsadbw
    xorq    %rax, %rax                 # byte offset = 0
    testq   %rsi, %rsi
    jz      .Lmkff_checksum_avx2_done

.Lmkff_checksum_avx2_loop:
    vmovdqu (%rdi,%rax), %ymm0
    vpsadbw %ymm4, %ymm0, %ymm1        # per 8-byte lane: sum of bytes
    vpaddq  %ymm1, %ymm3, %ymm3        # accumulate (64-bit lanes, no overflow risk here)
    addq    $32, %rax
    cmpq    %rsi, %rax
    jb      .Lmkff_checksum_avx2_loop

.Lmkff_checksum_avx2_done:
    vextracti128 $1, %ymm3, %xmm1      # high 128 bits (lanes 2,3)
    vpaddq       %xmm1, %xmm3, %xmm0   # xmm0 = lane0+lane2, lane1+lane3
    vpshufd      $0x4e, %xmm0, %xmm1   # swap the two 64-bit halves
    vpaddq       %xmm1, %xmm0, %xmm0   # xmm0[63:0] == total sum in both halves
    vmovq        %xmm0, %rax
    vzeroupper
    ret
.size mkff_checksum_avx2_aligned32, . - mkff_checksum_avx2_aligned32

#if defined(__ELF__) && defined(__linux__)
.section .note.GNU-stack,"",@progbits
#endif

#endif /* __x86_64__ */
