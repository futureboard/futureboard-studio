#pragma once

#include <cstddef>
#include <cstdint>

// NativeEditorShell uses manual layout intentionally.

enum class DauxEditorKind : int {
  ChildHwndEmbed = 0,
  OwnedToolWindow = 1,
  DetachedNativeWindow = 2,
};

struct DauxEditorWindowCallbacks {
  void *user_data = nullptr;
  bool (*is_live)(void *user_data) = nullptr;
  bool (*is_attached)(void *user_data) = nullptr;
  bool (*is_resize_in_progress)(void *user_data) = nullptr;
  void (*set_resize_in_progress)(void *user_data, bool value) = nullptr;
  bool (*can_resize)(void *user_data) = nullptr;
  bool (*constrain_content_size)(void *user_data, int *width,
                                 int *height) = nullptr;
  void (*on_content_resized)(void *user_data, void *content_hwnd, int width,
                             int height) = nullptr;
  void (*on_dpi_changed)(void *user_data, void *shell_hwnd, void *content_hwnd,
                         int width, int height) = nullptr;
  void (*on_close_requested)(void *user_data) = nullptr;
};

struct DauxEditorWindowConfig {
  void *owner_hwnd = nullptr;
  const wchar_t *title = nullptr;
  // Human-readable plug-in name shown in the content "Loading Plugin" overlay
  // while the editor view is still attaching. Falls back to `title` when null.
  const wchar_t *plugin_name = nullptr;
  int host_kind = static_cast<int>(DauxEditorKind::OwnedToolWindow);
  int x = 0;
  int y = 0;
  int content_width = 640;
  int content_height = 480;
  bool pin_default = false;
  DauxEditorWindowCallbacks callbacks{};
};

struct DauxEditorWindow {
  void *shell_hwnd = nullptr;
  void *content_hwnd = nullptr;
  void *owner_hwnd = nullptr;
  void *internal = nullptr;
  int host_kind = static_cast<int>(DauxEditorKind::OwnedToolWindow);
  int titlebar_height = 0;
  int content_width = 0;
  int content_height = 0;
  bool pinned = false;
};

int daux_editor_resolve_host_kind();
const char *daux_editor_host_kind_name(int kind);
const char *daux_editor_selected_mode_label(int kind);

bool daux_editor_env_truthy(const char *name);
bool daux_editor_content_screen_rect(void *parent_hwnd, int x, int y, int width,
                                     int height, long *left, long *top,
                                     long *right, long *bottom);
void daux_editor_apply_tool_styles(void *shell_hwnd, void *owner_hwnd);
void daux_editor_apply_owner(DauxEditorWindow *window, void *owner_hwnd);
bool daux_editor_show_and_focus(DauxEditorWindow *window);
void daux_editor_raise_children(void *shell_hwnd);
bool daux_editor_create_window(const DauxEditorWindowConfig *config,
                               DauxEditorWindow *out_window);
void daux_editor_destroy_window(DauxEditorWindow *window);
// Loading/error overlay state for the content area (drawn until the plug-in's
// IPlugView attaches). `failed=false, message=null` shows "Loading Plugin
// <name>"; `failed=false, message!=null` shows a waiting state with the
// current lifecycle stage; `failed=true` shows an error state with `message`
// (UTF-16, may be null).
void daux_editor_set_load_state(DauxEditorWindow *window, bool failed,
                                const wchar_t *message);
bool daux_editor_resize_content(DauxEditorWindow *window, int content_width,
                                int content_height);
void daux_editor_set_pin_to_top(DauxEditorWindow *window, bool pinned);
bool daux_editor_is_window_valid(const DauxEditorWindow *window);
void *daux_editor_get_content_hwnd(const DauxEditorWindow *window);
int daux_editor_titlebar_height(void *shell_hwnd);
unsigned daux_editor_hwnd_dpi(void *hwnd);
void daux_editor_settle_pump_thread(unsigned max_ms);
