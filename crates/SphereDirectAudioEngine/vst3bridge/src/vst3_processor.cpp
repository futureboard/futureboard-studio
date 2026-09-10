#include "sphere_daux_vst3_processor.h"

#include <algorithm>
#include <array>
#include <atomic>
#include <chrono>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <memory>
#include <mutex>
#include <sstream>
#include <string>
#include <thread>
#include <vector>

// IPlugView is needed on all platforms for the editor bridge functions.
#include "pluginterfaces/gui/iplugview.h"
#include "pluginterfaces/gui/iplugviewcontentscalesupport.h"

#ifdef _WIN32
#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <dwmapi.h>
#include <dwrite.h>
#include <libloaderapi.h>
#include <objbase.h>
#include <windows.h>
#pragma comment(lib, "dwmapi.lib")
#pragma comment(lib, "dwrite.lib")
#pragma comment(lib, "ole32.lib")
#endif

#include "editor_windows.hpp"
#include "pluginterfaces/base/ipluginbase.h"
#include "pluginterfaces/vst/ivstaudioprocessor.h"
#include "pluginterfaces/vst/ivstcomponent.h"
#include "pluginterfaces/vst/ivsteditcontroller.h"
#include "pluginterfaces/vst/ivstevents.h"
#include "pluginterfaces/vst/ivstmidicontrollers.h"
#include "pluginterfaces/vst/ivstparameterchanges.h"
#include "pluginterfaces/vst/ivstprocesscontext.h"
#include "public.sdk/source/common/memorystream.h"
#include "public.sdk/source/vst/hosting/hostclasses.h"
#include "public.sdk/source/vst/hosting/module.h"
#include "public.sdk/source/vst/utility/uid.h"
#include "sphere_daux_editor_bridge.h"
#include "vst3_processor_internal.hpp"

// Only for `kARAMainFactoryClass`. ARA hosting itself lives in
// `crates/SphereAraHost`, which is a Windows/macOS-only dependency; this file
// just locates the ARA entry points and is built on every platform.
//
// The ARA SDK is an optional submodule (CI initializes it only where ARA is
// actually hosted — see `.github/workflows/submodules-init.sh`), so take the
// header when it is checked out and otherwise define the one thing used from
// it. The value is the category string an ARA-capable VST3 registers its main
// factory under; `ARAVST3.h` guards the same macro with `#if !defined`, so the
// two definitions can never disagree.
#if __has_include("ARAVST3.h")
#include "ARAVST3.h"
#else
#if !defined(kARAMainFactoryClass)
#define kARAMainFactoryClass "ARA Main Factory Class"
#endif
#endif

// IPlugFrame is a GUI-layer interface whose class IID is not emitted by the
// SDK IID TUs we compile (coreiids.cpp / vstinitiids.cpp). Our IPlugFrame
// implementation's queryInterface references IPlugFrame::iid, so define the
// symbol here. The IPlugFrame_iid constant is provided by iplugview.h.
namespace Steinberg {
DEF_CLASS_IID(IPlugFrame)
DEF_CLASS_IID(IPlugViewContentScaleSupport)
} // namespace Steinberg

namespace {

constexpr const char *kVst3AudioModuleClass = "Audio Module Class";
std::atomic<unsigned long long> g_next_editor_handle{1};






#ifdef _WIN32
constexpr const wchar_t *kDauxEditorWindowClass =
    L"FutureboardDauxVst3EditorWindow";
constexpr const wchar_t *kDauxEditorChildClass =
    L"FutureboardDauxVst3EditorAttach";
constexpr const wchar_t *kDauxEditorContentClass =
    L"FutureboardDauxVst3EditorContent";
// Detached top-level editor host (kind==2). Modeled on the VST3 SDK editorhost
// sample (public.sdk/samples/vst-hosting/editorhost): no background brush, host
// never paints its client area, WM_SIZE forwards onSize, WM_CLOSE asks the
// shell to tear down. A normal, independent OS window — no GPUI compositor over
// it.
constexpr const wchar_t *kDauxEditorDetachedClass =
    L"FutureboardDauxVst3EditorDetached";
constexpr COLORREF kDauxTitlebarDark = RGB(14, 19, 25);
constexpr int kDauxFallbackReloadId = 4101;
constexpr int kDauxFallbackGenericId = 4102;
constexpr int kDauxFallbackCloseId = 4103;

// Posted to the HWND's own thread when destroy is requested cross-thread.
constexpr UINT WM_DAUX_DESTROY = WM_APP + 50;

std::wstring widen_utf8(const char *value) {
  if (!value || !*value)
    return L"Plugin Editor";
  const int needed = MultiByteToWideChar(CP_UTF8, 0, value, -1, nullptr, 0);
  if (needed <= 0)
    return L"Plugin Editor";
  std::wstring out(static_cast<size_t>(needed), L'\0');
  MultiByteToWideChar(CP_UTF8, 0, value, -1, out.data(), needed);
  if (!out.empty() && out.back() == L'\0')
    out.pop_back();
  return out;
}

void set_daux_dark_titlebar(HWND hwnd) {
  if (!hwnd)
    return;
  BOOL dark = TRUE;
  HRESULT dark_hr = DwmSetWindowAttribute(
      hwnd, 20, &dark, sizeof(dark)); // DWMWA_USE_IMMERSIVE_DARK_MODE
  if (FAILED(dark_hr)) {
    dark_hr = DwmSetWindowAttribute(
        hwnd, 19, &dark, sizeof(dark)); // older Win10 dark-mode attribute
  }
  std::fprintf(stderr, "[NativeEditorShell] dwm dark_mode=%s hr=0x%08lx\n",
               SUCCEEDED(dark_hr) ? "ok" : "fail",
               static_cast<unsigned long>(dark_hr));

  const bool rounded_disabled = [] {
    const char *v = std::getenv("FUTUREBOARD_EDITOR_ROUNDED");
    return v && (_stricmp(v, "0") == 0 || _stricmp(v, "false") == 0 ||
                 _stricmp(v, "off") == 0);
  }();
  if (rounded_disabled) {
    std::fprintf(stderr, "[NativeEditorShell] rounded=disabled\n");
  } else {
    // DWMWCP_ROUND. Use numeric constants so older SDK headers still compile.
    const int preference = 2;
    const HRESULT rounded_hr =
        DwmSetWindowAttribute(hwnd, 33 /* DWMWA_WINDOW_CORNER_PREFERENCE */,
                              &preference, sizeof(preference));
    std::fprintf(stderr, "[NativeEditorShell] rounded=%s\n",
                 SUCCEEDED(rounded_hr) ? "enabled" : "unsupported");
  }

  COLORREF border = RGB(44, 46, 51);
  const HRESULT border_hr = DwmSetWindowAttribute(
      hwnd, 34 /* DWMWA_BORDER_COLOR */, &border, sizeof(border));
  std::fprintf(stderr, "[NativeEditorShell] dwm border_color=%s\n",
               SUCCEEDED(border_hr) ? "ok" : "fail");

  const HRESULT caption_hr = DwmSetWindowAttribute(
      hwnd, DWMWA_CAPTION_COLOR, &kDauxTitlebarDark, sizeof(kDauxTitlebarDark));
  std::fprintf(stderr, "[NativeEditorShell] dwm caption_color=%s\n",
               SUCCEEDED(caption_hr) ? "ok" : "fail");
}

void paint_dark_child(HWND hwnd) {
  HDC dc = GetDC(hwnd);
  if (!dc)
    return;
  RECT rc{};
  GetClientRect(hwnd, &rc);
  HBRUSH brush = CreateSolidBrush(RGB(11, 15, 20));
  FillRect(dc, &rc, brush);
  DeleteObject(brush);
  ReleaseDC(hwnd, dc);
}

bool daux_plugin_view_message_debug() {
  // FUTUREBOARD_PLUGIN_DEBUG=1 is the single end-to-end plugin debug switch;
  // the narrower flags remain for targeted tracing.
  static const bool enabled =
      std::getenv("FUTUREBOARD_PLUGIN_VIEW_DEBUG") != nullptr ||
      std::getenv("FUTUREBOARD_VST3_EDITOR_DEBUG") != nullptr ||
      std::getenv("FUTUREBOARD_PLUGIN_DEBUG") != nullptr;
  return enabled;
}

void daux_log_window_message(const char *tag, HWND hwnd, UINT msg) {
  if (!daux_plugin_view_message_debug())
    return;
  const char *name = nullptr;
  switch (msg) {
  case WM_CREATE:
    name = "WM_CREATE";
    break;
  case WM_SHOWWINDOW:
    name = "WM_SHOWWINDOW";
    break;
  case WM_SIZE:
    name = "WM_SIZE";
    break;
  case WM_CLOSE:
    name = "WM_CLOSE";
    break;
  case WM_DESTROY:
    name = "WM_DESTROY";
    break;
  case WM_DPICHANGED:
    name = "WM_DPICHANGED";
    break;
  case WM_PAINT:
    name = "WM_PAINT";
    break;
  case WM_ERASEBKGND:
    name = "WM_ERASEBKGND";
    break;
  case WM_TIMER:
    name = "WM_TIMER";
    break;
  default:
    break;
  }
  if (name) {
    std::fprintf(stderr, "[%s] %s hwnd=0x%p tid=%lu\n", tag, name,
                 static_cast<void *>(hwnd), GetCurrentThreadId());
  }
}

void daux_log_hwnd_state(const char *label, HWND top, HWND content) {
  const auto log_one = [label](const char *name, HWND hwnd) {
    if (!hwnd) {
      std::fprintf(stderr, "[plugin-view] hwnd_state label=%s %s=null\n", label,
                   name);
      return;
    }
    RECT client{};
    RECT screen{};
    GetClientRect(hwnd, &client);
    GetWindowRect(hwnd, &screen);
    const LONG_PTR style = GetWindowLongPtrW(hwnd, GWL_STYLE);
    const LONG_PTR ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
    std::fprintf(stderr,
                 "[plugin-view] hwnd_state label=%s %s=0x%p parent=0x%p "
                 "style=0x%Ix ex_style=0x%Ix "
                 "client=(%ld,%ld,%ld,%ld) screen=(%ld,%ld,%ld,%ld) visible=%d "
                 "iconic=%d\n",
                 label, name, static_cast<void *>(hwnd),
                 static_cast<void *>(GetParent(hwnd)),
                 static_cast<std::uintptr_t>(style),
                 static_cast<std::uintptr_t>(ex_style), client.left, client.top,
                 client.right, client.bottom, screen.left, screen.top,
                 screen.right, screen.bottom, IsWindowVisible(hwnd) ? 1 : 0,
                 IsIconic(hwnd) ? 1 : 0);
  };
  std::fprintf(
      stderr,
      "[plugin-view] hwnd_hierarchy label=%s top_hwnd=0x%p content_hwnd=0x%p "
      "content_hwnd_ne_top=%s content_parent=0x%p\n",
      label, static_cast<void *>(top), static_cast<void *>(content),
      (top && content && top != content) ? "true" : "false",
      static_cast<void *>(content ? GetParent(content) : nullptr));
  log_one("top", top);
  log_one("content", content);
}

// One-shot host wake timers installed at attach time (see embed_editor). Only
// these IDs may be killed in the wndprocs below: plugins commonly subclass the
// attach HWND and drive their repaint/meter/modal logic from their own
// WM_TIMER ticks — killing arbitrary timers freezes such editors after the
// first frame.
constexpr UINT_PTR kDauxWakeTimerTop = 0xDA01;
constexpr UINT_PTR kDauxWakeTimerContent = 0xDA02;

LRESULT CALLBACK daux_editor_content_wnd_proc(HWND hwnd, UINT msg,
                                              WPARAM wparam, LPARAM lparam) {
  daux_log_window_message("plugin-content-hwnd", hwnd, msg);
  switch (msg) {
  case WM_TIMER:
    if (wparam == kDauxWakeTimerTop || wparam == kDauxWakeTimerContent) {
      KillTimer(hwnd, wparam);
      return 0;
    }
    break; // plugin-installed timer — let DefWindowProc / subclass chain run
  case WM_ERASEBKGND:
    return 1; // do not repaint over GPU/WebView/OpenGL plug-in output
  case WM_PAINT: {
    PAINTSTRUCT ps{};
    BeginPaint(hwnd, &ps);
    EndPaint(hwnd, &ps);
    return 0;
  }
  case WM_MOUSEACTIVATE:
    // Plugin content clicks must activate without being eaten — the wrapper
    // (cross-process shell) handles the titlebar only.
    if (daux_plugin_view_message_debug()) {
      std::fprintf(
          stderr,
          "[PluginEditorInput] mouse_activate result=MA_ACTIVATE hwnd=0x%p\n",
          static_cast<void *>(hwnd));
    }
    return MA_ACTIVATE;
  case WM_LBUTTONDOWN: {
    // Generic rule: clicking plugin content focuses the plugin child under
    // the point so keyboard input follows the mouse (no vendor logic).
    const POINT pt{static_cast<short>(LOWORD(lparam)),
                   static_cast<short>(HIWORD(lparam))};
    HWND target = ChildWindowFromPointEx(
        hwnd, pt, CWP_SKIPINVISIBLE | CWP_SKIPDISABLED | CWP_SKIPTRANSPARENT);
    if (!target)
      target = hwnd;
    const HWND focus_before = GetFocus();
    SetFocus(target);
    if (daux_plugin_view_message_debug()) {
      std::fprintf(stderr,
                   "[PluginEditorInput] hit_test area=content point=(%ld,%ld) "
                   "focus_before=0x%p focus_after=0x%p\n",
                   pt.x, pt.y, static_cast<void *>(focus_before),
                   static_cast<void *>(GetFocus()));
    }
    break; // fall through to DefWindowProc so the click still routes
  }
  default:
    break;
  }
  return DefWindowProcW(hwnd, msg, wparam, lparam);
}

void register_editor_window_classes() {
  static std::once_flag once;
  std::call_once(once, [] {
    WNDCLASSEXW wc{};
    wc.cbSize = sizeof(WNDCLASSEXW);
    wc.lpfnWndProc = DefWindowProcW;
    wc.hInstance = GetModuleHandleW(nullptr);
    wc.hCursor = LoadCursorW(nullptr, MAKEINTRESOURCEW(32512));
    // Use black brush — prevents the white flash that occurs between the child
    // HWND being created and the IPlugView rendering its first frame.
    wc.hbrBackground = reinterpret_cast<HBRUSH>(GetStockObject(BLACK_BRUSH));
    wc.lpszClassName = kDauxEditorChildClass;
    RegisterClassExW(&wc);

    WNDCLASSEXW content{};
    content.cbSize = sizeof(WNDCLASSEXW);
    content.lpfnWndProc = daux_editor_content_wnd_proc;
    content.hInstance = GetModuleHandleW(nullptr);
    content.hCursor = LoadCursorW(nullptr, MAKEINTRESOURCEW(32512));
    content.hbrBackground = nullptr;
    content.lpszClassName = kDauxEditorContentClass;
    RegisterClassExW(&content);
  });
}
#endif

bool looks_like_zero_class_id(const std::string &value) {
  if (value.empty())
    return true;
  for (char c : value) {
    if (c != '0' && c != '-' && c != '{' && c != '}')
      return false;
  }
  return true;
}

VST3::Optional<VST3::UID>
first_audio_module_uid(const VST3::Hosting::PluginFactory &factory) {
  for (const auto &info : factory.classInfos()) {
    if (info.category() != kVst3AudioModuleClass)
      continue;
    return VST3::Optional<VST3::UID>(info.ID());
  }
  return {};
}

void log_factory_classes(const VST3::Hosting::PluginFactory &factory) {
  int index = 0;
  for (const auto &info : factory.classInfos()) {
    std::fprintf(
        stderr,
        "[DAUx VST3] factory class[%d] name='%s' category='%s' uid=%s\n",
        index++, info.name().c_str(), info.category().c_str(),
        info.ID().toString().c_str());
  }
}

} // namespace




#if defined(_WIN32)
bool daux_vst3_editor_debug() {
  static const bool enabled =
      std::getenv("FUTUREBOARD_VST3_EDITOR_DEBUG") != nullptr;
  return enabled;
}

inline bool daux_view_rect_equal(const Steinberg::ViewRect &a,
                                 const Steinberg::ViewRect &b) {
  return a.left == b.left && a.top == b.top && a.right == b.right &&
         a.bottom == b.bottom;
}

inline int daux_view_rect_width(const Steinberg::ViewRect &r) {
  return static_cast<int>(r.right - r.left);
}

inline int daux_view_rect_height(const Steinberg::ViewRect &r) {
  return static_cast<int>(r.bottom - r.top);
}

inline Steinberg::ViewRect daux_local_view_rect(int width, int height) {
  return Steinberg::ViewRect{
      0,
      0,
      static_cast<Steinberg::int32>(width),
      static_cast<Steinberg::int32>(height),
  };
}

bool daux_embed_apply_content_size(SphereDauxVst3Processor *p, int content_w,
                                   int content_h, const char *reason);

bool daux_adjust_window_rect_for_dpi(HWND hwnd, RECT *rect, DWORD style,
                                     DWORD ex_style) {
  if (!hwnd || !rect)
    return false;
  UINT dpi = GetDpiForWindow(hwnd);
  if (dpi == 0)
    dpi = 96;
  return AdjustWindowRectExForDpi(rect, style, FALSE, ex_style, dpi) != 0;
}

UINT daux_hwnd_dpi(HWND hwnd) {
  if (!hwnd || !IsWindow(hwnd))
    return 96;
  const UINT dpi = GetDpiForWindow(hwnd);
  return dpi > 0 ? dpi : 96;
}

void daux_log_editor_dpi(HWND ref_hwnd, const char *label) {
  const UINT dpi = daux_hwnd_dpi(ref_hwnd);
  const double scale = static_cast<double>(dpi) / 96.0;
  std::fprintf(stderr, "[PluginEditor] %s dpi=%u\n", label ? label : "dpi",
               dpi);
  std::fprintf(stderr, "[PluginEditor] %s scale=%.3f\n",
               label ? label : "scale", scale);
}

