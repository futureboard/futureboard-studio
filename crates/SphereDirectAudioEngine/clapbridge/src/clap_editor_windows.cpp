// Windows editor hosting for CLAP plug-ins.
//
// Reuses the shared native editor shell (`daux_editor_*` from
// vst3bridge/editor_windows.hpp) — titlebar, DPI handling, loading overlay,
// pin-to-top, and the bare-Space transport claim all come from there. Only the
// attach is CLAP-specific: the `clap.gui` extension's
// create → set_scale → get_size → set_parent → show sequence.

#if !defined(_WIN32)
#error "clap_editor_windows.cpp is Windows-only; other platforms use the mac/stub TU"
#endif

#include "clap_processor_internal.hpp"

#include <objbase.h>

namespace {

bool clap_embed_debug() {
  static const bool enabled =
      daux_clap_debug() ||
      std::getenv("FUTUREBOARD_PLUGIN_EMBED_DEBUG") != nullptr;
  return enabled;
}

std::wstring widen(const char *value) {
  if (!value || !*value)
    return {};
  const int needed = MultiByteToWideChar(CP_UTF8, 0, value, -1, nullptr, 0);
  if (needed <= 0)
    return {};
  std::wstring out(static_cast<size_t>(needed - 1), L'\0');
  MultiByteToWideChar(CP_UTF8, 0, value, -1, out.data(), needed);
  return out;
}

/// COM (STA) on the editor thread. Some CLAP GUIs create COM/WebView controls
/// on parent, and fail without an apartment. Idempotent.
void ensure_com_initialized() {
  static thread_local bool done = false;
  if (done)
    return;
  const HRESULT hr = CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);
  if (hr == RPC_E_CHANGED_MODE) {
    // Another apartment model already owns this thread; that is fine.
  }
  done = true;
}

// ── Shell callbacks ─────────────────────────────────────────────────────────

SphereDauxClapProcessor *from_user_data(void *user_data) {
  return static_cast<SphereDauxClapProcessor *>(user_data);
}

bool cb_is_live(void *user_data) {
  auto *p = from_user_data(user_data);
  return p && p->processor_valid.load(std::memory_order_acquire);
}

bool cb_is_attached(void *user_data) {
  auto *p = from_user_data(user_data);
  return p && p->editor_attached;
}

bool cb_is_resize_in_progress(void *user_data) {
  auto *p = from_user_data(user_data);
  return p && p->embed_resize_in_progress;
}

void cb_set_resize_in_progress(void *user_data, bool value) {
  if (auto *p = from_user_data(user_data))
    p->embed_resize_in_progress = value;
}

bool cb_can_resize(void *user_data) {
  auto *p = from_user_data(user_data);
  return p && p->editor_resizable;
}

/// CLAP gives the plug-in the final say on a host-driven resize through
/// `adjust_size`, so a drag is snapped to the nearest size the plug-in accepts
/// rather than forced.
bool cb_constrain_content_size(void *user_data, int *width, int *height) {
  auto *p = from_user_data(user_data);
  if (!p || !width || !height)
    return false;
  if (!p->editor_resizable) {
    if (p->embed_content_w > 0 && p->embed_content_h > 0) {
      *width = p->embed_content_w;
      *height = p->embed_content_h;
      return true;
    }
    return false;
  }
  if (p->ext_gui && p->ext_gui->adjust_size && p->gui_created) {
    auto w = static_cast<uint32_t>(std::max(1, *width));
    auto h = static_cast<uint32_t>(std::max(1, *height));
    if (p->ext_gui->adjust_size(p->plugin, &w, &h) && w > 0 && h > 0) {
      *width = static_cast<int>(w);
      *height = static_cast<int>(h);
      return true;
    }
  }
  return true;
}

void cb_on_content_resized(void *user_data, void *content_hwnd, int width,
                           int height) {
  auto *p = from_user_data(user_data);
  if (!p || width <= 0 || height <= 0)
    return;
  p->embed_content_w = width;
  p->embed_content_h = height;
  // Tell the plug-in its new size, then let it lay out its own child window.
  if (p->editor_resizable && p->gui_created && p->ext_gui && p->ext_gui->set_size) {
    p->ext_gui->set_size(p->plugin, static_cast<uint32_t>(width),
                         static_cast<uint32_t>(height));
  }
  auto content = reinterpret_cast<HWND>(content_hwnd);
  if (!content || !IsWindow(content))
    return;
  if (HWND child = GetWindow(content, GW_CHILD)) {
    SetWindowPos(child, nullptr, 0, 0, width, height,
                 SWP_NOZORDER | SWP_NOACTIVATE);
  }
}

