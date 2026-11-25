#include "sphere_plugin_host_vst3.h"

#include "pluginterfaces/base/ipluginbase.h"
#include "pluginterfaces/vst/ivstcomponent.h"
#include "public.sdk/source/vst/hosting/hostclasses.h"
#include "public.sdk/source/vst/hosting/module.h"
#include "public.sdk/source/vst/utility/stringconvert.h"
#include "public.sdk/source/vst/utility/uid.h"

// Only for `kARAMainFactoryClass`. An ARA-capable VST3 registers a second
// factory class in this category alongside its audio-module class, so ARA
// capability is visible from `getClassInfo` alone — no instantiation, and
// therefore no change to this scanner's crash-isolation model.
//
// The ARA SDK is an optional submodule (CI initializes it only where ARA is
// actually hosted — see `.github/workflows/submodules-init.sh`), while this
// scanner is built on every platform. Take the header when it is checked out
// and otherwise define the one thing used from it; `ARAVST3.h` guards the same
// macro with `#if !defined`, so the two definitions can never disagree.
#if __has_include("ARAVST3.h")
#include "ARAVST3.h"
#else
#if !defined(kARAMainFactoryClass)
#define kARAMainFactoryClass "ARA Main Factory Class"
#endif
#endif

#include "clap/clap.h"
#include "clap/factory/plugin-factory.h"

// Single source of the VST2 ABI, shared with the runtime bridge in
// SphereDirectAudioEngine so scanner and host can never drift.
#include "sphere_vst2_abi.h"

#include <atomic>
#include <cctype>
#include <chrono>
#include <cstddef>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <future>
#include <memory>
#include <new>
#include <string>
#include <thread>
#include <vector>

#ifdef _WIN32
#  define WIN32_LEAN_AND_MEAN
#  include <windows.h>
#else
#  include <dlfcn.h>
#endif