void daux_ensure_thread_dpi_awareness() {
  static std::once_flag once;
  std::call_once(once, [] {
    using SetThreadDpiAwarenessContextFn =
        DPI_AWARENESS_CONTEXT(WINAPI *)(DPI_AWARENESS_CONTEXT);
    using GetThreadDpiAwarenessContextFn = DPI_AWARENESS_CONTEXT(WINAPI *)();
    HMODULE user32 = GetModuleHandleW(L"user32.dll");
    auto *set_ctx =
        user32 ? reinterpret_cast<SetThreadDpiAwarenessContextFn>(
                     GetProcAddress(user32, "SetThreadDpiAwarenessContext"))
               : nullptr;
    auto *get_ctx =
        user32 ? reinterpret_cast<GetThreadDpiAwarenessContextFn>(
                     GetProcAddress(user32, "GetThreadDpiAwarenessContext"))
               : nullptr;
    if (set_ctx) {
      set_ctx(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    } else {
      SetProcessDPIAware();
    }
    if (get_ctx) {
      std::fprintf(stderr,
                   "[PluginEditor] dpi_awareness_context=0x%p tid=%lu\n",
                   static_cast<void *>(get_ctx()), GetCurrentThreadId());
    } else {
      std::fprintf(stderr,
                   "[PluginEditor] dpi_awareness_context=legacy tid=%lu\n",
                   GetCurrentThreadId());
    }
  });
}

bool daux_verify_child_client_rect(HWND child, int expected_w, int expected_h,
                                   const char *phase) {
  if (!child || !IsWindow(child)) {
    std::fprintf(stderr,
                 "[PluginEditor] ERROR %s child_hwnd invalid hwnd=0x%p\n",
                 phase ? phase : "verify", static_cast<void *>(child));
    return false;
  }
  RECT cr{};
  GetClientRect(child, &cr);
  const int cw = static_cast<int>(cr.right - cr.left);
  const int ch = static_cast<int>(cr.bottom - cr.top);
  if (cw <= 0 || ch <= 0 || cw != expected_w || ch != expected_h) {
    std::fprintf(stderr,
                 "[PluginEditor] ERROR %s child client=%dx%d expected=%dx%d\n",
                 phase ? phase : "verify", cw, ch, expected_w, expected_h);
    return false;
  }
  std::fprintf(stderr, "[PluginEditor] %s final child client=%dx%d\n",
               phase ? phase : "verify", cw, ch);
  return true;
}

bool daux_resize_child_client(HWND child, int content_w, int content_h) {
  if (!child || !IsWindow(child) || content_w <= 0 || content_h <= 0)
    return false;
  SetWindowPos(child, nullptr, 0, 0, content_w, content_h,
               SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW);
  return daux_verify_child_client_rect(child, content_w, content_h,
                                       "resize_child");
}

struct DauxPluginRootSizeProbe {
  HWND embed = nullptr;
  int target_width = 0;
  int target_height = 0;
  int matched_width = 0;
  int matched_height = 0;
  long long best_error = 0x7fffffffffffffffLL;
};

BOOL CALLBACK daux_probe_scaled_plugin_root(HWND child, LPARAM data) {
  auto *probe = reinterpret_cast<DauxPluginRootSizeProbe *>(data);
  if (!probe || !IsWindowVisible(child))
    return TRUE;
  RECT rect{};
  if (!GetClientRect(child, &rect))
    return TRUE;
  const int width = static_cast<int>(rect.right - rect.left);
  const int height = static_cast<int>(rect.bottom - rect.top);
  if (width < 16 || height < 16 || width > 8192 || height > 8192)
    return TRUE;
  POINT origin{0, 0};
  MapWindowPoints(child, probe->embed, &origin, 1);
  if (std::abs(origin.x) > 2 || std::abs(origin.y) > 2)
    return TRUE;
  const int tolerance_w = std::max(8, probe->target_width / 20);
  const int tolerance_h = std::max(8, probe->target_height / 20);
  const int error_w = std::abs(width - probe->target_width);
  const int error_h = std::abs(height - probe->target_height);
  if (error_w > tolerance_w || error_h > tolerance_h)
    return TRUE;
  const long long error = static_cast<long long>(error_w) + error_h;
  if (error < probe->best_error) {
    probe->best_error = error;
    probe->matched_width = width;
    probe->matched_height = height;
  }
  return TRUE;
}

bool daux_plugin_root_window_size(HWND embed, int expected_width,
                                  int expected_height, int *out_width,
                                  int *out_height) {
  if (!embed || !IsWindow(embed))
    return false;

  const UINT dpi = daux_hwnd_dpi(embed);
  if (dpi > 96) {
    DauxPluginRootSizeProbe probe{};
    probe.embed = embed;
    probe.target_width = MulDiv(expected_width, 96, static_cast<int>(dpi));
    probe.target_height = MulDiv(expected_height, 96, static_cast<int>(dpi));
    if (probe.target_width >= 16 && probe.target_height >= 16) {
      EnumChildWindows(embed, daux_probe_scaled_plugin_root,
                       reinterpret_cast<LPARAM>(&probe));
      if (probe.matched_width > 0 && probe.matched_height > 0) {
        if (out_width)
          *out_width = probe.matched_width;
        if (out_height)
          *out_height = probe.matched_height;
        return true;
      }
    }
  }

  long long best_area = 0;
  int best_width = 0;
  int best_height = 0;
  for (HWND child = GetWindow(embed, GW_CHILD); child;
       child = GetWindow(child, GW_HWNDNEXT)) {
    if (!IsWindowVisible(child))
      continue;
    RECT rect{};
    if (!GetClientRect(child, &rect))
      continue;
    const int width = static_cast<int>(rect.right - rect.left);
    const int height = static_cast<int>(rect.bottom - rect.top);
    if (width < 16 || height < 16 || width > 8192 || height > 8192)
      continue;
    const long long area = static_cast<long long>(width) * height;
    if (area > best_area) {
      best_area = area;
      best_width = width;
      best_height = height;
    }
  }
  if (best_area <= 0)
    return false;
  if (out_width)
    *out_width = best_width;
  if (out_height)
    *out_height = best_height;
  return true;
}

bool daux_editor_set_content_scale(SphereDauxVst3Processor *processor,
                                   HWND dpi_ref, const char *reason) {
  if (!processor || !processor->editor_view)
    return false;
  const UINT dpi = daux_hwnd_dpi(dpi_ref);
  const float scale = static_cast<float>(dpi) / 96.0f;
  Steinberg::IPlugViewContentScaleSupport *scale_support = nullptr;
  const auto query_result = processor->editor_view->queryInterface(
      Steinberg::IPlugViewContentScaleSupport::iid,
      reinterpret_cast<void **>(&scale_support));
  if ((query_result != Steinberg::kResultTrue &&
       query_result != Steinberg::kResultOk) ||
      !scale_support) {
    if (daux_vst3_editor_debug()) {
      std::fprintf(stderr,
                   "[vst3-editor] content_scale unsupported dpi=%u scale=%.3f "
                   "reason=%s\n",
                   dpi, scale, reason ? reason : "unknown");
    }
    return false;
  }
  const auto scale_result = scale_support->setContentScaleFactor(scale);
  scale_support->release();
  if (daux_vst3_editor_debug()) {
    std::fprintf(
        stderr,
        "[vst3-editor] content_scale result=0x%x dpi=%u scale=%.3f reason=%s\n",
        static_cast<unsigned>(scale_result), dpi, scale,
        reason ? reason : "unknown");
  }
  return scale_result == Steinberg::kResultTrue ||
         scale_result == Steinberg::kResultOk;
}

// IPlugFrame for VST3 editor hosting. Mirrors the SDK editorhost sample:
// plugView->setFrame(this) BEFORE plugView->attached(...). WebView2/CEF editors
// (UAD Native et al.) require a valid frame to bootstrap and call resizeView().
class PluginEditorFrame final : public Steinberg::IPlugFrame {
public:
  explicit PluginEditorFrame(SphereDauxVst3Processor *owner) : owner_(owner) {}

  Steinberg::tresult PLUGIN_API resizeView(
      Steinberg::IPlugView *view, Steinberg::ViewRect *newSize) override {
    const bool debug = daux_vst3_editor_debug();
    if (debug) {
      std::fprintf(stderr, "[vst3-editor] resizeView called view=0x%p\n",
                   static_cast<void *>(view));
    }
    if (newSize == nullptr || view == nullptr || !owner_) {
      if (debug)
        std::fprintf(stderr,
                     "[vst3-editor] resizeView rejected (invalid args)\n");
      return Steinberg::kInvalidArgument;
    }
    if (view != owner_->editor_view.get()) {
      if (debug)
        std::fprintf(stderr,
                     "[vst3-editor] resizeView rejected (view mismatch)\n");
      return Steinberg::kInvalidArgument;
    }
    if (resize_recursion_guard_) {
      if (debug)
        std::fprintf(stderr,
                     "[vst3-editor] resizeView rejected (recursion guard)\n");
      return Steinberg::kResultFalse;
    }

    Steinberg::ViewRect current{};
    if (view->getSize(&current) != Steinberg::kResultTrue) {
      if (debug)
        std::fprintf(stderr,
                     "[vst3-editor] resizeView rejected (getSize failed)\n");
      return Steinberg::kInternalError;
    }
    if (daux_view_rect_equal(current, *newSize)) {
      const int w = daux_view_rect_width(*newSize);
      const int h = daux_view_rect_height(*newSize);
      const bool applied =
          (w > 0 && h > 0)
              ? daux_embed_apply_content_size(owner_, w, h, "resizeView.no-op")
              : false;
      if (debug) {
        std::fprintf(
            stderr,
            "[vst3-frame] resizeView requested=(%d,%d,%d,%d) content=%dx%d\n",
            newSize->left, newSize->top, newSize->right, newSize->bottom, w, h);
        std::fprintf(stderr,
                     "[vst3-frame] resizeView applied=%dx%d changed=%d\n", w, h,
                     applied ? 1 : 0);
        std::fprintf(stderr, "[vst3-editor] resizeView accepted (no-op)\n");
      }
      return Steinberg::kResultTrue;
    }

    const int w = daux_view_rect_width(*newSize);
    const int h = daux_view_rect_height(*newSize);
    if (debug) {
      std::fprintf(
          stderr,
          "[vst3-frame] resizeView requested=(%d,%d,%d,%d) content=%dx%d\n",
          newSize->left, newSize->top, newSize->right, newSize->bottom, w, h);
    }

    resize_recursion_guard_ = true;
    bool applied = false;
    if (w > 0 && h > 0) {
      applied = daux_embed_apply_content_size(owner_, w, h, "resizeView");
    }
    resize_recursion_guard_ = false;

    Steinberg::ViewRect after{};
    if (view->getSize(&after) != Steinberg::kResultTrue) {
      if (debug)
        std::fprintf(stderr, "[vst3-editor] resizeView rejected (getSize after "
                             "resize failed)\n");
      return Steinberg::kInternalError;
    }
    if (!daux_view_rect_equal(after, *newSize)) {
      auto local = daux_local_view_rect(w, h);
      const auto on_size_res = view->onSize(&local);
      if (debug) {
        std::fprintf(stderr,
                     "[vst3-editor] onSize result=0x%x rect=(%d,%d,%d,%d)\n",
                     static_cast<unsigned>(on_size_res), local.left, local.top,
                     local.right, local.bottom);
      }
    }
    if (debug) {
      std::fprintf(stderr, "[vst3-frame] resizeView applied=%dx%d changed=%d\n",
                   w, h, applied ? 1 : 0);
      std::fprintf(stderr, "[vst3-editor] resizeView accepted\n");
    }
    return Steinberg::kResultOk;
  }

  Steinberg::tresult PLUGIN_API queryInterface(const Steinberg::TUID iid,
                                               void **obj) override {
    if (Steinberg::FUnknownPrivate::iidEqual(iid, Steinberg::IPlugFrame::iid) ||
        Steinberg::FUnknownPrivate::iidEqual(iid, Steinberg::FUnknown::iid)) {
      *obj = static_cast<Steinberg::IPlugFrame *>(this);
      addRef();
      return Steinberg::kResultTrue;
    }
    *obj = nullptr;
    return Steinberg::kNoInterface;
  }
  // Lifetime owned by the processor — a plug-in release() must not destroy us.
  Steinberg::uint32 PLUGIN_API addRef() override { return 1000; }
  Steinberg::uint32 PLUGIN_API release() override { return 1000; }

private:
  SphereDauxVst3Processor *owner_;
  bool resize_recursion_guard_{false};
};

// Create (if needed) and install the IPlugFrame on the view before attached().
void daux_editor_install_frame(SphereDauxVst3Processor *processor) {
  if (!processor || !processor->editor_view)
    return;
  if (!processor->editor_frame) {
    processor->editor_frame = new PluginEditorFrame(processor);
  }
  if (daux_vst3_editor_debug()) {
    std::fprintf(stderr, "[vst3-editor] setFrame called view=0x%p frame=0x%p\n",
                 static_cast<void *>(processor->editor_view.get()),
                 static_cast<void *>(processor->editor_frame));
  }
  const auto res = processor->editor_view->setFrame(processor->editor_frame);
  std::fprintf(stderr, "[vst3-editor] setFrame result=0x%x\n",
               static_cast<unsigned>(res));
}

void daux_editor_clear_frame(SphereDauxVst3Processor *processor) {
  if (!processor)
    return;
  if (processor->editor_view) {
    if (daux_vst3_editor_debug()) {
      std::fprintf(stderr, "[vst3-editor] setFrame null view=0x%p\n",
                   static_cast<void *>(processor->editor_view.get()));
    }
    processor->editor_view->setFrame(nullptr);
  }
  delete processor->editor_frame;
  processor->editor_frame = nullptr;
}
#endif // _WIN32

#if defined(__APPLE__)
// AppKit counterpart of the Windows PluginEditorFrame. Some VST3 editors only
// settle on their real size from attached(), then report it through
// IPlugFrame::resizeView. Without a frame macOS kept the original fallback
// window while the plug-in painted a smaller NSView into one corner.
class MacPluginEditorFrame final : public Steinberg::IPlugFrame {
public:
  explicit MacPluginEditorFrame(SphereDauxVst3Processor *owner)
      : owner_(owner) {}

  Steinberg::tresult PLUGIN_API resizeView(
      Steinberg::IPlugView *view, Steinberg::ViewRect *new_size) override {
    if (!owner_ || !view || !new_size ||
        view != owner_->editor_view.get())
      return Steinberg::kInvalidArgument;
    if (resize_in_progress_)
      return Steinberg::kResultFalse;

    int width = static_cast<int>(new_size->right - new_size->left);
    int height = static_cast<int>(new_size->bottom - new_size->top);
    if (width <= 0 || height <= 0)
      return Steinberg::kInvalidArgument;

    resize_in_progress_ = true;
    const bool applied =
        sphere_daux_editor_apply_plugin_resize(owner_, width, height) != 0;
    if (applied) {
      Steinberg::ViewRect current{};
      const auto size_result = view->getSize(&current);
      if ((size_result != Steinberg::kResultTrue &&
           size_result != Steinberg::kResultOk) ||
          current.left != new_size->left || current.top != new_size->top ||
          current.right != new_size->right ||
          current.bottom != new_size->bottom) {
        Steinberg::ViewRect local{0, 0,
                                  static_cast<Steinberg::int32>(width),
                                  static_cast<Steinberg::int32>(height)};
        view->onSize(&local);
      }
    }
    resize_in_progress_ = false;
    std::fprintf(stderr,
                 "[SphereVST3/mac] resizeView requested=%dx%d applied=%d\n",
                 width, height, applied ? 1 : 0);
    return applied ? Steinberg::kResultTrue : Steinberg::kResultFalse;
  }

  Steinberg::tresult PLUGIN_API queryInterface(const Steinberg::TUID iid,
                                               void **obj) override {
    if (!obj)
      return Steinberg::kInvalidArgument;
    if (Steinberg::FUnknownPrivate::iidEqual(iid, Steinberg::IPlugFrame::iid) ||
        Steinberg::FUnknownPrivate::iidEqual(iid, Steinberg::FUnknown::iid)) {
      *obj = static_cast<Steinberg::IPlugFrame *>(this);
      addRef();
      return Steinberg::kResultTrue;
    }
    *obj = nullptr;
    return Steinberg::kNoInterface;
  }

  Steinberg::uint32 PLUGIN_API addRef() override { return 1000; }
  Steinberg::uint32 PLUGIN_API release() override { return 1000; }

private:
  SphereDauxVst3Processor *owner_;
  bool resize_in_progress_{false};
};
#endif

// ── Bridge implementations for platform editor TUs
// ──────────────────────────── These give editor_mac.mm and editor_linux.cpp
// access to the TU-private globals (g_last_error, g_next_editor_handle) and the
// IPlugView members of SphereDauxVst3Processor without exposing the full struct
// definition.

extern "C" void sphere_daux_editor_set_error(const char *msg) {
  set_last_error(msg ? msg : "");
}

extern "C" unsigned long long sphere_daux_editor_next_handle(void) {
  return g_next_editor_handle.fetch_add(1);
}

#if defined(__APPLE__) || defined(__linux__)

extern "C" int sphere_daux_editor_create_view(SphereDauxVst3Processor *proc,
                                              const char *platform_type,
                                              int *out_width, int *out_height) {
  if (!proc || !proc->controller)
    return 0;
#if defined(__APPLE__)
  if (proc->editor_view && proc->editor_frame)
    proc->editor_view->setFrame(nullptr);
  delete proc->editor_frame;
  proc->editor_frame = nullptr;
#endif
  proc->editor_view = Steinberg::IPtr<Steinberg::IPlugView>::adopt(
      proc->controller->createView(Steinberg::Vst::ViewType::kEditor));
  if (!proc->editor_view) {
    std::fprintf(stderr, "[SphereVST3] createView FAILED for platform='%s'\n",
                 platform_type);
    return 0;
  }
  if (proc->editor_view->isPlatformTypeSupported(platform_type) !=
      Steinberg::kResultTrue) {
    std::fprintf(stderr,
                 "[SphereVST3] platform type '%s' not supported by plugin\n",
                 platform_type);
    proc->editor_view = nullptr;
    return 0;
  }
#if defined(__APPLE__)
  proc->editor_frame = new MacPluginEditorFrame(proc);
  const auto frame_result = proc->editor_view->setFrame(proc->editor_frame);
  std::fprintf(stderr, "[SphereVST3/mac] IPlugView::setFrame result=%d\n",
               (int)frame_result);
#endif
  Steinberg::ViewRect rect{};
  if (proc->editor_view->getSize(&rect) == Steinberg::kResultTrue) {
    const int w = rect.right - rect.left;
    const int h = rect.bottom - rect.top;
    if (w > 0 && out_width)
      *out_width = w;
    if (h > 0 && out_height)
      *out_height = h;
  }
  return 1;
}

extern "C" int sphere_daux_editor_attach_view(SphereDauxVst3Processor *proc,
                                              void *native_handle,
                                              const char *platform_type) {
  if (!proc || !proc->editor_view)
    return 0;
  const auto res = proc->editor_view->attached(native_handle, platform_type);
  std::fprintf(stderr, "[SphereVST3] IPlugView::attached('%s') result=%d\n",
               platform_type, (int)res);
  if (res != Steinberg::kResultTrue && res != Steinberg::kResultOk) {
#if defined(__APPLE__)
    proc->editor_view->setFrame(nullptr);
    delete proc->editor_frame;
    proc->editor_frame = nullptr;
#endif
    proc->editor_view = nullptr;
    proc->editor_attached = false;
    return 0;
  }
  proc->editor_attached = true;
  return 1;
}

extern "C" void
sphere_daux_editor_signal_user_close(SphereDauxVst3Processor *proc) {
  if (proc)
    proc->editor_user_closed.store(true, std::memory_order_release);
}

extern "C" int sphere_daux_editor_set_frame(SphereDauxVst3Processor *proc,
                                            void *frame) {
  if (!proc || !proc->editor_view)
    return 0;
  auto *plug_frame = static_cast<Steinberg::IPlugFrame *>(frame);
  const auto res = proc->editor_view->setFrame(plug_frame);
  std::fprintf(stderr, "[SphereVST3] IPlugView::setFrame(%p) result=%d\n", frame,
               (int)res);
  return (res == Steinberg::kResultTrue || res == Steinberg::kResultOk) ? 1 : 0;
}

extern "C" void sphere_daux_editor_notify_resize(SphereDauxVst3Processor *proc,
                                                 int width, int height) {
  if (!proc || !proc->editor_view)
    return;
  Steinberg::ViewRect rect{0, 0, static_cast<Steinberg::int32>(width),
                           static_cast<Steinberg::int32>(height)};
  proc->editor_view->onSize(&rect);
}

extern "C" int
sphere_daux_editor_get_view_size(SphereDauxVst3Processor *proc, int *out_width,
                                 int *out_height) {
  if (!proc || !proc->editor_view)
    return 0;
  Steinberg::ViewRect rect{};
  const auto result = proc->editor_view->getSize(&rect);
  const int width = static_cast<int>(rect.right - rect.left);
  const int height = static_cast<int>(rect.bottom - rect.top);
  if ((result != Steinberg::kResultTrue && result != Steinberg::kResultOk) ||
      width <= 0 || height <= 0)
    return 0;
  if (out_width)
    *out_width = width;
  if (out_height)
    *out_height = height;
  return 1;
}

extern "C" int
sphere_daux_editor_can_resize(SphereDauxVst3Processor *proc) {
  if (!proc || !proc->editor_view)
    return 0;
  const auto result = proc->editor_view->canResize();
  return (result == Steinberg::kResultTrue || result == Steinberg::kResultOk)
             ? 1
             : 0;
}

extern "C" int sphere_daux_editor_constrain_view_size(
    SphereDauxVst3Processor *proc, int *io_width, int *io_height) {
  if (!proc || !proc->editor_view || !io_width || !io_height ||
      *io_width <= 0 || *io_height <= 0)
    return 0;

  Steinberg::ViewRect rect{0, 0, static_cast<Steinberg::int32>(*io_width),
                           static_cast<Steinberg::int32>(*io_height)};
  if (sphere_daux_editor_can_resize(proc)) {
    const auto result = proc->editor_view->checkSizeConstraint(&rect);
    if (result != Steinberg::kResultTrue && result != Steinberg::kResultOk) {
      // A resizable declaration with a rejected constraint must not leave the
      // AppKit shell at an unapproved size. Snap back to the current getSize.
      return sphere_daux_editor_get_view_size(proc, io_width, io_height);
    }
  } else {
    if (!sphere_daux_editor_get_view_size(proc, io_width, io_height))
      return 0;
    return 1;
  }
  const int width = static_cast<int>(rect.right - rect.left);
  const int height = static_cast<int>(rect.bottom - rect.top);
  if (width <= 0 || height <= 0)
    return 0;
  *io_width = width;
  *io_height = height;
  return 1;
}

extern "C" void sphere_daux_editor_set_content_size(
    SphereDauxVst3Processor *proc, int width, int height) {
  if (!proc || width <= 0 || height <= 0)
    return;
  proc->editor_content_width = width;
  proc->editor_content_height = height;
}

extern "C" void sphere_daux_editor_detach_view(SphereDauxVst3Processor *proc) {
  if (!proc)
    return;
  if (proc->editor_view && proc->editor_attached) {
    const auto res = proc->editor_view->removed();
    std::fprintf(stderr,
                 "[SphereVST3] IPlugView::removed() result=%d handle=%llu\n",
                 (int)res, proc->editor_handle);
  }
#if defined(__APPLE__)
  if (proc->editor_view)
    proc->editor_view->setFrame(nullptr);
  delete proc->editor_frame;
  proc->editor_frame = nullptr;
#endif
  proc->editor_view = nullptr;
  proc->editor_attached = false;
}

extern "C" void sphere_daux_editor_store_native(
    SphereDauxVst3Processor *proc, void *native_window, void *native_embed,
    void *native_delegate, unsigned long long handle, const char *window_id,
    const char *title, int requested_width, int requested_height) {
  if (!proc)
    return;
  proc->editor_native_window = native_window;
  proc->editor_native_embed = native_embed;
  proc->editor_native_delegate = native_delegate;
  proc->editor_handle = handle;
  proc->editor_window_id = window_id ? window_id : "";
  proc->editor_title = title ? title : "";
  proc->editor_requested_width = requested_width;
  proc->editor_requested_height = requested_height;
  proc->editor_content_width = requested_width;
  proc->editor_content_height = requested_height;
}

extern "C" void sphere_daux_editor_clear_native(SphereDauxVst3Processor *proc) {
  if (!proc)
    return;
  proc->editor_native_window = nullptr;
  proc->editor_native_embed = nullptr;
  proc->editor_native_delegate = nullptr;
  proc->editor_handle = 0;
  proc->editor_window_id.clear();
  proc->editor_title.clear();
  proc->editor_requested_width = 0;
  proc->editor_requested_height = 0;
  proc->editor_content_width = 0;
  proc->editor_content_height = 0;
}

extern "C" void *
sphere_daux_editor_get_native_window(SphereDauxVst3Processor *p) {
  return p ? p->editor_native_window : nullptr;
}
extern "C" void *
sphere_daux_editor_get_native_embed(SphereDauxVst3Processor *p) {
  return p ? p->editor_native_embed : nullptr;
}
extern "C" void *
sphere_daux_editor_get_native_delegate(SphereDauxVst3Processor *p) {
  return p ? p->editor_native_delegate : nullptr;
}
extern "C" unsigned long long
sphere_daux_editor_get_handle(SphereDauxVst3Processor *p) {
  return p ? p->editor_handle : 0;
}
extern "C" const char *
sphere_daux_editor_get_window_id(SphereDauxVst3Processor *p) {
  return p ? p->editor_window_id.c_str() : "";
}
extern "C" const char *
sphere_daux_editor_get_title(SphereDauxVst3Processor *p) {
  return p ? p->editor_title.c_str() : "";
}
extern "C" int
sphere_daux_editor_get_requested_width(SphereDauxVst3Processor *p) {
  return p ? p->editor_requested_width : 0;
}
extern "C" int
sphere_daux_editor_get_requested_height(SphereDauxVst3Processor *p) {
  return p ? p->editor_requested_height : 0;
}

#endif // __APPLE__ || __linux__

// ── ComponentHandlerImpl::performEdit (needs full SphereDauxVst3Processor) ───

Steinberg::tresult PLUGIN_API ComponentHandlerImpl::performEdit(
    Steinberg::Vst::ParamID id, Steinberg::Vst::ParamValue value) {
  if (owner)
    owner->enqueue_param(id, value);
  static std::atomic<int> logged{0};
  const int n = logged.fetch_add(1);
  if (n < 16 || n % 50 == 0) {
    std::fprintf(
        stderr,
        "[SphereVST3] editor param -> processor param=%u value=%.6f count=%d\n",
        static_cast<unsigned int>(id), static_cast<double>(value), n + 1);
  }
  return Steinberg::kResultOk;
}

#ifdef _WIN32
void detach_editor_view(SphereDauxVst3Processor *processor) {
  if (!processor)
    return;
  if (processor->editor_view && processor->editor_attached) {
    // Mirror editorhost teardown: clear frame, then removed().
    if (daux_vst3_editor_debug()) {
      std::fprintf(stderr, "[vst3-editor] setFrame null view=0x%p\n",
                   static_cast<void *>(processor->editor_view.get()));
    }
    processor->editor_view->setFrame(nullptr);
    const auto removed_res = processor->editor_view->removed();
    std::fprintf(stderr,
                 "[SphereVST3] IPlugView::removed() result=0x%x handle=%llu\n",
                 static_cast<unsigned>(removed_res), processor->editor_handle);
    if (daux_vst3_editor_debug()) {
      std::fprintf(stderr, "[vst3-editor] removed result=0x%x\n",
                   static_cast<unsigned>(removed_res));
    }
  }
  delete processor->editor_frame;
  processor->editor_frame = nullptr;
  processor->editor_view = nullptr;
  processor->editor_attached = false;
}

void destroy_fallback_controls(SphereDauxVst3Processor *processor) {
  if (!processor)
    return;
  HWND controls[] = {
      processor->editor_fallback_label_hwnd,
      processor->editor_fallback_reload_hwnd,
      processor->editor_fallback_generic_hwnd,
      processor->editor_fallback_close_hwnd,
  };
  for (HWND control : controls) {
    if (control && IsWindow(control))
      DestroyWindow(control);
  }
  processor->editor_fallback_label_hwnd = nullptr;
  processor->editor_fallback_reload_hwnd = nullptr;
  processor->editor_fallback_generic_hwnd = nullptr;
  processor->editor_fallback_close_hwnd = nullptr;
}

// ── Generic VST3 editor resize contract (no vendor/plugin logic) ─────────────

// Resize-path log throttle: at most 4 [PluginEditorResize] bursts per second so
// interactive drags never flood stderr.
bool daux_resize_log_allow() {
  static std::atomic<unsigned long long> window_start{0};
  static std::atomic<unsigned> count{0};
  const unsigned long long now = GetTickCount64();
  const unsigned long long start = window_start.load(std::memory_order_relaxed);
  if (now - start >= 1000) {
    window_start.store(now, std::memory_order_relaxed);
    count.store(1, std::memory_order_relaxed);
    return true;
  }
  return count.fetch_add(1, std::memory_order_relaxed) < 4;
}

// IPlugView::canResize, queried once per created view and cached (spec item 1).
// kResultTrue means the editor supports host-driven resizing; everything else
// is treated as fixed-size.
bool daux_editor_view_resizable(SphereDauxVst3Processor *p) {
  if (!p || !p->editor_view)
    return false;
  if (p->editor_resizable_view != p->editor_view.get()) {
    const auto res = p->editor_view->canResize();
    p->editor_resizable = (res == Steinberg::kResultTrue);
    p->editor_resizable_view = p->editor_view.get();
    std::fprintf(stderr,
                 "[PluginEditorResize] canResize result=0x%x resizable=%d\n",
                 static_cast<unsigned>(res), p->editor_resizable ? 1 : 0);
  }
  return p->editor_resizable;
}

// Host/user-driven resize contract (editorhost `constrainSize`, spec item 2):
// fixed-size views snap to their current `getSize`; resizable views go through
// `IPlugView::checkSizeConstraint` and use the plugin-adjusted rect. `w`/`h`
// are PLUGIN CONTENT dimensions (never include titlebar/non-client frame).
// Returns true when the constraint changed the requested size.
bool daux_constrain_content_size(SphereDauxVst3Processor *p, int *w, int *h) {
  if (!p || !p->editor_view || !w || !h || *w <= 0 || *h <= 0)
    return false;
  const int want_w = *w;
  const int want_h = *h;
  if (!daux_editor_view_resizable(p)) {
    Steinberg::ViewRect cur{};
    const auto gs = p->editor_view->getSize(&cur);
    if (gs == Steinberg::kResultTrue || gs == Steinberg::kResultOk) {
      const int cw = daux_view_rect_width(cur);
      const int ch = daux_view_rect_height(cur);
      if (cw > 0 && ch > 0) {
        *w = cw;
        *h = ch;
      }
    }
  } else {
    Steinberg::ViewRect want{0, 0, want_w, want_h};
    const auto res = p->editor_view->checkSizeConstraint(&want);
    if (res == Steinberg::kResultTrue || res == Steinberg::kResultOk) {
      const int cw = daux_view_rect_width(want);
      const int ch = daux_view_rect_height(want);
      if (cw > 0 && ch > 0) {
        *w = cw;
        *h = ch;
      }
    }
    if (daux_resize_log_allow()) {
      std::fprintf(stderr,
                   "[PluginEditorResize] checkSizeConstraint result=0x%x\n",
                   static_cast<unsigned>(res));
    }
  }
  const bool changed = (*w != want_w || *h != want_h);
  if (daux_resize_log_allow()) {
    std::fprintf(stderr,
                 "[PluginEditorResize] desired_plugin=%dx%d "
                 "constrained_plugin=%dx%d resizable=%d\n",
                 want_w, want_h, *w, *h, p->editor_resizable ? 1 : 0);
  }
  return changed;
}

void resize_editor_view(SphereDauxVst3Processor *processor) {
  if (!processor || !processor->editor_view || !processor->editor_attach_hwnd)
    return;
  RECT rc{};
  GetClientRect(processor->editor_attach_hwnd, &rc);
  const int content_w = static_cast<int>(rc.right - rc.left);
  const int content_h = static_cast<int>(rc.bottom - rc.top);
  if (content_w <= 0 || content_h <= 0) {
    std::fprintf(
        stderr,
        "[PluginEditor] ERROR onSize skipped zero client=%dx%d handle=%llu\n",
        content_w, content_h, processor->editor_handle);
    return;
  }
  auto local = daux_local_view_rect(content_w, content_h);
  auto resize_res = processor->editor_view->onSize(&local);
  if (!daux_verify_child_client_rect(processor->editor_attach_hwnd, content_w,
                                     content_h, "onSize")) {
    daux_resize_child_client(processor->editor_attach_hwnd, content_w,
                             content_h);
    resize_res = processor->editor_view->onSize(&local);
    std::fprintf(stderr,
                 "[PluginEditor] onSize retry result=0x%x rect=%d,%d,%d,%d\n",
                 static_cast<unsigned>(resize_res), local.left, local.top,
                 local.right, local.bottom);
  }
  // Repaint the freshly sized child — never leave stale pixels at the edges
  // (spec item 3). FALSE: no background erase, the plugin owns its pixels.
  InvalidateRect(processor->editor_attach_hwnd, nullptr, FALSE);
  static std::atomic<unsigned int> resize_log_count{0};
  const unsigned int count = resize_log_count.fetch_add(1);
  if (count < 12 || count % 50 == 0) {
    std::fprintf(stderr,
                 "[SphereVST3] IPlugView::onSize() result=%d handle=%llu "
                 "rect=%d,%d,%d,%d count=%u\n",
                 (int)resize_res, processor->editor_handle, (int)local.left,
                 (int)local.top, (int)local.right, (int)local.bottom,
                 count + 1);
  }
  if (daux_resize_log_allow()) {
    RECT after{};
    GetClientRect(processor->editor_attach_hwnd, &after);
    std::fprintf(
        stderr,
        "[PluginEditorResize] child_rect=(0,0,%ld,%ld) onSize result=0x%x\n",
        after.right - after.left, after.bottom - after.top,
        static_cast<unsigned>(resize_res));
  }
}

bool daux_editor_window_live(void *user_data) {
  auto *p = static_cast<SphereDauxVst3Processor *>(user_data);
  return p && p->processor_valid.load(std::memory_order_acquire);
}

bool daux_editor_window_attached(void *user_data) {
  auto *p = static_cast<SphereDauxVst3Processor *>(user_data);
  return p && p->editor_attached;
}

bool daux_editor_window_resize_in_progress(void *user_data) {
  auto *p = static_cast<SphereDauxVst3Processor *>(user_data);
  return p && p->embed_resize_in_progress;
}

void daux_editor_window_set_resize_in_progress(void *user_data, bool value) {
  auto *p = static_cast<SphereDauxVst3Processor *>(user_data);
  if (p)
    p->embed_resize_in_progress = value;
}

bool daux_editor_window_can_resize(void *user_data) {
  return daux_editor_view_resizable(
      static_cast<SphereDauxVst3Processor *>(user_data));
}

bool daux_editor_window_constrain_size(void *user_data, int *width,
                                       int *height) {
  return daux_constrain_content_size(
      static_cast<SphereDauxVst3Processor *>(user_data), width, height);
}

void daux_editor_window_content_resized(void *user_data, void *content_hwnd,
                                        int width, int height) {
  auto *p = static_cast<SphereDauxVst3Processor *>(user_data);
  if (!p)
    return;
  p->editor_attach_hwnd = reinterpret_cast<HWND>(content_hwnd);
  p->embed_content_w = width;
  p->embed_content_h = height;
  resize_editor_view(p);
}

void daux_editor_window_dpi_changed(void *user_data, void *shell_hwnd,
                                    void *content_hwnd, int width, int height) {
  auto *p = static_cast<SphereDauxVst3Processor *>(user_data);
  HWND shell = reinterpret_cast<HWND>(shell_hwnd);
  HWND content = reinterpret_cast<HWND>(content_hwnd);
  if (!p || !p->editor_view || !content || !IsWindow(content))
    return;
  daux_editor_set_content_scale(p, shell, "WM_DPICHANGED");
  // The scale factor just changed the view's own idea of its size — a
  // fixed-size editor now wants (logical × new scale), which is not the
  // shell's client rect scaled from the old DPI (titlebar rounding, and an
  // editor that never accepted a free resize in the first place). Ask the
  // view, then recompute the windowed size from that answer so the shell
  // outer = content + titlebar at the new DPI, exactly like the open path.
  int content_w = width;
  int content_h = height;
  Steinberg::ViewRect rescaled{};
  const auto get_size_result = p->editor_view->getSize(&rescaled);
  if (get_size_result == Steinberg::kResultTrue ||
      get_size_result == Steinberg::kResultOk) {
    const int w = daux_view_rect_width(rescaled);
    const int h = daux_view_rect_height(rescaled);
    if (w > 0 && h > 0) {
      content_w = w;
      content_h = h;
    }
  }
  content_w = std::clamp(content_w, 160, 4096);
  content_h = std::clamp(content_h, 160, 4096);
  daux_constrain_content_size(p, &content_w, &content_h);
  const bool shell_changed =
      daux_embed_apply_content_size(p, content_w, content_h, "WM_DPICHANGED.getSize");
  daux_resize_child_client(content, content_w, content_h);
  auto local = daux_local_view_rect(content_w, content_h);
  const auto on_size_res = p->editor_view->onSize(&local);
  std::fprintf(stderr,
               "[PluginEditor] WM_DPICHANGED onSize result=0x%x client=%dx%d "
               "(suggested=%dx%d getSize=0x%x shell_resized=%d)\n",
               static_cast<unsigned>(on_size_res), content_w, content_h, width,
               height, static_cast<unsigned>(get_size_result),
               shell_changed ? 1 : 0);
}

void daux_editor_window_close_requested(void *user_data) {
  auto *p = static_cast<SphereDauxVst3Processor *>(user_data);
  if (p)
    p->embed_user_closed.store(true, std::memory_order_release);
}

DauxEditorWindowCallbacks
daux_editor_window_callbacks(SphereDauxVst3Processor *p) {
  DauxEditorWindowCallbacks callbacks{};
  callbacks.user_data = p;
  callbacks.is_live = daux_editor_window_live;
  callbacks.is_attached = daux_editor_window_attached;
  callbacks.is_resize_in_progress = daux_editor_window_resize_in_progress;
  callbacks.set_resize_in_progress = daux_editor_window_set_resize_in_progress;
  callbacks.can_resize = daux_editor_window_can_resize;
  callbacks.constrain_content_size = daux_editor_window_constrain_size;
  callbacks.on_content_resized = daux_editor_window_content_resized;
  callbacks.on_dpi_changed = daux_editor_window_dpi_changed;
  callbacks.on_close_requested = daux_editor_window_close_requested;
  return callbacks;
}

void layout_attach_or_fallback(SphereDauxVst3Processor *processor, HWND hwnd) {
  if (!processor || !hwnd)
    return;
  RECT rc{};
  GetClientRect(hwnd, &rc);
  const int w = rc.right - rc.left;
  const int h = rc.bottom - rc.top;
  if (processor->editor_attach_hwnd &&
      IsWindow(processor->editor_attach_hwnd)) {
    MoveWindow(processor->editor_attach_hwnd, 0, 0, w, h, TRUE);
    resize_editor_view(processor);
  }
  if (processor->editor_fallback_label_hwnd &&
      IsWindow(processor->editor_fallback_label_hwnd)) {
    const int panel_w = 360;
    const int x = std::max(16, (w - panel_w) / 2);
    const int y = std::max(18, (h - 96) / 2);
    MoveWindow(processor->editor_fallback_label_hwnd, x, y, panel_w, 24, TRUE);
    MoveWindow(processor->editor_fallback_reload_hwnd, x, y + 42, 104, 28,
               TRUE);
    MoveWindow(processor->editor_fallback_generic_hwnd, x + 116, y + 42, 124,
               28, TRUE);
    MoveWindow(processor->editor_fallback_close_hwnd, x + 252, y + 42, 82, 28,
               TRUE);
  }
}

void show_attach_failed_state(SphereDauxVst3Processor *processor,
                              const char *reason) {
  if (!processor || !processor->editor_hwnd)
    return;
  if (processor->editor_attach_hwnd &&
      IsWindow(processor->editor_attach_hwnd)) {
    paint_dark_child(processor->editor_attach_hwnd);
    DestroyWindow(processor->editor_attach_hwnd);
  }
  processor->editor_attach_hwnd = nullptr;
  processor->editor_view = nullptr;
  processor->editor_attached = false;
  destroy_fallback_controls(processor);

  processor->editor_fallback_label_hwnd = CreateWindowExW(
      0, L"STATIC", L"Editor failed to attach",
      WS_CHILD | WS_VISIBLE | SS_CENTER, 0, 0, 1, 1, processor->editor_hwnd,
      nullptr, GetModuleHandleW(nullptr), nullptr);
  processor->editor_fallback_reload_hwnd = CreateWindowExW(
      0, L"BUTTON", L"Reload Editor", WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON, 0,
      0, 1, 1, processor->editor_hwnd,
      reinterpret_cast<HMENU>(static_cast<INT_PTR>(kDauxFallbackReloadId)),
      GetModuleHandleW(nullptr), nullptr);
  processor->editor_fallback_generic_hwnd = CreateWindowExW(
      0, L"BUTTON", L"Generic Params",
      WS_CHILD | WS_VISIBLE | WS_DISABLED | BS_PUSHBUTTON, 0, 0, 1, 1,
      processor->editor_hwnd,
      reinterpret_cast<HMENU>(static_cast<INT_PTR>(kDauxFallbackGenericId)),
      GetModuleHandleW(nullptr), nullptr);
  processor->editor_fallback_close_hwnd = CreateWindowExW(
      0, L"BUTTON", L"Close", WS_CHILD | WS_VISIBLE | BS_PUSHBUTTON, 0, 0, 1, 1,
      processor->editor_hwnd,
      reinterpret_cast<HMENU>(static_cast<INT_PTR>(kDauxFallbackCloseId)),
      GetModuleHandleW(nullptr), nullptr);
  layout_attach_or_fallback(processor, processor->editor_hwnd);
  std::fprintf(
      stderr,
      "[SphereVST3] editor attach failed state shown handle=%llu reason=%s\n",
      processor->editor_handle, reason ? reason : "unknown");
}

extern "C" unsigned long long
sphere_daux_vst3_open_editor(SphereDauxVst3Processor *processor,
                             const char *window_id, const char *title,
                             int width, int height);

LRESULT CALLBACK daux_editor_window_proc(HWND hwnd, UINT msg, WPARAM wparam,
                                         LPARAM lparam) {
  auto *processor = reinterpret_cast<SphereDauxVst3Processor *>(
      GetWindowLongPtrW(hwnd, GWLP_USERDATA));
  switch (msg) {
  case WM_NCCREATE: {
    auto *create = reinterpret_cast<CREATESTRUCTW *>(lparam);
    processor =
        reinterpret_cast<SphereDauxVst3Processor *>(create->lpCreateParams);
    SetWindowLongPtrW(hwnd, GWLP_USERDATA,
                      reinterpret_cast<LONG_PTR>(processor));
    return TRUE;
  }
  case WM_SIZE:
    if (processor)
      layout_attach_or_fallback(processor, hwnd);
    return 0;
  case WM_SETFOCUS:
    if (processor && processor->editor_attach_hwnd)
      SetFocus(processor->editor_attach_hwnd);
    return 0;
  case WM_CTLCOLORSTATIC: {
    SetTextColor(reinterpret_cast<HDC>(wparam), RGB(231, 237, 245));
    SetBkColor(reinterpret_cast<HDC>(wparam), RGB(11, 15, 20));
    static HBRUSH dark_brush = CreateSolidBrush(RGB(11, 15, 20));
    return reinterpret_cast<LRESULT>(dark_brush);
  }
  case WM_ERASEBKGND: {
    RECT rc{};
    GetClientRect(hwnd, &rc);
    HBRUSH brush = CreateSolidBrush(RGB(11, 15, 20));
    FillRect(reinterpret_cast<HDC>(wparam), &rc, brush);
    DeleteObject(brush);
    return 1;
  }
  case WM_COMMAND:
    if (processor) {
      const int command_id = LOWORD(wparam);
      if (command_id == kDauxFallbackCloseId ||
          command_id == kDauxFallbackGenericId) {
        processor->close_editor_window();
        return 0;
      }
      if (command_id == kDauxFallbackReloadId) {
        const std::string window_id = processor->editor_window_id;
        const std::string title = processor->editor_title;
        const int requested_width = processor->editor_requested_width;
        const int requested_height = processor->editor_requested_height;
        processor->close_editor_window();
        sphere_daux_vst3_open_editor(processor, window_id.c_str(),
                                     title.empty() ? "Plugin Editor"
                                                   : title.c_str(),
                                     requested_width, requested_height);
        return 0;
      }
    }
    break;
  case WM_CLOSE:
    // User pressed the window's X button: destroy the editor shell/view only.
    // Processor and controller stay alive; only insert removal may destroy
    // them.
    if (processor) {
      processor->close_editor_window();
      return 0;
    }
    break;
  case WM_DAUX_DESTROY:
    // Posted by close_editor_window() when called cross-thread; execute the
    // DestroyWindow on this HWND's own thread.
    DestroyWindow(hwnd);
    return 0;
  case WM_DESTROY:
    // GWLP_USERDATA was already zeroed by close_editor_window() before this
    // message was dispatched, so processor may be null here.  Do cleanup only
    // if the pointer is still set (shouldn't normally happen but guard anyway).
    if (processor) {
      detach_editor_view(processor);
      if (processor->editor_attach_hwnd &&
          IsWindow(processor->editor_attach_hwnd)) {
        DestroyWindow(processor->editor_attach_hwnd);
      }
      destroy_fallback_controls(processor);
      processor->editor_attach_hwnd = nullptr;
      processor->editor_hwnd = nullptr;
      processor->editor_handle = 0;
      processor->editor_window_id.clear();
      processor->editor_title.clear();
    }
    return 0;
  default:
    break;
  }
  return DefWindowProcW(hwnd, msg, wparam, lparam);
}

/// Top-level window of this process that should own a legacy in-process editor.
///
/// An owned window floats above its owner and nothing else, which is what a
/// plug-in editor should do; the previous `WS_EX_TOPMOST` pinned it above every
/// application on the desktop. Returns `nullptr` when no suitable window exists
/// yet, which simply leaves the editor unowned rather than failing to open.
HWND daux_editor_owner_window() {
  struct Search {
    DWORD pid = 0;
    HWND found = nullptr;
  } search;
  search.pid = GetCurrentProcessId();
  EnumWindows(
      [](HWND hwnd, LPARAM data) -> BOOL {
        auto *search = reinterpret_cast<Search *>(data);
        DWORD pid = 0;
        GetWindowThreadProcessId(hwnd, &pid);
        if (pid != search->pid || !IsWindowVisible(hwnd))
          return TRUE;
        // Skip our own editor shells so one editor never owns another.
        wchar_t class_name[128]{};
        GetClassNameW(hwnd, class_name, 128);
        if (std::wcscmp(class_name, kDauxEditorWindowClass) == 0)
          return TRUE;
        if (GetWindow(hwnd, GW_OWNER) != nullptr)
          return TRUE;
        search->found = hwnd;
        return FALSE;
      },
      reinterpret_cast<LPARAM>(&search));
  return search.found;
}

void register_editor_parent_class() {
  static std::once_flag once;
  std::call_once(once, [] {
    WNDCLASSEXW wc{};
    wc.cbSize = sizeof(WNDCLASSEXW);
    wc.lpfnWndProc = daux_editor_window_proc;
    wc.hInstance = GetModuleHandleW(nullptr);
    wc.hCursor = LoadCursorW(nullptr, MAKEINTRESOURCEW(32512));
    wc.hbrBackground = reinterpret_cast<HBRUSH>(GetStockObject(BLACK_BRUSH));
    wc.lpszClassName = kDauxEditorWindowClass;
    RegisterClassExW(&wc);
  });
}

void daux_plugin_browser_runtime_release(SphereDauxVst3Processor *processor);

void SphereDauxVst3Processor::close_editor_window() {
  HWND hwnd = editor_hwnd;
  HWND embed_top = editor_embed_top_hwnd;
  HWND child = editor_attach_hwnd;
  // Zero back-pointer FIRST so any pending messages dispatched after this
  // cannot dereference the (potentially freed) SphereDauxVst3Processor.
  if (hwnd && IsWindow(hwnd)) {
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
  }
  const bool editor_window_owns_embed_top =
      embed_top && IsWindow(embed_top) && editor_window.shell_hwnd == embed_top;
  if (embed_top && IsWindow(embed_top) && !editor_window_owns_embed_top) {
    SetWindowLongPtrW(embed_top, GWLP_USERDATA, 0);
  }
  detach_editor_view(this);
  if (editor_window_owns_embed_top) {
    daux_editor_destroy_window(&editor_window);
    embed_top = nullptr;
    child = nullptr;
  }
  if (child && IsWindow(child)) {
    DestroyWindow(child);
  }
  if (embed_top && IsWindow(embed_top)) {
    DestroyWindow(embed_top);
  }
  destroy_fallback_controls(this);
  editor_attach_hwnd = nullptr;
  editor_embed_top_hwnd = nullptr;
  editor_window = DauxEditorWindow{};
  editor_parent_hwnd = nullptr;
  embed_mode = false;
  embed_geometry_valid = false;
  embed_content_w = 0;
  embed_content_h = 0;
  editor_hwnd = nullptr;
  editor_handle = 0;
  editor_window_id.clear();
  editor_title.clear();
  editor_requested_width = 0;
  editor_requested_height = 0;
  daux_plugin_browser_runtime_release(this);
  if (hwnd && IsWindow(hwnd)) {
    // Use PostMessage so the destroy is executed on the HWND's owning thread
    // (Electron main thread) rather than potentially a foreign thread.
    // WM_DAUX_DESTROY handler calls DestroyWindow directly.
    const DWORD hwnd_tid = GetWindowThreadProcessId(hwnd, nullptr);
    if (hwnd_tid == GetCurrentThreadId()) {
      DestroyWindow(hwnd);
    } else {
      PostMessageW(hwnd, WM_DAUX_DESTROY, 0, 0);
    }
  }
}

void SphereDauxVst3Processor::close_editor_view_only(const char *reason) {
  // Properly detach the IPlugView (calls removed()) and destroy ONLY the child
  // attach HWND.  The parent shell HWND remains alive so the same editor_handle
  // identity is preserved for the next reopen call.
  //
  // This is the correct hide/close path:
  //   detach view → destroy child HWND → hide parent HWND
  //
  // On next open() we always create a fresh child HWND.  Reusing the stale
  // child HWND after removed() was the root cause of the white-window
  // regression.
  detach_editor_view(this);
  if (editor_attach_hwnd && IsWindow(editor_attach_hwnd)) {
    DestroyWindow(editor_attach_hwnd);
  }
  destroy_fallback_controls(this);
  editor_attach_hwnd = nullptr;
  std::fprintf(stderr,
               "[SphereVST3] close_editor_view_only handle=%llu reason=%s "
               "childHwndDestroyed=1\n",
               editor_handle, reason ? reason : "unknown");
}

// ── GPUI-embedded editor (reuses this processor's controller) ────────────────

bool daux_embed_debug() {
  static const bool enabled =
      std::getenv("FUTUREBOARD_PLUGIN_VIEW_DEBUG") != nullptr ||
      std::getenv("FUTUREBOARD_PLUGIN_DEBUG") != nullptr;
  return enabled;
}

// Initialize COM as STA on the editor (GPUI UI) thread before any IPlugView::
// attached call. UAD Native, Slate, and other CEF/WebView-backed VST3s rely on
// this — without an STA on the thread that owns their parent HWND, the
// embedded Chromium host never finishes initializing and the editor stays
// blank. Idempotent and safe to re-enter: if the thread is already initialized
// to a different apartment we log the HRESULT and keep going (the host will
// still typically attach, just without our hint).
//
// Deliberately NOT paired with `CoUninitialize` — the editor thread keeps STA
// for the editor lifetime. Tearing down COM mid-editor will crash WebView
// hosts.
void daux_embed_ensure_com_initialized() {
  static thread_local HRESULT s_last_hr = static_cast<HRESULT>(0x7FFFFFFF);
  const HRESULT hr = CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);
  if (hr != s_last_hr) {
    s_last_hr = hr;
    const char *tag = "ok";
    if (hr == S_FALSE) {
      tag = "already initialized (STA)";
    } else if (hr == RPC_E_CHANGED_MODE) {
      tag = "RPC_E_CHANGED_MODE (thread already in MTA)";
    } else if (FAILED(hr)) {
      tag = "FAILED";
    }
    std::fprintf(stderr, "[vst3-editor] COM init hr=0x%08lx (%s) tid=%lu\n",
                 static_cast<unsigned long>(hr), tag, GetCurrentThreadId());
  }
}

