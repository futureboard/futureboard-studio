// editor_linux.cpp — GTK4 + X11 IPlugView embedding for Linux
//
// VST3 on Linux uses kPlatformTypeX11EmbedWindowID ("X11EmbedWindowID"):
// the host provides an X11 Window ID; the plugin uses the XEmbed protocol
// to reparent its own window inside ours.
//
// Architecture:
//  - GTK4 and its GLib main loop run on a dedicated background thread
//    ("the GTK thread") started lazily on the first editor open.
//  - All GLib/GTK operations are executed on that thread via g_idle_add()
//    synchronised with a mutex + condvar so callers from the Electron / Node
//    thread can wait for results.
//  - The X11 XID is retrieved via GDK's X11 backend API after the window is
//    realized.  If the GDK backend is not X11 (e.g. Wayland), opening fails
//    with a clear error message — VST3 Wayland embedding is not yet standard.
//
// VST3 spec reference:
//   pluginterfaces/gui/iplugview.h — kPlatformTypeX11EmbedWindowID = "X11EmbedWindowID"

#include <algorithm>
#include <condition_variable>
#include <cstdio>
#include <cstring>
#include <mutex>
#include <thread>
#include <vector>

#include <gtk/gtk.h>
#include <glib-unix.h>

#ifdef GDK_WINDOWING_X11
#  include <gdk/x11/gdkx.h>
#  include <X11/Xlib.h>
#  include <X11/keysym.h>
#endif
#ifdef GDK_WINDOWING_WAYLAND
#  include <gdk/wayland/gdkwayland.h>
#endif

#include "pluginterfaces/base/funknown.h"
#include "pluginterfaces/gui/iplugview.h"

#include "sphere_daux_editor_bridge.h"
#include "linux_editor_transport_key.hpp"

// The Linux run-loop interfaces are GUI-layer IIDs that the SDK IID TUs we
// compile (coreiids.cpp / vstinitiids.cpp) do not emit. Our IRunLoop frame's
// queryInterface references IRunLoop::iid, so define the symbol here (exactly
// once, in this the only Linux TU). IPlugFrame::iid is defined in
// vst3_processor.cpp and referenced here as extern.
namespace Steinberg {
namespace Linux {
DEF_CLASS_IID(IRunLoop)
} // namespace Linux
} // namespace Steinberg

// ── Forward declarations ──────────────────────────────────────────────────────

void close_editor_linux(SphereDauxVst3Processor* proc);
int  focus_editor_linux(SphereDauxVst3Processor* proc);

// ── GTK main-loop thread ──────────────────────────────────────────────────────

