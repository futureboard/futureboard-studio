#import <AudioToolbox/AudioToolbox.h>
#import <CoreFoundation/CoreFoundation.h>

#include "sphere_plugin_host_vst3.h"

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <new>
#include <string>
#include <vector>

namespace {

struct AuScanEntry {
  std::string name;
  std::string vendor;
  std::string category;
  std::string format;
  std::string path;
  std::string class_id;
  std::string version;
  bool is_instrument;
  bool is_effect;
  bool sdk_metadata_loaded;
};

static std::string fourcc_to_string(UInt32 code) {
  char bytes[5];
  bytes[0] = static_cast<char>((code >> 24) & 0xFF);
  bytes[1] = static_cast<char>((code >> 16) & 0xFF);
  bytes[2] = static_cast<char>((code >> 8) & 0xFF);
  bytes[3] = static_cast<char>(code & 0xFF);
  bytes[4] = '\0';
  return std::string(bytes);
}

static std::string cf_string_to_utf8(CFStringRef value) {
  if (value == nullptr) {
    return {};
  }
  char buffer[4096];
  if (!CFStringGetCString(value, buffer, sizeof(buffer), kCFStringEncodingUTF8)) {
    return {};
  }
  return std::string(buffer);
}

static std::string json_escape(const std::string& input) {
  std::string out;
  out.reserve(input.size() + 8);
  for (unsigned char ch : input) {
    switch (ch) {
      case '\\':
        out += "\\\\";
        break;
      case '"':
        out += "\\\"";
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
        if (ch < 0x20) {
          char hex[7];
          std::snprintf(hex, sizeof(hex), "\\u%04x", ch);
          out += hex;
        } else {
          out.push_back(static_cast<char>(ch));
        }
        break;
    }
  }
  return out;
}

static std::string category_for_type(OSType type) {
  switch (type) {
    case kAudioUnitType_MusicDevice:
      return "Instrument";
    case kAudioUnitType_Effect:
      return "Effect";
    case kAudioUnitType_MusicEffect:
      return "Music Effect";
    case kAudioUnitType_Generator:
      return "Generator";
    case kAudioUnitType_Panner:
      return "Panner";
    case kAudioUnitType_Mixer:
      return "Mixer";
    default:
      return "AudioUnit";
  }
}

static bool is_instrument_type(OSType type) {
  return type == kAudioUnitType_MusicDevice || type == kAudioUnitType_Generator;
}

static std::string component_identifier(const AudioComponentDescription& desc) {
  char buffer[128];
  std::snprintf(
      buffer,
      sizeof(buffer),
      "au:%08x:%08x:%08x",
      static_cast<unsigned>(desc.componentType),
      static_cast<unsigned>(desc.componentSubType),
      static_cast<unsigned>(desc.componentManufacturer));
  return std::string(buffer);
}

static std::string bundle_path_for_component(AudioComponent component) {
  if (component == nullptr) {
    return {};
  }
  // `AudioComponentCopyComponentInfo` is not declared by all macOS SDKs used
  // in CI. Keep scanning compatible by using the stable component identifier as
  // the registry path fallback; validation/loading uses the component id.
  return {};
}

static std::string version_for_component(AudioComponent component) {
  if (component == nullptr) {
    return {};
  }
  UInt32 version = 0;
  if (AudioComponentGetVersion(component, &version) != noErr || version == 0) {
    return {};
  }
  char buffer[32];
  std::snprintf(
      buffer,
      sizeof(buffer),
      "%u.%u.%u",
      static_cast<unsigned>((version >> 16) & 0xFFFF),
      static_cast<unsigned>((version >> 8) & 0xFF),
      static_cast<unsigned>(version & 0xFF));
  return std::string(buffer);
}

static void append_entry(std::vector<AuScanEntry>& entries, AudioComponent component) {
  if (component == nullptr) {
    return;
  }

  AudioComponentDescription desc {};
  if (AudioComponentGetDescription(component, &desc) != noErr) {
    return;
  }

  CFStringRef name_ref = nullptr;
  if (AudioComponentCopyName(component, &name_ref) != noErr || name_ref == nullptr) {
    return;
  }

  std::string name = cf_string_to_utf8(name_ref);
  CFRelease(name_ref);
  if (name.empty()) {
    name = "Unknown AudioUnit";
  }

  std::string vendor = fourcc_to_string(desc.componentManufacturer);
  if (vendor.empty() || vendor == "????") {
    vendor = "Unknown Vendor";
  }

  std::string path = bundle_path_for_component(component);
  if (path.empty()) {
    path = component_identifier(desc);
  }

  std::string version = version_for_component(component);
  std::string class_id = component_identifier(desc);

  AuScanEntry entry;
  entry.name = std::move(name);
  entry.vendor = std::move(vendor);
  entry.category = category_for_type(desc.componentType);
  entry.format = "AU";
  entry.path = std::move(path);
  entry.class_id = std::move(class_id);
  entry.version = std::move(version);
  entry.is_instrument = is_instrument_type(desc.componentType);
  entry.is_effect = !entry.is_instrument;
  entry.sdk_metadata_loaded = true;
  entries.push_back(std::move(entry));
}

static std::vector<AuScanEntry> enumerate_audio_units() {
  std::vector<AuScanEntry> entries;
  AudioComponentDescription search {};
  search.componentType = 0;
  search.componentSubType = 0;
  search.componentManufacturer = 0;
  search.componentFlags = 0;
  search.componentFlagsMask = 0;

  AudioComponent component = nullptr;
  while ((component = AudioComponentFindNext(component, &search)) != nullptr) {
    append_entry(entries, component);
  }
  return entries;
}

// Every SpherePluginHostString handed to Rust is released by
// `sphere_plugin_host_free_string` (vst3_scanner.cpp), which uses `delete[]`.
// Allocate with the matching `new[]`, and report out-of-memory as a null
// payload (Rust turns that into a scan error) rather than handing back a
// static string the caller would then free.
static SpherePluginHostString make_json_string(const std::string& json) {
  char* payload = new (std::nothrow) char[json.size() + 1];
  if (payload == nullptr) {
    return SpherePluginHostString{nullptr, 0};
  }
  std::memcpy(payload, json.data(), json.size());
  payload[json.size()] = '\0';
  return SpherePluginHostString{payload, json.size()};
}

static std::string entries_to_json(const std::vector<AuScanEntry>& entries) {
  std::string json = "[";
  for (size_t index = 0; index < entries.size(); ++index) {
    const AuScanEntry& entry = entries[index];
    if (index > 0) {
      json += ',';
    }
    json += "{";
    json += "\"name\":\"" + json_escape(entry.name) + "\",";
    json += "\"vendor\":\"" + json_escape(entry.vendor) + "\",";
    json += "\"category\":\"" + json_escape(entry.category) + "\",";
    json += "\"format\":\"" + json_escape(entry.format) + "\",";
    json += "\"path\":\"" + json_escape(entry.path) + "\",";
    json += "\"classId\":\"" + json_escape(entry.class_id) + "\",";
    json += "\"version\":\"" + json_escape(entry.version) + "\",";
    json += "\"sdkMetadataLoaded\":" + std::string(entry.sdk_metadata_loaded ? "true" : "false");
    json += "}";
  }
  json += "]";
  return json;
}

static bool validate_component_id(const char* component_id) {
  if (component_id == nullptr || component_id[0] == '\0') {
    return false;
  }

  unsigned type = 0;
  unsigned subtype = 0;
  unsigned manufacturer = 0;
  if (std::sscanf(component_id, "au:%x:%x:%x", &type, &subtype, &manufacturer) != 3) {
    return false;
  }

  AudioComponentDescription desc {};
  desc.componentType = static_cast<OSType>(type);
  desc.componentSubType = static_cast<OSType>(subtype);
  desc.componentManufacturer = static_cast<OSType>(manufacturer);

  AudioComponent component = AudioComponentFindNext(nullptr, &desc);
  if (component == nullptr) {
    return false;
  }

  AudioComponentInstance instance = nullptr;
  OSStatus status = AudioComponentInstanceNew(component, &instance);
  if (status != noErr || instance == nullptr) {
    return false;
  }
  AudioComponentInstanceDispose(instance);
  return true;
}

}  // namespace

extern "C" {

SPHERE_PLUGIN_HOST_API SpherePluginHostString sphere_au_scan_json() {
  const std::vector<AuScanEntry> entries = enumerate_audio_units();
  return make_json_string(entries_to_json(entries));
}

SPHERE_PLUGIN_HOST_API SpherePluginHostString sphere_au_validate_component_json(
    const char* component_id) {
  const bool ok = validate_component_id(component_id);
  const std::string json = ok ? "{\"ok\":true}" : "{\"ok\":false}";
  return make_json_string(json);
}

}  // extern "C"