// ── Generic browser/WebView runtime compatibility layer ──────────────────────
// Many modern VST3 plug-ins render their editor with an embedded browser engine
// and ship the runtime DLLs/resources *inside* the .vst3 bundle. The loader
// DLLs resolve dependents from their own directory, so before
// createView/attached we detect the bundled runtime and add ONLY its native
// dir(s) to the DLL search path for the lifetime of the editor — never
// globally, never permanently.
//
// Detection is keyed off marker files, not vendor names, so this is not
// UAD-only and never touches plug-ins that ship no browser runtime (e.g.
// FabFilter).

// Mirror of the Rust `PluginEditorRuntimeKind`.
enum class DauxEditorRuntimeKind {
  Native = 0,
  WebView2 = 1,
  Cef = 2,
  Chromium = 3,
  BrowserUnknown = 4,
};

const char *daux_editor_runtime_kind_name(DauxEditorRuntimeKind kind) {
  switch (kind) {
  case DauxEditorRuntimeKind::WebView2:
    return "WebView2";
  case DauxEditorRuntimeKind::Cef:
    return "Cef";
  case DauxEditorRuntimeKind::Chromium:
    return "Chromium";
  case DauxEditorRuntimeKind::BrowserUnknown:
    return "BrowserUnknown";
  case DauxEditorRuntimeKind::Native:
  default:
    return "Native";
  }
}

bool daux_plugin_webview_based_debug() {
  static const bool enabled =
      std::getenv("FUTUREBOARD_PLUGIN_WEBVIEW_DEBUG") != nullptr ||
      std::getenv("FUTUREBOARD_UAD_DEBUG") != nullptr;
  return enabled;
}

std::wstring daux_webview_runtime_arch_subdir() {
#if defined(_M_ARM64)
  return L"win-arm64";
#else
  return L"win-x64";
#endif
}

bool daux_path_exists_w(const std::wstring &path) {
  if (path.empty())
    return false;
  const DWORD attrs = GetFileAttributesW(path.c_str());
  return attrs != INVALID_FILE_ATTRIBUTES &&
         (attrs & FILE_ATTRIBUTE_DIRECTORY) == 0;
}

bool daux_dir_exists_w(const std::wstring &path) {
  if (path.empty())
    return false;
  const DWORD attrs = GetFileAttributesW(path.c_str());
  return attrs != INVALID_FILE_ATTRIBUTES &&
         (attrs & FILE_ATTRIBUTE_DIRECTORY) != 0;
}

std::wstring daux_join_path_w(std::wstring base, const wchar_t *suffix) {
  if (base.empty())
    return suffix ? suffix : L"";
  while (!base.empty() && (base.back() == L'\\' || base.back() == L'/')) {
    base.pop_back();
  }
  if (!suffix || !*suffix)
    return base;
  std::wstring out = std::move(base);
  out.push_back(L'\\');
  out += suffix;
  return out;
}

bool daux_file_in_dir(const std::wstring &dir, const wchar_t *file) {
  return daux_path_exists_w(daux_join_path_w(dir, file));
}

std::string daux_wide_to_utf8(const std::wstring &value) {
  if (value.empty())
    return {};
  const int len = WideCharToMultiByte(CP_UTF8, 0, value.c_str(), -1, nullptr, 0,
                                      nullptr, nullptr);
  if (len <= 1)
    return {};
  std::string out(static_cast<std::size_t>(len - 1), '\0');
  WideCharToMultiByte(CP_UTF8, 0, value.c_str(), -1, out.data(), len, nullptr,
                      nullptr);
  return out;
}

void daux_push_dir_unique(std::vector<std::wstring> &dirs,
                          const std::wstring &dir) {
  if (dir.empty())
    return;
  for (const auto &existing : dirs) {
    if (_wcsicmp(existing.c_str(), dir.c_str()) == 0)
      return;
  }
  dirs.push_back(dir);
}

// Result of scanning a .vst3 bundle for a bundled browser/WebView runtime.
struct DauxEditorRuntimeDetection {
  DauxEditorRuntimeKind kind = DauxEditorRuntimeKind::Native;
  std::vector<std::wstring>
      dll_dirs;                 // native dirs to add to the DLL search path
  std::wstring webview2_loader; // WebViewLoader.dll to verify-load (safe)
  std::wstring marker;          // diagnostic: first marker file found
};

// Scan the bundle directory for known browser/WebView runtime marker files.
// Bounded: probes a fixed list of candidate sub-directories, no recursion.
DauxEditorRuntimeDetection
daux_detect_editor_runtime(const std::string &plugin_path) {
  DauxEditorRuntimeDetection out;
  if (plugin_path.empty())
    return out;
  const std::wstring root = widen_utf8(plugin_path.c_str());
  const std::wstring arch = daux_webview_runtime_arch_subdir();

  static const wchar_t *kBaseRel[] = {
      L"", // bundle root
      L"Contents\\Resources",
      L"Contents\\x86_64-win",
      L"Contents\\Resources\\WebView2",
      L"Contents\\Resources\\CEF",
      L"Contents\\Resources\\Chromium",
      L"Contents\\Resources\\Browser",
      L"Contents\\Resources\\runtimes",
      L"Contents\\Resources\\bin",
  };

  bool found_webview2 = false;
  bool found_cef = false;
  bool found_chromium = false;
  bool found_browser = false;

  for (const wchar_t *rel : kBaseRel) {
    const std::wstring base = (*rel) ? daux_join_path_w(root, rel) : root;
    if (!daux_dir_exists_w(base))
      continue;

    // WebView2 fixed-version runtime: WebViewLoader.dll may sit directly in the
    // base dir or under runtimes\win-{arch}\native (and the bare
    // win-{arch}\native).
    const std::wstring runtimes_native = daux_join_path_w(
        daux_join_path_w(daux_join_path_w(base, L"runtimes"), arch.c_str()),
        L"native");
    const std::wstring arch_native =
        daux_join_path_w(daux_join_path_w(base, arch.c_str()), L"native");
    const std::wstring wv2_candidates[] = {base, runtimes_native, arch_native};
    for (const std::wstring &nd : wv2_candidates) {
      if (nd.empty() || !daux_dir_exists_w(nd))
        continue;
      const std::wstring loader = daux_join_path_w(nd, L"WebViewLoader.dll");
      if (daux_path_exists_w(loader)) {
        found_webview2 = true;
        daux_push_dir_unique(out.dll_dirs, nd);
        if (out.webview2_loader.empty())
          out.webview2_loader = loader;
        if (out.marker.empty())
          out.marker = loader;
      }
      if (daux_file_in_dir(nd, L"Microsoft.Web.WebView2.Core.dll")) {
        found_webview2 = true;
        daux_push_dir_unique(out.dll_dirs, nd);
        if (out.marker.empty())
          out.marker = daux_join_path_w(nd, L"Microsoft.Web.WebView2.Core.dll");
      }
    }

    // CEF / Chromium markers in the base dir.
    const bool has_libcef = daux_file_in_dir(base, L"libcef.dll");
    const bool has_chrome_elf = daux_file_in_dir(base, L"chrome_elf.dll");
    const bool has_cef_pak = daux_file_in_dir(base, L"cef.pak") ||
                             daux_file_in_dir(base, L"cef_100_percent.pak") ||
                             daux_file_in_dir(base, L"cef_200_percent.pak");
    const bool has_icu = daux_file_in_dir(base, L"icudtl.dat");
    const bool has_v8 = daux_file_in_dir(base, L"snapshot_blob.bin") ||
                        daux_file_in_dir(base, L"v8_context_snapshot.bin");
    const bool has_respak = daux_file_in_dir(base, L"resources.pak");

    if (has_libcef) {
      found_cef = true;
      daux_push_dir_unique(out.dll_dirs, base);
      if (out.marker.empty())
        out.marker = daux_join_path_w(base, L"libcef.dll");
    }
    if (has_cef_pak)
      found_cef = true;
    if (has_chrome_elf) {
      daux_push_dir_unique(out.dll_dirs, base);
      if (!has_libcef && !has_cef_pak)
        found_chromium = true;
      if (out.marker.empty())
        out.marker = daux_join_path_w(base, L"chrome_elf.dll");
    }
    if (has_icu || has_v8 || has_respak)
      found_browser = true;
  }

  if (found_webview2)
    out.kind = DauxEditorRuntimeKind::WebView2;
  else if (found_cef)
    out.kind = DauxEditorRuntimeKind::Cef;
  else if (found_chromium)
    out.kind = DauxEditorRuntimeKind::Chromium;
  else if (found_browser)
    out.kind = DauxEditorRuntimeKind::BrowserUnknown;
  else
    out.kind = DauxEditorRuntimeKind::Native;
  return out;
}

bool daux_plugin_is_browser_based(const std::string &plugin_path) {
  return daux_detect_editor_runtime(plugin_path).kind !=
         DauxEditorRuntimeKind::Native;
}

void daux_webview2_ensure_dll_search_policy() {
  static std::once_flag once;
  std::call_once(once, [] {
    if (!SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_DEFAULT_DIRS |
                                  LOAD_LIBRARY_SEARCH_USER_DIRS)) {
      std::fprintf(
          stderr,
          "[plugin-webview-based] SetDefaultDllDirectories failed err=%lu\n",
          GetLastError());
    } else if (daux_plugin_webview_based_debug()) {
      std::fprintf(stderr,
                   "[plugin-webview-based] SetDefaultDllDirectories ok\n");
    }
  });
}

// Add every detected native runtime dir to the per-process DLL search path and
// (for WebView2 only) verify-load the thin WebViewLoader.dll. CEF/Chromium
// loaders are NOT force-loaded — that would spin up Chromium on the wrong
// thread; the plug-in loads its own once the directory is discoverable.
bool daux_plugin_browser_runtime_prepare(SphereDauxVst3Processor *processor) {
  if (!processor)
    return false;
  if (!processor->plugin_browser_dll_cookies.empty() ||
      processor->plugin_browser_loader) {
    return true; // already prepared
  }

  const DauxEditorRuntimeDetection det =
      daux_detect_editor_runtime(processor->plugin_path);
  processor->plugin_browser_runtime_kind = static_cast<int>(det.kind);
  // Always log the detected editor runtime (Native vs WebView2 vs
  // CEF/Chromium): it decides whether a hung `attached()` is a
  // browser-subprocess issue or a native-GUI one, without needing a debug flag
  // set.
  std::fprintf(stderr, "[plugin-view] editor_runtime_kind=%s path=%s\n",
               daux_editor_runtime_kind_name(det.kind),
               processor->plugin_path.c_str());
  const bool debug = daux_plugin_webview_based_debug() || daux_embed_debug();

  if (det.kind == DauxEditorRuntimeKind::Native) {
    if (debug) {
      std::fprintf(stderr,
                   "[plugin-webview-based] runtime=Native (no bundled browser "
                   "runtime) path=%s\n",
                   processor->plugin_path.c_str());
    }
    return true; // normal native UI plug-in — nothing to do
  }

  if (debug) {
    std::fprintf(
        stderr,
        "[plugin-webview-based] runtime=%s marker=%s dll_dirs=%zu path=%s\n",
        daux_editor_runtime_kind_name(det.kind),
        daux_wide_to_utf8(det.marker).c_str(), det.dll_dirs.size(),
        processor->plugin_path.c_str());
  }

  if (det.dll_dirs.empty()) {
    // Browser engine detected by resource files (e.g. *.pak) but no native dir
    // to add — nothing actionable, but not a failure. Editor open continues.
    if (debug) {
      std::fprintf(stderr,
                   "[plugin-webview-based] runtime=%s detected via resources "
                   "only; no DLL dir to add\n",
                   daux_editor_runtime_kind_name(det.kind));
    }
    return true;
  }

  daux_webview2_ensure_dll_search_policy();

  std::vector<DLL_DIRECTORY_COOKIE> cookies;
  for (const std::wstring &dir : det.dll_dirs) {
    DLL_DIRECTORY_COOKIE cookie = AddDllDirectory(dir.c_str());
    if (!cookie) {
      const DWORD err = GetLastError();
      std::fprintf(stderr,
                   "[plugin-webview-based] AddDllDirectory failed err=%lu\n",
                   err);
      for (DLL_DIRECTORY_COOKIE c : cookies)
        RemoveDllDirectory(c);
      set_last_error("Failed to configure plugin browser runtime search path "
                     "(AddDllDirectory err=" +
                     std::to_string(err) + ")");
      return false;
    }
    cookies.push_back(cookie);
    if (debug) {
      std::fprintf(stderr, "[plugin-webview-based] AddDllDirectory ok dir=%s\n",
                   daux_wide_to_utf8(dir).c_str());
    }
  }

  HMODULE loader = nullptr;
  if (!det.webview2_loader.empty()) {
    loader = LoadLibraryW(det.webview2_loader.c_str());
    if (!loader) {
      const DWORD err = GetLastError();
      std::fprintf(stderr,
                   "[plugin-webview-based] LoadLibrary WebViewLoader.dll "
                   "failed err=%lu\n",
                   err);
      for (DLL_DIRECTORY_COOKIE c : cookies)
        RemoveDllDirectory(c);
      set_last_error(
          std::string("Failed to load plugin WebView2 runtime from ") +
          daux_wide_to_utf8(det.webview2_loader) +
          " (GetLastError=" + std::to_string(err) + ")");
      return false;
    }
    if (debug) {
      std::fprintf(stderr,
                   "[plugin-webview-based] LoadLibrary WebViewLoader.dll ok\n");
    }
  }

  processor->plugin_browser_dll_cookies = std::move(cookies);
  processor->plugin_browser_loader = loader;
  return true;
}