void cb_on_dpi_changed(void *user_data, void *shell_hwnd, void *content_hwnd,
                       int width, int height) {
  auto *p = from_user_data(user_data);
  if (p && p->gui_created && p->ext_gui && p->ext_gui->set_scale) {
    const UINT dpi = daux_editor_hwnd_dpi(shell_hwnd);
    p->ext_gui->set_scale(p->plugin, static_cast<double>(dpi) / 96.0);
  }
  cb_on_content_resized(user_data, content_hwnd, width, height);
}

void cb_on_close_requested(void *user_data) {
  if (auto *p = from_user_data(user_data))
    p->embed_user_closed.store(true, std::memory_order_release);
}

DauxEditorWindowCallbacks make_callbacks(SphereDauxClapProcessor *p) {
  DauxEditorWindowCallbacks callbacks{};
  callbacks.user_data = p;
  callbacks.is_live = cb_is_live;
  callbacks.is_attached = cb_is_attached;
  callbacks.is_resize_in_progress = cb_is_resize_in_progress;
  callbacks.set_resize_in_progress = cb_set_resize_in_progress;
  callbacks.can_resize = cb_can_resize;
  callbacks.constrain_content_size = cb_constrain_content_size;
  callbacks.on_content_resized = cb_on_content_resized;
  callbacks.on_dpi_changed = cb_on_dpi_changed;
  callbacks.on_close_requested = cb_on_close_requested;
  return callbacks;
}

// ── Geometry ────────────────────────────────────────────────────────────────

/// Keep the editor shell aligned with the GPUI-provided host region.
/// Mirrors the VST3 and VST2 bridges; the three cannot share an implementation
/// because each owns its own processor struct.
bool sync_geometry(SphereDauxClapProcessor *p, int x, int y, int w, int h) {
  HWND top = p ? p->editor_embed_top_hwnd : nullptr;
  if (!p || !top || !IsWindow(top))
    return false;
  // Detached: a standalone OS window owns its own position and size.
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
    GetClientRect(p->editor_attach_hwnd, &rc);
    if (HWND child = GetWindow(p->editor_attach_hwnd, GW_CHILD)) {
      SetWindowPos(child, nullptr, 0, 0, rc.right - rc.left,
                   rc.bottom - rc.top, SWP_NOZORDER | SWP_NOACTIVATE);
    }
  }
  daux_editor_raise_children(top);
  return true;
}

/// Run the `clap.gui` attach sequence into `content`.
bool attach_editor(SphereDauxClapProcessor *p, HWND content) {
  if (!p || !p->plugin || !p->ext_gui || !content || !IsWindow(content))
    return false;

  if (!p->gui_created) {
    if (!p->ext_gui->is_api_supported ||
        !p->ext_gui->is_api_supported(p->plugin, CLAP_WINDOW_API_WIN32, false)) {
      clap_set_last_error("CLAP plug-in does not support an embedded Win32 GUI");
      return false;
    }
    if (!p->ext_gui->create ||
        !p->ext_gui->create(p->plugin, CLAP_WINDOW_API_WIN32, false)) {
      clap_set_last_error("clap_plugin_gui->create() returned false");
      return false;
    }
    p->gui_created = true;
  }

  if (p->ext_gui->set_scale) {
    const UINT dpi = daux_editor_hwnd_dpi(content);
    p->ext_gui->set_scale(p->plugin, static_cast<double>(dpi) / 96.0);
  }

  int width = p->embed_content_w;
  int height = p->embed_content_h;
  p->preferred_gui_size(&width, &height);

  clap_window_t window{};
  window.api = CLAP_WINDOW_API_WIN32;
  window.win32 = content;
  if (!p->ext_gui->set_parent || !p->ext_gui->set_parent(p->plugin, &window)) {
    clap_set_last_error("clap_plugin_gui->set_parent() returned false");
    return false;
  }

  if (p->ext_gui->show) {
    p->ext_gui->show(p->plugin);
  }

  // Re-query after show: some plug-ins only settle their size once visible.
  p->preferred_gui_size(&width, &height);
  if (width > 0 && height > 0) {
    p->embed_content_w = width;
    p->embed_content_h = height;
    daux_editor_resize_content(&p->editor_window, width, height);
  }

  RECT rc{};
  GetClientRect(content, &rc);
  if (HWND child = GetWindow(content, GW_CHILD)) {
    SetWindowPos(child, nullptr, 0, 0, rc.right - rc.left, rc.bottom - rc.top,
                 SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW);
  }

  p->editor_attached = true;
  p->editor_attach_hwnd = content;
  daux_editor_set_load_state(&p->editor_window, false, nullptr);

  std::fprintf(stderr,
               "[clap-editor] attached instance=%s size=%dx%d content=0x%p "
               "resizable=%d\n",
               p->embed_instance_label.empty()
                   ? "<unknown>"
                   : p->embed_instance_label.c_str(),
               width, height, static_cast<void *>(content),
               p->editor_resizable ? 1 : 0);
  return true;
}