namespace {

// Match VST3 SDK string literals exactly.
constexpr const char* kAudioModuleClass = "Audio Module Class";

SpherePluginHostString make_string(std::string value) {
  auto* data = new (std::nothrow) char[value.size() + 1];
  if (!data) {
    return {nullptr, 0};
  }
  std::memcpy(data, value.data(), value.size());
  data[value.size()] = '\0';
  return {data, static_cast<unsigned long long>(value.size())};
}

std::string escape_json(const std::string& value) {
  std::string out;
  out.reserve(value.size() + 8);
  for (char c : value) {
    if (c == '\\' || c == '"') {
      out.push_back('\\');
    }
    if (c == '\n') {
      out += "\\n";
      continue;
    }
    if (c == '\r') {
      out += "\\r";
      continue;
    }
    if (c == '\t') {
      out += "\\t";
      continue;
    }
    out.push_back(c);
  }
  return out;
}

std::string lower_extension(const std::filesystem::path& path) {
  auto ext = path.extension().string();
  for (auto& c : ext) {
    c = static_cast<char>(std::tolower(static_cast<unsigned char>(c)));
  }
  return ext;
}

bool is_vst3_bundle(const std::filesystem::path& path) {
  return lower_extension(path) == ".vst3";
}

bool is_clap_plugin(const std::filesystem::path& path) {
  return lower_extension(path) == ".clap";
}

std::string uid_to_string(const Steinberg::TUID cid) {
  return VST3::UID::fromTUID(cid).toString();
}

template <typename T>
std::string char_array_to_string(const T* value, Steinberg::uint32 max) {
  return Steinberg::Vst::StringConvert::convert(value, max);
}

struct ClassEntry {
  std::string name;
  std::string vendor;
  std::string category;
  std::string sub_categories;
  std::string class_id;
  std::string version;
  std::string sdk_version;
  /// This module also registers an ARA main-factory class paired with this
  /// audio-module class, so the plug-in can be driven through ARA. See the
  /// pairing rules in `ARAVST3.h`.
  bool is_ara = false;
  /// Bus layout read from one instantiated component (see
  /// `probe_vst3_buses`). All false / zero when the probe failed.
  bool midi_input = false;
  bool midi_output = false;
  int audio_outputs = 0;
};

/// Modules with more audio-module classes than this are not bus-probed. Shells
/// that large (WaveShell: hundreds of classes) take minutes to instantiate
/// class by class, and no known MIDI effect ships inside one.
constexpr std::size_t kMaxBusProbeClasses = 16;

/// Wall-clock budget for all bus probes of one module, well inside the 30 s
/// per-bundle scan timeout (`DEFAULT_BUNDLE_SCAN_TIMEOUT_SECS`) so a slow
/// module still reports its rows instead of timing out.
constexpr std::chrono::seconds kBusProbeBudget{10};

/// Instantiate the component of one audio-module class just long enough to
/// read its bus layout, then tear it down. No processing, no editor, no
/// controller — `initialize()` / bus counts / `terminate()` only.
///
/// VST3 has no MIDI-effect class category: chord tools and arpeggiators
/// register as `Instrument` or `Fx` and are told apart only by an event
/// *output* bus, which factory class info does not carry.
///
/// Any failure leaves the entry at "unknown" (false / zero). A plug-in that
/// hangs or crashes here is contained by the per-bundle scan child process and
/// its timeout (`src/scan/isolation.rs`).
void probe_vst3_buses(const VST3::Hosting::PluginFactory& factory,
                      Steinberg::Vst::HostApplication& host_context,
                      ClassEntry& entry, bool debug) {
  try {
    const auto uid = VST3::UID::fromString(entry.class_id);
    if (!uid) {
      return;
    }
    auto component = factory.createInstance<Steinberg::Vst::IComponent>(*uid);
    if (!component) {
      if (debug) {
        std::fprintf(stderr,
                     "[SpherePluginHost]     bus probe: createInstance failed\n");
      }
      return;
    }
    if (component->initialize(&host_context) != Steinberg::kResultOk) {
      if (debug) {
        std::fprintf(stderr,
                     "[SpherePluginHost]     bus probe: initialize failed\n");
      }
      return;
    }
    const auto event_in =
        component->getBusCount(Steinberg::Vst::kEvent, Steinberg::Vst::kInput);
    const auto event_out =
        component->getBusCount(Steinberg::Vst::kEvent, Steinberg::Vst::kOutput);
    const auto audio_out =
        component->getBusCount(Steinberg::Vst::kAudio, Steinberg::Vst::kOutput);
    component->terminate();

    entry.midi_input = event_in > 0;
    entry.midi_output = event_out > 0;
    entry.audio_outputs = audio_out > 0 ? static_cast<int>(audio_out) : 0;
    if (debug) {
      std::fprintf(stderr,
                   "[SpherePluginHost]     bus probe: event in=%d out=%d, "
                   "audio out=%d\n",
                   static_cast<int>(event_in), static_cast<int>(event_out),
                   static_cast<int>(audio_out));
    }
  } catch (...) {
    entry.midi_input = false;
    entry.midi_output = false;
    entry.audio_outputs = 0;
    if (debug) {
      std::fprintf(stderr, "[SpherePluginHost]     bus probe: exception\n");
    }
  }
}

/// Set once a bus probe was still inside a plug-in when its deadline passed.
/// That thread is left running — a thread cannot be stopped safely inside
/// vendor code — with its module kept loaded, so the scanner process has to
/// end without the normal teardown (`sphere_vst3_scan_left_work_running`).
std::atomic<bool> g_scan_left_work_running{false};

/// [`probe_vst3_buses`] with a real deadline.
///
/// The probe instantiates and initializes the component, and some plug-ins do
/// their whole start-up there: ZENOLOGY loads its sound engine in
/// `initialize()` and takes ~50 s. The budget used to be checked only between
/// classes, so one slow `initialize()` ran the bundle past the 30 s per-bundle
/// timeout and every row of it was dropped — the plug-in vanished from the
/// list although its class info had been read in under a second.
///
/// The probe runs on its own thread; `entry` gets its bus layout when the
/// probe finishes by `deadline`. Otherwise the entry keeps the "unknown" bus
/// layout, the thread is detached with everything it uses kept alive (the
/// module stays loaded, `host_context` is never freed), and `false` is
/// returned so no further class is probed.
bool probe_vst3_buses_bounded(const VST3::Hosting::Module::Ptr& module,
                              const VST3::Hosting::PluginFactory& factory,
                              Steinberg::Vst::HostApplication* host_context,
                              ClassEntry& entry, bool debug,
                              std::chrono::steady_clock::time_point deadline) {
  struct Job {
    VST3::Hosting::Module::Ptr module;
    VST3::Hosting::PluginFactory factory;
    Steinberg::Vst::HostApplication* host;
    ClassEntry entry;
    bool debug;
    std::promise<void> done;
  };
  auto job = std::make_shared<Job>(
      Job{module, factory, host_context, entry, debug, std::promise<void>()});
  auto finished = job->done.get_future();
  std::thread worker;
  try {
    worker = std::thread([job] {
      probe_vst3_buses(job->factory, *job->host, job->entry, job->debug);
      job->done.set_value();
    });
  } catch (...) {
    // No thread, no probe: the entry stays "unknown", as on a failed probe.
    return true;
  }
  if (finished.wait_until(deadline) == std::future_status::ready) {
    worker.join();
    entry.midi_input = job->entry.midi_input;
    entry.midi_output = job->entry.midi_output;
    entry.audio_outputs = job->entry.audio_outputs;
    return true;
  }
  worker.detach();
  g_scan_left_work_running.store(true);
  if (debug) {
    std::fprintf(stderr,
                 "[SpherePluginHost]     bus probe still running at its "
                 "deadline; listed without a bus layout\n");
  }
  return false;
}

/// Emit an entry for a module whose metadata could not be read.
///
/// `sdkMetadataLoaded:false` plus `loadError` is what marks it: the Rust side
/// turns these into scan *failures* rather than plug-in rows, so a broken or
/// wrong-architecture module stays reportable by path and reason instead of
/// appearing in the browser as a plug-in named after its file.
void append_fallback_entry(std::string& json, bool& first,
                           const std::filesystem::path& plugin_path,
                           const char* format, const std::string& load_error) {
  if (!first) {
    json += ",";
  }
  first = false;
  const auto name = plugin_path.stem().string();
  const auto full_path = plugin_path.string();
  json += "{\"name\":\"" + escape_json(name) + "\",";
  json += "\"vendor\":\"Unknown Vendor\",";
  json += "\"category\":\"Uncategorized\",";
  json += "\"subCategories\":\"\",";
  json += "\"format\":\"" + std::string(format) + "\",";
  json += "\"path\":\"" + escape_json(full_path) + "\",";
  json += "\"modulePath\":\"" + escape_json(full_path) + "\",";
  json += "\"classId\":null,\"version\":\"\",\"sdkVersion\":\"\",";
  json += "\"loadError\":\"" +
          escape_json(load_error.empty() ? "Plug-in module could not be loaded"
                                         : load_error) +
          "\",";
  json += "\"isShellChild\":false,\"sdkMetadataLoaded\":false}";
}

/// Architecture sub-directory this build can load out of a VST3 bundle.
/// A bundle that only ships the other word size is not broken — it just is not
/// for this host, and reporting it as a load failure is more honest (and much
/// faster) than letting `LoadLibrary` fail on it.
constexpr const char* kHostArchDir =
#if defined(_WIN32)
#  if defined(_M_ARM64) || defined(__aarch64__)
    "arm64-win";
#  elif defined(_WIN64)
    "x86_64-win";
#  else
    "x86-win";
#  endif
#elif defined(__APPLE__)
    "MacOS";
#elif defined(__aarch64__)
    "aarch64-linux";
#else
    "x86_64-linux";
#endif

/// Empty when the bundle is loadable here. Otherwise a reason to report.
///
/// Only bundles that actually carry a `Contents/` directory are judged: the
/// legacy single-DLL `.vst3` layout has no architecture folders, and the loader
/// is left to decide.
std::string vst3_arch_mismatch_reason(const std::filesystem::path& bundle) {
  std::error_code ec;
  const auto contents = bundle / "Contents";
  if (!std::filesystem::is_directory(contents, ec)) {
    return {};
  }
  bool saw_arch_dir = false;
  std::string available;
  for (const auto& entry : std::filesystem::directory_iterator(contents, ec)) {
    if (ec) {
      return {};
    }
    if (!entry.is_directory(ec)) {
      continue;
    }
    const auto name = entry.path().filename().string();
    // `Resources` / `moduleinfo.json` siblings are not architecture folders.
    if (name == "Resources") {
      continue;
    }
    if (name == kHostArchDir) {
      return {};
    }
    saw_arch_dir = true;
    if (!available.empty()) {
      available += ", ";
    }
    available += name;
  }
  if (!saw_arch_dir) {
    return {};
  }
  return std::string("Plug-in has no ") + kHostArchDir +
         " build (found: " + available + ")";
}

std::string clap_feature_string(const char* const* features) {
  if (!features) {
    return "";
  }

  std::string joined;
  for (std::size_t i = 0; features[i]; ++i) {
    if (i > 0) {
      joined += "|";
    }
    joined += features[i];
  }
  return joined;
}

bool clap_has_feature(const char* const* features, const char* expected) {
  if (!features || !expected) {
    return false;
  }
  for (std::size_t i = 0; features[i]; ++i) {
    if (std::strcmp(features[i], expected) == 0) {
      return true;
    }
  }
  return false;
}

std::string clap_category(const char* const* features) {
  if (clap_has_feature(features, CLAP_PLUGIN_FEATURE_INSTRUMENT)) {
    return "Instrument";
  }
  if (clap_has_feature(features, CLAP_PLUGIN_FEATURE_AUDIO_EFFECT)) {
    return "Audio Effect";
  }
  if (clap_has_feature(features, CLAP_PLUGIN_FEATURE_NOTE_EFFECT)) {
    return "Note Effect";
  }
  if (features && features[0]) {
    return features[0];
  }
  return "Uncategorized";
}

class SharedLibrary {
 public:
  explicit SharedLibrary(const std::filesystem::path& path) {
#ifdef _WIN32
    handle_ = LoadLibraryW(path.wstring().c_str());
#else
    handle_ = dlopen(path.string().c_str(), RTLD_NOW | RTLD_LOCAL);
#endif
  }

