// clap_editor_linux.cpp — GTK4 + X11 CLAP GUI hosting for Linux.
//
// CLAP's Linux embedding uses CLAP_WINDOW_API_X11 ("x11", XEmbed, physical
// pixels) — there is no standardized Wayland embedding surface for a foreign
// plug-in window (clap/ext/gui.h says so directly: "embed is currently not
// supported, use floating windows" for CLAP_WINDOW_API_WAYLAND), so this
// mirrors what vst3bridge/src/editor_linux.cpp already does for VST3 on
// Linux: a host-owned top-level GtkWindow realized on X11 (XWayland when the
// Studio itself runs under pure Wayland), never a GPUI-embedded child.
//
// Architecture, shared with editor_linux.cpp:
//  - GTK4 and its GLib main loop run on a dedicated background thread (the
//    "GTK thread"), started lazily on first editor open.
//  - Every GTK/GLib call is executed on that thread via g_idle_add(),
//    synchronised with a mutex + condvar so the IPC/main thread can wait for
//    the result.
//
// Unlike VST3, CLAP has no per-plugin "run loop" object queried off a host
// frame. A GUI-bearing plug-in instead advertises clap.posix-fd-support /
// clap.timer-support on itself and expects the *host* to offer the matching
// host-side extension (queried through clap_host_t::get_extension, which is
// shared cross-platform code in clap_processor.cpp). Those host extension
// callbacks are implemented below and bridge straight onto this file's GTK
// thread — the direct analogue of vst3bridge's LinuxRunLoopFrame.

#if defined(_WIN32) || defined(__APPLE__)
#error "clap_editor_linux.cpp must not be compiled on Windows or macOS"
#endif

#include "clap_processor_internal.hpp"

#include <condition_variable>
#include <cstdint>
#include <mutex>
#include <thread>
#include <unordered_map>
#include <vector>

#include <gtk/gtk.h>
#include <glib-unix.h>

#ifdef GDK_WINDOWING_X11
#include <gdk/x11/gdkx.h>
#include <X11/Xlib.h>
#endif