/// Create the shell and attach. Shared by the embedded and standalone paths.
unsigned long long open_shell(SphereDauxClapProcessor *p, HWND owner, int kind,
                              int x, int y, int width, int height) {
  ensure_com_initialized();

  if (!p || !p->plugin) {
    clap_set_last_error("CLAP editor: no plug-in instance");
    return 0;
  }
  if (!p->ext_gui) {
    clap_set_last_error("CLAP plug-in exposes no GUI extension");
    return 0;
  }

  // Create the GUI up front so `get_size` can report a real preferred size and
  // the shell opens at it, instead of resizing visibly after attach.
  int editor_w = width > 0 ? width : 640;
  int editor_h = height > 0 ? height : 480;
  if (!p->gui_created && p->ext_gui->is_api_supported &&
      p->ext_gui->is_api_supported(p->plugin, CLAP_WINDOW_API_WIN32, false) &&
      p->ext_gui->create &&
      p->ext_gui->create(p->plugin, CLAP_WINDOW_API_WIN32, false)) {
    p->gui_created = true;
  }
  p->preferred_gui_size(&editor_w, &editor_h);
  p->embed_content_w = editor_w;
  p->embed_content_h = editor_h;

  const std::wstring title_w = p->editor_title.empty()
                                   ? std::wstring(L"Unknown Plugin")
                                   : widen(p->editor_title.c_str());

  DauxEditorWindowConfig config{};
  config.owner_hwnd = owner;
  config.title = title_w.c_str();
  config.plugin_name = title_w.c_str();
  config.host_kind = kind;
  config.x = x;
  config.y = y;
  config.content_width = editor_w;
  config.content_height = editor_h;
  config.pin_default = daux_editor_env_truthy("FUTUREBOARD_EDITOR_PIN_DEFAULT");
  config.callbacks = make_callbacks(p);

  // Reclaim a stale shell from a previous failed open before overwriting it.
  // DestroyWindow is only valid on the creating thread.
  if (daux_editor_is_window_valid(&p->editor_window)) {
    HWND stale = reinterpret_cast<HWND>(p->editor_window.shell_hwnd);
    if (stale &&
        GetWindowThreadProcessId(stale, nullptr) == GetCurrentThreadId()) {
      daux_editor_destroy_window(&p->editor_window);
    }
  }
  p->editor_window = DauxEditorWindow{};

  if (!daux_editor_create_window(&config, &p->editor_window)) {
    clap_set_last_error("CLAP editor: native editor window creation failed");
    return 0;
  }
  HWND top = reinterpret_cast<HWND>(p->editor_window.shell_hwnd);
  HWND content = reinterpret_cast<HWND>(p->editor_window.content_hwnd);
  if (!top || !content) {
    daux_editor_destroy_window(&p->editor_window);
    p->editor_window = DauxEditorWindow{};
    clap_set_last_error("CLAP editor: shell returned invalid HWNDs");
    return 0;
  }

  p->editor_embed_top_hwnd = top;
  p->embed_host_kind = kind;
  p->embed_user_closed.store(false, std::memory_order_release);
  p->embed_geometry_valid = false;

  if (!attach_editor(p, content)) {
    const std::wstring message = L"Editor failed to open";
    daux_editor_set_load_state(&p->editor_window, true, message.c_str());
    return 0;
  }

  p->editor_handle = clap_next_editor_handle();
  daux_editor_show_and_focus(&p->editor_window);
  return p->editor_handle;
}

} // namespace

// ── Processor member teardown ───────────────────────────────────────────────