void daux_plugin_browser_runtime_release(SphereDauxVst3Processor *processor) {
  if (!processor)
    return;
  if (processor->plugin_browser_loader) {
    FreeLibrary(processor->plugin_browser_loader);
    processor->plugin_browser_loader = nullptr;
  }
  for (DLL_DIRECTORY_COOKIE cookie : processor->plugin_browser_dll_cookies) {
    if (cookie)
      RemoveDllDirectory(cookie);
  }
  processor->plugin_browser_dll_cookies.clear();
}

// Reposition/resize the host window to the requested region. Returns true only
// when the applied rect actually changed, so idle frames do no SetWindowPos /
// onSize / raise work (mirrors the SpherePluginHost anti-flicker fix).
bool daux_embed_sync_geometry(SphereDauxVst3Processor *p, int x, int y, int w,
                              int h, bool log_reposition) {
  HWND top = p ? p->editor_embed_top_hwnd : nullptr;
  if (!p || !top || !IsWindow(top))
    return false;
  // Detached: a standalone OS window owns its own position/size (user-movable,
  // resizeView-driven). Never snap it to the GPUI parent region.
  if (p->embed_host_kind == 2)
    return false;
  p->embed_host_x = x;
  p->embed_host_y = y;
  p->embed_host_w = w;
  p->embed_host_h = h;
  if (p->embed_host_kind == 1 && p->editor_parent_hwnd) {
    if (!IsWindow(p->editor_parent_hwnd))
      return false;
    const bool parent_visible = IsWindowVisible(p->editor_parent_hwnd) &&
                                !IsIconic(p->editor_parent_hwnd);
    ShowWindow(top, parent_visible ? SW_SHOWNA : SW_HIDE);
    RECT screen{};
    if (!daux_editor_content_screen_rect(p->editor_parent_hwnd, x, y, w, h,
                                         &screen.left, &screen.top,
                                         &screen.right, &screen.bottom)) {
      return false;
    }
    if (p->embed_geometry_valid && EqualRect(&screen, &p->embed_last_applied))
      return false;
    p->embed_last_applied = screen;
    p->embed_geometry_valid = true;
    SetWindowPos(top, p->editor_parent_hwnd, screen.left, screen.top,
                 screen.right - screen.left, screen.bottom - screen.top,
                 SWP_NOACTIVATE | SWP_SHOWWINDOW);
    daux_editor_apply_tool_styles(top, p->editor_parent_hwnd);
  } else {
    RECT want{x, y, x + w, y + h};
    if (p->embed_geometry_valid && EqualRect(&want, &p->embed_last_applied))
      return false;
    p->embed_last_applied = want;
    p->embed_geometry_valid = true;
    SetWindowPos(top, HWND_TOP, x, y, w, h, SWP_SHOWWINDOW | SWP_NOACTIVATE);
  }
  EnableWindow(top, TRUE);
  if (p->editor_attach_hwnd && IsWindow(p->editor_attach_hwnd)) {
    RECT rc{};
    GetClientRect(top, &rc);
    SetWindowPos(p->editor_attach_hwnd, nullptr, 0, 0, rc.right - rc.left,
                 rc.bottom - rc.top,
                 SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW);
  }
  daux_editor_raise_children(top);
  if (log_reposition && daux_embed_debug()) {
    std::fprintf(
        stderr, "[plugin-view] daux reposition top=0x%p content=0x%p mode=%s\n",
        static_cast<void *>(top), static_cast<void *>(p->editor_attach_hwnd),
        daux_editor_host_kind_name(p->embed_host_kind));
  }
  return true;
}