  ~SharedLibrary() {
#ifdef _WIN32
    if (handle_) {
      FreeLibrary(static_cast<HMODULE>(handle_));
    }
#else
    if (handle_) {
      dlclose(handle_);
    }
#endif
  }

  SharedLibrary(const SharedLibrary&) = delete;
  SharedLibrary& operator=(const SharedLibrary&) = delete;

  bool valid() const { return handle_ != nullptr; }

  void* symbol(const char* name) const {
    if (!handle_) {
      return nullptr;
    }
#ifdef _WIN32
    return reinterpret_cast<void*>(GetProcAddress(static_cast<HMODULE>(handle_), name));
#else
    return dlsym(handle_, name);
#endif
  }

 private:
  void* handle_ = nullptr;
};

#ifdef __APPLE__
std::filesystem::path clap_executable_path(const std::filesystem::path& plugin_path) {
  if (!std::filesystem::is_directory(plugin_path)) {
    return plugin_path;
  }
  const auto name = plugin_path.stem().string();
  return plugin_path / "Contents" / "MacOS" / name;
}
#else
std::filesystem::path clap_executable_path(const std::filesystem::path& plugin_path) {
  return plugin_path;
}
#endif

// ── VST2 ─────────────────────────────────────────────────────────────────────
//
// VST2 has no factory to interrogate: metadata only exists on an *instantiated*
// AEffect. So the scan opens each candidate with a minimal audioMaster, reads
// its name/vendor/category/IO, and closes it again — still no processing, no
// editor, no realtime audio. This runs inside the isolating scanner subprocess
// (see scan/isolation.rs), which is what makes loading unknown binaries safe.

bool is_vst2_module(const std::filesystem::path& path) {
#ifdef __APPLE__
  // macOS VST2 is a `.vst` bundle.
  return lower_extension(path) == ".vst";
#else
  // Windows VST2 is a bare `.dll` — indistinguishable by name from any other
  // library, so `vst2_entry_point` below is what actually decides.
  const auto ext = lower_extension(path);
  return ext == ".dll" || ext == ".vst2";
#endif
}

std::filesystem::path vst2_executable_path(const std::filesystem::path& plugin_path) {
#ifdef __APPLE__
  if (!std::filesystem::is_directory(plugin_path)) {
    return plugin_path;
  }
  return plugin_path / "Contents" / "MacOS" / plugin_path.stem().string();
#else
  return plugin_path;
#endif
}

/// Empty when the module is loadable here. Otherwise a reason to report, so a
/// 32-bit plug-in says so instead of failing as a generic load error.
std::string vst2_arch_mismatch_reason(const std::filesystem::path& module_path) {
#ifdef _WIN32
  std::FILE* file = nullptr;
  if (_wfopen_s(&file, module_path.wstring().c_str(), L"rb") != 0 || !file) {
    return {};
  }
  unsigned char dos[64] = {};
  std::string reason;
  if (std::fread(dos, 1, sizeof(dos), file) == sizeof(dos) && dos[0] == 'M' &&
      dos[1] == 'Z') {
    const long pe_offset = static_cast<long>(dos[60]) |
                           (static_cast<long>(dos[61]) << 8) |
                           (static_cast<long>(dos[62]) << 16) |
                           (static_cast<long>(dos[63]) << 24);
    unsigned char pe[6] = {};
    if (std::fseek(file, pe_offset, SEEK_SET) == 0 &&
        std::fread(pe, 1, sizeof(pe), file) == sizeof(pe) && pe[0] == 'P' &&
        pe[1] == 'E') {
      const unsigned machine =
          static_cast<unsigned>(pe[4]) | (static_cast<unsigned>(pe[5]) << 8);
      constexpr unsigned kAmd64 = 0x8664;
      constexpr unsigned kArm64 = 0xAA64;
      constexpr unsigned kI386 = 0x014C;
#if defined(_WIN64)
      // A 32-bit module is scanned by the 32-bit scanner
      // (FutureboardPluginScannerX86.exe), never by this one.
      if (machine != kAmd64 && machine != kArm64) {
        reason = machine == kI386
                     ? "Plug-in is a 32-bit VST2 build; it is scanned and "
                       "loaded through the 32-bit plug-in bridge"
                     : "Plug-in is not built for this processor";
      }
#else
      // The 32-bit scanner of the plug-in bridge loads 32-bit modules only.
      if (machine != kI386) {
        reason = "Plug-in is not a 32-bit build; the 64-bit scanner reads it";
      }
#endif
    }
  }
  std::fclose(file);
  return reason;
#else
  (void)module_path;
  return {};
#endif
}

using Vst2EntryProc = AEffect*(SPHERE_VST2_CC*)(AudioMasterCallback);

Vst2EntryProc vst2_entry_point(const SharedLibrary& library) {
  static const char* kNames[] = {"VSTPluginMain", "main", "main_macho",
                                 "main_plugin"};
  for (const char* name : kNames) {
    if (auto* symbol = library.symbol(name)) {
      return reinterpret_cast<Vst2EntryProc>(symbol);
    }
  }
  return nullptr;
}

/// Shell sub-plug-in id the next instantiation should select. VST2 asks for it
/// from inside the entry point, before any AEffect exists, so it has to be
/// reachable without one.
thread_local int32_t g_scan_shell_id = 0;

/// Minimal host callback for scanning. Answers only what a plug-in needs to
/// finish `open()`; everything else is declined so nothing is claimed that the
/// scanner does not actually provide.
intptr_t SPHERE_VST2_CC vst2_scan_audio_master(AEffect*, int32_t opcode, int32_t,
                                               intptr_t, void* ptr, float) {
  switch (opcode) {
    case audioMasterVersion:
      return 2400;
    case audioMasterCurrentId:
      return g_scan_shell_id;
    case audioMasterGetSampleRate:
      return 44100;
    case audioMasterGetBlockSize:
      return 512;
    case audioMasterGetCurrentProcessLevel:
      return kVstProcessLevelUser;
    case audioMasterGetVendorString:
      if (ptr) {
        std::snprintf(static_cast<char*>(ptr), 64, "%s", "Futureboard Digital Technologies");
        return 1;
      }
      return 0;
    case audioMasterGetProductString:
      if (ptr) {
        std::snprintf(static_cast<char*>(ptr), 64, "%s", "Futureboard Studio");
        return 1;
      }
      return 0;
    case audioMasterGetVendorVersion:
      return 1;
    case audioMasterCanDo:
      if (ptr) {
        const std::string feature(static_cast<const char*>(ptr));
        if (feature == "shellCategory" || feature == "sendVstEvents" ||
            feature == "sendVstMidiEvent") {
          return 1;
        }
      }
      return -1;
    case audioMasterIdle:
      return 1;
    default:
      return 0;
  }
}

std::string vst2_dispatch_string(AEffect* effect, int32_t opcode, int32_t index) {
  if (!effect || !effect->dispatcher) {
    return {};
  }
  // 256 rather than the documented 64: some plug-ins overrun the spec'd size.
  char buffer[256] = {};
  effect->dispatcher(effect, opcode, index, 0, buffer, 0.f);
  buffer[sizeof(buffer) - 1] = '\0';
  return std::string(buffer);
}

const char* vst2_category_name(intptr_t plug_category, bool is_synth) {
  if (is_synth) {
    return "Instrument";
  }
  switch (plug_category) {
    case kPlugCategSynth:
    case kPlugCategGenerator:
      return "Instrument";
    case kPlugCategAnalysis:
      return "Analyzer";
    case kPlugCategMastering:
      return "Mastering";
    case kPlugCategSpacializer:
    case kPlugCategRoomFx:
    case kPlugSurroundFx:
      return "Spatial";
    case kPlugCategRestoration:
      return "Restoration";
    case kPlugCategEffect:
      return "Effect";
    default:
      return "Effect";
  }
}

struct Vst2ScanEntry {
  std::string name;
  std::string vendor;
  std::string category;
  std::string sub_categories;
  std::string class_id;
  std::string version;
  bool is_shell_child = false;
};

/// Open one (optionally shell-selected) plug-in and read its metadata.
/// `out_shell_children` is filled only for a shell module's first pass.
bool vst2_read_entry(Vst2EntryProc entry, int32_t shell_id,
                     const std::filesystem::path& plugin_path,
                     Vst2ScanEntry* out,
                     std::vector<int32_t>* out_shell_children) {
  g_scan_shell_id = shell_id;
  AEffect* effect = entry(&vst2_scan_audio_master);
  g_scan_shell_id = 0;
  if (!effect || effect->magic != kEffectMagic || !effect->dispatcher) {
    return false;
  }

  effect->dispatcher(effect, effOpen, 0, 0, nullptr, 0.f);

  const bool is_synth = (effect->flags & effFlagsIsSynth) != 0;
  const auto plug_category =
      effect->dispatcher(effect, effGetPlugCategory, 0, 0, nullptr, 0.f);

  std::string name = vst2_dispatch_string(effect, effGetEffectName, 0);
  if (name.empty()) {
    name = vst2_dispatch_string(effect, effGetProductString, 0);
  }
  if (name.empty()) {
    name = plugin_path.stem().string();
  }
  std::string vendor = vst2_dispatch_string(effect, effGetVendorString, 0);
  if (vendor.empty()) {
    vendor = "Unknown Vendor";
  }
  const auto vendor_version =
      effect->dispatcher(effect, effGetVendorVersion, 0, 0, nullptr, 0.f);

  if (out_shell_children && plug_category == kPlugCategShell) {
    // Shell module (Waves, Kontakt): enumerate the sub-plug-ins it hosts.
    // Bounded so a plug-in that never returns 0 cannot spin the scanner.
    constexpr int kMaxShellChildren = 1024;
    for (int i = 0; i < kMaxShellChildren; ++i) {
      char child_name[128] = {};
      const auto child_id = effect->dispatcher(effect, effShellGetNextPlugin, 0,
                                               0, child_name, 0.f);
      if (child_id == 0) {
        break;
      }
      out_shell_children->push_back(static_cast<int32_t>(child_id));
    }
  }

  out->name = std::move(name);
  out->vendor = std::move(vendor);
  out->category = vst2_category_name(plug_category, is_synth);
  out->sub_categories = is_synth ? "Instrument" : "Fx";
  out->version = vendor_version > 0 ? std::to_string(vendor_version) : "";
  out->class_id = "vst2:" + std::to_string(shell_id != 0 ? shell_id
                                                         : effect->uniqueID);
  out->is_shell_child = shell_id != 0;

  effect->dispatcher(effect, effClose, 0, 0, nullptr, 0.f);
  return true;
}

void append_vst2_entry(std::string& json, bool& first,
                       const std::filesystem::path& plugin_path,
                       const Vst2ScanEntry& entry) {
  if (!first) {
    json += ",";
  }
  first = false;
  const auto path_string = plugin_path.string();
  json += "{\"name\":\"" + escape_json(entry.name) + "\",";
  json += "\"vendor\":\"" + escape_json(entry.vendor) + "\",";
  json += "\"category\":\"" + escape_json(entry.category) + "\",";
  json += "\"subCategories\":\"" + escape_json(entry.sub_categories) + "\",";
  json += "\"format\":\"VST2\",";
  json += "\"path\":\"" + escape_json(path_string) + "\",";
  json += "\"modulePath\":\"" + escape_json(path_string) + "\",";
  json += "\"classId\":\"" + escape_json(entry.class_id) + "\",";
  json += "\"version\":\"" + escape_json(entry.version) + "\",";
  json += "\"sdkVersion\":\"VST 2.4\",";
  json += "\"isShellChild\":" +
          std::string(entry.is_shell_child ? "true" : "false") + ",";
  json += "\"sdkMetadataLoaded\":true}";
}

} // namespace

