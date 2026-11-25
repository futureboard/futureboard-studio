#include "DesktopBar.hpp"

#include <windows.h>
#include <commctrl.h>
#include <shellapi.h>

#include <algorithm>

namespace mixviz {

namespace {

constexpr UINT_PTR kSubclassId = 0x4D58565A; // "MXVZ"

LRESULT CALLBACK barSubclassProc(HWND hwnd, UINT msg, WPARAM wParam, LPARAM lParam, UINT_PTR, DWORD_PTR ref) {
    auto* bar = reinterpret_cast<DesktopBar*>(ref);
    if (msg == WM_NCDESTROY) {
        bar->detach();
        RemoveWindowSubclass(hwnd, barSubclassProc, kSubclassId);
        return DefSubclassProc(hwnd, msg, wParam, lParam);
    }
    if (bar->onMessage(msg, wParam, lParam)) return 0;
    const LRESULT result = DefSubclassProc(hwnd, msg, wParam, lParam);
    // Let SDL apply its own DPI/display handling first, then snap back into the bar slot.
    if (msg == WM_DPICHANGED || msg == WM_DISPLAYCHANGE) bar->reposition();
    return result;
}

APPBARDATA makeData(HWND hwnd) {
    APPBARDATA abd{};
    abd.cbSize = sizeof(abd);
    abd.hWnd = hwnd;
    abd.uEdge = ABE_TOP;
    return abd;
}

} // namespace

void DesktopBar::enableDpiAwareness() {
    SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
}

bool DesktopBar::attach(SDL_Window* window, int heightPx) {
    detach();
    HWND hwnd = static_cast<HWND>(
        SDL_GetPointerProperty(SDL_GetWindowProperties(window), SDL_PROP_WINDOW_WIN32_HWND_POINTER, nullptr));
    if (!hwnd) return false;

    height_ = heightPx;
    callbackMsg_ = RegisterWindowMessageW(L"MixvizAppBarNotify");

    APPBARDATA abd = makeData(hwnd);
    abd.uCallbackMessage = callbackMsg_;
    if (!SHAppBarMessage(ABM_NEW, &abd)) return false;
    hwnd_ = hwnd;

    SetWindowSubclass(hwnd, barSubclassProc, kSubclassId, reinterpret_cast<DWORD_PTR>(this));
    reposition();
    return true;
}

void DesktopBar::detach() {
    if (!hwnd_) return;
    HWND hwnd = static_cast<HWND>(hwnd_);
    hwnd_ = nullptr;
    APPBARDATA abd = makeData(hwnd);
    SHAppBarMessage(ABM_REMOVE, &abd);
    RemoveWindowSubclass(hwnd, barSubclassProc, kSubclassId);
}

void DesktopBar::reposition() {
    if (!hwnd_) return;
    HWND hwnd = static_cast<HWND>(hwnd_);

    MONITORINFO mi{};
    mi.cbSize = sizeof(mi);
    GetMonitorInfoW(MonitorFromPoint(POINT{0, 0}, MONITOR_DEFAULTTOPRIMARY), &mi);

    // Propose the full-width strip, let the shell adjust for other bars, then fix the height.
    APPBARDATA abd = makeData(hwnd);
    abd.rc = mi.rcMonitor;
    abd.rc.bottom = abd.rc.top + height_;
    SHAppBarMessage(ABM_QUERYPOS, &abd);
    abd.rc.bottom = abd.rc.top + height_;
    SHAppBarMessage(ABM_SETPOS, &abd);

    // The window itself is moved by SDL (see takeSlot); moving it here directly would
    // leave SDL's cached size, and therefore the renderer, out of date.
    const int slot[4] = {abd.rc.left, abd.rc.top, abd.rc.right - abd.rc.left, abd.rc.bottom - abd.rc.top};
    if (!std::equal(slot, slot + 4, slot_)) {
        std::copy(slot, slot + 4, slot_);
        slotChanged_ = true;
    }
}

bool DesktopBar::takeSlot(int& x, int& y, int& w, int& h) {
    if (!slotChanged_) return false;
    slotChanged_ = false;
    x = slot_[0]; y = slot_[1]; w = slot_[2]; h = slot_[3];
    return true;
}

bool DesktopBar::onMessage(unsigned msg, uintptr_t wParam, intptr_t lParam) {
    if (!hwnd_) return false;
    HWND hwnd = static_cast<HWND>(hwnd_);

    if (msg == callbackMsg_) {
        switch (wParam) {
        case ABN_POSCHANGED: // taskbar or another AppBar moved
            reposition();
            break;
        case ABN_FULLSCREENAPP: // step aside while a fullscreen app (game, video) is active
            SetWindowPos(hwnd, lParam ? HWND_BOTTOM : HWND_TOPMOST, 0, 0, 0, 0,
                         SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            break;
        default: break;
        }
        return true;
    }
    if (msg == WM_ACTIVATE || msg == WM_WINDOWPOSCHANGED) {
        APPBARDATA abd = makeData(hwnd);
        SHAppBarMessage(msg == WM_ACTIVATE ? ABM_ACTIVATE : ABM_WINDOWPOSCHANGED, &abd);
    }
    return false; // let SDL see everything else
}

} // namespace mixviz