namespace {

// ── GTK main-loop thread ────────────────────────────────────────────────────
// Identical pattern to vst3bridge/src/editor_linux.cpp::gtk_thread_main /
// ensure_gtk — kept as a separate instance here (not shared) because the two
// bridges are independent static libraries with no common runtime, matching
// how Windows/macOS already give each format its own editor TU.

GMainLoop *s_main_loop = nullptr;
std::mutex s_init_mutex;
std::condition_variable s_init_cv;
bool s_gtk_ready = false;

void gtk_thread_main() {
  // Must run before any other Xlib call in the process — see the identical
  // comment in vst3bridge/src/editor_linux.cpp::gtk_thread_main. Both bridges
  // can have an editor open at once, so both independently guard this.
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

bool ensure_gtk() {
  static std::once_flag once;
  std::call_once(once, [] { std::thread(gtk_thread_main).detach(); });

  std::unique_lock<std::mutex> lk(s_init_mutex);
  return s_init_cv.wait_for(lk, std::chrono::seconds(5),
                            [] { return s_gtk_ready; });
}

// ── clap.posix-fd-support / clap.timer-support (host side) ─────────────────
//
// Every registration/callback here runs on the GTK thread: CLAP's GUI-related
// calls are main-thread-only, and for these instances "main thread" is the
// GTK thread once an editor is open (register_fd/register_timer typically
// arrive from inside clap_plugin_gui->create(), which we already call from
// there). CLAP gives no explicit "instance is gone" fd/timer teardown, unlike
// VST3's removed()/IRunLoop pairing, so the host sweeps every registration
// for a processor when its editor closes.

struct RunLoopReg {
  SphereDauxClapProcessor *proc{nullptr};
  int fd{-1};          // -1 for timer registrations
  clap_id timer_id{0}; // 0 for fd registrations
  guint source_id{0};
};

std::mutex s_run_loop_mutex;
std::unordered_map<SphereDauxClapProcessor *, std::vector<RunLoopReg *>>
    s_run_loop_regs;
std::atomic<clap_id> s_next_timer_id{1};

gboolean run_loop_fd_cb(gint fd, GIOCondition condition, gpointer user_data) {
  auto *reg = static_cast<RunLoopReg *>(user_data);
  SphereDauxClapProcessor *proc = reg->proc;
  if (proc && proc->plugin) {
    const auto *ext = static_cast<const clap_plugin_posix_fd_support_t *>(
        proc->plugin->get_extension(proc->plugin, CLAP_EXT_POSIX_FD_SUPPORT));
    if (ext && ext->on_fd) {
      clap_posix_fd_flags_t flags = 0;
      if (condition & G_IO_IN)
        flags |= CLAP_POSIX_FD_READ;
      if (condition & G_IO_OUT)
        flags |= CLAP_POSIX_FD_WRITE;
      if (condition & (G_IO_ERR | G_IO_HUP))
        flags |= CLAP_POSIX_FD_ERROR;
      ext->on_fd(proc->plugin, fd, flags);
    }
  }
  return G_SOURCE_CONTINUE;
}

gboolean run_loop_timer_cb(gpointer user_data) {
  auto *reg = static_cast<RunLoopReg *>(user_data);
  SphereDauxClapProcessor *proc = reg->proc;
  if (proc && proc->plugin) {
    const auto *ext = static_cast<const clap_plugin_timer_support_t *>(
        proc->plugin->get_extension(proc->plugin, CLAP_EXT_TIMER_SUPPORT));
    if (ext && ext->on_timer) {
      ext->on_timer(proc->plugin, reg->timer_id);
    }
  }
  return G_SOURCE_CONTINUE;
}

void register_run_loop_reg(SphereDauxClapProcessor *proc, RunLoopReg *reg) {
  std::lock_guard<std::mutex> lk(s_run_loop_mutex);
  s_run_loop_regs[proc].push_back(reg);
}

/// Remove and delete every registration belonging to `proc`. Called from
/// close_editor_on_gtk_thread so a closed editor cannot leave a plug-in's
/// fd/timer callbacks firing into a destroyed GUI.
void release_run_loop_regs(SphereDauxClapProcessor *proc) {
  std::vector<RunLoopReg *> regs;
  {
    std::lock_guard<std::mutex> lk(s_run_loop_mutex);
    auto it = s_run_loop_regs.find(proc);
    if (it == s_run_loop_regs.end()) {
      return;
    }
    regs = std::move(it->second);
    s_run_loop_regs.erase(it);
  }
  for (auto *reg : regs) {
    if (reg->source_id) {
      g_source_remove(reg->source_id);
    }
    delete reg;
  }
}

} // namespace

bool clap_host_register_fd_linux(const clap_host_t *host, int fd,
                                 clap_posix_fd_flags_t flags) {
  auto *proc = host ? static_cast<SphereDauxClapProcessor *>(host->host_data)
                    : nullptr;
  if (!proc || fd < 0) {
    return false;
  }
  GIOCondition condition = static_cast<GIOCondition>(G_IO_ERR | G_IO_HUP);
  if (flags & CLAP_POSIX_FD_READ) {
    condition = static_cast<GIOCondition>(condition | G_IO_IN);
  }
  if (flags & CLAP_POSIX_FD_WRITE) {
    condition = static_cast<GIOCondition>(condition | G_IO_OUT);
  }
  auto *reg = new RunLoopReg{proc, fd, 0, 0};
  reg->source_id = g_unix_fd_add_full(G_PRIORITY_DEFAULT, fd, condition,
                                      run_loop_fd_cb, reg, nullptr);
  register_run_loop_reg(proc, reg);
  return true;
}

bool clap_host_unregister_fd_linux(const clap_host_t *host, int fd) {
  auto *proc = host ? static_cast<SphereDauxClapProcessor *>(host->host_data)
                    : nullptr;
  if (!proc) {
    return false;
  }
  std::lock_guard<std::mutex> lk(s_run_loop_mutex);
  auto it = s_run_loop_regs.find(proc);
  if (it == s_run_loop_regs.end()) {
    return false;
  }
  auto &regs = it->second;
  for (auto vit = regs.begin(); vit != regs.end(); ++vit) {
    if ((*vit)->timer_id == 0 && (*vit)->fd == fd) {
      if ((*vit)->source_id) {
        g_source_remove((*vit)->source_id);
      }
      delete *vit;
      regs.erase(vit);
      return true;
    }
  }
  return false;
}

bool clap_host_modify_fd_linux(const clap_host_t *host, int fd,
                               clap_posix_fd_flags_t flags) {
  // GLib has no in-place condition update for a live g_unix_fd_add_full
  // source, so the simplest correct implementation is unregister + re-add.
  if (!clap_host_unregister_fd_linux(host, fd)) {
    return false;
  }
  return clap_host_register_fd_linux(host, fd, flags);
}

bool clap_host_register_timer_linux(const clap_host_t *host,
                                    uint32_t period_ms, clap_id *timer_id) {
  auto *proc = host ? static_cast<SphereDauxClapProcessor *>(host->host_data)
                    : nullptr;
  if (!proc || !timer_id) {
    return false;
  }
  const clap_id id = s_next_timer_id.fetch_add(1, std::memory_order_relaxed);
  auto *reg = new RunLoopReg{proc, -1, id, 0};
  reg->source_id =
      g_timeout_add_full(G_PRIORITY_DEFAULT, period_ms == 0 ? 1 : period_ms,
                         run_loop_timer_cb, reg, nullptr);
  register_run_loop_reg(proc, reg);
  *timer_id = id;
  return true;
}

bool clap_host_unregister_timer_linux(const clap_host_t *host,
                                      clap_id timer_id) {
  auto *proc = host ? static_cast<SphereDauxClapProcessor *>(host->host_data)
                    : nullptr;
  if (!proc) {
    return false;
  }
  std::lock_guard<std::mutex> lk(s_run_loop_mutex);
  auto it = s_run_loop_regs.find(proc);
  if (it == s_run_loop_regs.end()) {
    return false;
  }
  auto &regs = it->second;
  for (auto vit = regs.begin(); vit != regs.end(); ++vit) {
    if ((*vit)->fd < 0 && (*vit)->timer_id == timer_id) {
      if ((*vit)->source_id) {
        g_source_remove((*vit)->source_id);
      }
      delete *vit;
      regs.erase(vit);
      return true;
    }
  }
  return false;
}

// ── Window open/close/focus ─────────────────────────────────────────────────

namespace {

void close_editor_on_gtk_thread(SphereDauxClapProcessor *proc);

struct OpenTask {
  SphereDauxClapProcessor *proc{nullptr};
  const char *window_id{nullptr};
  const char *title{nullptr};
  int width{0};
  int height{0};