extern "C" bool sphere_vst3_scan_left_work_running() {
  return g_scan_left_work_running.load();
}

extern "C" SpherePluginHostString sphere_vst3_scan_path_json(const char* path) {
  if (!path) {
    return make_string("[]");
  }

  const bool debug = std::getenv("SPHERE_PLUGIN_HOST_DEBUG") != nullptr;

  // Phase 1 host scanner: load VST3 factory metadata, then instantiate each
  // audio-module component once to read its bus layout (`probe_vst3_buses`;
  // skipped for large shells and capped by `kBusProbeBudget`). No processing,
  // no editor, no controller, no realtime audio.
  std::filesystem::path root(path);
  if (!std::filesystem::exists(root)) {
    return make_string("[]");
  }

  std::string json = "[";
  bool first = true;

  const auto append = [&](const std::filesystem::path& plugin_path) {
    if (debug) {
      std::fprintf(stderr, "[SpherePluginHost] Scanning VST3: %s\n",
                   plugin_path.string().c_str());
    }

    const auto arch_reason = vst3_arch_mismatch_reason(plugin_path);
    if (!arch_reason.empty()) {
      if (debug) {
        std::fprintf(stderr, "[SpherePluginHost]   %s\n", arch_reason.c_str());
      }
      append_fallback_entry(json, first, plugin_path, "VST3", arch_reason);
      return;
    }

    std::string error;
    auto module = VST3::Hosting::Module::create(plugin_path.string(), error);
    if (!module) {
      if (debug) {
        std::fprintf(stderr, "[SpherePluginHost]   VST3 module load failed: %s\n",
                     error.c_str());
      }
      // Reported as a failure, not as a plug-in.
      append_fallback_entry(json, first, plugin_path, "VST3", error);
      return;
    }

    const auto factory = module->getFactory();
    // On the heap: a bus probe left running past its deadline still uses it
    // (and the plug-in may keep the pointer), so it is freed only when no
    // probe was abandoned. See `probe_vst3_buses_bounded`.
    auto* host_context = new Steinberg::Vst::HostApplication();
    factory.setHostContext(host_context);
    const auto factory_info = factory.info();
    const auto& raw_factory = factory.get();
    const auto raw_count = raw_factory ? raw_factory->countClasses() : 0;
    const auto class_count =
        raw_count > 0 ? static_cast<Steinberg::int32>(raw_count) : 0;

    if (debug) {
      std::fprintf(stderr, "[SpherePluginHost]   VST3 factory class count: %d\n",
                   class_count);
    }

    Steinberg::FUnknownPtr<Steinberg::IPluginFactory3> f3(raw_factory);
    Steinberg::FUnknownPtr<Steinberg::IPluginFactory2> f2(raw_factory);

    // Collect audio/plugin classes first so we can compute isShellChild.
    std::vector<ClassEntry> audio_classes;
    // Names of the ARA main-factory classes this module registers. ARA pairs a
    // main factory with its audio module by exact class name, so this is the
    // key the second pass below matches on.
    std::vector<std::string> ara_factory_names;
    int skipped = 0;

    for (Steinberg::int32 i = 0; i < class_count; ++i) {
      std::string name, vendor, category, sub_categories, class_id, version,
          sdk_version;
      bool ok = false;

      Steinberg::PClassInfoW ci3{};
      Steinberg::PClassInfo2 ci2{};
      Steinberg::PClassInfo ci{};

      if (f3 && f3->getClassInfoUnicode(i, &ci3) == Steinberg::kResultTrue) {
        name = char_array_to_string(ci3.name, Steinberg::PClassInfo::kNameSize);
        vendor =
            char_array_to_string(ci3.vendor, Steinberg::PClassInfo2::kVendorSize);
        category = char_array_to_string(ci3.category,
                                        Steinberg::PClassInfo::kCategorySize);
        sub_categories = char_array_to_string(
            ci3.subCategories, Steinberg::PClassInfo2::kSubCategoriesSize);
        version = char_array_to_string(ci3.version,
                                       Steinberg::PClassInfo2::kVersionSize);
        sdk_version = char_array_to_string(ci3.sdkVersion,
                                           Steinberg::PClassInfo2::kVersionSize);
        class_id = uid_to_string(ci3.cid);
        ok = true;
      } else if (f2 &&
                 f2->getClassInfo2(i, &ci2) == Steinberg::kResultTrue) {
        name = char_array_to_string(ci2.name, Steinberg::PClassInfo::kNameSize);
        vendor =
            char_array_to_string(ci2.vendor, Steinberg::PClassInfo2::kVendorSize);
        category = char_array_to_string(ci2.category,
                                        Steinberg::PClassInfo::kCategorySize);
        sub_categories = char_array_to_string(
            ci2.subCategories, Steinberg::PClassInfo2::kSubCategoriesSize);
        version = char_array_to_string(ci2.version,
                                       Steinberg::PClassInfo2::kVersionSize);
        class_id = uid_to_string(ci2.cid);
        ok = true;
      } else if (raw_factory->getClassInfo(i, &ci) == Steinberg::kResultTrue) {
        name = char_array_to_string(ci.name, Steinberg::PClassInfo::kNameSize);
        category = char_array_to_string(ci.category,
                                        Steinberg::PClassInfo::kCategorySize);
        class_id = uid_to_string(ci.cid);
        ok = true;
      }

      if (debug) {
        std::fprintf(stderr,
                     "[SpherePluginHost]   class[%d]: name=%s category=%s\n",
                     i, name.c_str(), category.c_str());
      }

      if (!ok) {
        ++skipped;
        continue;
      }

      // An ARA main-factory class is not a plug-in of its own — it is the ARA
      // entry point belonging to the audio-module class of the same name. Note
      // it and keep going; it must not become a catalog row.
      if (category == kARAMainFactoryClass) {
        if (debug) {
          std::fprintf(stderr,
                       "[SpherePluginHost]     -> ARA main factory for \"%s\"\n",
                       name.c_str());
        }
        ara_factory_names.push_back(name);
        continue;
      }

      // Only VST3 audio module classes are user-visible plug-ins. Some vendors
      // also expose Plugin Compatibility or Controller classes from the same
      // module; listing those creates duplicate rows for one plug-in.
      if (category != kAudioModuleClass) {
        if (debug) {
          std::fprintf(stderr,
                       "[SpherePluginHost]     -> skipped (non-audio module class)\n");
        }
        ++skipped;
        continue;
      }

      if (vendor.empty()) {
        vendor = factory_info.vendor();
      }

      audio_classes.push_back(
          {name, vendor, category, sub_categories, class_id, version, sdk_version});
    }

    // `ARAVST3.h` (IMainFactory docs): pairing an ARA main factory with its
    // audio module "is usually trivial because there typically is only one such
    // class in the binary". Equal class names are required only for shell
    // binaries hosting several plug-ins, where the host would otherwise have to
    // instantiate every processor to tell them apart.
    //
    // So the unambiguous single-plug-in case pairs regardless of naming —
    // vendors do rename it (Synchro Arts ships "RePitch VST" beside
    // "RePitch VSTAra") — and only a shell falls back to the name rule.
    if (audio_classes.size() == 1 && ara_factory_names.size() == 1) {
      audio_classes[0].is_ara = true;
    } else {
      for (auto& entry : audio_classes) {
        for (const auto& ara_name : ara_factory_names) {
          if (ara_name == entry.name) {
            entry.is_ara = true;
            break;
          }
        }
      }
    }

    if (debug) {
      std::fprintf(stderr,
                   "[SpherePluginHost]   Accepted: %zu plugin classes, "
                   "skipped: %d, ARA factories: %zu\n",
                   audio_classes.size(), skipped, ara_factory_names.size());
    }

    // isShellChild: this module exposes more than one audio plugin class.
    const bool is_shell = (audio_classes.size() > 1);
    const std::string module_path = plugin_path.string();

    // The bus probe instantiates a component per class, which costs orders of
    // magnitude more than reading class info: a 718-class WaveShell reads in
    // ~1 s but did not finish probing in 240 s, far past the 30 s per-bundle
    // timeout, which would have dropped every row of the bundle. Large shells
    // are therefore not probed, and probing stops once the budget is spent;
    // classes left unprobed report `midiOutput:false` exactly as before.
    const bool probe_buses = audio_classes.size() <= kMaxBusProbeClasses;
    if (debug && !probe_buses) {
      std::fprintf(stderr,
                   "[SpherePluginHost]   Bus probe skipped: %zu classes > %zu\n",
                   audio_classes.size(), kMaxBusProbeClasses);
    }
    const auto probe_deadline = std::chrono::steady_clock::now() + kBusProbeBudget;
    bool probe_abandoned = false;

    for (auto& entry : audio_classes) {
      if (probe_buses && !probe_abandoned) {
        if (std::chrono::steady_clock::now() < probe_deadline) {
          if (debug) {
            std::fprintf(stderr, "[SpherePluginHost]   Probing buses: %s\n",
                         entry.name.c_str());
          }
          probe_abandoned = !probe_vst3_buses_bounded(
              module, factory, host_context, entry, debug, probe_deadline);
        } else if (debug) {
          std::fprintf(stderr,
                       "[SpherePluginHost]   Bus probe budget spent, not "
                       "probing: %s\n",
                       entry.name.c_str());
        }
      }

      if (!first) {
        json += ",";
      }
      first = false;
      json += "{\"name\":\"" + escape_json(entry.name) + "\",";
      json += "\"vendor\":\"" + escape_json(entry.vendor) + "\",";
      json += "\"category\":\"" + escape_json(entry.category) + "\",";
      json += "\"subCategories\":\"" + escape_json(entry.sub_categories) + "\",";
      json += "\"format\":\"VST3\",";
      json += "\"path\":\"" + escape_json(module_path) + "\",";
      json += "\"modulePath\":\"" + escape_json(module_path) + "\",";
      json += "\"classId\":\"" + escape_json(entry.class_id) + "\",";
      json += "\"version\":\"" + escape_json(entry.version) + "\",";
      json += "\"sdkVersion\":\"" + escape_json(entry.sdk_version) + "\",";
      json += "\"isShellChild\":" +
              std::string(is_shell ? "true" : "false") + ",";
      json += "\"isAra\":" + std::string(entry.is_ara ? "true" : "false") + ",";
      json += "\"midiInput\":" +
              std::string(entry.midi_input ? "true" : "false") + ",";
      json += "\"midiOutput\":" +
              std::string(entry.midi_output ? "true" : "false") + ",";
      json += "\"audioOutputs\":" + std::to_string(entry.audio_outputs) + ",";
      json += "\"sdkMetadataLoaded\":true}";
    }
    if (!probe_abandoned) {
      delete host_context;
    }
  };

  if (is_vst3_bundle(root)) {
    append(root);
  } else if (std::filesystem::is_directory(root)) {
    // A `.vst3` bundle is a leaf. `X.vst3/Contents/x86_64-win/X.vst3` matches
    // the same extension as the bundle directory itself, so a plain recursive
    // walk reported every bundle-format plug-in twice — under two different
    // paths, which produced two different stable ids that dedup could not
    // collapse. `disable_recursion_pending` prunes the subtree at the bundle.
    std::error_code ec;
    auto it = std::filesystem::recursive_directory_iterator(
        root, std::filesystem::directory_options::skip_permission_denied, ec);
    const std::filesystem::recursive_directory_iterator end;
    for (; !ec && it != end; it.increment(ec)) {
      if (is_vst3_bundle(it->path())) {
        append(it->path());
        it.disable_recursion_pending();
      }
    }
  }
  json += "]";
  return make_string(json);
}