void SphereDauxClapProcessor::close_embed_editor(const char *reason) {
  if (!editor_attached && !gui_created &&
      !daux_editor_is_window_valid(&editor_window)) {
    return;
  }

  if (gui_created && plugin && ext_gui) {
    if (ext_gui->hide) {
      ext_gui->hide(plugin);
    }
    if (ext_gui->destroy) {
      ext_gui->destroy(plugin);
    }
    gui_created = false;
  }
  editor_attached = false;
  editor_attach_hwnd = nullptr;
  // The host-owned view path borrows the caller's window. Drop the borrow so a
  // later attach starts clean; the window itself is never touched from here.
  view_host_attached = false;
  view_host_parent = nullptr;
  view_host_resize_pending.store(false, std::memory_order_release);

  if (daux_editor_is_window_valid(&editor_window)) {
    HWND shell = reinterpret_cast<HWND>(editor_window.shell_hwnd);
    // DestroyWindow is only legal on the creating thread; a cross-thread call
    // is a silent no-op in Win32, so skip it rather than issue an invalid one.
    if (!shell ||
        GetWindowThreadProcessId(shell, nullptr) == GetCurrentThreadId()) {
      daux_editor_destroy_window(&editor_window);
    }
  }
  editor_window = DauxEditorWindow{};
  editor_embed_top_hwnd = nullptr;
  editor_parent_hwnd = nullptr;
  embed_mode = false;
  embed_geometry_valid = false;
  embed_last_applied = RECT{};
  editor_handle = 0;

  if (clap_embed_debug()) {
    std::fprintf(stderr, "[clap-editor] detached reason=%s instance=%s\n",
                 reason ? reason : "unspecified",
                 embed_instance_label.empty() ? "<unknown>"
                                              : embed_instance_label.c_str());
  }
}

void SphereDauxClapProcessor::close_editor_window() {
  close_embed_editor("shutdown");
}

// ── C API ───────────────────────────────────────────────────────────────────

extern "C" {

unsigned long long sphere_daux_clap_embed_editor(SphereDauxClapProcessor *p,
                                                 unsigned long long parent,
                                                 int x, int y, int width,
                                                 int height) {
  HWND parent_hwnd =
      reinterpret_cast<HWND>(static_cast<std::uintptr_t>(parent));
  if (!p || !parent_hwnd || !IsWindow(parent_hwnd) || width <= 0 ||
      height <= 0) {
    clap_set_last_error("CLAP embed editor: invalid parent HWND or region");
    return 0;
  }

  // Reuse: an already-attached editor only needs its geometry re-synced.
  if (p->embed_mode && p->editor_attached && p->editor_embed_top_hwnd &&
      IsWindow(p->editor_embed_top_hwnd)) {
    p->editor_parent_hwnd = parent_hwnd;
    if (p->embed_host_kind == 2) {
      daux_editor_apply_owner(&p->editor_window, parent_hwnd);
      daux_editor_show_and_focus(&p->editor_window);
    }
    p->embed_geometry_valid = false;
    sync_geometry(p, x, y, width, height);
    return p->editor_handle;
  }

  const int kind = daux_editor_resolve_host_kind();
  p->editor_parent_hwnd = parent_hwnd;
  p->embed_mode = true;
  const auto handle = open_shell(p, parent_hwnd, kind, x, y, width, height);
  if (handle == 0) {
    p->embed_mode = false;
    return 0;
  }
  sync_geometry(p, x, y, width, height);
  return handle;
}

void sphere_daux_clap_embed_set_bounds(SphereDauxClapProcessor *p, int x, int y,
                                       int width, int height) {
  if (!p || width <= 0 || height <= 0) {
    return;
  }
  sync_geometry(p, x, y, width, height);
}

void sphere_daux_clap_embed_refresh(SphereDauxClapProcessor *p) {
  if (!p || !p->embed_mode || !p->editor_embed_top_hwnd) {
    return;
  }
  // Cheap per-frame poll: re-apply the last host region so a parent-window move
  // is tracked. sync_geometry no-ops when the screen rect is unchanged. CLAP
  // GUIs run their own timers via clap.timer-support, so there is no idle call
  // to make here.
  p->embed_geometry_valid = false;
  sync_geometry(p, p->embed_host_x, p->embed_host_y, p->embed_host_w,
                p->embed_host_h);
}

unsigned long long
sphere_daux_clap_embed_attach_hwnd(SphereDauxClapProcessor *p) {
  if (!p || !p->editor_attach_hwnd) {
    return 0;
  }
  return static_cast<unsigned long long>(
      reinterpret_cast<std::uintptr_t>(p->editor_attach_hwnd));
}

void sphere_daux_clap_embed_detach(SphereDauxClapProcessor *p) {
  if (!p) {
    return;
  }
  p->close_embed_editor("embed_detach");
}

int sphere_daux_clap_embed_is_valid(SphereDauxClapProcessor *p) {
  if (!p || !p->embed_mode || !p->editor_attached) {
    return 0;
  }
  return (p->editor_embed_top_hwnd && IsWindow(p->editor_embed_top_hwnd)) ? 1
                                                                          : 0;
}

int sphere_daux_clap_embed_has_visible_ui(SphereDauxClapProcessor *p) {
  if (!p || !p->editor_attach_hwnd || !IsWindow(p->editor_attach_hwnd)) {
    return 0;
  }
  HWND child = GetWindow(p->editor_attach_hwnd, GW_CHILD);
  return (child && IsWindowVisible(child)) ? 1 : 0;
}

unsigned long long sphere_daux_clap_open_editor(SphereDauxClapProcessor *p,
                                                const char *window_id,
                                                const char *title, int width,
                                                int height) {
  if (!p) {
    return 0;
  }
  p->editor_window_id = window_id ? window_id : "";
  if (title && *title) {
    p->editor_title = title;
  }
  if (p->editor_attached && p->editor_embed_top_hwnd &&
      IsWindow(p->editor_embed_top_hwnd)) {
    daux_editor_show_and_focus(&p->editor_window);
    return p->editor_handle;
  }
  p->embed_mode = false;
  // Standalone editor: a detached top-level window the user owns.
  return open_shell(p, nullptr, 2, 0, 0, width, height);
}

void sphere_daux_clap_close_editor(SphereDauxClapProcessor *p) {
  if (!p) {
    return;
  }
  p->close_embed_editor("close_editor");
}

int sphere_daux_clap_focus_editor(SphereDauxClapProcessor *p) {
  if (!p || !daux_editor_is_window_valid(&p->editor_window)) {
    return 0;
  }
  return daux_editor_show_and_focus(&p->editor_window) ? 1 : 0;
}

} // extern "C"

