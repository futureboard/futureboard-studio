#ifndef MKFF_WINDOWS_WIN_COMMON_H
#define MKFF_WINDOWS_WIN_COMMON_H

/*
 * Single include point for the Windows/D3D11/DXVA headers so every
 * translation unit in this platform module sees the same macro setup
 * and include order:
 *
 *   WIN32_LEAN_AND_MEAN - skip rarely-needed Win32 API surface
 *   COBJMACROS          - enables the C-callable
 *                          Interface_Method(instance, args...) macros
 *                          for COM calls (otherwise every call site
 *                          would need the more verbose
 *                          instance->lpVtbl->Method(instance, args...)
 *                          form)
 *
 * <windows.h> must come first: d3d11.h/dxva.h/dxva2api.h all assume
 * its fundamental macros (EXTERN_C, CONST, ...) and DEFINE_GUID setup
 * are already in scope — including it last produced malformed
 * DEFINE_GUID expansions in dxva.h (MSVC error C2059 at each GUID
 * definition).
 *
 * <d3d9.h> is included purely for the D3DPOOL type: dxva2api.h's
 * struct definitions reference it even though this module never uses
 * any actual D3D9 (or DXVA2 IDirectXVideoDecoder) functionality — only
 * dxva2api.h's H.264 decoder profile GUID constant
 * (DXVA2_ModeH264_VLD_NoFGT). This is a compile-time-only header
 * dependency: nothing here calls a D3D9 function, so there's no
 * d3d9.lib link dependency.
 *
 * DXVA_PicParams_H264 / DXVA_Qmatrix_H264 / DXVA_Slice_H264_Short /
 * DXVA_PicEntry_H264 come from <dxva.h> — the same structures DXVA2
 * and D3D11 video decode share verbatim (Microsoft documents this
 * explicitly; it's also exactly what FFmpeg's d3d11va_h264.c and
 * dxva2_h264.c both do).
 */

#define WIN32_LEAN_AND_MEAN
#define COBJMACROS

#include <windows.h>

#include <d3d9.h>
#include <d3d11.h>
#include <dxgi1_2.h>
#include <dxva.h>
#include <dxva2api.h>

#endif /* MKFF_WINDOWS_WIN_COMMON_H */