extern "C" SpherePluginHostString sphere_clap_scan_path_json(const char* path) {
  if (!path) {
    return make_string("[]");
  }

  const bool debug = std::getenv("SPHERE_PLUGIN_HOST_DEBUG") != nullptr;
  std::filesystem::path root(path);
  if (!std::filesystem::exists(root)) {
    return make_string("[]");
  }

  std::string json = "[";
  bool first = true;

  const auto append = [&](const std::filesystem::path& plugin_path) {
    if (debug) {
      std::fprintf(stderr, "[SpherePluginHost] Scanning CLAP: %s\n",
                   plugin_path.string().c_str());
    }

    const auto executable_path = clap_executable_path(plugin_path);
    SharedLibrary library(executable_path);
    if (!library.valid()) {
      if (debug) {
        std::fprintf(stderr, "[SpherePluginHost]   CLAP module load failed\n");
      }
      append_fallback_entry(json, first, plugin_path, "CLAP",
                            "CLAP module could not be loaded (wrong "
                            "architecture or missing dependency)");
      return;
    }

    auto* entry = reinterpret_cast<const clap_plugin_entry_t*>(library.symbol("clap_entry"));
    if (!entry || !entry->init || !entry->deinit || !entry->get_factory) {
      if (debug) {
        std::fprintf(stderr, "[SpherePluginHost]   CLAP entry symbol invalid\n");
      }
      append_fallback_entry(json, first, plugin_path, "CLAP",
                            "CLAP entry point is missing or incomplete");
      return;
    }

    const auto path_string = plugin_path.string();
    if (!entry->init(path_string.c_str())) {
      if (debug) {
        std::fprintf(stderr, "[SpherePluginHost]   CLAP init failed\n");
      }
      append_fallback_entry(json, first, plugin_path, "CLAP",
                            "CLAP entry init() returned false");
      return;
    }

    const auto* factory = reinterpret_cast<const clap_plugin_factory_t*>(
        entry->get_factory(CLAP_PLUGIN_FACTORY_ID));
    if (!factory || !factory->get_plugin_count || !factory->get_plugin_descriptor) {
      entry->deinit();
      append_fallback_entry(json, first, plugin_path, "CLAP",
                            "CLAP plug-in factory is unavailable");
      return;
    }

    const uint32_t plugin_count = factory->get_plugin_count(factory);
    const bool is_shell = plugin_count > 1;
    for (uint32_t i = 0; i < plugin_count; ++i) {
      const auto* descriptor = factory->get_plugin_descriptor(factory, i);
      if (!descriptor) {
        continue;
      }

      const std::string name = descriptor->name ? descriptor->name : plugin_path.stem().string();
      const std::string vendor = descriptor->vendor ? descriptor->vendor : "Unknown Vendor";
      const std::string version = descriptor->version ? descriptor->version : "";
      const std::string class_id = descriptor->id ? descriptor->id : "";
      const std::string category = clap_category(descriptor->features);
      const std::string features = clap_feature_string(descriptor->features);

      if (!first) {
        json += ",";
      }
      first = false;
      json += "{\"name\":\"" + escape_json(name) + "\",";
      json += "\"vendor\":\"" + escape_json(vendor) + "\",";
      json += "\"category\":\"" + escape_json(category) + "\",";
      json += "\"subCategories\":\"" + escape_json(features) + "\",";
      json += "\"format\":\"CLAP\",";
      json += "\"path\":\"" + escape_json(path_string) + "\",";
      json += "\"modulePath\":\"" + escape_json(path_string) + "\",";
      if (class_id.empty()) {
        json += "\"classId\":null,";
      } else {
        json += "\"classId\":\"" + escape_json(class_id) + "\",";
      }
      json += "\"version\":\"" + escape_json(version) + "\",";
      json += "\"sdkVersion\":\"CLAP " +
              std::to_string(entry->clap_version.major) + "." +
              std::to_string(entry->clap_version.minor) + "." +
              std::to_string(entry->clap_version.revision) + "\",";
      json += "\"isShellChild\":" +
              std::string(is_shell ? "true" : "false") + ",";
      json += "\"sdkMetadataLoaded\":true}";
    }

    entry->deinit();
  };

  if (is_clap_plugin(root)) {
    append(root);
  } else if (std::filesystem::is_directory(root)) {
    // Same leaf rule as VST3: a macOS `.clap` bundle contains an executable of
    // the same name, so recursing into a matched bundle double-reports it.
    std::error_code ec;
    auto it = std::filesystem::recursive_directory_iterator(
        root, std::filesystem::directory_options::skip_permission_denied, ec);
    const std::filesystem::recursive_directory_iterator end;
    for (; !ec && it != end; it.increment(ec)) {
      if (is_clap_plugin(it->path())) {
        append(it->path());
        it.disable_recursion_pending();
      }
    }
  }

  json += "]";
  return make_string(json);
}

