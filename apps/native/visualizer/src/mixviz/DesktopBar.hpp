#pragma once
#include <SDL3/SDL.h>

#include <cstdint>

namespace mixviz {

// Docks an SDL window as a Windows application desktop toolbar (AppBar) along
// the top edge of the primary monitor: full monitor width, fixed height, always
// on top. The shell reserves that strip, so maximized windows stay below it.
class DesktopBar {
public:
    DesktopBar() = default;
    ~DesktopBar() { detach(); }
    DesktopBar(const DesktopBar&) = delete;
    DesktopBar& operator=(const DesktopBar&) = delete;

    // Call before SDL_Init so window sizes are real (physical) pixels.
    static void enableDpiAwareness();

    // heightPx is in physical pixels.
    bool attach(SDL_Window* window, int heightPx);
    void detach(); // unregisters the AppBar; must run before exit to free the reserved strip
    bool attached() const { return hwnd_ != nullptr; }

    // Re-claims the strip (after taskbar moves, display or DPI changes).
    void reposition();

    // Changes the reserved strip's height (physical pixels) while attached.
    void setHeight(int heightPx) {
        height_ = heightPx;
        reposition();
    }

    // SDL does not notice size changes made behind its back, so the bar records the
    // slot it claimed and the main loop applies it through SDL. Returns true once
    // per change.
    bool takeSlot(int& x, int& y, int& w, int& h);

    // Internal: window-message hook (called from the Win32 subclass procedure).
    bool onMessage(unsigned msg, uintptr_t wParam, intptr_t lParam);

private:
    int slot_[4] = {0, 0, 0, 0};
    bool slotChanged_ = false;
    void* hwnd_ = nullptr;
    unsigned callbackMsg_ = 0;
    int height_ = 320;
};

} // namespace mixviz
