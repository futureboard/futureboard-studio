#include "sphere_plugin_host_vst3.h"

#include <cstring>
#include <new>

namespace {

// Rust releases every returned string with `sphere_plugin_host_free_string`
// (`delete[]`), so the payload must come from `new[]`, never static storage.
SpherePluginHostString empty_json_array() {
  static const char kEmpty[] = "[]";
  auto* data = new (std::nothrow) char[sizeof(kEmpty)];
  if (!data) return {nullptr, 0};
  std::memcpy(data, kEmpty, sizeof(kEmpty));
  return {data, sizeof(kEmpty) - 1};
}

}  // namespace

extern "C" {

SPHERE_PLUGIN_HOST_API SpherePluginHostString sphere_au_scan_json() {
  return empty_json_array();
}

SPHERE_PLUGIN_HOST_API SpherePluginHostString sphere_au_validate_component_json(
    const char* /*component_id*/) {
  return empty_json_array();
}

}  // extern "C"