// ── Host-owned view host ────────────────────────────────────────────────────
//
// Everything above this line creates windows. Nothing below it does.
//
// The host — GPUI — owns the window a plug-in editor lives in: it creates the
// child HWND, positions it, paints the surround, and decides when it goes away.
// This side is reduced to the part of the CLAP GUI contract that has to be
// here: create the GUI, hand it the HWND it was given, pass sizes both ways,
// and destroy it. There is no shell, no titlebar and no window procedure of
// ours on this path, which is what makes the host the single owner of the
// editor geometry.
//
// `clap_host_gui->request_resize` is the one callback that would otherwise need
// a window here. It is recorded instead (`view_host_resize_*`): the host polls
// the request, resizes its own surface to whatever it can give, and reports
// back through `sphere_daux_clap_view_set_size`.

namespace {

/// Smallest and largest content size this host will hand a plug-in. Matches the
/// VST3 view host so a bad `get_size` cannot drag the shell to an absurd size.
constexpr int kViewHostMinSide = 64;
constexpr int kViewHostMaxSide = 8192;

void view_host_clamp(int *width, int *height) {
  if (width) {
    *width = std::clamp(*width, kViewHostMinSide, kViewHostMaxSide);
  }
  if (height) {
    *height = std::clamp(*height, kViewHostMinSide, kViewHostMaxSide);
  }
}

/// Destroys the GUI. Touches no window: the parent belongs to the host, which
/// decides on its own terms when it goes away.
void view_host_release(SphereDauxClapProcessor *p, const char *reason) {
  if (!p) {
    return;
  }
  if (p->gui_created && p->plugin && p->ext_gui) {
    if (p->ext_gui->hide) {
      p->ext_gui->hide(p->plugin);
    }
    if (p->ext_gui->destroy) {
      p->ext_gui->destroy(p->plugin);
    }
    p->gui_created = false;
  }
  p->view_host_attached = false;
  if (p->editor_attach_hwnd == p->view_host_parent) {
    // Only clear the borrowed handle, never destroy it.
    p->editor_attach_hwnd = nullptr;
  }
  p->view_host_parent = nullptr;
  p->view_host_resize_pending.store(false, std::memory_order_release);
  p->editor_attached = false;
  std::fprintf(stderr, "[clap-view-host] released reason=%s\n",
               reason ? reason : "unknown");
}

} // namespace

