// Space → transport for a host-owned GTK plug-in editor on Linux.
//
// Included by each editor translation unit (VST3 and CLAP). Each bridge runs
// its own GTK thread, so the watch list below is per translation unit. Both
// claim the same process-wide counter, which the plug-in host drains into
// one TransportToggleRequested.
//
// Core XGrabKey does not see keys delivered through XInput2, which is how
// GTK and most plug-ins read the keyboard. XGetInputFocus is also often
// None, PointerRoot, or a top-level the plug-in opened in this process
// rather than a child of our GtkWindow. The poll therefore claims a bare
// Space when any of these is true:
//   - X focus (or the WM's active window) is our editor or a descendant
//   - one of our editor GtkWindows is active and not sitting in a text field
//   - the active window belongs to this process and is not a dialog

#pragma once

#include <algorithm>
#include <cstdio>
#include <mutex>
#include <vector>

#include <gtk/gtk.h>

#include <unistd.h>

#ifdef GDK_WINDOWING_X11
#include <gdk/x11/gdkx.h>
#include <X11/Xatom.h>
#include <X11/Xlib.h>
#include <X11/extensions/XInput2.h>
#include <X11/keysym.h>
#endif

extern "C" void sphere_daux_vst3_claim_transport_toggle(void);

namespace sphere_linux_transport {

namespace {

#ifdef GDK_WINDOWING_X11

struct WatchedEditor {
    Window xid;
    GtkWidget* window;
};

std::mutex g_editors_mu;
std::vector<WatchedEditor> g_editors;
guint g_space_poll = 0;
bool g_space_was_down = false;

const unsigned int k_grab_masks[] = {
    0u,
    LockMask,
    Mod2Mask,
    LockMask | Mod2Mask,
    Mod5Mask,
    LockMask | Mod5Mask,
    Mod2Mask | Mod5Mask,
    LockMask | Mod2Mask | Mod5Mask,
};

void claim_debounced(const char* source) {
    static guint64 last_ms = 0;
    const guint64 now = g_get_monotonic_time() / 1000;
    if (last_ms != 0 && now - last_ms < 80) {
        return;
    }
    last_ms = now;
    sphere_daux_vst3_claim_transport_toggle();
    std::fprintf(stderr, "[plugin-editor/linux] transport key claimed source=%s\n",
                 source);
}

bool is_transport_key(guint keyval, GdkModifierType state) {
    if (keyval != GDK_KEY_space && keyval != GDK_KEY_KP_Space) {
        return false;
    }
    const GdkModifierType blocking = static_cast<GdkModifierType>(
        GDK_CONTROL_MASK | GDK_ALT_MASK | GDK_SUPER_MASK | GDK_META_MASK |
        GDK_HYPER_MASK);
    return (state & blocking) == 0;
}

gboolean on_key_pressed(GtkEventControllerKey*, guint keyval, guint,
                        GdkModifierType state, gpointer) {
    if (!is_transport_key(keyval, state)) {
        return FALSE;
    }
    claim_debounced("gtk-capture");
    return TRUE;
}

bool keycode_is_down(const char keys[32], KeyCode code) {
    if (code == 0) {
        return false;
    }
    return (keys[code / 8] & (1 << (code % 8))) != 0;
}

bool xid_is_editor(Window xid) {
    for (const WatchedEditor& editor : g_editors) {
        if (editor.xid == xid) {
            return true;
        }
    }
    return false;
}

// Walk `window` toward the root. True when it is one of our editor surfaces
// or a descendant of one (the XEmbed / CLAP child).
bool window_is_under_editor(Display* dpy, Window window) {
    if (window == None || window == PointerRoot) {
        return false;
    }
    Window current = window;
    for (int depth = 0; depth < 64 && current != None; ++depth) {
        if (xid_is_editor(current)) {
            return true;
        }
        Window root = None;
        Window parent = None;
        Window* children = nullptr;
        unsigned int nchildren = 0;
        if (!XQueryTree(dpy, current, &root, &parent, &children, &nchildren)) {
            break;
        }
        if (children) {
            XFree(children);
        }
        if (parent == None || parent == current || parent == root) {
            break;
        }
        current = parent;
    }
    return false;
}

bool property_cardinal(Display* dpy, Window window, Atom prop, unsigned long* out) {
    Atom actual = None;
    int format = 0;
    unsigned long count = 0;
    unsigned long bytes = 0;
    unsigned char* data = nullptr;
    const int status = XGetWindowProperty(dpy, window, prop, 0, 1, False, AnyPropertyType,
                                          &actual, &format, &count, &bytes, &data);
    if (status != Success || data == nullptr || count < 1 || format != 32) {
        if (data) {
            XFree(data);
        }
        return false;
    }
    *out = static_cast<unsigned long>(*reinterpret_cast<unsigned long*>(data));
    XFree(data);
    return true;
}

bool window_is_dialog(Display* dpy, Window window) {
    Atom type_prop = XInternAtom(dpy, "_NET_WM_WINDOW_TYPE", False);
    Atom dialog = XInternAtom(dpy, "_NET_WM_WINDOW_TYPE_DIALOG", False);
    Atom actual = None;
    int format = 0;
    unsigned long count = 0;
    unsigned long bytes = 0;
    unsigned char* data = nullptr;
    const int status = XGetWindowProperty(dpy, window, type_prop, 0, 8, False, XA_ATOM,
                                          &actual, &format, &count, &bytes, &data);
    if (status != Success || data == nullptr) {
        if (data) {
            XFree(data);
        }
        return false;
    }
    bool found = false;
    if (format == 32) {
        auto* atoms = reinterpret_cast<unsigned long*>(data);
        for (unsigned long i = 0; i < count; ++i) {
            if (static_cast<Atom>(atoms[i]) == dialog) {
                found = true;
                break;
            }
        }
    }
    XFree(data);
    return found;
}

// A plug-in that opens its own top-level (not an XEmbed child) still lives in
// this process. A file dialog is `_NET_WM_WINDOW_TYPE_DIALOG` somewhere in
// the chain and keeps Space for its text field, even when a child of that
// dialog is the window the WM calls active.
bool active_window_is_our_editor(Display* dpy, Window active) {
    if (active == None || active == PointerRoot) {
        return false;
    }
    const Atom pid_prop = XInternAtom(dpy, "_NET_WM_PID", False);
    const unsigned long ours = static_cast<unsigned long>(::getpid());
    bool saw_dialog = false;
    bool saw_ours = false;
    Window current = active;
    for (int depth = 0; depth < 64 && current != None; ++depth) {
        if (window_is_dialog(dpy, current)) {
            saw_dialog = true;
        }
        if (xid_is_editor(current)) {
            saw_ours = true;
        }
        unsigned long pid = 0;
        if (property_cardinal(dpy, current, pid_prop, &pid) && pid == ours) {
            saw_ours = true;
        }
        Window root = None;
        Window parent = None;
        Window* children = nullptr;
        unsigned int nchildren = 0;
        if (!XQueryTree(dpy, current, &root, &parent, &children, &nchildren)) {
            break;
        }
        if (children) {
            XFree(children);
        }
        if (parent == None || parent == current || parent == root) {
            break;
        }
        current = parent;
    }
    return saw_ours && !saw_dialog;
}

Window net_active_window(Display* dpy) {
    const Atom prop = XInternAtom(dpy, "_NET_ACTIVE_WINDOW", False);
    unsigned long value = 0;
    if (!property_cardinal(dpy, DefaultRootWindow(dpy), prop, &value)) {
        return None;
    }
    return static_cast<Window>(value);
}

// True when a watched editor is the active toplevel and its own GTK focus is
// not a text caret. Plug-in text fields are not GTK widgets of this window;
// Space there is the transport, same as the Win32 hook outside a text class.
bool watched_gtk_window_wants_space() {
    bool active = false;
    for (const WatchedEditor& editor : g_editors) {
        if (editor.window == nullptr || !GTK_IS_WINDOW(editor.window)) {
            continue;
        }
        if (!gtk_window_is_active(GTK_WINDOW(editor.window))) {
            continue;
        }
        active = true;
        GtkWidget* focus = gtk_window_get_focus(GTK_WINDOW(editor.window));
        if (focus != nullptr && (GTK_IS_EDITABLE(focus) || GTK_IS_TEXT_VIEW(focus))) {
            return false;
        }
    }
    return active;
}

gboolean poll_space(gpointer) {
    {
        std::lock_guard<std::mutex> lock(g_editors_mu);
        if (g_editors.empty()) {
            g_space_poll = 0;
            g_space_was_down = false;
            return G_SOURCE_REMOVE;
        }
    }

    GdkDisplay* display = gdk_display_get_default();
    if (display == nullptr || !GDK_IS_X11_DISPLAY(display)) {
        return G_SOURCE_CONTINUE;
    }
    Display* dpy = gdk_x11_display_get_xdisplay(display);
    if (dpy == nullptr) {
        return G_SOURCE_CONTINUE;
    }

    gdk_x11_display_error_trap_push(display);

    char keys[32] = {};
    XQueryKeymap(dpy, keys);
    const KeyCode space = XKeysymToKeycode(dpy, XK_space);
    const KeyCode kp_space = XKeysymToKeycode(dpy, XK_KP_Space);
    const bool space_down = keycode_is_down(keys, space) || keycode_is_down(keys, kp_space);
    const bool rising = space_down && !g_space_was_down;
    g_space_was_down = space_down;

    const char* source = nullptr;
    if (rising) {
        const KeyCode control_l = XKeysymToKeycode(dpy, XK_Control_L);
        const KeyCode control_r = XKeysymToKeycode(dpy, XK_Control_R);
        const KeyCode alt_l = XKeysymToKeycode(dpy, XK_Alt_L);
        const KeyCode alt_r = XKeysymToKeycode(dpy, XK_Alt_R);
        const KeyCode super_l = XKeysymToKeycode(dpy, XK_Super_L);
        const KeyCode super_r = XKeysymToKeycode(dpy, XK_Super_R);
        const bool modified = keycode_is_down(keys, control_l) || keycode_is_down(keys, control_r) ||
                              keycode_is_down(keys, alt_l) || keycode_is_down(keys, alt_r) ||
                              keycode_is_down(keys, super_l) || keycode_is_down(keys, super_r);
        if (!modified) {
            Window focus = None;
            int revert = RevertToNone;
            XGetInputFocus(dpy, &focus, &revert);
            if (window_is_under_editor(dpy, focus)) {
                source = "x11-focus-poll";
            } else if (watched_gtk_window_wants_space()) {
                source = "gtk-window-active";
            } else {
                const Window active = net_active_window(dpy);
                if (window_is_under_editor(dpy, active) || active_window_is_our_editor(dpy, active)) {
                    source = "x11-active-window";
                }
            }
        }
    }

    gdk_x11_display_error_trap_pop_ignored(display);

    if (source != nullptr) {
        claim_debounced(source);
    }
    return G_SOURCE_CONTINUE;
}

void remember_editor(Window xid, GtkWidget* window) {
    std::lock_guard<std::mutex> lock(g_editors_mu);
    for (const WatchedEditor& editor : g_editors) {
        if (editor.xid == xid) {
            return;
        }
    }
    g_editors.push_back(WatchedEditor{xid, window});
    if (g_space_poll == 0) {
        g_space_poll = g_timeout_add(8, poll_space, nullptr);
    }
}

void forget_editor(Window xid) {
    std::lock_guard<std::mutex> lock(g_editors_mu);
    g_editors.erase(std::remove_if(g_editors.begin(), g_editors.end(),
                                   [xid](const WatchedEditor& editor) {
                                       return editor.xid == xid;
                                   }),
                    g_editors.end());
    if (g_editors.empty() && g_space_poll != 0) {
        g_source_remove(g_space_poll);
        g_space_poll = 0;
        g_space_was_down = false;
    }
}

void grab_xi_space(Display* dpy, Window xid, KeyCode code) {
    int opcode = 0;
    int event = 0;
    int error = 0;
    if (!XQueryExtension(dpy, "XInputExtension", &opcode, &event, &error)) {
        return;
    }
    int major = 2;
    int minor = 2;
    if (XIQueryVersion(dpy, &major, &minor) != Success) {
        return;
    }
    unsigned char bits[XIMaskLen(XI_LASTEVENT)] = {};
    XIEventMask evmask;
    evmask.deviceid = XIAllMasterDevices;
    evmask.mask_len = sizeof(bits);
    evmask.mask = bits;
    XISetMask(bits, XI_KeyPress);
    XISetMask(bits, XI_KeyRelease);
    for (unsigned int mask : k_grab_masks) {
        XIGrabModifiers mods;
        mods.modifiers = static_cast<int>(mask);
        mods.status = 0;
        XIGrabKeycode(dpy, XIAllMasterDevices, static_cast<int>(code), xid, GrabModeAsync,
                      GrabModeAsync, False, &evmask, 1, &mods);
    }
}

void ungrab_xi_space(Display* dpy, Window xid, KeyCode code) {
    int opcode = 0;
    int event = 0;
    int error = 0;
    if (!XQueryExtension(dpy, "XInputExtension", &opcode, &event, &error)) {
        return;
    }
    for (unsigned int mask : k_grab_masks) {
        XIGrabModifiers mods;
        mods.modifiers = static_cast<int>(mask);
        mods.status = 0;
        XIUngrabKeycode(dpy, XIAllMasterDevices, static_cast<int>(code), xid, 1, &mods);
    }
}

void grab_on_surface(GtkWidget* window) {
    if (window == nullptr || !GTK_IS_NATIVE(window)) {
        return;
    }
    GdkSurface* surface = gtk_native_get_surface(GTK_NATIVE(window));
    if (surface == nullptr || !GDK_IS_X11_SURFACE(surface)) {
        return;
    }
    GdkDisplay* display = gdk_surface_get_display(surface);
    Display* dpy = gdk_x11_display_get_xdisplay(display);
    const Window xid = gdk_x11_surface_get_xid(surface);
    const KeyCode code = XKeysymToKeycode(dpy, XK_space);
    if (dpy == nullptr || code == 0 || xid == 0) {
        return;
    }
    gdk_x11_display_error_trap_push(display);
    for (unsigned int mask : k_grab_masks) {
        XGrabKey(dpy, code, mask, xid, False, GrabModeAsync, GrabModeAsync);
    }
    grab_xi_space(dpy, xid, code);
    XFlush(dpy);
    gdk_x11_display_error_trap_pop_ignored(display);
    remember_editor(xid, window);
}

void ungrab_on_surface(GtkWidget* window) {
    if (window == nullptr || !GTK_IS_NATIVE(window)) {
        return;
    }
    GdkSurface* surface = gtk_native_get_surface(GTK_NATIVE(window));
    if (surface == nullptr || !GDK_IS_X11_SURFACE(surface)) {
        return;
    }
    GdkDisplay* display = gdk_surface_get_display(surface);
    Display* dpy = gdk_x11_display_get_xdisplay(display);
    const Window xid = gdk_x11_surface_get_xid(surface);
    const KeyCode code = XKeysymToKeycode(dpy, XK_space);
    if (dpy == nullptr || code == 0 || xid == 0) {
        forget_editor(xid);
        return;
    }
    gdk_x11_display_error_trap_push(display);
    for (unsigned int mask : k_grab_masks) {
        XUngrabKey(dpy, code, mask, xid);
    }
    ungrab_xi_space(dpy, xid, code);
    XFlush(dpy);
    gdk_x11_display_error_trap_pop_ignored(display);
    forget_editor(xid);
}

#endif  // GDK_WINDOWING_X11

}  // namespace

inline void watch_editor_window(GtkWidget* window) {
    if (window == nullptr) {
        return;
    }
    if (g_object_get_data(G_OBJECT(window), "fb-transport-key") != nullptr) {
        return;
    }
    g_object_set_data(G_OBJECT(window), "fb-transport-key", GINT_TO_POINTER(1));
    GtkEventController* controller = gtk_event_controller_key_new();
    gtk_event_controller_set_propagation_phase(controller, GTK_PHASE_CAPTURE);
#ifdef GDK_WINDOWING_X11
    g_signal_connect(controller, "key-pressed", G_CALLBACK(on_key_pressed), nullptr);
#else
    (void)controller;
#endif
    gtk_widget_add_controller(window, controller);
#ifdef GDK_WINDOWING_X11
    grab_on_surface(window);
#else
    (void)window;
#endif
}

inline void unwatch_editor_window(GtkWidget* window) {
#ifdef GDK_WINDOWING_X11
    ungrab_on_surface(window);
#else
    (void)window;
#endif
}

}  // namespace sphere_linux_transport