namespace {

GMainLoop*              s_main_loop    = nullptr;
std::mutex              s_init_mutex;
std::condition_variable s_init_cv;
bool                    s_gtk_ready    = false;

// Space is claimed in linux_editor_transport_key.hpp (shared with the CLAP
// editor). The counter it increments is drained by the plug-in host.
void install_transport_key_handlers(GtkWidget* window) {
    sphere_linux_transport::watch_editor_window(window);
}

void ungrab_transport_key_on_x11_window(GtkWidget* window) {
    sphere_linux_transport::unwatch_editor_window(window);
}

/// Entry point for the dedicated GTK event-loop thread.
void gtk_thread_main() {
    // Must run before any other Xlib call in the process. Xlib is not
    // thread-safe by default, and a plug-in editor may open its own GLX
    // context on its own render thread once attached (e.g. a baseview/glow
    // editor), concurrently with GDK's own X11 traffic on this thread.
    // Defensive: the specific SIGSEGV-in-glXChooseFBConfig crash this file was
    // last debugged for turned out to be a plug-in-thread stack overflow (see
    // `raise_default_thread_stack_for_plugin_editors` in
    // futureboard_plugin_host.rs), not an unguarded-Xlib race — but running two
    // X11 clients on separate threads in one process without this is still
    // undefined per the Xlib spec, so it stays.
    XInitThreads();
    gtk_init(); // GTK4: no argc/argv

    {
        std::lock_guard<std::mutex> lk(s_init_mutex);
        s_main_loop = g_main_loop_new(nullptr, FALSE);
        s_gtk_ready = true;
    }
    s_init_cv.notify_all();

    g_main_loop_run(s_main_loop); // blocks until g_main_loop_quit()

    g_main_loop_unref(s_main_loop);
    s_main_loop = nullptr;
}

/// Ensure the GTK thread is running.  Blocks until GTK is fully initialised.
/// Returns true on success.
bool ensure_gtk() {
    static std::once_flag once;
    std::call_once(once, []() {
        std::thread(gtk_thread_main).detach();
    });

    std::unique_lock<std::mutex> lk(s_init_mutex);
    return s_init_cv.wait_for(lk, std::chrono::seconds(5),
                              [] { return s_gtk_ready; });
}

// ── IPlugFrame + Linux::IRunLoop ─────────────────────────────────────────────
//
// VST3 on Linux has no global event loop, so the host must hand the plug-in an
// IRunLoop (queried off the IPlugFrame). The plug-in registers its X11 socket
// file descriptor and repaint timers through it; the host drives them from its
// own main loop. Here we bridge those registrations onto the GTK thread's GLib
// main context. Without this the editor window attaches but never repaints.
//
// All methods run on the GTK thread: the plug-in calls them from attached() /
// removed() / its own event and timer callbacks, all of which we dispatch on
// the GTK thread. No locking is therefore required for the registration list.
class LinuxRunLoopFrame final : public Steinberg::IPlugFrame,
                                public Steinberg::Linux::IRunLoop {
public:
    LinuxRunLoopFrame(SphereDauxVst3Processor* proc, GtkWindow* window)
        : proc_(proc), window_(window) {}

    ~LinuxRunLoopFrame() {
        // Drop leftover GLib sources. Do NOT release handlers here: after
        // IPlugView::removed() many plugins (JUCE / Surge XT) destroy their
        // ITimerHandler / IEventHandler objects without unregistering, so
        // handler->release() is a use-after-free (host SIGSEGV on editor close).
        // Clean plugins already called unregister* and emptied regs_.
        disarm_sources(/*release_handlers=*/false);
    }

    /// Remove every GLib fd/timer source. When `release_handlers` is false the
    /// COM refs are abandoned (safe after removed()); when true they are
    /// released (only safe while the plug-in still owns the handlers).
    void disarm_sources(bool release_handlers) {
        for (auto* reg : regs_) {
            if (reg->source_id) {
                g_source_remove(reg->source_id);
                reg->source_id = 0;
            }
            if (release_handlers && reg->handler) {
                reg->handler->release();
            }
            reg->handler = nullptr;
            delete reg;
        }
        regs_.clear();
    }

    // ── IPlugFrame ───────────────────────────────────────────────────────────
    Steinberg::tresult PLUGIN_API resizeView(Steinberg::IPlugView* view,
                                             Steinberg::ViewRect* newSize) override {
        if (!view || !newSize) return Steinberg::kInvalidArgument;
        const int w = newSize->right - newSize->left;
        const int h = newSize->bottom - newSize->top;
        if (window_ && w > 0 && h > 0 && !resizing_) {
            resizing_ = true;
            gtk_window_set_default_size(window_, w, h);
            Steinberg::ViewRect local{0, 0, w, h};
            view->onSize(&local); // let the plug-in fit its child to the new rect
            resizing_ = false;
        }
        return Steinberg::kResultTrue;
    }

    // ── Linux::IRunLoop ────────────────────────────────────────────────────────
    Steinberg::tresult PLUGIN_API registerEventHandler(
        Steinberg::Linux::IEventHandler* handler,
        Steinberg::Linux::FileDescriptor fd) override {
        if (!handler) return Steinberg::kInvalidArgument;
        handler->addRef();
        auto* reg = new Reg{handler, nullptr, fd, 0};
        reg->source_id = g_unix_fd_add_full(
            G_PRIORITY_DEFAULT, fd,
            static_cast<GIOCondition>(G_IO_IN | G_IO_ERR | G_IO_HUP),
            &LinuxRunLoopFrame::fd_cb, reg, nullptr);
        regs_.push_back(reg);
        return Steinberg::kResultTrue;
    }

    Steinberg::tresult PLUGIN_API unregisterEventHandler(
        Steinberg::Linux::IEventHandler* handler) override {
        return remove_reg([&](Reg* r) { return r->handler == handler; });
    }

    Steinberg::tresult PLUGIN_API registerTimer(
        Steinberg::Linux::ITimerHandler* handler,
        Steinberg::Linux::TimerInterval milliseconds) override {
        if (!handler) return Steinberg::kInvalidArgument;
        if (milliseconds == 0) milliseconds = 1; // GLib requires a non-zero period
        handler->addRef();
        auto* reg = new Reg{handler, nullptr, -1, 0};
        reg->source_id = g_timeout_add_full(
            G_PRIORITY_DEFAULT, static_cast<guint>(milliseconds),
            &LinuxRunLoopFrame::timer_cb, reg, nullptr);
        regs_.push_back(reg);
        return Steinberg::kResultTrue;
    }

    Steinberg::tresult PLUGIN_API unregisterTimer(
        Steinberg::Linux::ITimerHandler* handler) override {
        return remove_reg(
            [&](Reg* r) { return r->handler == handler; });
    }

    // ── FUnknown (shared final overrider for both interface paths) ──────────────
    Steinberg::tresult PLUGIN_API queryInterface(const Steinberg::TUID iid,
                                                 void** obj) override {
        if (Steinberg::FUnknownPrivate::iidEqual(iid,
                                                 Steinberg::Linux::IRunLoop::iid)) {
            *obj = static_cast<Steinberg::Linux::IRunLoop*>(this);
            addRef();
            return Steinberg::kResultTrue;
        }
        if (Steinberg::FUnknownPrivate::iidEqual(iid, Steinberg::IPlugFrame::iid) ||
            Steinberg::FUnknownPrivate::iidEqual(iid, Steinberg::FUnknown::iid)) {
            *obj = static_cast<Steinberg::IPlugFrame*>(this);
            addRef();
            return Steinberg::kResultTrue;
        }
        *obj = nullptr;
        return Steinberg::kNoInterface;
    }
    // Lifetime is owned by the editor window, not the plug-in: a plug-in
    // release() must not destroy us (matches the Windows PluginEditorFrame).
    Steinberg::uint32 PLUGIN_API addRef() override { return 1000; }
    Steinberg::uint32 PLUGIN_API release() override { return 1000; }

private:
    // One registration record; `handler` is stored as FUnknown* so a single
    // list holds both event- and timer-handlers (they share addRef/release).
    struct Reg {
        Steinberg::FUnknown* handler{nullptr};
        void*                unused{nullptr};
        int                  fd{-1};
        guint                source_id{0};
    };

    static gboolean fd_cb(gint fd, GIOCondition, gpointer ud) {
        auto* reg = static_cast<Reg*>(ud);
        static_cast<Steinberg::Linux::IEventHandler*>(
            static_cast<void*>(reg->handler))
            ->onFDIsSet(fd);
        return G_SOURCE_CONTINUE;
    }

    static gboolean timer_cb(gpointer ud) {
        auto* reg = static_cast<Reg*>(ud);
        static_cast<Steinberg::Linux::ITimerHandler*>(
            static_cast<void*>(reg->handler))
            ->onTimer();
        return G_SOURCE_CONTINUE;
    }

    template <typename Pred>
    Steinberg::tresult remove_reg(Pred pred) {
        for (auto it = regs_.begin(); it != regs_.end(); ++it) {
            if (pred(*it)) {
                if ((*it)->source_id) g_source_remove((*it)->source_id);
                if ((*it)->handler)   (*it)->handler->release();
                delete *it;
                regs_.erase(it);
                return Steinberg::kResultTrue;
            }
        }
        return Steinberg::kResultFalse;
    }

    SphereDauxVst3Processor* proc_{nullptr};
    GtkWindow*               window_{nullptr};
    bool                     resizing_{false};
    std::vector<Reg*>        regs_;
};

// ── Resize callback ────────────────────────────────────────────────────────

struct ResizeCtx {
    SphereDauxVst3Processor* proc;
};

#ifdef GDK_WINDOWING_X11
static void on_surface_layout(GdkSurface* /*surface*/,
                               int          width,
                               int          height,
                               gpointer     user_data) {
    auto* ctx = static_cast<ResizeCtx*>(user_data);
    if (ctx && ctx->proc) {
        sphere_daux_editor_notify_resize(ctx->proc, width, height);
    }
}
#endif

// ── Teardown (must run on the GTK thread) ─────────────────────────────────
//
// Shared by idle_close_editor (cross-thread close request) and the window's
// own close-request handler (user pressed the titlebar X). Both already run on
// the GTK thread, so this runs the teardown inline — it must NOT go through
// close_editor_linux() from the close-request handler, which would g_idle_add()
// and then block-wait on the GTK thread for an idle source that can only run
// once the handler returns (self-deadlock). Idempotent.
struct DestroyWindowTask {
    GtkWidget*         window{nullptr};
    LinuxRunLoopFrame* frame{nullptr};
};

// Idle destroy: never call gtk_window_destroy from inside close-request.
// GTK4/GDK asserts if the surface still has an EGL native window (common when
// a GL/XEmbed plug-in like Surge XT just detached) — flushing the main context
// after removed() lets the plug-in drop its GL child first.
static gboolean idle_destroy_editor_window(gpointer user_data) {
    auto* task = static_cast<DestroyWindowTask*>(user_data);
    // Let pending X11/unmap/GL teardown from IPlugView::removed() settle.
    for (int i = 0; i < 8; ++i) {
        if (!g_main_context_iteration(nullptr, FALSE)) break;
    }
    if (task->window) {
        gtk_widget_set_visible(task->window, FALSE);
        for (int i = 0; i < 4; ++i) {
            if (!g_main_context_iteration(nullptr, FALSE)) break;
        }
        gtk_window_destroy(GTK_WINDOW(task->window));
        g_object_unref(task->window); // matches g_object_ref in idle_open_editor
        task->window = nullptr;
    }
    delete task->frame;
    task->frame = nullptr;
    delete task;
    std::fprintf(stderr, "[SphereVST3/linux] editor closed\n");
    return G_SOURCE_REMOVE;
}

static void close_editor_on_gtk_thread(SphereDauxVst3Processor* proc) {
    void* win_ptr = sphere_daux_editor_get_native_window(proc);
    if (!win_ptr) return;

    ungrab_transport_key_on_x11_window(static_cast<GtkWidget*>(win_ptr));

    // Grab the IRunLoop frame (stored in native_embed) before clearing.
    auto* frame = static_cast<LinuxRunLoopFrame*>(
        sphere_daux_editor_get_native_embed(proc));

    sphere_daux_editor_clear_native(proc); // zero first to prevent re-entrancy
    // Steinberg order: setFrame(nullptr) then removed(). Plug-ins that clean up
    // unregister their IRunLoop handlers during these calls.
    sphere_daux_editor_set_frame(proc, nullptr);
    sphere_daux_editor_detach_view(proc);

    // Disarm leftover GLib sources. Never release leftover handlers — they may
    // already be freed inside removed() (JUCE/Surge).
    if (frame) {
        frame->disarm_sources(/*release_handlers=*/false);
    }

    // Defer GTK destroy off the close-request stack and after removed() so
    // GDK's egl_native_window can clear (avoids gdksurface.c assertion abort).
    auto* task = new DestroyWindowTask{static_cast<GtkWidget*>(win_ptr), frame};
    g_idle_add(idle_destroy_editor_window, task);
}

// ── Open-task (executed on the GTK thread via g_idle_add) ─────────────────

struct OpenTask {
    SphereDauxVst3Processor* proc{nullptr};
    const char*              window_id{nullptr};
    const char*              title{nullptr};
    int                      width{0};
    int                      height{0};