bool daux_embed_apply_content_size(SphereDauxVst3Processor *p, int content_w,
                                   int content_h, const char *reason) {
  if (!p || content_w <= 0 || content_h <= 0)
    return false;
  if (p->embed_resize_in_progress)
    return false;

  const bool debug = daux_embed_debug() || daux_vst3_editor_debug();

  // Detached: size the standalone top-level window so its CLIENT area equals
  // the plug-in's preferred size (editorhost pattern). Position is left to the
  // user.
  if (p->embed_host_kind == 2) {
    bool changed = false;
    HWND top = p->editor_embed_top_hwnd;
    if (top && IsWindow(top)) {
      // Borderless (WM_NCCALCSIZE==0): the window outer == its client. Size it
      // to the plug-in content plus the reserved custom titlebar; the content
      // child sits below the titlebar.
      const int th = daux_editor_titlebar_height(top);
      const int win_w = content_w;
      const int win_h = content_h + th;
      RECT cur{};
      GetWindowRect(top, &cur);
      if ((cur.right - cur.left) != win_w || (cur.bottom - cur.top) != win_h) {
        changed =
            daux_editor_resize_content(&p->editor_window, content_w, content_h);
      }
    }
    p->embed_content_w = content_w;
    p->embed_content_h = content_h;
    if (debug) {
      std::fprintf(stderr,
                   "[plugin-view] auto_size mode=detached plugin=%dx%d "
                   "reason=%s changed=%d\n",
                   content_w, content_h, reason ? reason : "unknown",
                   changed ? 1 : 0);
    }
    return changed;
  }

  const int header_h = std::max(0, p->embed_host_y);
  const int shell_client_w = std::max(content_w, p->embed_host_x + content_w);
  const int shell_client_h = header_h + content_h;
  bool changed = false;

  p->embed_resize_in_progress = true;

  // Main-owned bridge shell: parent is the shell content HWND — resize via IPC.
  if (p->embed_mode && p->editor_parent_hwnd &&
      IsWindow(p->editor_parent_hwnd) && !p->editor_hwnd) {
    p->pending_main_shell_w = content_w;
    p->pending_main_shell_h = content_h;
    p->pending_main_shell_resize.store(true, std::memory_order_release);
  } else if (p->embed_mode && p->editor_parent_hwnd &&
             IsWindow(p->editor_parent_hwnd)) {
    RECT client{};
    GetClientRect(p->editor_parent_hwnd, &client);
    const int current_w = client.right - client.left;
    const int current_h = client.bottom - client.top;
    if (current_w != shell_client_w || current_h != shell_client_h) {
      SetWindowPos(p->editor_parent_hwnd, nullptr, 0, 0, shell_client_w,
                   shell_client_h, SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
      changed = true;
    }
  } else if (p->editor_hwnd && IsWindow(p->editor_hwnd)) {
    RECT wr{0, 0, content_w, content_h};
    const DWORD style =
        static_cast<DWORD>(GetWindowLongPtrW(p->editor_hwnd, GWL_STYLE));
    const DWORD ex_style =
        static_cast<DWORD>(GetWindowLongPtrW(p->editor_hwnd, GWL_EXSTYLE));
    if (!daux_adjust_window_rect_for_dpi(p->editor_hwnd, &wr, style,
                                         ex_style)) {
      AdjustWindowRectEx(&wr, style, FALSE, ex_style);
    }
    const int shell_w = wr.right - wr.left;
    const int shell_h = wr.bottom - wr.top;
    RECT win{};
    GetWindowRect(p->editor_hwnd, &win);
    if ((win.right - win.left) != shell_w ||
        (win.bottom - win.top) != shell_h) {
      SetWindowPos(p->editor_hwnd, nullptr, 0, 0, shell_w, shell_h,
                   SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE);
      changed = true;
    }
  }

  if (p->editor_embed_top_hwnd && IsWindow(p->editor_embed_top_hwnd)) {
    const bool host_changed =
        daux_embed_sync_geometry(p, 0, header_h, content_w, content_h, false);
    changed = changed || host_changed;
  }

  p->embed_content_w = content_w;
  p->embed_content_h = content_h;
  p->embed_resize_in_progress = false;

  if (p->pending_main_shell_resize.load(std::memory_order_acquire)) {
    const UINT dpi = (p->editor_parent_hwnd && IsWindow(p->editor_parent_hwnd))
                         ? GetDpiForWindow(p->editor_parent_hwnd)
                         : 96;
    std::fprintf(stderr,
                 "[PluginEditor] resizeView requested instance=%s size=%dx%d "
                 "dpi=%u reason=%s\n",
                 p->embed_instance_label.empty()
                     ? "<unknown>"
                     : p->embed_instance_label.c_str(),
                 content_w, content_h, dpi, reason ? reason : "unknown");
    std::fprintf(stderr, "[PluginEditor] container resized client=%dx%d\n",
                 content_w, content_h);
  }

  if (debug) {
    std::fprintf(stderr,
                 "[plugin-view] auto_size plugin=%dx%d shell=%dx%d "
                 "content=(0,%d,%d,%d) reason=%s changed=%d\n",
                 content_w, content_h, shell_client_w, shell_client_h, header_h,
                 content_w, content_h, reason ? reason : "unknown",
                 changed ? 1 : 0);
  }
  return changed;
}

bool daux_embed_has_visible_ui(SphereDauxVst3Processor *p) {
  if (!p || !p->editor_attach_hwnd || !IsWindow(p->editor_attach_hwnd))
    return false;
  if (!IsWindowVisible(p->editor_attach_hwnd))
    return false;
  RECT cr{};
  GetClientRect(p->editor_attach_hwnd, &cr);
  if (cr.right - cr.left < 4 || cr.bottom - cr.top < 4)
    return false;
  struct Ctx {
    int visible = 0;
  } ctx{};
  EnumChildWindows(
      p->editor_attach_hwnd,
      [](HWND hwnd, LPARAM lp) -> BOOL {
        if (!IsWindowVisible(hwnd))
          return TRUE;
        RECT r{};
        GetWindowRect(hwnd, &r);
        if (r.right > r.left && r.bottom > r.top)
          reinterpret_cast<Ctx *>(lp)->visible++;
        return TRUE;
      },
      reinterpret_cast<LPARAM>(&ctx));
  if (ctx.visible > 0)
    return true;
  if (p->editor_view) {
    Steinberg::ViewRect sz{};
    const auto gs = p->editor_view->getSize(&sz);
    if (gs == Steinberg::kResultTrue || gs == Steinberg::kResultOk) {
      if (sz.right - sz.left > 16 && sz.bottom - sz.top > 16)
        return true;
    }
  }
  return false;
}

void SphereDauxVst3Processor::close_embed_editor(const char *reason) {
  detach_editor_view(this); // IPlugView::removed(); keeps controller/processor
  if (editor_window.shell_hwnd &&
      editor_window.shell_hwnd == editor_embed_top_hwnd) {
    daux_editor_destroy_window(&editor_window);
  } else if (editor_attach_hwnd && IsWindow(editor_attach_hwnd)) {
    DestroyWindow(editor_attach_hwnd);
  }
  if (editor_embed_top_hwnd && IsWindow(editor_embed_top_hwnd)) {
    // Stop the top WndProc from touching this processor during teardown.
    SetWindowLongPtrW(editor_embed_top_hwnd, GWLP_USERDATA, 0);
    DestroyWindow(editor_embed_top_hwnd);
  }
  embed_user_closed.store(false, std::memory_order_release);
  editor_attach_hwnd = nullptr;
  editor_embed_top_hwnd = nullptr;
  editor_window = DauxEditorWindow{};
  editor_parent_hwnd = nullptr;
  embed_mode = false;
  embed_geometry_valid = false;
  embed_content_w = 0;
  embed_content_h = 0;
  embed_resize_in_progress = false;
  editor_handle = 0;
  daux_plugin_browser_runtime_release(this);
  std::fprintf(
      stderr,
      "[SphereVST3] close_embed_editor reason=%s (processor kept alive)\n",
      reason ? reason : "unknown");
}
#endif

// ── C API
// ─────────────────────────────────────────────────────────────────────

extern "C" int sphere_daux_vst3_bridge_probe(void) {
  std::fprintf(stderr, "[DAUx VST3] bridge probe ok\n");
  std::fflush(stderr);
  return 0xDA03;
}

extern "C" const char *sphere_daux_vst3_last_error(void) {
  return g_last_error.c_str();
}

/// Creates and initializes an instance, optionally stopping short of activation.
///
/// `activate_now == false` is the ARA path: `ARAVST3.h` requires the document
/// controller binding to happen before the first `setActive()`, so the caller
/// binds first and then calls `sphere_daux_vst3_activate`.
static SphereDauxVst3Processor *
daux_vst3_create_impl(const char *plugin_path, const char *class_id,
                      double sample_rate, bool activate_now) {
  set_last_error("");
  if (!plugin_path || !*plugin_path) {
    set_last_error("empty plugin_path");
    return nullptr;
  }

  std::fprintf(stderr, "[DAUx VST3] create entered: path=%s classId=%s\n",
               plugin_path, class_id ? class_id : "");
#if defined(_WIN32)
  std::fprintf(stderr,
               "[process] role=plugin_host mode=in_process pid=%lu tid=%lu\n",
               GetCurrentProcessId(), GetCurrentThreadId());
#endif
  std::fflush(stderr);

  auto instance = std::make_unique<SphereDauxVst3Processor>();
  std::fprintf(stderr, "[DAUx VST3] instance created: path=%s classId=%s\n",
               plugin_path, class_id ? class_id : "");
  std::string error;
  instance->module = VST3::Hosting::Module::create(plugin_path, error);
  if (!instance->module) {
    set_last_error("module load failed: " + error);
    std::fprintf(stderr, "[DAUx VST3] module load FAILED: %s\n", error.c_str());
    return nullptr;
  }
  std::fprintf(stderr, "[DAUx VST3] plugin loaded: %s\n", plugin_path);

  const auto factory = instance->module->getFactory();
  factory.setHostContext(&instance->host_context);
  log_factory_classes(factory);
  {
    std::ostringstream classes;
    int index = 0;
    for (const auto &info : factory.classInfos()) {
      if (index > 0)
        classes << " | ";
      classes << "[" << index << "] name='" << info.name() << "' category='"
              << info.category() << "' uid=" << info.ID().toString();
      ++index;
    }
    if (index == 0)
      set_last_error("factory has no classes");
    else
      set_last_error("factory classes: " + classes.str());
  }

  const std::string requested = class_id ? class_id : "";
  VST3::Optional<VST3::UID> uid;
  if (!looks_like_zero_class_id(requested))
    uid = VST3::UID::fromString(requested);
  if (!uid) {
    std::fprintf(stderr, "[DAUx VST3] classId missing/zero/invalid; trying "
                         "first Audio Module Class fallback\n");
    uid = first_audio_module_uid(factory);
  }
  if (!uid) {
    set_last_error(g_last_error + "; no Audio Module Class found");
    std::fprintf(stderr,
                 "[DAUx VST3] no Audio Module Class found in factory\n");
    return nullptr;
  }

  instance->component =
      factory.createInstance<Steinberg::Vst::IComponent>(*uid);
  if (!instance->component) {
    std::fprintf(stderr,
                 "[DAUx VST3] create IComponent FAILED for classId=%s; trying "
                 "first Audio Module Class fallback\n",
                 requested.c_str());
    uid = first_audio_module_uid(factory);
    if (uid)
      instance->component =
          factory.createInstance<Steinberg::Vst::IComponent>(*uid);
    if (!instance->component) {
      set_last_error(g_last_error +
                     "; create IComponent failed for requested classId='" +
                     requested + "' and fallback");
      std::fprintf(stderr,
                   "[DAUx VST3] create IComponent FAILED after fallback\n");
      return nullptr;
    }
  }
  std::fprintf(stderr, "[DAUx VST3] component created classId=%s\n",
               uid->toString().c_str());
  for (const auto &info : factory.classInfos()) {
    if (info.ID() == *uid) {
      instance->class_name = info.name();
      break;
    }
  }
  if (auto pb =
          Steinberg::FUnknownPtr<Steinberg::IPluginBase>(instance->component)) {
    if (pb->initialize(&instance->host_context) != Steinberg::kResultOk) {
      set_last_error(g_last_error + "; component initialize failed");
      std::fprintf(stderr, "[DAUx VST3] component initialize FAILED\n");
      return nullptr;
    }
    std::fprintf(stderr, "[SphereVST3] component initialized result=0 ok\n");
  } else {
    set_last_error(g_last_error + "; component does not implement IPluginBase");
    std::fprintf(stderr,
                 "[DAUx VST3] component does not implement IPluginBase\n");
    return nullptr;
  }

  if (instance->component->queryInterface(
          Steinberg::Vst::IAudioProcessor::iid,
          reinterpret_cast<void **>(&instance->processor)) !=
          Steinberg::kResultTrue ||
      !instance->processor) {
    set_last_error(g_last_error +
                   "; component does not implement IAudioProcessor");
    std::fprintf(stderr,
                 "[DAUx VST3] component does not implement IAudioProcessor\n");
    return nullptr;
  }
  std::fprintf(stderr, "[SphereVST3] processor found\n");

  // Obtain IEditController (either from the component itself or a separate
  // class)
  Steinberg::Vst::IEditController *raw_ctrl = nullptr;
  if (instance->component->queryInterface(
          Steinberg::Vst::IEditController::iid,
          reinterpret_cast<void **>(&raw_ctrl)) == Steinberg::kResultTrue) {
    instance->controller =
        Steinberg::IPtr<Steinberg::Vst::IEditController>::adopt(raw_ctrl);
    instance->controller_is_component = true;
    std::fprintf(
        stderr,
        "[SphereVST3] controller initialized result=0 ok component-owned\n");
  } else {
    Steinberg::TUID ctrl_cid{};
    if (instance->component->getControllerClassId(ctrl_cid) ==
        Steinberg::kResultTrue) {
      instance->controller =
          factory.createInstance<Steinberg::Vst::IEditController>(
              VST3::UID(ctrl_cid));
      if (instance->controller) {
        if (auto pb = Steinberg::FUnknownPtr<Steinberg::IPluginBase>(
                instance->controller)) {
          if (pb->initialize(&instance->host_context) != Steinberg::kResultOk) {
            std::fprintf(stderr, "[DAUx VST3] controller initialize FAILED\n");
            instance->controller = nullptr;
          } else {
            std::fprintf(stderr,
                         "[SphereVST3] controller initialized result=0 ok\n");
          }
        }
      }
    }
  }

  // Connect component ↔ controller
  if (instance->controller) {
    instance->component_connection =
        Steinberg::FUnknownPtr<Steinberg::Vst::IConnectionPoint>(
            instance->component);
    instance->controller_connection =
        Steinberg::FUnknownPtr<Steinberg::Vst::IConnectionPoint>(
            instance->controller);
    if (instance->component_connection && instance->controller_connection) {
      instance->component_connection->connect(instance->controller_connection);
      instance->controller_connection->connect(instance->component_connection);
      std::fprintf(stderr, "[DAUx VST3] component/controller connected\n");
    }
  }

  instance->plugin_path = plugin_path ? plugin_path : "";

  instance->deferred_sample_rate = sample_rate;
  if (activate_now) {
    if (!instance->setup(sample_rate)) {
      set_last_error(g_last_error + "; setup failed");
      instance->shutdown();
      return nullptr;
    }
    instance->activated = true;
  } else {
    std::fprintf(stderr,
                 "[DAUx VST3] created unactivated for ARA binding: %s\n",
                 plugin_path ? plugin_path : "");
  }

  set_last_error("");
  std::fprintf(stderr, "[DAUx VST3] processor ready: %s handle=0x%p\n",
               plugin_path, static_cast<void *>(instance.get()));
  return instance.release();
}

extern "C" SphereDauxVst3Processor *
sphere_daux_vst3_create(const char *plugin_path, const char *class_id,
                        double sample_rate) {
  return daux_vst3_create_impl(plugin_path, class_id, sample_rate, true);
}

extern "C" SphereDauxVst3Processor *
sphere_daux_vst3_create_for_ara(const char *plugin_path, const char *class_id,
                                double sample_rate) {
  return daux_vst3_create_impl(plugin_path, class_id, sample_rate, false);
}

extern "C" int sphere_daux_vst3_activate(SphereDauxVst3Processor *processor) {
  if (!processor)
    return 0;
  if (processor->activated)
    return 1;
  if (!processor->setup(processor->deferred_sample_rate)) {
    set_last_error(g_last_error + "; deferred setup failed");
    return 0;
  }
  processor->activated = true;
  std::fprintf(stderr, "[DAUx VST3] deferred activation ok\n");
  return 1;
}

extern "C" void
sphere_daux_vst3_stop_processing(SphereDauxVst3Processor *processor) {
  if (!processor)
    return;
  if (processor->processor && processor->processing) {
    processor->processor->setProcessing(false);
  }
  processor->processing = false;
  // `setActive(false)` releases the plug-in's render resources. Not guarded on
  // `activated`: an instance can be active without this host having run the
  // deferred activation, and the call is a no-op on one that is not.
  if (processor->component) {
    processor->component->setActive(false);
  }
  std::fprintf(stderr,
               "[SphereVST3] processing stopped (instance kept alive)\n");
}

extern "C" void sphere_daux_vst3_destroy(SphereDauxVst3Processor *processor) {
  if (!processor)
    return;
  std::fprintf(stderr, "[SphereVST3] destroying processor handle=0x%p\n",
               static_cast<void *>(processor));
#if defined(_WIN32)
  // Mark invalid BEFORE zeroing GWLP_USERDATA so the window proc can check
  // this flag even if it races between the zero and a pending message.
  processor->processor_valid.store(false, std::memory_order_seq_cst);
  // Zero the back-pointer so any still-pending WM_TIMER/WM_PAINT cannot
  // dereference the struct after it is freed.
  if (processor->editor_hwnd && IsWindow(processor->editor_hwnd)) {
    SetWindowLongPtrW(processor->editor_hwnd, GWLP_USERDATA, 0);
  }
#elif defined(__APPLE__) || defined(__linux__)
  processor->processor_valid.store(false, std::memory_order_seq_cst);
#endif
  processor->shutdown();
  delete processor;
}

extern "C" int
sphere_daux_vst3_process_stereo_sample(SphereDauxVst3Processor *processor,
                                       float in_l, float in_r, float *out_l,
                                       float *out_r) {
  if (!processor || !processor->processor || !out_l || !out_r)
    return 0;

  processor->prepare_process_io(nullptr, 0);

  // Fill input, clear output
  processor->input_l = in_l;
  processor->input_r = in_r;
  processor->output_l = 0.f;
  processor->output_r = 0.f;
  processor->input_bus.numChannels = 2;
  processor->input_bus.channelBuffers32 = processor->input_channels;
  processor->output_bus.numChannels = 2;
  processor->output_bus.channelBuffers32 = processor->output_channels;
  processor->output_bus.silenceFlags = 0;
  processor->process_data.numSamples = 1;
  processor->process_data.numInputs =
      processor->audio_input_bus_count > 0 ? 1 : 0;
  processor->process_data.inputs =
      processor->audio_input_bus_count > 0 ? &processor->input_bus : nullptr;
  processor->process_data.numOutputs =
      processor->audio_output_bus_count > 0 ? 1 : 0;
  processor->process_data.outputs =
      processor->audio_output_bus_count > 0 ? &processor->output_bus : nullptr;

  const auto result = processor->processor->process(processor->process_data);

  processor->last_input_peak = std::max(std::abs(static_cast<double>(in_l)),
                                        std::abs(static_cast<double>(in_r)));
  processor->last_output_peak =
      std::max(std::abs(static_cast<double>(processor->output_l)),
               std::abs(static_cast<double>(processor->output_r)));
  processor->last_difference_peak =
      std::max(std::abs(static_cast<double>(processor->output_l - in_l)),
               std::abs(static_cast<double>(processor->output_r - in_r)));

  // First-process debug log (fires once, outside the hot path thereafter)
  if (!processor->first_process_done) {
    processor->first_process_done = true;
    std::fprintf(stderr,
                 "[SphereVST3] first process %s inputPeakL=%.6f "
                 "outputPeakL=%.6f diffPeak=%.6f\n",
                 result == Steinberg::kResultOk ? "ok" : "failed",
                 processor->last_input_peak, processor->last_output_peak,
                 processor->last_difference_peak);
  }

  if (result != Steinberg::kResultOk)
    return 0;

  processor->process_count += 1;

  *out_l = processor->output_l;
  *out_r = processor->output_r;
  return 1;
}

extern "C" int
sphere_daux_vst3_process_stereo_block(SphereDauxVst3Processor *processor,
                                      const float *in_l, const float *in_r,
                                      float *out_l, float *out_r, int frames) {
  return sphere_daux_vst3_process_stereo_block_with_midi(
      processor, in_l, in_r, out_l, out_r, frames, nullptr, 0);
}

extern "C" int sphere_daux_vst3_process_stereo_block_with_midi(
    SphereDauxVst3Processor *processor, const float *in_l, const float *in_r,
    float *out_l, float *out_r, int frames,
    const SphereDauxVst3MidiEvent *events, int event_count) {
  if (!processor || !processor->processor || !in_l || !in_r || !out_l ||
      !out_r || frames <= 0) {
    return 0;
  }

  processor->prepare_process_io(events, event_count);

  float *input_channels[2] = {
      const_cast<float *>(in_l),
      const_cast<float *>(in_r),
  };
  processor->input_bus.numChannels = 2;
  processor->input_bus.channelBuffers32 = input_channels;
  processor->process_data.numSamples = frames;
  processor->process_data.numInputs =
      processor->audio_input_bus_count > 0 ? 1 : 0;
  processor->process_data.inputs =
      processor->audio_input_bus_count > 0 ? &processor->input_bus : nullptr;

  static thread_local std::array<
      std::array<float, SphereDauxVst3Processor::kMaxProcessFrames>,
      SphereDauxVst3Processor::kMaxBridgeChannels>
      planes{};
  static thread_local std::array<Steinberg::Vst::AudioBusBuffers,
                                 SphereDauxVst3Processor::kMaxBridgeBuses>
      output_buses{};
  static thread_local std::array<
      std::array<float *, SphereDauxVst3Processor::kMaxBridgeChannels>,
      SphereDauxVst3Processor::kMaxBridgeBuses>
      output_channel_ptrs{};
  static thread_local std::array<float,
                                 SphereDauxVst3Processor::kMaxProcessFrames>
      discard_plane{};
  const int n = std::min(frames, SphereDauxVst3Processor::kMaxProcessFrames);
  int flat_channel = 0;
  const int buses_used = processor->output_bus_count_for_process();
  for (int bus = 0; bus < buses_used; ++bus) {
    const int bus_channels = processor->output_bus_channels_for_process(bus);
    output_buses[bus].numChannels = bus_channels;
    output_buses[bus].silenceFlags = 0;
    for (int ch = 0; ch < bus_channels; ++ch) {
      float *plane = flat_channel < SphereDauxVst3Processor::kMaxBridgeChannels
                         ? planes[flat_channel].data()
                         : discard_plane.data();
      std::fill(plane, plane + n, 0.0f);
      output_channel_ptrs[bus][ch] = plane;
      if (flat_channel < SphereDauxVst3Processor::kMaxBridgeChannels) {
        ++flat_channel;
      }
    }
    output_buses[bus].channelBuffers32 =
        bus_channels > 0 ? output_channel_ptrs[bus].data() : nullptr;
  }
  processor->process_data.numSamples = n;
  processor->process_data.numOutputs = buses_used;
  processor->process_data.outputs =
      buses_used > 0 ? output_buses.data() : nullptr;

  const auto result = processor->processor->process(processor->process_data);

  if (daux_vst3_midi_debug() && event_count > 0) {
    std::fprintf(stderr, "[vst3-process] frames=%d midi_events=%d result=%d\n",
                 frames, event_count, (int)result);
  }

  double input_peak_l = 0.0;
  double input_peak_r = 0.0;
  double output_peak_l = 0.0;
  double output_peak_r = 0.0;
  double diff_peak = 0.0;
  std::array<Vst3BusAudioStats, SphereDauxVst3Processor::kMaxBridgeBuses>
      bus_stats{};
  for (int bus = 0; bus < buses_used; ++bus) {
    bus_stats[bus] =
        SphereDauxVst3Processor::compute_bus_stats(output_buses[bus], n);
  }
  for (int i = 0; i < frames; ++i) {
    if (i < n) {
      out_l[i] = 0.0f;
      out_r[i] = 0.0f;
    }
    input_peak_l =
        std::max(input_peak_l, std::abs(static_cast<double>(in_l[i])));
    input_peak_r =
        std::max(input_peak_r, std::abs(static_cast<double>(in_r[i])));
  }
  for (int bus = 0; bus < buses_used; ++bus) {
    const int bus_channels = output_buses[bus].numChannels;
    if (bus_channels <= 0 || output_buses[bus].channelBuffers32 == nullptr)
      continue;
    for (int i = 0; i < n; ++i) {
      if (bus_channels == 1 && output_buses[bus].channelBuffers32[0]) {
        const float sample = output_buses[bus].channelBuffers32[0][i];
        out_l[i] += sample;
        out_r[i] += sample;
      } else {
        for (int ch = 0; ch < bus_channels; ++ch) {
          const float *src = output_buses[bus].channelBuffers32[ch];
          if (!src)
            continue;
          if ((ch & 1) == 0) {
            out_l[i] += src[i];
          } else {
            out_r[i] += src[i];
          }
        }
      }
    }
  }
  for (int i = 0; i < n; ++i) {
    output_peak_l =
        std::max(output_peak_l, std::abs(static_cast<double>(out_l[i])));
    output_peak_r =
        std::max(output_peak_r, std::abs(static_cast<double>(out_r[i])));
    diff_peak =
        std::max(diff_peak, std::abs(static_cast<double>(out_l[i] - in_l[i])));
    diff_peak =
        std::max(diff_peak, std::abs(static_cast<double>(out_r[i] - in_r[i])));
  }
  processor->last_input_peak = std::max(input_peak_l, input_peak_r);
  processor->last_output_peak = std::max(output_peak_l, output_peak_r);
  processor->last_difference_peak = diff_peak;
  processor->log_process_audio_out_once(n, buses_used, output_buses.data(),
                                        bus_stats.data());

  if (!processor->first_process_done) {
    processor->first_process_done = true;
    std::fprintf(stderr,
                 "[SphereVST3] first process %s frames=%d inputPeakL=%.6f "
                 "outputPeakL=%.6f diffPeak=%.6f\n",
                 result == Steinberg::kResultOk ? "ok" : "failed", frames,
                 input_peak_l, output_peak_l, processor->last_difference_peak);
  }

  processor->process_data.numSamples = 1;
  processor->process_data.numInputs =
      processor->audio_input_bus_count > 0 ? 1 : 0;
  processor->process_data.numOutputs =
      processor->output_bus_count_for_process();
  processor->process_data.inputEvents = nullptr;
  processor->input_bus.channelBuffers32 = processor->input_channels;
  processor->output_bus.channelBuffers32 = processor->output_channels;
  processor->process_data.inputs =
      processor->audio_input_bus_count > 0 ? &processor->input_bus : nullptr;
  processor->process_data.outputs = nullptr;

  if (result != Steinberg::kResultOk)
    return 0;
  processor->process_count += 1;
  return 1;
}

extern "C" int sphere_daux_vst3_process_main_output_block_with_midi(
    SphereDauxVst3Processor *processor, const float *in_l, const float *in_r,
    float *out_interleaved, int frames, int output_channels,
    const SphereDauxVst3MidiEvent *events, int event_count) {
  if (!processor || !processor->processor || !in_l || !in_r ||
      !out_interleaved || frames <= 0 || output_channels <= 0) {
    return 0;
  }

  const int channels =
      std::max(1, std::min({output_channels,
                            processor->bridge_audio_output_channel_count > 0
                                ? processor->bridge_audio_output_channel_count
                                : 2,
                            SphereDauxVst3Processor::kMaxBridgeChannels}));

  processor->prepare_process_io(events, event_count);

  float *input_channels[2] = {
      const_cast<float *>(in_l),
      const_cast<float *>(in_r),
  };
  processor->input_bus.numChannels = 2;
  processor->input_bus.channelBuffers32 = input_channels;
  processor->process_data.numSamples = frames;
  processor->process_data.numInputs =
      processor->audio_input_bus_count > 0 ? 1 : 0;
  processor->process_data.inputs =
      processor->audio_input_bus_count > 0 ? &processor->input_bus : nullptr;

  // VST3 wants per-channel contiguous buffers. The bridge wire format is
  // interleaved, so process into fixed stack channel planes, then interleave.
  static thread_local std::array<
      std::array<float, SphereDauxVst3Processor::kMaxProcessFrames>,
      SphereDauxVst3Processor::kMaxBridgeChannels>
      planes{};
  static thread_local std::array<Steinberg::Vst::AudioBusBuffers,
                                 SphereDauxVst3Processor::kMaxBridgeBuses>
      output_buses{};
  static thread_local std::array<
      std::array<float *, SphereDauxVst3Processor::kMaxBridgeChannels>,
      SphereDauxVst3Processor::kMaxBridgeBuses>
      output_channel_ptrs{};
  // Channels past the read window still need valid buffers: a spec-conformant
  // instrument may render silence on every bus when `numOutputs` is short of
  // its active output-bus count. Provide a buffer for every active bus: real
  // planes for the first `kMaxBridgeChannels` flat channels we downmix, and a
  // shared throwaway plane (write-only, discarded) for the rest. Set
  // `numOutputs` to the full active-bus count.
  static thread_local std::array<float,
                                 SphereDauxVst3Processor::kMaxProcessFrames>
      discard_plane{};
  const int n = std::min(frames, SphereDauxVst3Processor::kMaxProcessFrames);
  int read_channels = 0; // channels captured in `planes` (what we downmix)
  const int output_buses_available = processor->output_bus_count_for_process();
  for (int bus = 0; bus < output_buses_available; ++bus) {
    const int bus_channels = processor->output_bus_channels_for_process(bus);
    output_buses[bus].numChannels = bus_channels;
    output_buses[bus].silenceFlags = 0;
    for (int ch = 0; ch < bus_channels; ++ch) {
      if (read_channels < SphereDauxVst3Processor::kMaxBridgeChannels) {
        output_channel_ptrs[bus][ch] = planes[read_channels].data();
        std::fill(planes[read_channels].begin(),
                  planes[read_channels].begin() + n, 0.0f);
        ++read_channels;
      } else {
        output_channel_ptrs[bus][ch] = discard_plane.data();
        std::fill(discard_plane.begin(), discard_plane.begin() + n, 0.0f);
      }
    }
    output_buses[bus].channelBuffers32 =
        bus_channels > 0 ? output_channel_ptrs[bus].data() : nullptr;
  }
  const int buses_used = output_buses_available;
  const int produced_channels = std::max(1, read_channels);
  processor->process_data.numSamples = n;
  processor->process_data.numOutputs = buses_used > 0 ? buses_used : 0;
  processor->process_data.outputs =
      buses_used > 0 ? output_buses.data() : nullptr;

  const auto result = processor->processor->process(processor->process_data);

  double input_peak_l = 0.0;
  double input_peak_r = 0.0;
  double output_peak = 0.0;
  double diff_peak = 0.0;
  std::array<Vst3BusAudioStats, SphereDauxVst3Processor::kMaxBridgeBuses>
      bus_stats{};
  for (int bus = 0; bus < buses_used; ++bus) {
    bus_stats[bus] =
        SphereDauxVst3Processor::compute_bus_stats(output_buses[bus], n);
  }
  for (int i = 0; i < n; ++i) {
    input_peak_l =
        std::max(input_peak_l, std::abs(static_cast<double>(in_l[i])));
    input_peak_r =
        std::max(input_peak_r, std::abs(static_cast<double>(in_r[i])));
    for (int ch = 0; ch < std::min(produced_channels, channels); ++ch) {
      const float sample = planes[ch][i];
      out_interleaved[i * channels + ch] = sample;
      output_peak =
          std::max(output_peak, std::abs(static_cast<double>(sample)));
      if (ch == 0)
        diff_peak = std::max(diff_peak,
                             std::abs(static_cast<double>(sample - in_l[i])));
      if (ch == 1)
        diff_peak = std::max(diff_peak,
                             std::abs(static_cast<double>(sample - in_r[i])));
    }
  }

  processor->last_input_peak = std::max(input_peak_l, input_peak_r);
  processor->last_output_peak = output_peak;
  processor->last_difference_peak = diff_peak;
  processor->log_process_audio_out_once(n, buses_used, output_buses.data(),
                                        bus_stats.data());

  if (!processor->first_process_done) {
    processor->first_process_done = true;
    std::fprintf(stderr,
                 "[SphereVST3] first process %s frames=%d channels=%d "
                 "outputPeak=%.6f diffPeak=%.6f\n",
                 result == Steinberg::kResultOk ? "ok" : "failed", n,
                 produced_channels, processor->last_output_peak,
                 processor->last_difference_peak);
  }

  processor->process_data.numSamples = 1;
  processor->process_data.numInputs =
      processor->audio_input_bus_count > 0 ? 1 : 0;
  processor->process_data.numOutputs =
      processor->output_bus_count_for_process();
  processor->process_data.inputEvents = nullptr;
  processor->input_bus.channelBuffers32 = processor->input_channels;
  processor->output_bus.numChannels = 2;
  processor->output_bus.channelBuffers32 = processor->output_channels;
  processor->process_data.inputs =
      processor->audio_input_bus_count > 0 ? &processor->input_bus : nullptr;
  processor->process_data.outputs = nullptr;

  if (result != Steinberg::kResultOk)
    return 0;
  processor->process_count += 1;
  return produced_channels;
}

extern "C" int
sphere_daux_vst3_event_input_bus_count(SphereDauxVst3Processor *processor) {
  if (!processor)
    return 0;
  return processor->event_input_bus_count;
}

extern "C" int
sphere_daux_vst3_audio_input_bus_count(SphereDauxVst3Processor *processor) {
  if (!processor)
    return 0;
  return processor->audio_input_bus_count;
}

extern "C" int
sphere_daux_vst3_audio_output_bus_count(SphereDauxVst3Processor *processor) {
  if (!processor)
    return 0;
  return processor->audio_output_bus_count;
}

extern "C" int sphere_daux_vst3_main_audio_input_channel_count(
    SphereDauxVst3Processor *processor) {
  if (!processor)
    return 0;
  return processor->main_audio_input_channel_count;
}

extern "C" int sphere_daux_vst3_main_audio_output_channel_count(
    SphereDauxVst3Processor *processor) {
  if (!processor)
    return 0;
  return processor->bridge_audio_output_channel_count;
}

/// Per-bus output channel counts in the exact order the bridge flattens them
/// into the interleaved block (bus-by-bus: bus0 channels, then bus1 channels…).
/// Writes up to `max_count` entries into `out_counts` and returns the number
/// written. The host uses this to reconstruct real plugin output-bus boundaries
/// (one mixer strip per bus) instead of assuming every channel pair is a stereo
/// bus — a mono bus must become its own stereo strip, never paired with the
/// next bus.
extern "C" int
sphere_daux_vst3_output_bus_channel_counts(SphereDauxVst3Processor *processor,
                                           int *out_counts, int max_count) {
  if (!processor || !out_counts || max_count <= 0)
    return 0;
  const int buses = processor->output_bus_count_for_process();
  const int n = std::min(buses, max_count);
  for (int i = 0; i < n; ++i) {
    out_counts[i] = processor->output_bus_channels_for_process(i);
  }
  return n;
}

/// Enqueue a normalized parameter change for delivery on the next process call.
/// Safe to call from any thread (audio thread or UI thread).
extern "C" void sphere_daux_vst3_set_param(SphereDauxVst3Processor *processor,
                                           unsigned int param_id,
                                           double value) {
  if (!processor)
    return;
  processor->enqueue_param(static_cast<Steinberg::Vst::ParamID>(param_id),
                           static_cast<Steinberg::Vst::ParamValue>(value));
}

/// Update the transport ProcessContext delivered to the plugin on the next
/// process() call. Replaces the old hardcoded 120 BPM / 4-4 / always-playing
/// stub with real engine transport. Called once per block on the same thread
/// that drives process() (audio callback in-process, producer thread in the
/// bridge host), so it never races process().
///
/// `tempo`/`time_sig_*` fall back to sane defaults if non-positive; valid flags
/// are set only for the fields actually provided. `playing`/`recording` are
/// 0/1. `ppq` is the project quarter-note position; `bar_ppq` the quarter-note
/// position of the current bar start.
extern "C" void sphere_daux_vst3_set_process_context(
    SphereDauxVst3Processor *processor, double tempo, int time_sig_num,
    int time_sig_den, long long project_time_samples, double ppq,
    double bar_ppq, int playing, int recording) {
  if (!processor)
    return;
  auto &ctx = processor->process_context;
  ctx.tempo = tempo > 0.0 ? tempo : 120.0;
  ctx.timeSigNumerator = time_sig_num > 0 ? time_sig_num : 4;
  ctx.timeSigDenominator = time_sig_den > 0 ? time_sig_den : 4;
  ctx.projectTimeSamples =
      static_cast<Steinberg::Vst::TSamples>(project_time_samples);
  ctx.continousTimeSamples =
      static_cast<Steinberg::Vst::TSamples>(project_time_samples);
  ctx.projectTimeMusic = static_cast<Steinberg::Vst::TQuarterNotes>(ppq);
  ctx.barPositionMusic = static_cast<Steinberg::Vst::TQuarterNotes>(bar_ppq);
  Steinberg::uint32 state =
      Steinberg::Vst::ProcessContext::kTempoValid |
      Steinberg::Vst::ProcessContext::kTimeSigValid |
      Steinberg::Vst::ProcessContext::kProjectTimeMusicValid |
      Steinberg::Vst::ProcessContext::kBarPositionValid |
      Steinberg::Vst::ProcessContext::kContTimeValid;
  if (playing)
    state |= Steinberg::Vst::ProcessContext::kPlaying;
  if (recording)
    state |= Steinberg::Vst::ProcessContext::kRecording;
  ctx.state = state;
}

extern "C" unsigned long long
sphere_daux_vst3_open_editor(SphereDauxVst3Processor *processor,
                             const char *window_id, const char *title,
                             int width, int height) {
  if (!processor || !processor->controller) {
    std::fprintf(stderr,
                 "[SphereVST3] editor open failed processor=%p controller=%p "
                 "exists=%d\n",
                 static_cast<void *>(processor),
                 processor ? static_cast<void *>(processor->controller.get())
                           : nullptr,
                 processor && processor->controller ? 1 : 0);
    return 0;
  }
#if defined(__APPLE__)
  return open_editor_mac(processor, window_id, title, width, height);
#elif defined(__linux__)
  return open_editor_linux(processor, window_id, title, width, height);
#elif !defined(_WIN32)
  (void)window_id;
  (void)title;
  (void)width;
  (void)height;
  set_last_error("DAUx VST3 editor: unsupported platform");
  return 0;
#else
  if (processor->editor_hwnd && IsWindow(processor->editor_hwnd)) {
    ShowWindow(processor->editor_hwnd, SW_SHOWNORMAL);
    UpdateWindow(processor->editor_hwnd);
    SetForegroundWindow(processor->editor_hwnd);
    if (processor->editor_attach_hwnd)
      SetFocus(processor->editor_attach_hwnd);
    std::fprintf(stderr,
                 "[SphereVST3] editor already open; focused existing shell "
                 "handle=%llu windowId=%s\n",
                 processor->editor_handle, processor->editor_window_id.c_str());
    return processor->editor_handle;

    // Parent shell still alive (was hidden after user-close or
    // programmatic-close). Always create a FRESH child HWND + fresh IPlugView —
    // never reuse the stale child HWND.  After IPlugView::removed() the child
    // HWND has no vendor content; re-attaching to it yields a white/blank
    // window (the root cause).
    std::fprintf(
        stderr,
        "[SphereVST3] editor reopen: creating fresh child HWND + IPlugView "
        "handle=%llu windowId=%s\n",
        processor->editor_handle, processor->editor_window_id.c_str());

    // Measure the current client area of the parent shell to match size.
    RECT rc{};
    GetClientRect(processor->editor_hwnd, &rc);
    const int w = (rc.right - rc.left > 0) ? (rc.right - rc.left)
                                           : (width > 0 ? width : 820);
    const int h = (rc.bottom - rc.top > 0) ? (rc.bottom - rc.top)
                                           : (height > 0 ? height : 560);

    // Create fresh child attach HWND.
    HWND child = CreateWindowExW(
        0, kDauxEditorContentClass, L"",
        WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | WS_CLIPCHILDREN, 0, 0, w, h,
        processor->editor_hwnd, nullptr, GetModuleHandleW(nullptr), nullptr);
    if (!child) {
      std::fprintf(stderr,
                   "[SphereVST3] editor reopen: CreateWindowExW child FAILED "
                   "handle=%llu\n",
                   processor->editor_handle);
      return 0;
    }
    std::fprintf(
        stderr,
        "[SphereVST3] editor reopen: fresh child HWND=0x%p handle=%llu\n",
        static_cast<void *>(child), processor->editor_handle);
    processor->editor_attach_hwnd = child;

    // Create fresh IPlugView.
    processor->editor_view = Steinberg::IPtr<Steinberg::IPlugView>::adopt(
        processor->controller->createView(Steinberg::Vst::ViewType::kEditor));
    std::fprintf(
        stderr, "[SphereVST3] editor reopen: createView %s handle=%llu\n",
        processor->editor_view ? "ok" : "FAILED", processor->editor_handle);
    if (!processor->editor_view) {
      DestroyWindow(child);
      processor->editor_attach_hwnd = nullptr;
      return 0;
    }
    if (processor->editor_view->isPlatformTypeSupported(
            Steinberg::kPlatformTypeHWND) != Steinberg::kResultTrue) {
      processor->editor_view = nullptr;
      DestroyWindow(child);
      processor->editor_attach_hwnd = nullptr;
      std::fprintf(
          stderr,
          "[SphereVST3] editor reopen: HWND not supported handle=%llu\n",
          processor->editor_handle);
      return 0;
    }

    // Set the frame BEFORE attached() — required by WebView/CEF editors.
    daux_editor_install_frame(processor);

    // Attach to fresh child HWND.
    const auto attach_res = processor->editor_view->attached(
        reinterpret_cast<void *>(child), Steinberg::kPlatformTypeHWND);
    std::fprintf(stderr,
                 "[SphereVST3] editor reopen: IPlugView::attached() result=%d "
                 "handle=%llu\n",
                 (int)attach_res, processor->editor_handle);
    if (attach_res != Steinberg::kResultTrue &&
        attach_res != Steinberg::kResultOk) {
      daux_editor_clear_frame(processor);
      processor->editor_view = nullptr;
      DestroyWindow(child);
      processor->editor_attach_hwnd = nullptr;
      return 0;
    }
    processor->editor_attached = true;

    // Update window title (latency/CPU may change between opens).
    if (title && *title) {
      SetWindowTextW(processor->editor_hwnd, widen_utf8(title).c_str());
      std::fprintf(stderr,
                   "[SphereVST3] editor reopen: title='%s' handle=%llu\n",
                   title, processor->editor_handle);
    }

    resize_editor_view(processor);
    ShowWindow(processor->editor_hwnd, SW_SHOWNORMAL);
    UpdateWindow(processor->editor_hwnd);
    SetForegroundWindow(processor->editor_hwnd);
    // HWND_TOP raises the editor within the normal z-order. HWND_TOPMOST would
    // pin it above every other application on the desktop, not just above
    // Futureboard — always-on-top is a user choice (see the `pinned` toggle in
    // editorplatform/windows/editor_windows_api.cpp), never the default.
    SetWindowPos(processor->editor_hwnd, HWND_TOP, 0, 0, 0, 0,
                 SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    std::fprintf(stderr,
                 "[SphereVST3] editor reopen: complete handle=%llu "
                 "mainHWND=0x%p attachHWND=0x%p\n",
                 processor->editor_handle,
                 static_cast<void *>(processor->editor_hwnd),
                 static_cast<void *>(child));
    return processor->editor_handle;
  }

  daux_ensure_thread_dpi_awareness();
  register_editor_window_classes();
  register_editor_parent_class();

  processor->editor_window_id = window_id ? window_id : "";
  processor->editor_title = title && *title ? title : "Plugin Editor";
  processor->editor_requested_width = width;
  processor->editor_requested_height = height;
  const std::string plugin_instance_id = processor->editor_window_id;
  const auto identity_colon = plugin_instance_id.find_last_of(':');
  const char *identity = identity_colon == std::string::npos
                             ? plugin_instance_id.c_str()
                             : plugin_instance_id.c_str() + identity_colon + 1;
  std::fprintf(stderr,
               "[SphereVST3] editor open request pluginInstanceId=%s "
               "windowId=%s controller=%p exists=%d\n",
               identity, processor->editor_window_id.c_str(),
               static_cast<void *>(processor->controller.get()),
               processor->controller ? 1 : 0);

  if (!daux_plugin_browser_runtime_prepare(processor)) {
    return 0;
  }

  processor->editor_view = Steinberg::IPtr<Steinberg::IPlugView>::adopt(
      processor->controller->createView(Steinberg::Vst::ViewType::kEditor));
  std::fprintf(stderr, "[PluginEditor] create view ok\n");
  std::fprintf(stderr,
               "[SphereVST3] IPlugView createView pluginInstanceId=%s ptr=%p "
               "exists=%d\n",
               identity, static_cast<void *>(processor->editor_view.get()),
               processor->editor_view ? 1 : 0);
  if (!processor->editor_view) {
    daux_plugin_browser_runtime_release(processor);
    if (daux_plugin_is_browser_based(processor->plugin_path)) {
      set_last_error("Browser/WebView-based plugin editor createView failed "
                     "(controller returned null view)");
    } else {
      set_last_error("controller did not create editor view");
    }
    return 0;
  }
  if (processor->editor_view->isPlatformTypeSupported(
          Steinberg::kPlatformTypeHWND) != Steinberg::kResultTrue) {
    processor->editor_view = nullptr;
    set_last_error("editor view does not support HWND");
    return 0;
  }

  Steinberg::ViewRect preferred{};
  int editor_width = width > 0 ? width : 820;
  int editor_height = height > 0 ? height : 560;
  const auto get_size_result = processor->editor_view->getSize(&preferred);
  std::fprintf(stderr,
               "[SphereVST3] IPlugView::getSize() result=%d rect=%d,%d,%d,%d "
               "pluginInstanceId=%s\n",
               (int)get_size_result, (int)preferred.left, (int)preferred.top,
               (int)preferred.right, (int)preferred.bottom, identity);
  if (get_size_result == Steinberg::kResultTrue) {
    const int preferred_width = preferred.right - preferred.left;
    const int preferred_height = preferred.bottom - preferred.top;
    if (preferred_width > 0)
      editor_width = preferred_width;
    if (preferred_height > 0)
      editor_height = preferred_height;
  }
  std::fprintf(stderr, "[PluginEditor] getSize rect=%d,%d,%d,%d\n",
               (int)preferred.left, (int)preferred.top, (int)preferred.right,
               (int)preferred.bottom);
  std::fprintf(stderr, "[PluginEditor] view_get_size=%dx%d client_size=%dx%d\n",
               editor_width, editor_height, editor_width, editor_height);

  RECT rect{0, 0, editor_width, editor_height};
  const DWORD outer_style = WS_OVERLAPPEDWINDOW;
  // No WS_EX_TOPMOST: a plug-in editor that outranks every other application on
  // the desktop cannot be worked behind. It is owned by the app window instead
  // (below), which floats it over Futureboard only.
  const DWORD outer_ex_style = 0;
  if (!AdjustWindowRectExForDpi(&rect, outer_style, FALSE, outer_ex_style,
                                96)) {
    AdjustWindowRectEx(&rect, outer_style, FALSE, outer_ex_style);
  }
  const auto wide_title = widen_utf8(title && *title ? title : "Plugin Editor");
  HWND hwnd =
      CreateWindowExW(outer_ex_style, kDauxEditorWindowClass,
                      wide_title.c_str(), WS_OVERLAPPEDWINDOW, CW_USEDEFAULT,
                      CW_USEDEFAULT, rect.right - rect.left,
                      rect.bottom - rect.top, daux_editor_owner_window(),
                      nullptr, GetModuleHandleW(nullptr), processor);
  if (!hwnd) {
    processor->editor_view = nullptr;
    set_last_error("CreateWindowExW failed for DAUx VST3 editor");
    return 0;
  }
  set_daux_dark_titlebar(hwnd);
  const UINT dpi = daux_hwnd_dpi(hwnd);
  daux_log_editor_dpi(hwnd, "created top");
  RECT outer_rect{0, 0, editor_width, editor_height};
  if (!AdjustWindowRectExForDpi(&outer_rect, outer_style, FALSE, outer_ex_style,
                                dpi)) {
    AdjustWindowRectEx(&outer_rect, outer_style, FALSE, outer_ex_style);
  }
  const int outer_w = outer_rect.right - outer_rect.left;
  const int outer_h = outer_rect.bottom - outer_rect.top;
  std::fprintf(stderr, "[PluginEditor] created top hwnd=0x%p dpi=%u\n",
               static_cast<void *>(hwnd), dpi);
  std::fprintf(stderr, "[PluginEditor] outer_size=%dx%d\n", outer_w, outer_h);
  SetWindowPos(hwnd, HWND_TOP, 0, 0, outer_w, outer_h,
               SWP_NOMOVE | SWP_NOACTIVATE);

  HWND child =
      CreateWindowExW(0, kDauxEditorContentClass, L"",
                      WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | WS_CLIPCHILDREN,
                      0, 0, editor_width, editor_height, hwnd, nullptr,
                      GetModuleHandleW(nullptr), nullptr);
  if (!child) {
    DestroyWindow(hwnd);
    processor->editor_view = nullptr;
    set_last_error("CreateWindowExW failed for DAUx VST3 attach HWND");
    return 0;
  }

  processor->editor_hwnd = hwnd;
  processor->editor_attach_hwnd = child;
  processor->editor_handle = g_next_editor_handle.fetch_add(1);
  std::fprintf(stderr, "[PluginEditor] created child hwnd=0x%p client=%dx%d\n",
               static_cast<void *>(child), editor_width, editor_height);
  std::fprintf(stderr,
               "[SphereVST3] editor HWNDs pluginInstanceId=%s handle=%llu "
               "mainHWND=0x%p childHWND=0x%p\n",
               identity, processor->editor_handle, static_cast<void *>(hwnd),
               static_cast<void *>(child));

  // Set the frame BEFORE attached() — required by WebView/CEF editors.
  daux_editor_install_frame(processor);
  std::fprintf(stderr, "[PluginEditor] setFrame ok\n");

  const auto attach_result = processor->editor_view->attached(
      reinterpret_cast<void *>(child), Steinberg::kPlatformTypeHWND);
  std::fprintf(stderr, "[PluginEditor] attached result=0x%x\n",
               static_cast<unsigned>(attach_result));
  std::fprintf(stderr,
               "[SphereVST3] IPlugView::attached(child HWND) result=%d "
               "handle=%llu childHWND=0x%p pluginInstanceId=%s\n",
               (int)attach_result, processor->editor_handle,
               static_cast<void *>(child), identity);
  if (attach_result != Steinberg::kResultTrue &&
      attach_result != Steinberg::kResultOk) {
    set_last_error("IPlugView::attached(HWND) failed for DAUx VST3 editor");
    show_attach_failed_state(processor, "attached-failed");
    ShowWindow(hwnd, SW_SHOWNORMAL);
    UpdateWindow(hwnd);
    SetForegroundWindow(hwnd);
    return processor->editor_handle;
  }

  processor->editor_attached = true;
  {
    auto local = daux_local_view_rect(editor_width, editor_height);
    const auto on_size_res = processor->editor_view->onSize(&local);
    std::fprintf(stderr, "[PluginEditor] onSize result=0x%x\n",
                 static_cast<unsigned>(on_size_res));
    daux_verify_child_client_rect(child, editor_width, editor_height,
                                  "after attach");
  }
  ShowWindow(hwnd, SW_SHOWNORMAL);
  UpdateWindow(hwnd);
  SetForegroundWindow(hwnd);
  std::fprintf(stderr, "[PluginEditor] visible=true\n");
  std::fprintf(stderr,
               "[SphereVST3] editor opened same-instance handle=%llu "
               "windowId=%s mainHWND=0x%p attachHWND=0x%p\n",
               processor->editor_handle, processor->editor_window_id.c_str(),
               static_cast<void *>(hwnd), static_cast<void *>(child));
  return processor->editor_handle;
#endif
}

extern "C" void
sphere_daux_vst3_close_editor(SphereDauxVst3Processor *processor) {
  if (!processor)
    return;
#if defined(_WIN32)
  // Detach IPlugView and destroy the native editor shell.
  // Processor and controller are kept alive — only insert removal may destroy
  // them.
  if (processor->editor_hwnd && IsWindow(processor->editor_hwnd)) {
    const unsigned long long handle = processor->editor_handle;
    const std::string window_id = processor->editor_window_id;
    processor->close_editor_window();
    std::fprintf(
        stderr,
        "[SphereVST3] editor closed (programmatic) handle=%llu windowId=%s\n",
        handle, window_id.c_str());
  }
#elif defined(__APPLE__)
  close_editor_mac(processor);
#elif defined(__linux__)
  close_editor_linux(processor);
#endif
}

extern "C" int
sphere_daux_vst3_focus_editor(SphereDauxVst3Processor *processor) {
  if (!processor)
    return 0;
#if defined(_WIN32)
  if (processor->editor_hwnd && IsWindow(processor->editor_hwnd)) {
    ShowWindow(processor->editor_hwnd, SW_SHOWNORMAL);
    SetForegroundWindow(processor->editor_hwnd);
    if (processor->editor_attach_hwnd)
      SetFocus(processor->editor_attach_hwnd);
    return 1;
  }
#elif defined(__APPLE__)
  return focus_editor_mac(processor);
#elif defined(__linux__)
  return focus_editor_linux(processor);
#endif
  return 0;
}

// ── GPUI-embedded editor C ABI ───────────────────────────────────────────────
// These attach the EXISTING runtime instance's editor view (built from
// processor->controller) into a GPUI-provided parent HWND. They never create a
// new component/controller — GUI parameter edits flow through the same
// ComponentHandlerImpl that feeds the realtime processor.

extern "C" unsigned long long
sphere_daux_vst3_embed_editor(SphereDauxVst3Processor *processor,
                              unsigned long long parent_hwnd, int x, int y,
                              int width, int height) {
#ifdef _WIN32
  // Phase 4: ensure COM (STA) is live on the editor thread before any
  // IPlugView call. Some VST3 editors — notably UAD Native / Chromium-backed
  // hosts — never finish initializing their WebView without an STA on the
  // thread that owns the parent HWND. Idempotent / no-op when already
  // initialized.
  daux_embed_ensure_com_initialized();
  daux_ensure_thread_dpi_awareness();

  if (!processor || !processor->controller) {
    std::fprintf(
        stderr,
        "[vst3-editor] attach failed error=processor/controller missing\n");
    set_last_error("embed editor: processor/controller missing");
    return 0;
  }
  HWND parent =
      reinterpret_cast<HWND>(static_cast<std::uintptr_t>(parent_hwnd));
  if (!parent || !IsWindow(parent) || width <= 0 || height <= 0) {
    std::fprintf(stderr,
                 "[vst3-editor] attach failed error=invalid parent HWND or "
                 "region parent=0x%p w=%d h=%d\n",
                 static_cast<void *>(parent), width, height);
    set_last_error("embed editor: invalid parent HWND or region");
    return 0;
  }
  std::fprintf(stderr, "[plugin-view] sdk_reference=editorhost\n");
  std::fprintf(stderr,
               "[process] role=editor_host mode=in_process pid=%lu tid=%lu\n",
               GetCurrentProcessId(), GetCurrentThreadId());
  std::fprintf(stderr, "[plugin-view] ui_thread_id=%lu\n",
               GetCurrentThreadId());
  std::fprintf(stderr, "[plugin-view] platform_type=HWND\n");
  std::fprintf(stderr,
               "[vst3-editor] attach begin parent=0x%p platform=HWND "
               "region=(%d,%d,%d,%d) tid=%lu\n",
               static_cast<void *>(parent), x, y, width, height,
               GetCurrentThreadId());
  std::fprintf(stderr, "[VST3Editor] owner_hwnd=0x%p\n",
               static_cast<void *>(parent));

  // Reuse: if this instance already has an embedded editor attached, just
  // re-sync geometry and return the existing handle — never re-create.
  if (processor->embed_mode && processor->editor_attached &&
      processor->editor_embed_top_hwnd &&
      IsWindow(processor->editor_embed_top_hwnd) &&
      processor->editor_attach_hwnd &&
      IsWindow(processor->editor_attach_hwnd)) {
    processor->editor_parent_hwnd = parent;
    if (processor->embed_host_kind == 2) {
      daux_editor_apply_owner(&processor->editor_window, parent);
      daux_editor_show_and_focus(&processor->editor_window);
    }
    processor->embed_geometry_valid =
        false; // force re-apply against new parent
    daux_embed_sync_geometry(processor, x, y, width, height,
                             daux_embed_debug());
    std::fprintf(stderr,
                 "[SphereVST3] embed editor reuse handle=%llu (existing "
                 "instance/view)\n",
                 processor->editor_handle);
    return processor->editor_handle;
  }

  const int kind = daux_editor_resolve_host_kind();
  register_editor_window_classes();
  daux_log_editor_dpi(parent, "attach parent");

  std::fprintf(stderr, "[vst3-editor] create_tid=%lu\n", GetCurrentThreadId());
  // ── Phase A: open the editor shell with the "Loading Plugin <name>" overlay
  // BEFORE createView()/attached(). A slow or hanging plug-in (browser/CEF
  // editors block in attached(); some instruments stall in createView()) must
  // still show the editor window with a loading state instead of nothing. The
  // shell opens at the host-requested region size and is resized to the plug-in
  // preferred size once getSize() is known (Phase C). The content overlay stays
  // until the plug-in's IPlugView attaches and its child HWNDs cover it.
  int editor_w = width > 0 ? width : 640;
  int editor_h = height > 0 ? height : 480;

  // Real plug-in display name for the titlebar + content "Loading Plugin"
  // overlay (set via sphere_daux_vst3_set_editor_title before this call).
  // Falls back to "Unknown Plugin". Kept in a local so the wide buffers outlive
  // daux_editor_create_window.
  const std::wstring editor_title_w =
      processor->editor_title.empty()
          ? std::wstring(L"Unknown Plugin")
          : widen_utf8(processor->editor_title.c_str());
  DauxEditorWindowConfig window_config{};
  window_config.owner_hwnd = parent;
  window_config.title = editor_title_w.c_str();
  window_config.plugin_name = editor_title_w.c_str();
  window_config.host_kind = kind;
  window_config.x = x;
  window_config.y = y;
  window_config.content_width = editor_w;
  window_config.content_height = editor_h;
  window_config.pin_default =
      daux_editor_env_truthy("FUTUREBOARD_EDITOR_PIN_DEFAULT");
  window_config.callbacks = daux_editor_window_callbacks(processor);
  // Reclaim any stale shell left over from a previous failed open (we keep the
  // shell on failure to show an error, so a re-open must reclaim it before
  // overwriting editor_window). DestroyWindow is only valid on the creating
  // thread — true on the default main-thread editor path. In the opt-in
  // worker-thread mode the re-open may run elsewhere, so we skip the destroy
  // there (the stale window is reclaimed by close_editor_window at teardown)
  // rather than issue an invalid cross-thread DestroyWindow.
  if (daux_editor_is_window_valid(&processor->editor_window)) {
    HWND stale = reinterpret_cast<HWND>(processor->editor_window.shell_hwnd);
    if (stale &&
        GetWindowThreadProcessId(stale, nullptr) == GetCurrentThreadId()) {
      daux_editor_destroy_window(&processor->editor_window);
    }
  }
  processor->editor_window = DauxEditorWindow{};
  std::fprintf(stderr,
               "[editor-open] 10 shell_create_begin mode=%s size=%dx%d tid=%lu\n",
               daux_editor_selected_mode_label(kind), editor_w, editor_h,
               GetCurrentThreadId());
  if (!daux_editor_create_window(&window_config, &processor->editor_window)) {
    std::fprintf(stderr,
                 "[editor-open] FAILED stage=shell_create reason=create_window_returned_false\n");
    set_last_error("embed editor: native editor window creation failed");
    return 0;
  }
  std::fprintf(stderr, "[PluginEditorLifecycle] shell created mode=%s\n",
               daux_editor_selected_mode_label(kind));
  HWND top = reinterpret_cast<HWND>(processor->editor_window.shell_hwnd);
  HWND content = reinterpret_cast<HWND>(processor->editor_window.content_hwnd);
  if (!top || !content) {
    daux_editor_destroy_window(&processor->editor_window);
    set_last_error("embed editor: native editor window returned invalid HWNDs");
    return 0;
  }
  processor->embed_user_closed.store(false, std::memory_order_release);
  const int content_y_off = processor->editor_window.titlebar_height;
  const HWND content_parent = GetParent(content);
  const bool content_is_child = content_parent == top;
  std::fprintf(stderr, "[plugin-view] selected_host_mode=%s\n",
               daux_editor_selected_mode_label(kind));
  std::fprintf(stderr, "[plugin-view] top_hwnd=0x%p\n",
               static_cast<void *>(top));
  std::fprintf(stderr, "[plugin-view] content_hwnd=0x%p\n",
               static_cast<void *>(content));
  std::fprintf(stderr, "[plugin-view] content_is_child=%s\n",
               content_is_child ? "true" : "false");
  std::fprintf(stderr, "[plugin-view] content_parent=0x%p\n",
               static_cast<void *>(content_parent));
  std::fprintf(stderr, "[NativeEditorShell] dpi=%u\n", daux_hwnd_dpi(top));
  std::fprintf(stderr, "[NativeEditorShell] titlebar_h=%d\n", content_y_off);
  if (content == top || !content_is_child) {
    std::fprintf(stderr,
                 "[plugin-view] ERROR invalid HWND hierarchy top=0x%p "
                 "content=0x%p parent=0x%p\n",
                 static_cast<void *>(top), static_cast<void *>(content),
                 static_cast<void *>(content_parent));
    daux_editor_destroy_window(&processor->editor_window);
    set_last_error("embed editor: content HWND must be a child and must differ "
                   "from top HWND");
    return 0;
  }
  std::fprintf(stderr, "[PluginEditor] created top hwnd=0x%p dpi=%u\n",
               static_cast<void *>(top), daux_hwnd_dpi(top));
  std::fprintf(stderr,
               "[editor-open] 11 shell_create_ok shell_hwnd=0x%p content_hwnd=0x%p "
               "titlebar_h=%d\n",
               static_cast<void *>(top), static_cast<void *>(content),
               content_y_off);
  daux_log_hwnd_state("created", top, content);

  // Publish the host HWNDs now so the loading shell is addressable and the
  // Phase B/D failure paths can show an error in it. The IPlugFrame is
  // installed in Phase C (it needs the view).
  processor->editor_embed_top_hwnd = top;
  processor->editor_attach_hwnd = content;
  processor->editor_parent_hwnd = parent;
  processor->embed_host_kind = kind;
  processor->embed_mode = true;
  // Not attached yet: the content WndProc paints the "Loading Plugin <name>"
  // overlay until IPlugView::attached() completes (Phase D). The forced
  // UpdateWindow(content) below renders it before any blocking plug-in call.
  processor->editor_attached = false;
  processor->embed_host_x = x;
  processor->embed_host_y = y;
  processor->embed_host_w = width;
  processor->embed_host_h = height;

  // Show + force the loading overlay to paint synchronously, BEFORE the
  // potentially-blocking createView()/attached() below. Browser/WebView editors
  // also need a visible window to composite into during attached().
  ShowWindow(top, kind == 2 ? SW_SHOWNORMAL : SW_SHOWNA);
  ShowWindow(content, SW_SHOW);
  UpdateWindow(content);
  UpdateWindow(top);
  if (kind == 2) {
    // Bring the detached editor to the front at show time so it isn't buried
    // behind the DAW (initial bring-to-top on show is permitted).
    daux_editor_show_and_focus(&processor->editor_window);
  }
  std::fprintf(
      stderr,
      "[vst3-editor] loading_shell_shown top=0x%p content=0x%p name=%s\n",
      static_cast<void *>(top), static_cast<void *>(content),
      processor->editor_title.empty() ? "Unknown Plugin"
                                      : processor->editor_title.c_str());

  // ── Phase B: createView (may block / fail). The loading shell is already on
  // screen; on failure it stays and shows an error instead of vanishing.
  std::fprintf(stderr, "[PluginEditorLifecycle] editor view requested\n");
  std::fprintf(stderr,
               "[editor-open] 08 create_view_begin controller=0x%p tid=%lu\n",
               static_cast<void *>(processor->controller.get()),
               GetCurrentThreadId());
  if (!processor->editor_view) {
    if (!daux_plugin_browser_runtime_prepare(processor)) {
      set_last_error(
          "embed editor: plugin browser/web runtime failed to start");
      daux_editor_set_load_state(&processor->editor_window, true,
                                 L"Editor runtime failed to start");
      return 0;
    }
    processor->editor_view = Steinberg::IPtr<Steinberg::IPlugView>::adopt(
        processor->controller->createView(Steinberg::Vst::ViewType::kEditor));
  } else {
    std::fprintf(stderr,
                 "[vst3-editor] createView reuse prepared view ptr=0x%p\n",
                 static_cast<void *>(processor->editor_view.get()));
  }
  std::fprintf(stderr, "[PluginEditor] create view ok\n");
  std::fprintf(stderr, "[vst3-editor] createView result=%s ptr=0x%p\n",
               processor->editor_view ? "ok" : "null",
               static_cast<void *>(processor->editor_view.get()));
  std::fprintf(stderr, "[editor-open] 09 create_view_%s view=0x%p\n",
               processor->editor_view ? "ok" : "null",
               static_cast<void *>(processor->editor_view.get()));
  if (!processor->editor_view) {
    std::fprintf(stderr,
                 "[editor-open] FAILED stage=create_view reason=createView_returned_null\n");
    daux_plugin_browser_runtime_release(processor);
    if (daux_plugin_is_browser_based(processor->plugin_path)) {
      set_last_error("Browser/WebView-based plugin editor createView failed "
                     "(controller returned null view)");
    } else {
      set_last_error("embed editor: controller did not create view");
    }
    daux_editor_set_load_state(&processor->editor_window, true,
                               L"Editor view could not be created");
    return 0;
  }
  if (processor->editor_view->isPlatformTypeSupported(
          Steinberg::kPlatformTypeHWND) != Steinberg::kResultTrue) {
    processor->editor_view = nullptr;
    daux_plugin_browser_runtime_release(processor);
    std::fprintf(
        stderr,
        "[vst3-editor] attach failed error=view does not support HWND\n");
    set_last_error("embed editor: view does not support HWND");
    daux_editor_set_load_state(&processor->editor_window, true,
                               L"Editor is not supported on this platform");
    return 0;
  }

  // ── Phase C: getSize → preferred content size, resize the open shell to
  // match, then install the IPlugFrame before attached().
  //
  // Scale from the shell the view will actually live in, not the owner: the
  // detached editor can sit on a monitor with a different DPI than Studio's
  // main window, and a size queried at the wrong scale opens cropped or with
  // a gap until the next resize.
  daux_editor_set_content_scale(
      processor, (content && IsWindow(content)) ? content : parent,
      "before_getSize");

  Steinberg::ViewRect preferred{};
  const auto get_size_result = processor->editor_view->getSize(&preferred);
  int preferred_w = 0;
  int preferred_h = 0;
  if (get_size_result == Steinberg::kResultTrue ||
      get_size_result == Steinberg::kResultOk) {
    preferred_w = daux_view_rect_width(preferred);
    preferred_h = daux_view_rect_height(preferred);
  }
  // IPlugView::getSize is the content/client size source of truth. If it is
  // missing/invalid, use a safe non-tiny default instead of the provisional
  // wrapper size so heavyweight instruments do not open clipped.
  if (preferred_w > 0 && preferred_h > 0) {
    editor_w = preferred_w;
    editor_h = preferred_h;
  } else {
    editor_w = 900;
    editor_h = 600;
  }
  editor_w = std::clamp(editor_w, 160, 4096);
  editor_h = std::clamp(editor_h, 160, 4096);
  daux_constrain_content_size(processor, &editor_w, &editor_h);
  const UINT dpi = daux_hwnd_dpi(parent);
  daux_log_editor_dpi(parent, "embed");
  std::fprintf(stderr, "[PluginEditor] getSize rect=%d,%d,%d,%d\n",
               preferred.left, preferred.top, preferred.right,
               preferred.bottom);
  std::fprintf(stderr, "[PluginEditor] view_get_size=%dx%d client_size=%dx%d\n",
               editor_w, editor_h, editor_w, editor_h);
  std::fprintf(stderr,
               "[PluginEditor] view_size=%dx%d client_rect=%dx%d dpi=%u "
               "host_region=%dx%d\n",
               preferred_w, preferred_h, editor_w, editor_h, dpi, width,
               height);
  std::fprintf(stderr,
               "[vst3-editor] getSize result=0x%x width=%d height=%d "
               "rect=(%d,%d,%d,%d)\n",
               static_cast<unsigned>(get_size_result), editor_w, editor_h,
               preferred.left, preferred.top, preferred.right,
               preferred.bottom);
  std::fprintf(stderr, "[editor-size] plugin preferred size = %dx%d\n", editor_w,
               editor_h);
  std::fprintf(stderr, "[editor-size] titlebar height = %d\n", content_y_off);
  std::fprintf(stderr, "[editor-size] client rect = %dx%d\n", editor_w,
               editor_h);
  std::fprintf(stderr, "[NativeEditorShell] content_rect=(0,%d,%d,%d)\n",
               content_y_off, editor_w, editor_h);
  std::fprintf(stderr, "[PluginEditor] created child hwnd=0x%p client=%dx%d\n",
               static_cast<void *>(content), editor_w, editor_h);

  // Resize the already-open shell (top + content) to the plug-in's preferred
  // size now that getSize() is known: daux_embed_apply_content_size sizes the
  // top window (kind-aware), daux_resize_child_client sizes the content child
  // in place (SWP_NOMOVE preserves its titlebar offset).
  daux_embed_apply_content_size(processor, editor_w, editor_h,
                                "createView.getSize");
  daux_resize_child_client(content, editor_w, editor_h);
  {
    RECT top_rect{};
    RECT client_rect{};
    GetWindowRect(top, &top_rect);
    GetClientRect(content, &client_rect);
    std::fprintf(stderr, "[editor-size] shell outer target = %ldx%ld\n",
                 top_rect.right - top_rect.left, top_rect.bottom - top_rect.top);
    std::fprintf(stderr, "[editor-size] attach rect = %ld/%ld/%ld/%ld\n", 0L,
                 static_cast<long>(content_y_off), client_rect.right,
                 static_cast<long>(content_y_off) + client_rect.bottom);
  }
  daux_editor_install_frame(processor);
  std::fprintf(stderr, "[PluginEditor] setFrame ok\n");
  daux_log_hwnd_state("sized_before_attach", top, content);

  // Keep the loading overlay visible at the new size until attached().
  ShowWindow(top, kind == 2 ? SW_SHOWNORMAL : SW_SHOWNA);
  ShowWindow(content, SW_SHOW);
  InvalidateRect(content, nullptr, FALSE);
  UpdateWindow(content);
  UpdateWindow(top);
  std::fprintf(stderr,
               "[vst3-editor] shown_before_attach top=0x%p content=0x%p\n",
               static_cast<void *>(top), static_cast<void *>(content));

  // ── Phase D: settle the freshly-shown window (initial paint/size/activation)
  // before the plug-in attaches into it. Slow-UI editors (Qt/QML, VSTGUI,
  // Chromium/WebView) expect a live, pumping, realized window at attach time;
  // attaching into a shown-but-unpumped window can stall the attach or leave it
  // blank. Bounded — returns as soon as the queue goes quiet, never past the
  // cap.
  daux_editor_settle_pump_thread(250);

  const ULONGLONG attach_start_ms = GetTickCount64();
  auto attach_returned = std::make_shared<std::atomic<bool>>(false);
  auto attach_watchdog = attach_returned;
  // Detached watchdog: if attached() does not return, report every 5s (up to
  // 20s) AND enumerate the content child's windows. A Chromium/CEF editor that
  // got far enough to create its widget will show content_children>0 even while
  // attached() is still blocked — that distinguishes "stuck before creating any
  // UI" from "created UI but blocked waiting on a renderer/subprocess".
  std::thread([attach_watchdog, attach_start_ms, content, top] {
    for (int i = 0; i < 4; ++i) {
      std::this_thread::sleep_for(std::chrono::seconds(5));
      if (attach_watchdog->load(std::memory_order_acquire))
        return;
      int child_count = 0;
      EnumChildWindows(
          content,
          [](HWND, LPARAM lp) -> BOOL {
            (*reinterpret_cast<int *>(lp))++;
            return TRUE;
          },
          reinterpret_cast<LPARAM>(&child_count));
      std::fprintf(
          stderr,
          "[vst3-editor] attached still_blocked elapsed_ms=%llu content=0x%p "
          "top_visible=%d content_children=%d watchdog_tid=%lu\n",
          static_cast<unsigned long long>(GetTickCount64() - attach_start_ms),
          static_cast<void *>(content), IsWindowVisible(top) ? 1 : 0,
          child_count, GetCurrentThreadId());
    }
  }).detach();
  std::fprintf(
      stderr,
      "[vst3-editor] attached begin parent=0x%p attach_tid=%lu start_ms=%llu\n",
      static_cast<void *>(content), GetCurrentThreadId(),
      static_cast<unsigned long long>(attach_start_ms));
  std::fprintf(stderr,
               "[editor-open] 12 attach_begin parent_hwnd=0x%p attach_tid=%lu\n",
               static_cast<void *>(content), GetCurrentThreadId());
  const auto attach_res = processor->editor_view->attached(
      reinterpret_cast<void *>(content), Steinberg::kPlatformTypeHWND);
  attach_returned->store(true, std::memory_order_release);
  const ULONGLONG attach_end_ms = GetTickCount64();
  std::fprintf(stderr, "[PluginEditor] attached result=0x%x\n",
               static_cast<unsigned>(attach_res));
  std::fprintf(
      stderr,
      "[vst3-editor] attached result=0x%x content=0x%p attach_tid=%lu "
      "end_ms=%llu elapsed_ms=%llu\n",
      static_cast<unsigned>(attach_res), static_cast<void *>(content),
      GetCurrentThreadId(), static_cast<unsigned long long>(attach_end_ms),
      static_cast<unsigned long long>(attach_end_ms - attach_start_ms));
  if (attach_res != Steinberg::kResultTrue &&
      attach_res != Steinberg::kResultOk) {
    std::fprintf(stderr,
                 "[editor-open] 13 attach_fail result=0x%x content=0x%p\n",
                 static_cast<unsigned>(attach_res), static_cast<void *>(content));
    std::fprintf(stderr,
                 "[editor-open] FAILED stage=attach reason=attached_returned_error "
                 "result=0x%x\n",
                 static_cast<unsigned>(attach_res));
    daux_editor_clear_frame(processor);
    processor->editor_view = nullptr;
    processor->editor_attached = false;
    processor->embed_mode = false;
    processor->embed_geometry_valid = false;
    processor->embed_content_w = 0;
    processor->embed_content_h = 0;
    daux_plugin_browser_runtime_release(processor);
    // Keep the shell window (editor_window / editor_embed_top_hwnd /
    // editor_attach_hwnd) so the failure is visible instead of the editor
    // silently vanishing. The content WndProc now paints a failure state; the
    // window is reclaimed by close_editor_window() or a later re-open. The
    // host still receives handle=0 → EditorAttachFailed.
    if (daux_plugin_is_browser_based(processor->plugin_path)) {
      char msg[160];
      std::snprintf(msg, sizeof(msg),
                    "Browser/WebView-based plugin editor createView failed "
                    "(IPlugView::attached returned 0x%x)",
                    static_cast<unsigned>(attach_res));
      set_last_error(msg);
    } else {
      set_last_error("embed editor: IPlugView::attached(HWND) failed");
    }
    daux_editor_set_load_state(&processor->editor_window, true,
                               L"Editor failed to attach");
    return 0;
  }

  processor->editor_attached = true;
  std::fprintf(stderr, "[PluginEditorLifecycle] editor attached\n");
  std::fprintf(stderr,
               "[editor-open] 13 attach_ok result=0x%x content=0x%p\n",
               static_cast<unsigned>(attach_res), static_cast<void *>(content));
  processor->editor_embed_top_hwnd = top;
  processor->editor_attach_hwnd = content;
  processor->editor_parent_hwnd = parent;
  processor->editor_hwnd = nullptr; // embed mode: no daux-owned shell
  processor->embed_host_kind = kind;
  processor->embed_mode = true;
  processor->embed_geometry_valid = false;
  processor->editor_handle = g_next_editor_handle.fetch_add(1);
  // Editor resize contract (spec item 1): query canResize once per view, after
  // attach (some editors only report it reliably once attached). The flag is
  // forwarded to the main app via EditorAttached so the wrapper can lock its
  // size for fixed-size editors.
  daux_editor_view_resizable(processor);

  daux_editor_set_content_scale(processor, content, "after_attach");
  daux_embed_apply_content_size(processor, editor_w, editor_h, "attached");
  {
    Steinberg::ViewRect after_attach_size{};
    const auto after_get_size_result =
        processor->editor_view->getSize(&after_attach_size);
    const int after_w = daux_view_rect_width(after_attach_size);
    const int after_h = daux_view_rect_height(after_attach_size);
    int size_w = after_w > 0 ? after_w : editor_w;
    int size_h = after_h > 0 ? after_h : editor_h;
    size_w = std::clamp(size_w, 160, 4096);
    size_h = std::clamp(size_h, 160, 4096);
    daux_constrain_content_size(processor, &size_w, &size_h);
    int root_w = 0;
    int root_h = 0;
    if (daux_plugin_root_window_size(content, size_w, size_h, &root_w, &root_h) &&
        (root_w != size_w || root_h != size_h)) {
      std::fprintf(stderr,
                   "[SphereVST3/win] native root overrides stale getSize "
                   "getSize=%dx%d root=%dx%d dpi=%u\n",
                   size_w, size_h, root_w, root_h, daux_hwnd_dpi(content));
      size_w = root_w;
      size_h = root_h;
    }
    size_w = std::clamp(size_w, 160, 4096);
    size_h = std::clamp(size_h, 160, 4096);
    daux_embed_apply_content_size(processor, size_w, size_h, "after_attach.getSize");
    daux_resize_child_client(content, size_w, size_h);
    std::fprintf(stderr,
                 "[vst3-editor] getSize after_attach result=0x%x width=%d "
                 "height=%d rect=(%d,%d,%d,%d)\n",
                 static_cast<unsigned>(after_get_size_result), size_w, size_h,
                 after_attach_size.left, after_attach_size.top,
                 after_attach_size.right, after_attach_size.bottom);
    editor_w = size_w;
    editor_h = size_h;
    std::fprintf(stderr, "[editor-size] client rect = %dx%d\n", editor_w,
                 editor_h);
  }
  {
    auto local = daux_local_view_rect(editor_w, editor_h);
    std::fprintf(stderr, "[PluginEditorLifecycle] initial resize size=%dx%d\n",
                 editor_w, editor_h);
    const auto on_size_res = processor->editor_view->onSize(&local);
    std::fprintf(stderr, "[editor-size] onSize result = 0x%x\n",
                 static_cast<unsigned>(on_size_res));
    std::fprintf(stderr, "[PluginEditor] onSize result=0x%x\n",
                 static_cast<unsigned>(on_size_res));
    std::fprintf(stderr,
                 "[editor-open] 14 resize width=%d height=%d onSize_result=0x%x\n",
                 editor_w, editor_h, static_cast<unsigned>(on_size_res));
    std::fprintf(stderr,
                 "[vst3-editor] onSize result=0x%x rect=(%d,%d,%d,%d)\n",
                 static_cast<unsigned>(on_size_res), local.left, local.top,
                 local.right, local.bottom);
    if (!daux_verify_child_client_rect(content, editor_w, editor_h,
                                       "after attach")) {
      daux_resize_child_client(content, editor_w, editor_h);
      processor->editor_view->onSize(&local);
    }
  }
  daux_log_hwnd_state("after_attach_onSize", top, content);
  if (kind == 1)
    daux_editor_apply_tool_styles(top, parent);

  ShowWindow(top, kind == 2 ? SW_SHOWNORMAL : SW_SHOWNA);
  ShowWindow(content, SW_SHOW);
  UpdateWindow(top);
  UpdateWindow(content);
  std::fprintf(stderr, "[PluginEditor] visible=true\n");
  std::fprintf(stderr,
               "[editor-open] 15 show_update_redraw top=0x%p content=0x%p "
               "top_visible=%d content_visible=%d\n",
               static_cast<void *>(top), static_cast<void *>(content),
               IsWindowVisible(top) ? 1 : 0, IsWindowVisible(content) ? 1 : 0);
  {
    RECT crc{};
    GetWindowRect(content, &crc);
    std::fprintf(
        stderr,
        "[PluginEditorHWND] wrapper=0x%p child=0x%p child_enabled=%d "
        "child_visible=%d child_style=0x%Ix child_ex_style=0x%Ix\n",
        static_cast<void *>(top), static_cast<void *>(content),
        IsWindowEnabled(content) ? 1 : 0, IsWindowVisible(content) ? 1 : 0,
        static_cast<std::uintptr_t>(GetWindowLongPtrW(content, GWL_STYLE)),
        static_cast<std::uintptr_t>(GetWindowLongPtrW(content, GWL_EXSTYLE)));
    std::fprintf(stderr, "[PluginEditorHWND] child_rect=(%ld,%ld,%ld,%ld)\n",
                 crc.left, crc.top, crc.right, crc.bottom);
  }

  // Phase 2: enumerate plug-in-created child windows. For Chromium/CEF/WebView
  // editors (UAD Native, Slate, some iZotope) the host HWND will commonly have
  // ZERO children at this point because the WebView is still booting on an
  // internal helper thread. The delayed-ready poller in Rust re-checks at
  // 100/500/1000/3000/5000 ms.
  {
    int child_count = 0;
    EnumChildWindows(
        content,
        [](HWND hwnd, LPARAM lp) -> BOOL {
          char cls[64] = {0};
          GetClassNameA(hwnd, cls, sizeof(cls));
          RECT r{};
          GetWindowRect(hwnd, &r);
          DWORD tid = 0;
          GetWindowThreadProcessId(hwnd, &tid);
          const LONG_PTR style = GetWindowLongPtr(hwnd, GWL_STYLE);
          std::fprintf(stderr,
                       "[vst3-editor]   child hwnd=0x%p class='%s' visible=%d "
                       "rect=(%ld,%ld %ldx%ld) "
                       "style=0x%08lx tid=%lu\n",
                       static_cast<void *>(hwnd), cls,
                       IsWindowVisible(hwnd) ? 1 : 0, r.left, r.top,
                       r.right - r.left, r.bottom - r.top,
                       static_cast<unsigned long>(style), tid);
          (*reinterpret_cast<int *>(lp))++;
          return TRUE;
        },
        reinterpret_cast<LPARAM>(&child_count));
    std::fprintf(stderr,
                 "[vst3-editor] EnumChildWindows count=%d content=0x%p\n",
                 child_count, static_cast<void *>(content));
  }

  // Phase 5: post-attach paint hygiene — repaint the host + any plug-in
  // children. WebView plug-ins sometimes need an explicit invalidate before
  // their first frame goes on screen.
  ShowWindow(top, kind == 2 ? SW_SHOWNORMAL : SW_SHOWNA);
  ShowWindow(content, SW_SHOW);
  InvalidateRect(content, nullptr, TRUE);
  UpdateWindow(content);
  EnumChildWindows(
      content,
      [](HWND hwnd, LPARAM) -> BOOL {
        if (!IsWindow(hwnd))
          return TRUE;
        InvalidateRect(hwnd, nullptr, TRUE);
        UpdateWindow(hwnd);
        return TRUE;
      },
      0);

  if (!IsWindowVisible(content) || !daux_embed_has_visible_ui(processor)) {
    std::fprintf(stderr,
                 "[SphereVST3] embed editor attached but no visible UI yet "
                 "handle=%llu mode=%s "
                 "(deferring to delayed-ready poller)\n",
                 processor->editor_handle, daux_editor_host_kind_name(kind));
    // Leave it attached — Rust will poll embed_has_visible_ui at 100/500/1000/
    // 3000/5000 ms (Phase 6) before declaring the editor blank.
  }

  std::fprintf(stderr,
               "[SphereVST3] embed editor ok handle=%llu mode=%s parent=0x%p "
               "content=0x%p "
               "region=(%d,%d,%d,%d) (reused runtime instance)\n",
               processor->editor_handle, daux_editor_host_kind_name(kind),
               static_cast<void *>(parent), static_cast<void *>(content), x, y,
               width, height);
  std::fprintf(stderr,
               "[editor-open] 16 editor_ready handle=%llu mode=%s shell_hwnd=0x%p "
               "content_hwnd=0x%p size=%dx%d\n",
               processor->editor_handle, daux_editor_host_kind_name(kind),
               static_cast<void *>(top), static_cast<void *>(content), editor_w,
               editor_h);
  return processor->editor_handle;
#elif defined(__linux__)
  // Host-owned model on Linux: the external plugin-host process owns a
  // top-level GTK4/X11 window. `parent_hwnd` is only a DPI/position/owner
  // reference cross-process — never a real parent (XEmbed across processes is
  // exactly what host-owned mode avoids), so we ignore it and create our own
  // top-level window via the standalone editor path (open_editor_linux), which
  // attaches the IPlugView with a Linux::IRunLoop so the editor actually paints.
  (void)parent_hwnd;
  (void)x;
  (void)y;
  processor->editor_user_closed.store(false, std::memory_order_release);
  std::string title_copy = processor->editor_title;
  std::string window_id_copy = processor->editor_window_id;
  return open_editor_linux(
      processor, window_id_copy.c_str(),
      title_copy.empty() ? "Plugin Editor" : title_copy.c_str(), width, height);
#elif defined(__APPLE__)
  // Host-owned model on macOS, same shape as Linux: AppKit has no public
  // cross-process NSView embedding, so `parent_hwnd` cannot be a parent here —
  // it is only the main app's owner reference. The editor therefore lives in a
  // top-level NSWindow owned by this process, created through the standalone
  // editor path (open_editor_mac), which attaches the IPlugView to an NSView it
  // owns.
  (void)parent_hwnd;
  (void)x;
  (void)y;
  if (!processor || !processor->controller) {
    set_last_error("embed editor: processor/controller missing");
    return 0;
  }
  processor->editor_user_closed.store(false, std::memory_order_release);
  {
    std::string title_copy = processor->editor_title;
    std::string window_id_copy = processor->editor_window_id;
    return open_editor_mac(
        processor, window_id_copy.c_str(),
        title_copy.empty() ? "Plugin Editor" : title_copy.c_str(), width,
        height);
  }
#else
  (void)processor;
  (void)parent_hwnd;
  (void)x;
  (void)y;
  (void)width;
  (void)height;
  return 0;
#endif
}

extern "C" void
sphere_daux_vst3_embed_set_bounds(SphereDauxVst3Processor *processor, int x,
                                  int y, int width, int height) {
#ifdef _WIN32
  if (!processor || !processor->embed_mode)
    return;
  if (width <= 0 || height <= 0)
    return;
  if (daux_resize_log_allow()) {
    std::fprintf(stderr,
                 "[PluginEditorResize] wrapper_client=%dx%d origin=(%d,%d)\n",
                 width, height, x, y);
  }
  // Generic VST3 resize contract (spec items 1/2): never hand the plugin a
  // size it did not agree to. Fixed-size views snap back to their getSize;
  // resizable views go through checkSizeConstraint. `width`/`height` arrive
  // as the wrapper's CONTENT client size (titlebar already excluded by the
  // main app), so this is plugin content size end to end.
  int content_w = width;
  int content_h = height;
  if (processor->editor_view && processor->editor_attached) {
    daux_constrain_content_size(processor, &content_w, &content_h);
  }
  const HWND focus_before = GetFocus();
  processor->embed_host_x = x;
  processor->embed_host_y = y;
  processor->embed_host_w = content_w;
  processor->embed_host_h = content_h;
  processor->embed_content_w = content_w;
  processor->embed_content_h = content_h;
  processor->embed_geometry_valid = false;
  if (daux_embed_sync_geometry(processor, x, y, content_w, content_h,
                               daux_embed_debug())) {
    daux_editor_set_content_scale(processor,
                                  processor->editor_parent_hwnd
                                      ? processor->editor_parent_hwnd
                                      : processor->editor_attach_hwnd,
                                  "set_bounds");
    resize_editor_view(processor);
    std::fprintf(stderr, "[plugin-host] host_hwnd resize %dx%d\n", content_w,
                 content_h);
    // Repaint everything the resize exposed — no stale edge pixels.
    if (processor->editor_embed_top_hwnd &&
        IsWindow(processor->editor_embed_top_hwnd)) {
      InvalidateRect(processor->editor_embed_top_hwnd, nullptr, FALSE);
    }
    // Input contract after resize (spec item 10): a geometry change must not
    // steal focus from the plugin subtree.
    if (focus_before && IsWindow(focus_before) &&
        processor->editor_attach_hwnd &&
        (focus_before == processor->editor_attach_hwnd ||
         IsChild(processor->editor_attach_hwnd, focus_before)) &&
        GetFocus() != focus_before) {
      SetFocus(focus_before);
    }
  }
  // The wrapper asked for a size the plugin rejected or adjusted: tell the
  // main app so the shell snaps to the constrained size instead of leaving
  // blank/garbage area around the plugin content. This converges in one round
  // trip — the next ResizeEditor arrives already constrained and is a no-op.
  if (content_w != width || content_h != height) {
    processor->pending_main_shell_w = content_w;
    processor->pending_main_shell_h = content_h;
    processor->pending_main_shell_resize.store(true, std::memory_order_release);
    std::fprintf(stderr,
                 "[PluginEditorResize] wrapper_snapback requested=%dx%d "
                 "constrained=%dx%d\n",
                 width, height, content_w, content_h);
  }
#else
  (void)processor;
  (void)x;
  (void)y;
  (void)width;
  (void)height;
#endif
}

extern "C" void
sphere_daux_vst3_embed_refresh(SphereDauxVst3Processor *processor) {
#ifdef _WIN32
  if (!processor || !processor->embed_mode || !processor->editor_attach_hwnd)
    return;
  if (!IsWindow(processor->editor_attach_hwnd))
    return;
  // Detached: standalone window manages its own geometry — nothing to re-sync.
  if (processor->embed_host_kind == 2)
    return;
  // Idle frames: re-sync geometry only (tracks parent moves). onSize/pump only
  // run when the applied rect actually changed — no per-frame flicker/spam.
  if (daux_embed_sync_geometry(processor, processor->embed_host_x,
                               processor->embed_host_y, processor->embed_host_w,
                               processor->embed_host_h, false)) {
    daux_editor_set_content_scale(processor,
                                  processor->editor_parent_hwnd
                                      ? processor->editor_parent_hwnd
                                      : processor->editor_attach_hwnd,
                                  "refresh");
    resize_editor_view(processor);
  }
#else
  (void)processor;
#endif
}

// ── Rust-owned view host ─────────────────────────────────────────────────────
//
// Everything above this line creates windows. Nothing below it does.
//
// The host — GPUI — owns the window a plug-in editor lives in: it creates the
// child HWND, positions it, paints the surround, decides when it goes away, and
// is the only thing that resizes it. This side is reduced to the part of the
// VST3 contract that genuinely has to be in C++: create the view, hand it a
// frame, attach it to the HWND it was given, pass sizes both ways, and let it
// go. There is no shell, no titlebar, no content window and no window procedure
// on this path, which is what makes the host the single owner of the editor's
// geometry.
//
// `IPlugFrame::resizeView` is the one call that would otherwise need a window
// here. It is recorded instead: the host polls the request, resizes its own
// surface to whatever it can give, and reports back through
// `sphere_daux_vst3_view_set_size`. A plug-in is never told a size the host did
// not actually apply.

/// Smallest and largest content size this host will hand a plug-in.
///
/// Outside the platform arms below because it is the same promise on all of
/// them: whoever owns the window, no plug-in is ever told a size outside this
/// range.
constexpr int kViewHostMinSide = 64;
constexpr int kViewHostMaxSide = 8192;

/// Clamps a content size into the range this host is willing to hand a plug-in.
///
/// `maybe_unused`: the platform arm that ships on a given target may not clamp
/// — the fallback stubs below answer 0 to everything and have no size to bound.
[[maybe_unused]] static void view_host_clamp(int *width, int *height) {
  if (width) {
    *width = std::clamp(*width, kViewHostMinSide, kViewHostMaxSide);
  }
  if (height) {
    *height = std::clamp(*height, kViewHostMinSide, kViewHostMaxSide);
  }
}

#ifdef _WIN32
// Not in an anonymous namespace: the processor struct forward-declares
// `HostViewFrame` so it can hold a pointer to one, and a namespaced class of the
// same name is a different type to that declaration.

/// `IPlugFrame` that records resize requests instead of acting on them.
class HostViewFrame final : public Steinberg::IPlugFrame {
public:
  explicit HostViewFrame(SphereDauxVst3Processor *owner) : owner_(owner) {}

  Steinberg::tresult PLUGIN_API
  resizeView(Steinberg::IPlugView *view,
             Steinberg::ViewRect *newSize) override {
    if (!owner_ || !view || !newSize || view != owner_->editor_view.get()) {
      return Steinberg::kResultFalse;
    }
    const int width = daux_view_rect_width(*newSize);
    const int height = daux_view_rect_height(*newSize);
    if (width < kViewHostMinSide || height < kViewHostMinSide ||
        width > kViewHostMaxSide || height > kViewHostMaxSide) {
      return Steinberg::kResultFalse;
    }
    owner_->view_host_resize_w = width;
    owner_->view_host_resize_h = height;
    owner_->view_host_resize_pending.store(true, std::memory_order_release);
    if (daux_vst3_editor_debug()) {
      std::fprintf(stderr, "[view-host] resizeView requested=%dx%d\n", width,
                   height);
    }
    // Accepted as a *request*. The plug-in learns the size that was really
    // granted from the onSize the host sends once it has resized.
    return Steinberg::kResultTrue;
  }

  Steinberg::tresult PLUGIN_API queryInterface(const Steinberg::TUID iid,
                                               void **obj) override {
    if (!obj) {
      return Steinberg::kInvalidArgument;
    }
    if (Steinberg::FUnknownPrivate::iidEqual(iid, Steinberg::IPlugFrame::iid) ||
        Steinberg::FUnknownPrivate::iidEqual(iid, Steinberg::FUnknown::iid)) {
      *obj = static_cast<Steinberg::IPlugFrame *>(this);
      addRef();
      return Steinberg::kResultTrue;
    }
    *obj = nullptr;
    return Steinberg::kNoInterface;
  }

  Steinberg::uint32 PLUGIN_API addRef() override {
    return static_cast<Steinberg::uint32>(++refs_);
  }

  Steinberg::uint32 PLUGIN_API release() override {
    const auto remaining = --refs_;
    if (remaining == 0) {
      delete this;
      return 0;
    }
    return static_cast<Steinberg::uint32>(remaining);
  }

private:
  SphereDauxVst3Processor *owner_{nullptr};
  std::atomic<int> refs_{1};
};

/// Releases the view and its frame. Touches no window: the parent belongs to
/// the host, which decides on its own terms when it goes away.
static void view_host_release(SphereDauxVst3Processor *processor, const char *reason) {
  if (!processor) {
    return;
  }
  if (processor->editor_view && processor->view_host_attached) {
    // The SDK's order: clear the frame, then removed(). A view told to let go
    // of its parent while a frame is being destroyed underneath it has
    // somewhere to call back into that no longer exists.
    processor->editor_view->setFrame(nullptr);
    processor->editor_view->removed();
  }
  if (processor->view_host_frame) {
    processor->view_host_frame->release();
    processor->view_host_frame = nullptr;
  }
  processor->editor_view = nullptr;
  processor->view_host_attached = false;
  if (processor->editor_attach_hwnd == processor->view_host_parent) {
    // Only clear the borrowed handle, never destroy it.
    processor->editor_attach_hwnd = nullptr;
  }
  processor->view_host_parent = nullptr;
  processor->view_host_resize_pending.store(false, std::memory_order_release);
  processor->editor_attached = false;
  daux_plugin_browser_runtime_release(processor);
  std::fprintf(stderr, "[view-host] released reason=%s\n",
               reason ? reason : "unknown");
}

extern "C" int sphere_daux_vst3_view_attach(SphereDauxVst3Processor *processor,
                                            unsigned long long parent_hwnd,
                                            int width, int height,
                                            int *out_width, int *out_height) {
  if (!processor || !processor->controller) {
    set_last_error("view host: processor/controller missing");
    return 0;
  }
  HWND parent =
      reinterpret_cast<HWND>(static_cast<std::uintptr_t>(parent_hwnd));
  if (!parent || !IsWindow(parent)) {
    set_last_error("view host: invalid parent HWND");
    return 0;
  }
  // Chromium- and WebView-backed editors never finish initializing without an
  // STA on the thread that owns the parent window, and a view attached from a
  // DPI-unaware thread reports the wrong size. Both are idempotent.
  daux_embed_ensure_com_initialized();
  daux_ensure_thread_dpi_awareness();

  // Re-attaching to the same window is the host asking for the size again, not
  // a request to tear a live view down and build another.
  if (processor->view_host_attached && processor->view_host_parent == parent &&
      processor->editor_view) {
    if (out_width || out_height) {
      Steinberg::ViewRect current{};
      if (processor->editor_view->getSize(&current) == Steinberg::kResultTrue) {
        if (out_width) {
          *out_width = daux_view_rect_width(current);
        }
        if (out_height) {
          *out_height = daux_view_rect_height(current);
        }
      }
    }
    return 1;
  }
  if (processor->view_host_attached) {
    view_host_release(processor, "reattach-to-new-parent");
  }

  std::fprintf(stderr,
               "[view-host] attach begin parent=0x%p request=%dx%d tid=%lu\n",
               static_cast<void *>(parent), width, height,
               GetCurrentThreadId());

  if (!processor->editor_view) {
    if (!daux_plugin_browser_runtime_prepare(processor)) {
      set_last_error("view host: plug-in browser runtime failed to start");
      return 0;
    }
    processor->editor_view = Steinberg::IPtr<Steinberg::IPlugView>::adopt(
        processor->controller->createView(Steinberg::Vst::ViewType::kEditor));
  }
  if (!processor->editor_view) {
    daux_plugin_browser_runtime_release(processor);
    set_last_error("view host: controller did not create a view");
    return 0;
  }
  if (processor->editor_view->isPlatformTypeSupported(
          Steinberg::kPlatformTypeHWND) != Steinberg::kResultTrue) {
    processor->editor_view = nullptr;
    daux_plugin_browser_runtime_release(processor);
    set_last_error("view host: view does not support HWND");
    return 0;
  }

  daux_editor_set_content_scale(processor, parent, "view_host.before_getSize");

  // getSize is the source of truth for the content size. The host's requested
  // region is only a starting point: a plug-in with a fixed editor gets the
  // size it asks for, and the host lays out around whatever comes back.
  int content_w = width > 0 ? width : 0;
  int content_h = height > 0 ? height : 0;
  Steinberg::ViewRect preferred{};
  if (processor->editor_view->getSize(&preferred) == Steinberg::kResultTrue) {
    const int preferred_w = daux_view_rect_width(preferred);
    const int preferred_h = daux_view_rect_height(preferred);
    if (preferred_w > 0 && preferred_h > 0) {
      content_w = preferred_w;
      content_h = preferred_h;
    }
  }
  if (content_w <= 0 || content_h <= 0) {
    content_w = 900;
    content_h = 600;
  }
  view_host_clamp(&content_w, &content_h);
  daux_constrain_content_size(processor, &content_w, &content_h);
  view_host_clamp(&content_w, &content_h);

  // The frame goes on before attached(): browser-backed editors bootstrap
  // through it, and a view that resizes itself during attach has nowhere to
  // report that without one.
  processor->view_host_frame = new HostViewFrame(processor);
  const auto frame_res =
      processor->editor_view->setFrame(processor->view_host_frame);
  if (frame_res != Steinberg::kResultTrue &&
      frame_res != Steinberg::kResultOk) {
    std::fprintf(stderr, "[view-host] setFrame rejected result=0x%x\n",
                 static_cast<unsigned>(frame_res));
  }

  const auto attach_res = processor->editor_view->attached(
      reinterpret_cast<void *>(parent), Steinberg::kPlatformTypeHWND);
  if (attach_res != Steinberg::kResultTrue &&
      attach_res != Steinberg::kResultOk) {
    std::fprintf(stderr, "[view-host] attach failed result=0x%x\n",
                 static_cast<unsigned>(attach_res));
    processor->editor_view->setFrame(nullptr);
    processor->view_host_frame->release();
    processor->view_host_frame = nullptr;
    processor->editor_view = nullptr;
    daux_plugin_browser_runtime_release(processor);
    set_last_error("view host: IPlugView::attached failed");
    return 0;
  }

  processor->view_host_parent = parent;
  processor->view_host_attached = true;
  processor->editor_attached = true;
  // Published so the shared "is the editor really showing anything" and focus
  // helpers keep working on this path. It is only ever *read* here: the window
  // belongs to the host, and `embed_mode` stays false, which is what keeps the
  // legacy teardown from ever destroying it.
  processor->editor_attach_hwnd = parent;
  processor->view_host_resize_pending.store(false, std::memory_order_release);

  // Some editors settle on their real size only inside attached().
  Steinberg::ViewRect settled{};
  if (processor->editor_view->getSize(&settled) == Steinberg::kResultTrue) {
    const int settled_w = daux_view_rect_width(settled);
    const int settled_h = daux_view_rect_height(settled);
    if (settled_w > 0 && settled_h > 0) {
      content_w = settled_w;
      content_h = settled_h;
      view_host_clamp(&content_w, &content_h);
    }
  }

  if (out_width) {
    *out_width = content_w;
  }
  if (out_height) {
    *out_height = content_h;
  }
  std::fprintf(stderr, "[view-host] attached parent=0x%p content=%dx%d\n",
               static_cast<void *>(parent), content_w, content_h);
  return 1;
}

extern "C" void
sphere_daux_vst3_view_detach(SphereDauxVst3Processor *processor) {
  if (!processor || !processor->view_host_attached) {
    return;
  }
  view_host_release(processor, "host-detach");
}

extern "C" int
sphere_daux_vst3_view_is_attached(SphereDauxVst3Processor *processor) {
  return (processor && processor->view_host_attached && processor->editor_view)
             ? 1
             : 0;
}

extern "C" int
sphere_daux_vst3_view_set_size(SphereDauxVst3Processor *processor, int width,
                               int height) {
  if (!processor || !processor->view_host_attached || !processor->editor_view) {
    return 0;
  }
  if (width <= 0 || height <= 0) {
    return 0;
  }
  view_host_clamp(&width, &height);
  Steinberg::ViewRect rect{};
  rect.left = 0;
  rect.top = 0;
  rect.right = width;
  rect.bottom = height;
  const auto res = processor->editor_view->onSize(&rect);
  return (res == Steinberg::kResultTrue || res == Steinberg::kResultOk) ? 1 : 0;
}

extern "C" int
sphere_daux_vst3_view_get_size(SphereDauxVst3Processor *processor,
                               int *out_width, int *out_height) {
  if (!processor || !processor->editor_view || !out_width || !out_height) {
    return 0;
  }
  Steinberg::ViewRect rect{};
  if (processor->editor_view->getSize(&rect) != Steinberg::kResultTrue) {
    return 0;
  }
  const int width = daux_view_rect_width(rect);
  const int height = daux_view_rect_height(rect);
  if (width <= 0 || height <= 0) {
    return 0;
  }
  *out_width = width;
  *out_height = height;
  return 1;
}

extern "C" int
sphere_daux_vst3_view_can_resize(SphereDauxVst3Processor *processor) {
  if (!processor || !processor->editor_view) {
    return 0;
  }
  return processor->editor_view->canResize() == Steinberg::kResultTrue ? 1 : 0;
}

extern "C" int
sphere_daux_vst3_view_constrain(SphereDauxVst3Processor *processor,
                                int *io_width, int *io_height) {
  if (!processor || !io_width || !io_height) {
    return 0;
  }
  view_host_clamp(io_width, io_height);
  const bool ok = daux_constrain_content_size(processor, io_width, io_height);
  view_host_clamp(io_width, io_height);
  return ok ? 1 : 0;
}

extern "C" int
sphere_daux_vst3_view_take_resize_request(SphereDauxVst3Processor *processor,
                                          int *out_width, int *out_height) {
  if (!processor || !out_width || !out_height) {
    return 0;
  }
  if (!processor->view_host_resize_pending.exchange(
          false, std::memory_order_acq_rel)) {
    return 0;
  }
  *out_width = processor->view_host_resize_w;
  *out_height = processor->view_host_resize_h;
  return 1;
}
#elif defined(__APPLE__)
// ── macOS: the view host is a window of this process's own ──────────────────
//
// The comment above describes the Windows mechanism, where the host hands this
// side a window and this side only fills it. AppKit has no public way to put an
// `NSView` inside a window another process owns, so there is no handle for the
// host to hand over and the same job is split differently: the *window* is
// created here, next to the view, by `open_editor_mac`.
//
// Everything else about the contract is unchanged, and deliberately so — the
// caller drives macOS through the same seven calls it drives Windows through,
// and never learns which side of the boundary the window came from. What
// changes is only who applies a size: on Windows the host resizes its own
// surface and reports back, here the resize *is* the report.

extern "C" int sphere_daux_vst3_view_attach(SphereDauxVst3Processor *processor,
                                            unsigned long long parent_hwnd,
                                            int width, int height,
                                            int *out_width, int *out_height) {
  if (!processor || !processor->controller) {
    set_last_error("view host: processor/controller missing");
    return 0;
  }
  // Never a parent. The caller passes 0 on this platform; anything else is an
  // owner/DPI reference from a process whose window handles mean nothing here.
  (void)parent_hwnd;
  if (width <= 0 || height <= 0) {
    set_last_error("view host: invalid requested editor size");
    return 0;
  }
  view_host_clamp(&width, &height);
  // A previous window's close flag must not be read as this one's. The host
  // polls it to report `EditorClosed`, and a stale `true` would tear down an
  // editor that just opened.
  processor->editor_user_closed.store(false, std::memory_order_release);

  // Copied out: `open_editor_mac` can re-enter through the main queue and
  // `sphere_daux_editor_store_native` rewrites both strings on the way.
  const std::string window_id = processor->editor_window_id;
  const std::string title = processor->editor_title;
  const unsigned long long handle =
      open_editor_mac(processor, window_id.c_str(),
                      title.empty() ? "Plugin Editor" : title.c_str(), width,
                      height);
  if (handle == 0) {
    set_last_error("view host: the plug-in's macOS editor window could not be "
                   "opened");
    return 0;
  }

  // What the editor actually settled on, which is not always what it was asked
  // for: `open_editor_mac` re-reads `getSize` after `attached()` because some
  // editors only choose their real size there, and sizes its window to that.
  int content_w = width;
  int content_h = height;
  if (!sphere_daux_editor_get_view_size(processor, &content_w, &content_h)) {
    content_w = processor->editor_content_width > 0
                    ? processor->editor_content_width
                    : width;
    content_h = processor->editor_content_height > 0
                    ? processor->editor_content_height
                    : height;
  }
  view_host_clamp(&content_w, &content_h);
  if (out_width) {
    *out_width = content_w;
  }
  if (out_height) {
    *out_height = content_h;
  }
  std::fprintf(stderr,
               "[view-host] attached host_owned=1 handle=%llu content=%dx%d\n",
               handle, content_w, content_h);
  return 1;
}

extern "C" void
sphere_daux_vst3_view_detach(SphereDauxVst3Processor *processor) {
  if (!processor) {
    return;
  }
  // `IPlugView::removed()` first, then the window — `close_editor_mac` does
  // both in that order. The audio instance is untouched.
  close_editor_mac(processor);
}

extern "C" int
sphere_daux_vst3_view_is_attached(SphereDauxVst3Processor *processor) {
  return (processor && processor->editor_attached &&
          processor->editor_native_window)
             ? 1
             : 0;
}

extern "C" int
sphere_daux_vst3_view_set_size(SphereDauxVst3Processor *processor, int width,
                               int height) {
  if (!processor || !processor->editor_view ||
      !processor->editor_native_window) {
    return 0;
  }
  if (width <= 0 || height <= 0) {
    return 0;
  }
  view_host_clamp(&width, &height);
  // The same size contract the Windows arm applies: a fixed-size view snaps
  // back to its own `getSize`, a resizable one goes through
  // `checkSizeConstraint`, and only the agreed size reaches the window.
  if (!sphere_daux_editor_constrain_view_size(processor, &width, &height)) {
    return 0;
  }
  view_host_clamp(&width, &height);
  resize_editor_mac(processor, width, height, "view-host-set-size");
  return 1;
}

extern "C" int
sphere_daux_vst3_view_get_size(SphereDauxVst3Processor *processor,
                               int *out_width, int *out_height) {
  if (!processor || !out_width || !out_height) {
    return 0;
  }
  return sphere_daux_editor_get_view_size(processor, out_width, out_height);
}

extern "C" int
sphere_daux_vst3_view_can_resize(SphereDauxVst3Processor *processor) {
  if (!processor) {
    return 0;
  }
  return sphere_daux_editor_can_resize(processor);
}

extern "C" int
sphere_daux_vst3_view_constrain(SphereDauxVst3Processor *processor,
                                int *io_width, int *io_height) {
  if (!processor || !io_width || !io_height) {
    return 0;
  }
  view_host_clamp(io_width, io_height);
  const int ok =
      sphere_daux_editor_constrain_view_size(processor, io_width, io_height);
  view_host_clamp(io_width, io_height);
  return ok;
}

extern "C" int
sphere_daux_vst3_view_take_resize_request(SphereDauxVst3Processor *processor,
                                          int *out_width, int *out_height) {
  // Always empty, and not because it is unimplemented: `MacPluginEditorFrame`
  // applies a plug-in's `resizeView` to the window as it arrives, because that
  // window is right here. A request is never left pending for the host to
  // collect, so answering "none" is the truth rather than a stub.
  (void)processor;
  (void)out_width;
  (void)out_height;
  return 0;
}

#else
extern "C" int sphere_daux_vst3_view_attach(SphereDauxVst3Processor *,
                                            unsigned long long, int, int, int *,
                                            int *) {
  return 0;
}
extern "C" void sphere_daux_vst3_view_detach(SphereDauxVst3Processor *) {}
extern "C" int sphere_daux_vst3_view_is_attached(SphereDauxVst3Processor *) {
  return 0;
}
extern "C" int sphere_daux_vst3_view_set_size(SphereDauxVst3Processor *, int,
                                              int) {
  return 0;
}
extern "C" int sphere_daux_vst3_view_get_size(SphereDauxVst3Processor *, int *,
                                              int *) {
  return 0;
}
extern "C" int sphere_daux_vst3_view_can_resize(SphereDauxVst3Processor *) {
  return 0;
}
extern "C" int sphere_daux_vst3_view_constrain(SphereDauxVst3Processor *, int *,
                                               int *) {
  return 0;
}
extern "C" int
sphere_daux_vst3_view_take_resize_request(SphereDauxVst3Processor *, int *,
                                          int *) {
  return 0;
}
#endif

// ── Editor chrome strip ─────────────────────────────────────────────────────
//
// The strip itself is shared across every format bridge and addressed by the
// window it lives in (`sphere_daux_editor_chrome.h`). All this bridge has to
// supply is which window that is — the studio drives the rest.

/// The `NSWindow*` of this instance's host-owned editor, as an opaque handle.
///
/// 0 on Windows, and 0 whenever no editor is open. The caller passes it
/// straight to the chrome ABI, which treats 0 as "no strip to update".
extern "C" unsigned long long
sphere_daux_vst3_editor_native_window(SphereDauxVst3Processor *processor) {
#if defined(__APPLE__)
  if (!processor) {
    return 0;
  }
  return reinterpret_cast<unsigned long long>(processor->editor_native_window);
#else
  (void)processor;
  return 0;
#endif
}

extern "C" void
sphere_daux_vst3_embed_detach(SphereDauxVst3Processor *processor) {
#ifdef _WIN32
  if (!processor || !processor->embed_mode)
    return;
  processor->close_embed_editor("embed_detach");
#elif defined(__linux__)
  // Host-owned top-level: close the GTK window + detach the view. The audio
  // instance stays alive (close_editor_linux only tears down the editor).
  close_editor_linux(processor);
#elif defined(__APPLE__)
  // Host-owned top-level NSWindow; the audio instance stays alive.
  close_editor_mac(processor);
#else
  (void)processor;
#endif
}

// Real Win32 HWND of the embed subtree root (the host child created under the
// main app's content HWND; falls back to the `IPlugView::attached` content
// child). `sphere_daux_vst3_embed_editor` returns an opaque monotonic handle,
// NOT an HWND — callers that need to pump/focus/audit the editor subtree must
// use this.
extern "C" unsigned long long
sphere_daux_vst3_embed_attach_hwnd(SphereDauxVst3Processor *processor) {
#ifdef _WIN32
  if (!processor)
    return 0;
  if (processor->editor_embed_top_hwnd &&
      IsWindow(processor->editor_embed_top_hwnd)) {
    return reinterpret_cast<unsigned long long>(
        processor->editor_embed_top_hwnd);
  }
  if (processor->editor_attach_hwnd &&
      IsWindow(processor->editor_attach_hwnd)) {
    return reinterpret_cast<unsigned long long>(processor->editor_attach_hwnd);
  }
  return 0;
#elif defined(__linux__) || defined(__APPLE__)
  // No cross-process window handle here; the host-owned window lives in this
  // process. Return the opaque editor handle so the host records a non-zero
  // host_hwnd (its Win32 message-pump calls are no-ops on these platforms).
  if (!processor)
    return 0;
  return processor->editor_native_window ? processor->editor_handle : 0;
#else
  (void)processor;
  return 0;
#endif
}

extern "C" int
sphere_daux_vst3_embed_is_valid(SphereDauxVst3Processor *processor) {
#ifdef _WIN32
  return (processor && processor->embed_mode && processor->editor_attach_hwnd &&
          IsWindow(processor->editor_attach_hwnd))
             ? 1
             : 0;
#elif defined(__linux__) || defined(__APPLE__)
  return (processor && processor->editor_native_window) ? 1 : 0;
#else
  (void)processor;
  return 0;
#endif
}

extern "C" int
sphere_daux_vst3_embed_has_visible_ui(SphereDauxVst3Processor *processor) {
#ifdef _WIN32
  return daux_embed_has_visible_ui(processor) ? 1 : 0;
#elif defined(__linux__) || defined(__APPLE__)
  return (processor && processor->editor_attached) ? 1 : 0;
#else
  (void)processor;
  return 0;
#endif
}

extern "C" int
sphere_daux_vst3_embed_host_kind(SphereDauxVst3Processor *processor) {
#ifdef _WIN32
  if (!processor || !processor->embed_mode)
    return -1;
  return processor->embed_host_kind; // 0 child, 1 tool, 2 detached
#elif defined(__linux__) || defined(__APPLE__)
  // Always a detached top-level window here (host-owned model).
  return (processor && processor->editor_native_window) ? 2 : -1;
#else
  (void)processor;
  return -1;
#endif
}

// Detached mode only: returns 1 (and resets) if the user closed the standalone
// editor window (WM_CLOSE). The Rust shell polls this to tear the editor down.
extern "C" int
sphere_daux_vst3_embed_take_user_close(SphereDauxVst3Processor *processor) {
#ifdef _WIN32
  if (!processor)
    return 0;
  return processor->embed_user_closed.exchange(false, std::memory_order_acq_rel)
             ? 1
             : 0;
#elif defined(__linux__) || defined(__APPLE__)
  if (!processor)
    return 0;
  return processor->editor_user_closed.exchange(false, std::memory_order_acq_rel)
             ? 1
             : 0;
#else
  (void)processor;
  return 0;
#endif
}

// Process-wide transport-key counter shared by Win32 / AppKit / GTK editors and
// the host's low-level keyboard filters. Host IPC drains this into
// HostEvent::TransportToggleRequested; main app never touches it.
namespace {
std::atomic<unsigned int> g_transport_toggle_requests{0};
} // namespace

extern "C" void sphere_daux_vst3_claim_transport_toggle(void) {
  g_transport_toggle_requests.fetch_add(1, std::memory_order_relaxed);
}

extern "C" unsigned int sphere_daux_vst3_take_transport_toggle_requests(void) {
  return g_transport_toggle_requests.exchange(0, std::memory_order_relaxed);
}

extern "C" void
sphere_daux_vst3_embed_set_waiting_stage(SphereDauxVst3Processor *processor,
                                         const char *stage) {
#ifdef _WIN32
  if (!processor)
    return;
  const std::wstring stage_w =
      widen_utf8(stage && *stage ? stage : "attach_view");
  daux_editor_set_load_state(&processor->editor_window, false, stage_w.c_str());
  std::fprintf(stderr,
               "[PluginEditorLifecycle] editor ready timeout stage=%s\n",
               stage && *stage ? stage : "attach_view");
#else
  (void)processor;
  (void)stage;
#endif
}

extern "C" void
sphere_daux_vst3_embed_set_instance_label(SphereDauxVst3Processor *processor,
                                          const char *instance_id) {
#ifdef _WIN32
  if (!processor)
    return;
  processor->embed_instance_label = instance_id ? instance_id : "";
#elif defined(__linux__) || defined(__APPLE__)
  if (!processor)
    return;
  processor->editor_window_id = instance_id ? instance_id : "";
#else
  (void)processor;
  (void)instance_id;
#endif
}

// Human-readable editor title / plug-in name (UTF-8). Used for the shell
// titlebar and the content "Loading Plugin <name>" overlay. Set before
// embed_editor so the loading shell shows the real name immediately.
extern "C" void
sphere_daux_vst3_set_editor_title(SphereDauxVst3Processor *processor,
                                  const char *title) {
#ifdef _WIN32
  if (!processor)
    return;
  processor->editor_title = title ? title : "";
#elif defined(__linux__) || defined(__APPLE__)
  if (!processor)
    return;
  processor->editor_title = title ? title : "";
#else
  (void)processor;
  (void)title;
#endif
}

extern "C" int
sphere_daux_vst3_prepare_editor_view(SphereDauxVst3Processor *processor,
                                     int *out_width, int *out_height) {
#ifdef _WIN32
  daux_embed_ensure_com_initialized();
  daux_ensure_thread_dpi_awareness();
  if (!processor || !processor->controller)
    return 0;
  if (!processor->editor_view) {
    if (!daux_plugin_browser_runtime_prepare(processor))
      return 0;
    processor->editor_view = Steinberg::IPtr<Steinberg::IPlugView>::adopt(
        processor->controller->createView(Steinberg::Vst::ViewType::kEditor));
    if (!processor->editor_view)
      return 0;
    if (processor->editor_view->isPlatformTypeSupported(
            Steinberg::kPlatformTypeHWND) != Steinberg::kResultTrue) {
      processor->editor_view = nullptr;
      daux_plugin_browser_runtime_release(processor);
      return 0;
    }
    daux_editor_install_frame(processor);
  }
  Steinberg::ViewRect sz{};
  const auto gs = processor->editor_view->getSize(&sz);
  const int w = daux_view_rect_width(sz);
  const int h = daux_view_rect_height(sz);
  if (gs != Steinberg::kResultTrue && gs != Steinberg::kResultOk)
    return 0;
  if (w <= 0 || h <= 0)
    return 0;
  if (out_width)
    *out_width = w;
  if (out_height)
    *out_height = h;
  std::fprintf(stderr,
               "[PluginEditor] prepare getSize instance=%s view_size=%dx%d\n",
               processor->embed_instance_label.empty()
                   ? "<unknown>"
                   : processor->embed_instance_label.c_str(),
               w, h);
  return 1;
#else
  (void)processor;
  (void)out_width;
  (void)out_height;
  return 0;
#endif
}

extern "C" int
sphere_daux_vst3_take_pending_shell_resize(SphereDauxVst3Processor *processor,
                                           int *out_width, int *out_height) {
#ifdef _WIN32
  if (!processor)
    return 0;
  if (!processor->pending_main_shell_resize.load(std::memory_order_acquire))
    return 0;
  processor->pending_main_shell_resize.store(false, std::memory_order_release);
  if (out_width)
    *out_width = processor->pending_main_shell_w;
  if (out_height)
    *out_height = processor->pending_main_shell_h;
  return 1;
#else
  (void)processor;
  (void)out_width;
  (void)out_height;
  return 0;
#endif
}

// IPlugView::canResize for the current editor view: 1 resizable, 0 fixed-size,
// -1 unknown (no view). The main app uses this to lock the wrapper window for
// fixed-size editors (spec item 1/8).
extern "C" int
sphere_daux_vst3_editor_resizable(SphereDauxVst3Processor *processor) {
#ifdef _WIN32
  if (!processor || !processor->editor_view)
    return -1;
  return daux_editor_view_resizable(processor) ? 1 : 0;
#elif defined(__APPLE__) || defined(__linux__)
  if (!processor || !processor->editor_view)
    return -1;
  return sphere_daux_editor_can_resize(processor);
#else
  (void)processor;
  return -1;
#endif
}

extern "C" int
sphere_daux_vst3_embed_content_size(SphereDauxVst3Processor *processor,
                                    int *out_width, int *out_height) {
#ifdef _WIN32
  if (!processor || !processor->embed_mode)
    return 0;
  if (processor->embed_content_w <= 0 || processor->embed_content_h <= 0)
    return 0;
  if (out_width)
    *out_width = processor->embed_content_w;
  if (out_height)
    *out_height = processor->embed_content_h;
  return 1;
#elif defined(__APPLE__) || defined(__linux__)
  if (!processor || !processor->editor_native_window ||
      processor->editor_content_width <= 0 ||
      processor->editor_content_height <= 0)
    return 0;
  if (out_width)
    *out_width = processor->editor_content_width;
  if (out_height)
    *out_height = processor->editor_content_height;
  return 1;
#else
  (void)processor;
  (void)out_width;
  (void)out_height;
  return 0;
#endif
}

extern "C" unsigned long long
sphere_daux_vst3_process_count(SphereDauxVst3Processor *processor) {
  return processor ? processor->process_count : 0;
}

extern "C" double
sphere_daux_vst3_last_input_peak(SphereDauxVst3Processor *processor) {
  return processor ? processor->last_input_peak : 0.0;
}

extern "C" double
sphere_daux_vst3_last_output_peak(SphereDauxVst3Processor *processor) {
  return processor ? processor->last_output_peak : 0.0;
}

extern "C" double
sphere_daux_vst3_last_difference_peak(SphereDauxVst3Processor *processor) {
  return processor ? processor->last_difference_peak : 0.0;
}

extern "C" int sphere_daux_vst3_is_valid(SphereDauxVst3Processor *processor) {
#if defined(_WIN32) || defined(__APPLE__) || defined(__linux__)
  return (processor &&
          processor->processor_valid.load(std::memory_order_acquire))
             ? 1
             : 0;
#else
  return processor ? 1 : 0;
#endif
}

extern "C" int
sphere_daux_vst3_get_latency_samples(SphereDauxVst3Processor *processor) {
  if (!processor || !processor->processor)
    return 0;
  const auto latency = processor->processor->getLatencySamples();
  return static_cast<int>(latency);
}

// ── ARA 2 entry points ───────────────────────────────────────────────────────
//
// An ARA-capable VST3 registers a second factory class in the
// `kARAMainFactoryClass` category whose name equals its audio-module class name.
// Only that pairing is trusted here: a main factory with no name match belongs
// to a different plug-in in the same shell module.

namespace {

/// The ARA main-factory class matching `processor`'s audio-module class, if any.
VST3::Optional<VST3::UID>
daux_ara_main_factory_uid(SphereDauxVst3Processor *processor) {
  if (!processor || !processor->module)
    return {};
  const auto factory = processor->module->getFactory();

  // Same pairing rule the scanner applies (`vst3_scanner.cpp`), and for the same
  // reason: `ARAVST3.h` requires equal class names only for shell binaries that
  // host several plug-ins, and calls the single-plug-in case trivial. Vendors do
  // rename the ARA class — Synchro Arts ships "RePitch VST" beside
  // "RePitch VSTAra" — so a name-only match here would advertise a plug-in in the
  // menu that then refuses to open.
  VST3::Optional<VST3::UID> only_ara;
  int ara_count = 0;
  int audio_count = 0;
  for (const auto &info : factory.classInfos()) {
    if (info.category() == kARAMainFactoryClass) {
      ++ara_count;
      if (ara_count == 1)
        only_ara = VST3::Optional<VST3::UID>(info.ID());
      if (!processor->class_name.empty() && info.name() == processor->class_name)
        return VST3::Optional<VST3::UID>(info.ID());
    } else if (info.category() == kVstAudioEffectClass) {
      ++audio_count;
    }
  }
  if (audio_count == 1 && ara_count == 1)
    return only_ara;
  return {};
}

} // namespace

extern "C" int
sphere_daux_vst3_ara_is_supported(SphereDauxVst3Processor *processor) {
  return daux_ara_main_factory_uid(processor) ? 1 : 0;
}

extern "C" void *
sphere_daux_vst3_ara_create_main_factory(SphereDauxVst3Processor *processor) {
  set_last_error("");
  const auto uid = daux_ara_main_factory_uid(processor);
  if (!uid) {
    set_last_error("module has no matching ARA main factory class");
    return nullptr;
  }
  auto instance =
      processor->module->getFactory().createInstance<Steinberg::FUnknown>(*uid);
  if (!instance) {
    set_last_error("ARA main factory class could not be instantiated");
    return nullptr;
  }
  // Hand the caller the reference `createInstance` already produced; the
  // matching release is `sphere_daux_vst3_ara_release_main_factory`.
  return static_cast<void *>(instance.take());
}

extern "C" void sphere_daux_vst3_ara_release_main_factory(void *unknown) {
  if (unknown) {
    static_cast<Steinberg::FUnknown *>(unknown)->release();
  }
}

extern "C" void *
sphere_daux_vst3_ara_component_unknown(SphereDauxVst3Processor *processor) {
  if (!processor || !processor->component)
    return nullptr;
  // Borrowed: the component is owned by the processor, and ARA binding must not
  // outlive it.
  return static_cast<void *>(
      static_cast<Steinberg::FUnknown *>(processor->component.get()));
}

// ── Plugin state persistence ─────────────────────────────────────────────────
//
// Raw VST3 state streams, exactly as the plugin writes them:
//   component  — IComponent::getState (the processor state; this is the blob
//                IEditController::setComponentState receives on restore)
//   controller — IEditController::getState (GUI-side state; only meaningful
//                for split component/controller plugins)
// Buffers returned by get_state are malloc-owned by the caller and must be
// released with sphere_daux_vst3_state_free.

namespace {

// Copy a captured MemoryStream into a malloc buffer. An empty stream is a
// valid (zero-length) state, not an error.
bool daux_copy_stream(const Steinberg::MemoryStream &stream,
                      unsigned char **out_data, int *out_len) {
  const auto size = stream.getSize();
  if (size <= 0) {
    *out_data = nullptr;
    *out_len = 0;
    return true;
  }
  auto *buf =
      static_cast<unsigned char *>(std::malloc(static_cast<size_t>(size)));
  if (!buf)
    return false;
  std::memcpy(buf, stream.getData(), static_cast<size_t>(size));
  *out_data = buf;
  *out_len = static_cast<int>(size);
  return true;
}

} // namespace

// Capture the plugin's current state. Returns 1 on success (zero-length blobs
// are valid — some plugins have no state), 0 on failure. A plugin returning
// kNotImplemented from getState is treated as "no state", not failure.
extern "C" int sphere_daux_vst3_get_state(SphereDauxVst3Processor *processor,
                                          unsigned char **out_component,
                                          int *out_component_len,
                                          unsigned char **out_controller,
                                          int *out_controller_len) {
  if (out_component)
    *out_component = nullptr;
  if (out_component_len)
    *out_component_len = 0;
  if (out_controller)
    *out_controller = nullptr;
  if (out_controller_len)
    *out_controller_len = 0;
  if (!processor || !processor->component || !out_component ||
      !out_component_len || !out_controller || !out_controller_len) {
    return 0;
  }

  Steinberg::MemoryStream component_stream;
  const auto comp_res = processor->component->getState(&component_stream);
  if (comp_res != Steinberg::kResultOk) {
    component_stream.setSize(0);
  }

  Steinberg::MemoryStream controller_stream;
  // For single-component plugins the controller IS the component — its state
  // already lives in the component stream; querying it again would duplicate.
  if (processor->controller && !processor->controller_is_component) {
    const auto ctrl_res = processor->controller->getState(&controller_stream);
    if (ctrl_res != Steinberg::kResultOk) {
      controller_stream.setSize(0);
    }
  }

  if (!daux_copy_stream(component_stream, out_component, out_component_len))
    return 0;
  if (!daux_copy_stream(controller_stream, out_controller,
                        out_controller_len)) {
    std::free(*out_component);
    *out_component = nullptr;
    *out_component_len = 0;
    return 0;
  }
  std::fprintf(stderr,
               "[SphereVST3] get_state ok component_bytes=%d "
               "controller_bytes=%d comp_result=0x%x\n",
               *out_component_len, *out_controller_len,
               static_cast<unsigned>(comp_res));
  return 1;
}

// Restore a previously captured state. Restore order follows the VST3
// workflow: IComponent::setState, then IEditController::setComponentState
// (same component blob, fresh cursor), then IEditController::setState with
// the controller blob. Returns 1 when the component state was applied.
extern "C" int sphere_daux_vst3_set_state(SphereDauxVst3Processor *processor,
                                          const unsigned char *component_data,
                                          int component_len,
                                          const unsigned char *controller_data,
                                          int controller_len) {
  if (!processor || !processor->component)
    return 0;

  int ok = 1;
  if (component_data && component_len > 0) {
    // Non-owning stream over the caller's buffer; cursor starts at 0.
    Steinberg::MemoryStream comp_stream(
        const_cast<unsigned char *>(component_data), component_len);
    const auto res = processor->component->setState(&comp_stream);
    if (res != Steinberg::kResultOk) {
      std::fprintf(
          stderr,
          "[SphereVST3] set_state component setState result=0x%x bytes=%d\n",
          static_cast<unsigned>(res), component_len);
      ok = (res == Steinberg::kNotImplemented) ? 1 : 0;
    }
    if (processor->controller && !processor->controller_is_component) {
      Steinberg::MemoryStream sync_stream(
          const_cast<unsigned char *>(component_data), component_len);
      const auto sync_res =
          processor->controller->setComponentState(&sync_stream);
      if (sync_res != Steinberg::kResultOk) {
        std::fprintf(
            stderr,
            "[SphereVST3] set_state controller setComponentState result=0x%x\n",
            static_cast<unsigned>(sync_res));
      }
    }
  }
  if (controller_data && controller_len > 0 && processor->controller &&
      !processor->controller_is_component) {
    Steinberg::MemoryStream ctrl_stream(
        const_cast<unsigned char *>(controller_data), controller_len);
    const auto res = processor->controller->setState(&ctrl_stream);
    if (res != Steinberg::kResultOk) {
      std::fprintf(
          stderr,
          "[SphereVST3] set_state controller setState result=0x%x bytes=%d\n",
          static_cast<unsigned>(res), controller_len);
    }
  }
  std::fprintf(stderr,
               "[SphereVST3] set_state applied component_bytes=%d "
               "controller_bytes=%d ok=%d\n",
               component_len, controller_len, ok);
  return ok;
}

extern "C" void sphere_daux_vst3_state_free(unsigned char *data) {
  std::free(data);
}

namespace {

std::string daux_json_escape(const std::string &value) {
  std::string out;
  out.reserve(value.size() + 8);
  out.push_back('"');
  for (const char ch : value) {
    switch (ch) {
    case '"':
      out += "\\\"";
      break;
    case '\\':
      out += "\\\\";
      break;
    case '\n':
      out += "\\n";
      break;
    case '\r':
      out += "\\r";
      break;
    case '\t':
      out += "\\t";
      break;
    default:
      if (static_cast<unsigned char>(ch) < 0x20) {
        char buf[7];
        std::snprintf(buf, sizeof(buf), "\\u%04x",
                      static_cast<unsigned char>(ch));
        out += buf;
      } else {
        out.push_back(ch);
      }
      break;
    }
  }
  out.push_back('"');
  return out;
}

std::string
daux_append_parameter_json(Steinberg::Vst::IEditController *controller,
                           const Steinberg::Vst::ParameterInfo &info) {
  const std::string title = vst3_tchar_to_utf8(info.title);
  const std::string short_title = vst3_tchar_to_utf8(info.shortTitle);
  const std::string unit = vst3_tchar_to_utf8(info.units);
  const bool automatable =
      (info.flags & Steinberg::Vst::ParameterInfo::kCanAutomate) != 0;
  const bool hidden =
      (info.flags & Steinberg::Vst::ParameterInfo::kIsHidden) != 0;
  const bool read_only =
      (info.flags & Steinberg::Vst::ParameterInfo::kIsReadOnly) != 0;
  const double value_normalized = controller->getParamNormalized(info.id);

  std::string json = "{";
  json += "\"id\":" + std::to_string(info.id) + ",";
  json += "\"title\":" + daux_json_escape(title) + ",";
  json += "\"short_title\":" + daux_json_escape(short_title) + ",";
  json += "\"unit\":" + daux_json_escape(unit) + ",";
  json +=
      std::string("\"automatable\":") + (automatable ? "true" : "false") + ",";
  json += std::string("\"hidden\":") + (hidden ? "true" : "false") + ",";
  json += std::string("\"read_only\":") + (read_only ? "true" : "false") + ",";
  json += "\"value_normalized\":" + std::to_string(value_normalized);
  json += "}";
  return json;
}

} // namespace

extern "C" char *
sphere_daux_vst3_list_parameters_json(SphereDauxVst3Processor *processor) {
  if (!processor || !processor->controller) {
    return nullptr;
  }
  Steinberg::Vst::IEditController *controller = processor->controller;
  const Steinberg::int32 count = controller->getParameterCount();
  std::string json = "[";
  bool first = true;
  for (Steinberg::int32 i = 0; i < count; ++i) {
    Steinberg::Vst::ParameterInfo info{};
    if (controller->getParameterInfo(i, info) != Steinberg::kResultOk) {
      continue;
    }
    if (!first) {
      json += ",";
    }
    first = false;
    json += daux_append_parameter_json(controller, info);
  }
  json += "]";
  char *out = static_cast<char *>(std::malloc(json.size() + 1));
  if (!out) {
    return nullptr;
  }
  std::memcpy(out, json.c_str(), json.size() + 1);
  return out;
}

extern "C" void sphere_daux_vst3_parameters_json_free(char *data) {
  std::free(data);
}