extern "C" {

int sphere_daux_clap_view_attach(SphereDauxClapProcessor *p,
                                 unsigned long long parent_hwnd, int width,
                                 int height, int *out_width, int *out_height) {
  if (!p || !p->plugin) {
    clap_set_last_error("view host: no CLAP plug-in instance");
    return 0;
  }
  if (!p->ext_gui) {
    clap_set_last_error("view host: CLAP plug-in exposes no GUI extension");
    return 0;
  }
  HWND parent =
      reinterpret_cast<HWND>(static_cast<std::uintptr_t>(parent_hwnd));
  if (!parent || !IsWindow(parent)) {
    clap_set_last_error("view host: invalid parent HWND");
    return 0;
  }
  // WebView-backed CLAP GUIs never finish initializing without an STA on the
  // thread that owns the parent window.
  ensure_com_initialized();

  // Re-attaching to the same window is the host asking for the size again, not
  // a request to tear a live GUI down and build another.
  if (p->view_host_attached && p->view_host_parent == parent) {
    int current_w = p->embed_content_w;
    int current_h = p->embed_content_h;
    p->preferred_gui_size(&current_w, &current_h);
    if (out_width) {
      *out_width = current_w;
    }
    if (out_height) {
      *out_height = current_h;
    }
    return 1;
  }
  if (p->view_host_attached) {
    view_host_release(p, "reattach-to-new-parent");
  }

  std::fprintf(
      stderr,
      "[clap-view-host] attach begin parent=0x%p request=%dx%d tid=%lu\n",
      static_cast<void *>(parent), width, height, GetCurrentThreadId());

  if (!p->gui_created) {
    if (!p->ext_gui->is_api_supported ||
        !p->ext_gui->is_api_supported(p->plugin, CLAP_WINDOW_API_WIN32,
                                      false)) {
      clap_set_last_error(
          "view host: CLAP plug-in does not support an embedded Win32 GUI");
      return 0;
    }
    if (!p->ext_gui->create ||
        !p->ext_gui->create(p->plugin, CLAP_WINDOW_API_WIN32, false)) {
      clap_set_last_error("view host: clap_plugin_gui->create() returned false");
      return 0;
    }
    p->gui_created = true;
  }

  if (p->ext_gui->set_scale) {
    const UINT dpi = daux_editor_hwnd_dpi(parent);
    p->ext_gui->set_scale(p->plugin, static_cast<double>(dpi) / 96.0);
  }

  // get_size is the source of truth for the content size. The requested region
  // is only a starting point: a fixed GUI gets the size it asks for, and the
  // host lays out around whatever comes back.
  int content_w = width > 0 ? width : 0;
  int content_h = height > 0 ? height : 0;
  p->preferred_gui_size(&content_w, &content_h);

  clap_window_t window{};
  window.api = CLAP_WINDOW_API_WIN32;
  window.win32 = parent;
  if (!p->ext_gui->set_parent || !p->ext_gui->set_parent(p->plugin, &window)) {
    if (p->ext_gui->destroy) {
      p->ext_gui->destroy(p->plugin);
    }
    p->gui_created = false;
    clap_set_last_error("view host: clap_plugin_gui->set_parent() failed");
    return 0;
  }
  if (p->ext_gui->show) {
    p->ext_gui->show(p->plugin);
  }
  // Some GUIs settle on their real size only once visible.
  p->preferred_gui_size(&content_w, &content_h);
  if (content_w <= 0 || content_h <= 0) {
    content_w = 900;
    content_h = 600;
  }
  view_host_clamp(&content_w, &content_h);

  p->view_host_parent = parent;
  p->view_host_attached = true;
  p->editor_attached = true;
  // Published so the shared "is the editor really showing anything" and focus
  // helpers keep working on this path. It is only ever read here: the window
  // belongs to the host, and `embed_mode` stays false, which is what keeps the
  // shell teardown from ever destroying it.
  p->editor_attach_hwnd = parent;
  p->embed_content_w = content_w;
  p->embed_content_h = content_h;
  p->view_host_resize_pending.store(false, std::memory_order_release);

  if (HWND child = GetWindow(parent, GW_CHILD)) {
    SetWindowPos(child, nullptr, 0, 0, content_w, content_h,
                 SWP_NOZORDER | SWP_NOACTIVATE | SWP_SHOWWINDOW);
  }

  if (out_width) {
    *out_width = content_w;
  }
  if (out_height) {
    *out_height = content_h;
  }
  std::fprintf(stderr,
               "[clap-view-host] attached instance=%s parent=0x%p "
               "content=%dx%d resizable=%d\n",
               p->embed_instance_label.empty()
                   ? "<unknown>"
                   : p->embed_instance_label.c_str(),
               static_cast<void *>(parent), content_w, content_h,
               p->editor_resizable ? 1 : 0);
  return 1;
}

void sphere_daux_clap_view_detach(SphereDauxClapProcessor *p) {
  if (!p || !p->view_host_attached) {
    return;
  }
  view_host_release(p, "host-detach");
}

int sphere_daux_clap_view_is_attached(SphereDauxClapProcessor *p) {
  return (p && p->view_host_attached && p->gui_created) ? 1 : 0;
}

int sphere_daux_clap_view_set_size(SphereDauxClapProcessor *p, int width,
                                   int height) {
  if (!p || !p->view_host_attached || width <= 0 || height <= 0) {
    return 0;
  }
  view_host_clamp(&width, &height);
  p->embed_content_w = width;
  p->embed_content_h = height;
  bool ok = true;
  if (p->editor_resizable && p->gui_created && p->ext_gui &&
      p->ext_gui->set_size) {
    ok = p->ext_gui->set_size(p->plugin, static_cast<uint32_t>(width),
                              static_cast<uint32_t>(height));
  }
  if (HWND child = GetWindow(p->view_host_parent, GW_CHILD)) {
    SetWindowPos(child, nullptr, 0, 0, width, height,
                 SWP_NOZORDER | SWP_NOACTIVATE);
  }
  return ok ? 1 : 0;
}

int sphere_daux_clap_view_get_size(SphereDauxClapProcessor *p, int *out_width,
                                   int *out_height) {
  if (!p || !out_width || !out_height) {
    return 0;
  }
  return p->preferred_gui_size(out_width, out_height) ? 1 : 0;
}

int sphere_daux_clap_view_can_resize(SphereDauxClapProcessor *p) {
  return (p && p->editor_resizable) ? 1 : 0;
}

int sphere_daux_clap_view_constrain(SphereDauxClapProcessor *p, int *io_width,
                                    int *io_height) {
  if (!p || !io_width || !io_height) {
    return 0;
  }
  view_host_clamp(io_width, io_height);
  if (!p->editor_resizable) {
    // Fixed GUI: it only ever has the one size it reports.
    int fixed_w = p->embed_content_w;
    int fixed_h = p->embed_content_h;
    if (!p->preferred_gui_size(&fixed_w, &fixed_h) &&
        (fixed_w <= 0 || fixed_h <= 0)) {
      return 0;
    }
    *io_width = fixed_w;
    *io_height = fixed_h;
    view_host_clamp(io_width, io_height);
    return 1;
  }
  if (p->ext_gui && p->ext_gui->adjust_size && p->gui_created) {
    auto w = static_cast<uint32_t>(std::max(1, *io_width));
    auto h = static_cast<uint32_t>(std::max(1, *io_height));
    if (p->ext_gui->adjust_size(p->plugin, &w, &h) && w > 0 && h > 0) {
      *io_width = static_cast<int>(w);
      *io_height = static_cast<int>(h);
      view_host_clamp(io_width, io_height);
    }
  }
  return 1;
}

int sphere_daux_clap_view_take_resize_request(SphereDauxClapProcessor *p,
                                              int *out_width, int *out_height) {
  if (!p || !out_width || !out_height) {
    return 0;
  }
  if (!p->view_host_resize_pending.exchange(false, std::memory_order_acq_rel)) {
    return 0;
  }
  *out_width = p->view_host_resize_w.load(std::memory_order_relaxed);
  *out_height = p->view_host_resize_h.load(std::memory_order_relaxed);
  return 1;
}


/// No host-owned editor window on this platform, so nothing for the chrome to
/// attach to — and a 0 handle is exactly how the chrome ABI says so.
unsigned long long
sphere_daux_clap_editor_native_window(SphereDauxClapProcessor *) {
  return 0;
}

} // extern "C"