    unsigned long long       result{0};
    std::mutex               done_mutex;
    std::condition_variable  done_cv;
    bool                     done{false};
};

static gboolean idle_open_editor(gpointer user_data) {
    auto* task = static_cast<OpenTask*>(user_data);
    SphereDauxVst3Processor* proc = task->proc;

    // ── Create GtkWindow ──────────────────────────────────────────────────

    GtkWidget* window = gtk_window_new();

    const char* title_str = (task->title && *task->title) ? task->title : "Plugin Editor";
    gtk_window_set_title(GTK_WINDOW(window), title_str);
    gtk_window_set_default_size(GTK_WINDOW(window),
                                task->width  > 0 ? task->width  : 820,
                                task->height > 0 ? task->height : 560);
    gtk_window_set_resizable(GTK_WINDOW(window), TRUE);

    // Dark background via CSS provider
    GtkCssProvider* css = gtk_css_provider_new();
    gtk_css_provider_load_from_string(css,
        "window { background-color: #0b0f14; }");
    gtk_style_context_add_provider_for_display(
        gtk_widget_get_display(window),
        GTK_STYLE_PROVIDER(css),
        GTK_STYLE_PROVIDER_PRIORITY_APPLICATION);
    g_object_unref(css);

    // Realize the window to create the underlying GDK surface / X11 window
    // BEFORE querying the IPlugView preferred size (the plugin may need a
    // realised surface for DPI querying on some toolkits).
    gtk_widget_realize(window);

    // ── Query IPlugView preferred size & create the view ─────────────────

    int editor_width  = task->width  > 0 ? task->width  : 820;
    int editor_height = task->height > 0 ? task->height : 560;

    // Official VST3 platform type string (NOT the incorrect "XcbWindow" alias
    // some hosts historically used). Surge XT / VSTGUI / SDK editorhost all
    // check isPlatformTypeSupported("X11EmbedWindowID").
    // Use the literal: Steinberg::kPlatformTypeX11EmbedWindowID is a non-
    // constexpr FIDString (= const char*) in the SDK headers.
    const char* kLinuxPlatformType = Steinberg::kPlatformTypeX11EmbedWindowID;

    if (!sphere_daux_editor_create_view(proc, kLinuxPlatformType,
                                        &editor_width, &editor_height)) {
        std::fprintf(stderr,
                     "[SphereVST3/linux] create_view('%s') failed\n",
                     kLinuxPlatformType);
        gtk_window_destroy(GTK_WINDOW(window));
        goto done;
    }

    gtk_window_set_default_size(GTK_WINDOW(window), editor_width, editor_height);

    // ── Get X11 Window ID ─────────────────────────────────────────────────

    {
        GdkSurface* surface = gtk_native_get_surface(GTK_NATIVE(window));

#ifdef GDK_WINDOWING_X11
        if (!GDK_IS_X11_SURFACE(surface)) {
#else
        if (true) {
#endif
            sphere_daux_editor_set_error(
                "DAUx VST3 editor: GDK backend is not X11 — "
                "VST3 XEmbed requires an X11 display (set GDK_BACKEND=x11)");
            std::fprintf(stderr,
                         "[SphereVST3/linux] GDK backend is not X11; "
                         "cannot obtain XID for VST3 embed\n");
            sphere_daux_editor_detach_view(proc);
            gtk_window_destroy(GTK_WINDOW(window));
            goto done;
        }

#ifdef GDK_WINDOWING_X11
        // Ensure the X11 window exists (realize pushes creation to GDK
        // but the actual XCreateWindow may be deferred until first map on
        // some GDK versions; gdk_x11_surface_get_xid forces it).
        Window xid = gdk_x11_surface_get_xid(surface);
        std::fprintf(stderr,
                     "[SphereVST3/linux] X11 XID=0x%lx w=%d h=%d platform=%s\n",
                     xid, editor_width, editor_height, kLinuxPlatformType);

        // ── Install IPlugFrame + IRunLoop BEFORE attached() ───────────────
        // The plug-in queries the frame for Linux::IRunLoop during attached()
        // to register its X11 fd and repaint timers. Owned here (deleted in
        // idle_close_editor); stored in the otherwise-unused native_embed slot.
        auto* frame = new LinuxRunLoopFrame(proc, GTK_WINDOW(window));
        sphere_daux_editor_set_frame(proc, frame);

        // Map the host window before attach — Steinberg editorhost and VSTGUI
        // XEmbed expect a mapped parent XID when the plug-in reparents.
        gtk_window_present(GTK_WINDOW(window));

        // Space → transport before the plug-in (or its XEmbed child) can eat it.
        install_transport_key_handlers(window);

        // ── Attach IPlugView ──────────────────────────────────────────────
        // Pass the X11 Window ID cast to void* (XEmbed parent).
        if (!sphere_daux_editor_attach_view(proc,
                                            reinterpret_cast<void*>(
                                                static_cast<uintptr_t>(xid)),
                                            kLinuxPlatformType)) {
            std::fprintf(stderr,
                         "[SphereVST3/linux] attach_view('%s') failed\n",
                         kLinuxPlatformType);
            sphere_daux_editor_set_frame(proc, nullptr);
            delete frame;
            ungrab_transport_key_on_x11_window(window);
            gtk_window_destroy(GTK_WINDOW(window));
            goto done;
        }

        // ── Connect resize signal on the GDK surface ──────────────────────
        auto* resize_ctx = new ResizeCtx{proc};
        g_signal_connect_data(
            surface, "layout",
            G_CALLBACK(on_surface_layout),
            resize_ctx,
            [](gpointer data, GClosure*) { delete static_cast<ResizeCtx*>(data); },
            G_CONNECT_DEFAULT);

        // ── Store native state ─────────────────────────────────────────────
        unsigned long long handle = sphere_daux_editor_next_handle();

        // We keep a strong reference to the GtkWidget via g_object_ref.
        g_object_ref(window);

        sphere_daux_editor_store_native(
            proc,
            window,      // native_window  (GtkWidget*, g_object_ref'd)
            frame,       // native_embed   (reused on Linux to hold the IRunLoop frame)
            nullptr,     // native_delegate
            handle,
            task->window_id ? task->window_id : "",
            title_str,
            task->width, task->height);

        // ── Close signal: user pressed the window X button ────────────────
        // We use a lambda-style GCallback via g_signal_connect_data with a
        // heap-allocated copy of the processor pointer.
        struct CloseCtx { SphereDauxVst3Processor* proc; };
        auto* close_ctx = new CloseCtx{proc};
        g_signal_connect_data(
            window, "close-request",
            G_CALLBACK(+[](GtkWindow*, gpointer ud) -> gboolean {
                auto* ctx = static_cast<CloseCtx*>(ud);
                if (ctx->proc) {
                    // Signal the host (host-owned mode) so it reports
                    // EditorClosed, then tear down inline on this (GTK) thread.
                    sphere_daux_editor_signal_user_close(ctx->proc);
                    close_editor_on_gtk_thread(ctx->proc);
                }
                return TRUE; // we destroyed the window ourselves
            }),
            close_ctx,
            [](gpointer data, GClosure*) { delete static_cast<CloseCtx*>(data); },
            G_CONNECT_DEFAULT);

        std::fprintf(stderr,
                     "[SphereVST3/linux] editor opened handle=%llu "
                     "xid=0x%lx windowId=%s\n",
                     handle, xid,
                     task->window_id ? task->window_id : "");

        task->result = handle;
#endif // GDK_WINDOWING_X11
    }

done:
    {
        std::lock_guard<std::mutex> lk(task->done_mutex);
        task->done = true;
    }
    task->done_cv.notify_all();
    return G_SOURCE_REMOVE;
}

// ── Close-task ────────────────────────────────────────────────────────────

struct CloseTask {
    SphereDauxVst3Processor* proc{nullptr};
    std::mutex               done_mutex;
    std::condition_variable  done_cv;
    bool                     done{false};
};

static gboolean idle_close_editor(gpointer user_data) {
    auto* task = static_cast<CloseTask*>(user_data);
    close_editor_on_gtk_thread(task->proc);

    {
        std::lock_guard<std::mutex> lk(task->done_mutex);
        task->done = true;
    }
    task->done_cv.notify_all();
    return G_SOURCE_REMOVE;
}

} // namespace

// ── Public platform functions ─────────────────────────────────────────────────

unsigned long long open_editor_linux(
    SphereDauxVst3Processor* proc,
    const char*              window_id,
    const char*              title,
    int                      width,
    int                      height)
{
    if (!proc) return 0;

    if (!ensure_gtk()) {
        sphere_daux_editor_set_error("GTK4 initialisation timed out");
        std::fprintf(stderr, "[SphereVST3/linux] GTK4 init failed\n");
        return 0;
    }

    // Already open? Focus it.
    if (sphere_daux_editor_get_native_window(proc)) {
        return focus_editor_linux(proc) ? sphere_daux_editor_get_handle(proc) : 0;
    }

    OpenTask task;
    task.proc      = proc;
    task.window_id = window_id;
    task.title     = title;
    task.width     = width;
    task.height    = height;

    g_idle_add(idle_open_editor, &task);

    std::unique_lock<std::mutex> lk(task.done_mutex);
    task.done_cv.wait(lk, [&] { return task.done; });
    return task.result;
}

void close_editor_linux(SphereDauxVst3Processor* proc) {
    if (!proc) return;
    if (!sphere_daux_editor_get_native_window(proc)) return;
    if (!s_gtk_ready) return;

    CloseTask task;
    task.proc = proc;

    g_idle_add(idle_close_editor, &task);

    std::unique_lock<std::mutex> lk(task.done_mutex);
    task.done_cv.wait(lk, [&] { return task.done; });
}

int focus_editor_linux(SphereDauxVst3Processor* proc) {
    if (!proc || !s_gtk_ready) return 0;
    void* win_ptr = sphere_daux_editor_get_native_window(proc);
    if (!win_ptr) return 0;

    g_idle_add([](gpointer ud) -> gboolean {
        auto* window = static_cast<GtkWidget*>(ud);
        gtk_window_present(GTK_WINDOW(window));
        return G_SOURCE_REMOVE;
    }, win_ptr);
    return 1;
}

void shutdown_editor_linux(SphereDauxVst3Processor* proc) {
    close_editor_linux(proc);
}