extern "C" SpherePluginHostString sphere_vst2_scan_path_json(const char* path) {
  if (!path) {
    return make_string("[]");
  }

  const bool debug = std::getenv("SPHERE_PLUGIN_HOST_DEBUG") != nullptr;
  std::filesystem::path root(path);
  if (!std::filesystem::exists(root)) {
    return make_string("[]");
  }

  std::string json = "[";
  bool first = true;

  const auto append = [&](const std::filesystem::path& plugin_path) {
    if (debug) {
      std::fprintf(stderr, "[SpherePluginHost] Scanning VST2: %s\n",
                   plugin_path.string().c_str());
    }

    const auto executable_path = vst2_executable_path(plugin_path);
    const auto arch_reason = vst2_arch_mismatch_reason(executable_path);
    if (!arch_reason.empty()) {
      append_fallback_entry(json, first, plugin_path, "VST2", arch_reason);
      return;
    }

    SharedLibrary library(executable_path);
    if (!library.valid()) {
      // On Windows every `.dll` in a VST2 folder reaches here, including
      // support libraries the plug-ins ship beside themselves. A load failure
      // is only worth reporting once we know it is a VST2 module, which we
      // cannot know without loading it — so stay silent rather than fill the
      // browser with failures for files that were never plug-ins.
      if (debug) {
        std::fprintf(stderr, "[SpherePluginHost]   VST2 module load failed\n");
      }
      return;
    }

    auto entry = vst2_entry_point(library);
    if (!entry) {
      // Not a VST2 plug-in — just another DLL sitting in the folder. Skipping
      // silently is the whole point of probing the export.
      if (debug) {
        std::fprintf(stderr,
                     "[SpherePluginHost]   no VST2 entry point (not a plug-in)\n");
      }
      return;
    }

    std::vector<int32_t> shell_children;
    Vst2ScanEntry scanned;
    if (!vst2_read_entry(entry, 0, plugin_path, &scanned, &shell_children)) {
      append_fallback_entry(json, first, plugin_path, "VST2",
                            "VST2 entry point returned no valid AEffect");
      return;
    }

    if (shell_children.empty()) {
      append_vst2_entry(json, first, plugin_path, scanned);
      return;
    }

    // A shell module is a container, not a plug-in: emit one row per
    // sub-plug-in, each re-instantiated so its own name and category are real
    // rather than the shell's.
    for (const int32_t child_id : shell_children) {
      Vst2ScanEntry child;
      if (vst2_read_entry(entry, child_id, plugin_path, &child, nullptr)) {
        append_vst2_entry(json, first, plugin_path, child);
      }
    }
  };

  if (std::filesystem::is_regular_file(root) || is_vst2_module(root)) {
    append(root);
  } else if (std::filesystem::is_directory(root)) {
    std::error_code ec;
    auto it = std::filesystem::recursive_directory_iterator(
        root, std::filesystem::directory_options::skip_permission_denied, ec);
    const std::filesystem::recursive_directory_iterator end;
    for (; !ec && it != end; it.increment(ec)) {
      if (is_vst2_module(it->path())) {
        append(it->path());
        // A macOS `.vst` bundle holds its own executable; recursing into a
        // matched bundle would report it twice.
        it.disable_recursion_pending();
      }
    }
  }

  json += "]";
  return make_string(json);
}

extern "C" void sphere_plugin_host_free_string(SpherePluginHostString value) {
  delete[] value.data;
}