  unsigned long long result{0};
  std::mutex done_mutex;
  std::condition_variable done_cv;
  bool done{false};
};

gboolean idle_open_editor(gpointer user_data) {
  auto *task = static_cast<OpenTask *>(user_data);
  SphereDauxClapProcessor *p = task->proc;

  if (!p->plugin || !p->ext_gui) {
    clap_set_last_error("CLAP editor: plug-in exposes no GUI");
    goto done;
  }

#ifndef GDK_WINDOWING_X11
  clap_set_last_error(
      "DAUx CLAP editor: built without GDK X11 backend support");
  goto done;
#else
  if (!p->ext_gui->is_api_supported ||
      !p->ext_gui->is_api_supported(p->plugin, CLAP_WINDOW_API_X11, false)) {
    clap_set_last_error("CLAP plug-in does not support an embedded X11 GUI");
    goto done;
  }

  {
    p->editor_window_id = task->window_id ? task->window_id : "";
    if (task->title && *task->title) {
      p->editor_title = task->title;
    }
    const char *title_str =
        p->editor_title.empty() ? "Plug-in Editor" : p->editor_title.c_str();

    int w = task->width > 0 ? task->width : 640;
    int h = task->height > 0 ? task->height : 480;

    GtkWidget *window = gtk_window_new();
    gtk_window_set_title(GTK_WINDOW(window), title_str);
    gtk_window_set_default_size(GTK_WINDOW(window), w, h);
    gtk_window_set_resizable(GTK_WINDOW(window), TRUE);

    GtkCssProvider *css = gtk_css_provider_new();
    gtk_css_provider_load_from_string(css, "window { background-color: #0b0f14; }");
    gtk_style_context_add_provider_for_display(
        gtk_widget_get_display(window), GTK_STYLE_PROVIDER(css),
        GTK_STYLE_PROVIDER_PRIORITY_APPLICATION);
    g_object_unref(css);

    // Realize before querying/attaching: forces the underlying X11 window
    // (and XID) to exist, matching editor_linux.cpp's VST3 sequence.
    gtk_widget_realize(window);

    GdkSurface *surface = gtk_native_get_surface(GTK_NATIVE(window));
    if (!GDK_IS_X11_SURFACE(surface)) {
      clap_set_last_error(
          "DAUx CLAP editor: GDK backend is not X11 — CLAP embedding "
          "requires an X11 display (set GDK_BACKEND=x11)");
      gtk_window_destroy(GTK_WINDOW(window));
      goto done;
    }
    Window xid = gdk_x11_surface_get_xid(surface);

    if (!p->gui_created) {
      if (!p->ext_gui->create ||
          !p->ext_gui->create(p->plugin, CLAP_WINDOW_API_X11, false)) {
        clap_set_last_error("clap_plugin_gui->create() returned false");
        gtk_window_destroy(GTK_WINDOW(window));
        goto done;
      }
      p->gui_created = true;
    }

    if (p->ext_gui->set_scale) {
      const double scale = gdk_surface_get_scale_factor(surface);
      p->ext_gui->set_scale(p->plugin, scale > 0.0 ? scale : 1.0);
    }

    p->preferred_gui_size(&w, &h);
    gtk_window_set_default_size(GTK_WINDOW(window), w, h);

    clap_window_t clap_window{};
    clap_window.api = CLAP_WINDOW_API_X11;
    clap_window.x11 = xid;
    if (!p->ext_gui->set_parent ||
        !p->ext_gui->set_parent(p->plugin, &clap_window)) {
      clap_set_last_error("clap_plugin_gui->set_parent() returned false");
      if (p->ext_gui->destroy) {
        p->ext_gui->destroy(p->plugin);
      }
      p->gui_created = false;
      gtk_window_destroy(GTK_WINDOW(window));
      goto done;
    }

    gtk_window_present(GTK_WINDOW(window));

    if (p->ext_gui->show) {
      p->ext_gui->show(p->plugin);
    }

    // Re-query after show: some plug-ins only settle their size once visible.
    p->preferred_gui_size(&w, &h);
    if (w > 0 && h > 0) {
      p->embed_content_w = w;
      p->embed_content_h = h;
      gtk_window_set_default_size(GTK_WINDOW(window), w, h);
    }

    p->editor_attached = true;
    p->embed_mode = false;
    p->embed_host_kind = 2; // detached top-level, matches clap_editor_mac.mm

    g_object_ref(window);
    p->editor_native_window = window;
    p->editor_handle = clap_next_editor_handle();

    struct CloseCtx {
      SphereDauxClapProcessor *proc;
    };
    auto *close_ctx = new CloseCtx{p};
    g_signal_connect_data(
        window, "close-request",
        G_CALLBACK(+[](GtkWindow *, gpointer ud) -> gboolean {
          auto *ctx = static_cast<CloseCtx *>(ud);
          if (ctx->proc) {
            ctx->proc->embed_user_closed.store(true, std::memory_order_release);
            close_editor_on_gtk_thread(ctx->proc);
          }
          return TRUE; // we tear the window down ourselves
        }),
        close_ctx,
        [](gpointer data, GClosure *) { delete static_cast<CloseCtx *>(data); },
        G_CONNECT_DEFAULT);

    std::fprintf(stderr,
                 "[clap-editor/linux] opened handle=%llu xid=0x%lx size=%dx%d\n",
                 p->editor_handle, xid, w, h);

    task->result = p->editor_handle;
  }
#endif // GDK_WINDOWING_X11

done:
  {
    std::lock_guard<std::mutex> lk(task->done_mutex);
    task->done = true;
  }
  task->done_cv.notify_all();
  return G_SOURCE_REMOVE;
}

struct DestroyWindowTask {
  GtkWidget *window{nullptr};
};

// Deferred destroy, off the close-request call stack: GTK4/GDK can assert if
// the surface still has a live EGL native window (common right after a
// GL-backed plug-in editor detaches) — see the identical rationale in
// vst3bridge/src/editor_linux.cpp::idle_destroy_editor_window.
gboolean idle_destroy_editor_window(gpointer user_data) {
  auto *task = static_cast<DestroyWindowTask *>(user_data);
  for (int i = 0; i < 8; ++i) {
    if (!g_main_context_iteration(nullptr, FALSE))
      break;
  }
  if (task->window) {
    gtk_widget_set_visible(task->window, FALSE);
    for (int i = 0; i < 4; ++i) {
      if (!g_main_context_iteration(nullptr, FALSE))
        break;
    }
    gtk_window_destroy(GTK_WINDOW(task->window));
    g_object_unref(task->window); // matches g_object_ref in idle_open_editor
    task->window = nullptr;
  }
  delete task;
  std::fprintf(stderr, "[clap-editor/linux] closed\n");
  return G_SOURCE_REMOVE;
}

// Shared by idle_close_editor (cross-thread close request) and the window's
// own close-request handler (both already run on the GTK thread). Must NOT
// go through clap_close_editor_linux() from the close-request handler — that
// g_idle_add()s and blocks waiting for an idle source that can only run once
// the handler returns (self-deadlock). Idempotent.
void close_editor_on_gtk_thread(SphereDauxClapProcessor *proc) {
  if (!proc->editor_native_window) {
    return;
  }
  auto *window = static_cast<GtkWidget *>(proc->editor_native_window);

  if (proc->gui_created && proc->plugin && proc->ext_gui) {
    if (proc->ext_gui->hide) {
      proc->ext_gui->hide(proc->plugin);
    }
    if (proc->ext_gui->destroy) {
      proc->ext_gui->destroy(proc->plugin);
    }
    proc->gui_created = false;
  }
  proc->editor_attached = false;
  proc->embed_mode = false;
  proc->editor_native_window = nullptr;
  proc->editor_handle = 0;

  release_run_loop_regs(proc);

  auto *task = new DestroyWindowTask{window};
  g_idle_add(idle_destroy_editor_window, task);
}

struct CloseTask {
  SphereDauxClapProcessor *proc{nullptr};
  std::mutex done_mutex;
  std::condition_variable done_cv;
  bool done{false};
};

gboolean idle_close_editor(gpointer user_data) {
  auto *task = static_cast<CloseTask *>(user_data);
  close_editor_on_gtk_thread(task->proc);
  {
    std::lock_guard<std::mutex> lk(task->done_mutex);
    task->done = true;
  }
  task->done_cv.notify_all();
  return G_SOURCE_REMOVE;
}

} // namespace

unsigned long long clap_open_editor_linux(SphereDauxClapProcessor *proc,
                                          const char *window_id,
                                          const char *title, int width,
                                          int height) {
  if (!proc) {
    return 0;
  }
  if (!ensure_gtk()) {
    clap_set_last_error("GTK4 initialisation timed out");
    return 0;
  }

  if (proc->editor_native_window) {
    return clap_focus_editor_linux(proc) ? proc->editor_handle : 0;
  }

  OpenTask task;
  task.proc = proc;
  task.window_id = window_id;
  task.title = title;
  task.width = width;
  task.height = height;

  g_idle_add(idle_open_editor, &task);

  std::unique_lock<std::mutex> lk(task.done_mutex);
  task.done_cv.wait(lk, [&] { return task.done; });
  return task.result;
}

void clap_close_editor_linux(SphereDauxClapProcessor *proc) {
  if (!proc || !proc->editor_native_window || !s_gtk_ready) {
    return;
  }

  CloseTask task;
  task.proc = proc;

  g_idle_add(idle_close_editor, &task);

  std::unique_lock<std::mutex> lk(task.done_mutex);
  task.done_cv.wait(lk, [&] { return task.done; });
}

int clap_focus_editor_linux(SphereDauxClapProcessor *proc) {
  if (!proc || !s_gtk_ready || !proc->editor_native_window) {
    return 0;
  }
  g_idle_add(
      [](gpointer ud) -> gboolean {
        gtk_window_present(GTK_WINDOW(static_cast<GtkWidget *>(ud)));
        return G_SOURCE_REMOVE;
      },
      proc->editor_native_window);
  return 1;
}

// ── C API ────────────────────────────────────────────────────────────────────
//
// embed_*/view_* stay stubbed exactly as clap_editor_stub.cpp reports them:
// nothing calls the GPUI-embedded or host-owned-view tiers on Linux today
// (VST3 doesn't use them here either — see editor_linux.cpp), so implementing
// them would be speculative.

extern "C" {

unsigned long long sphere_daux_clap_open_editor(SphereDauxClapProcessor *p,
                                                const char *window_id,
                                                const char *title, int width,
                                                int height) {
  return clap_open_editor_linux(p, window_id, title, width, height);
}

void sphere_daux_clap_close_editor(SphereDauxClapProcessor *p) {
  clap_close_editor_linux(p);
}

int sphere_daux_clap_focus_editor(SphereDauxClapProcessor *p) {
  return clap_focus_editor_linux(p);
}

unsigned long long
sphere_daux_clap_editor_native_window(SphereDauxClapProcessor *p) {
  if (!p) {
    return 0;
  }
  return static_cast<unsigned long long>(
      reinterpret_cast<std::uintptr_t>(p->editor_native_window));
}

unsigned long long sphere_daux_clap_embed_editor(SphereDauxClapProcessor *,
                                                 unsigned long long, int, int,
                                                 int, int) {
  clap_set_last_error("CLAP embedded editor is not supported on this platform");
  return 0;
}

void sphere_daux_clap_embed_set_bounds(SphereDauxClapProcessor *, int, int,
                                       int, int) {}

void sphere_daux_clap_embed_refresh(SphereDauxClapProcessor *) {}

unsigned long long
sphere_daux_clap_embed_attach_hwnd(SphereDauxClapProcessor *) {
  return 0;
}

void sphere_daux_clap_embed_detach(SphereDauxClapProcessor *) {}

int sphere_daux_clap_embed_is_valid(SphereDauxClapProcessor *) { return 0; }

int sphere_daux_clap_embed_has_visible_ui(SphereDauxClapProcessor *) {
  return 0;
}

// ── Host-owned view host ────────────────────────────────────────────────────

int sphere_daux_clap_view_attach(SphereDauxClapProcessor *, unsigned long long,
                                 int, int, int *, int *) {
  clap_set_last_error("CLAP host-owned view is not supported on this platform");
  return 0;
}

void sphere_daux_clap_view_detach(SphereDauxClapProcessor *) {}

int sphere_daux_clap_view_is_attached(SphereDauxClapProcessor *) { return 0; }

int sphere_daux_clap_view_set_size(SphereDauxClapProcessor *, int, int) {
  return 0;
}

int sphere_daux_clap_view_get_size(SphereDauxClapProcessor *, int *, int *) {
  return 0;
}

int sphere_daux_clap_view_can_resize(SphereDauxClapProcessor *) { return 0; }

int sphere_daux_clap_view_constrain(SphereDauxClapProcessor *, int *, int *) {
  return 0;
}

int sphere_daux_clap_view_take_resize_request(SphereDauxClapProcessor *, int *,
                                              int *) {
  return 0;
}

} // extern "C"
