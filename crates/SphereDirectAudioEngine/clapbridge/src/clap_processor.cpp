// Cross-platform CLAP runtime core for the DAUx bridge.
//
// Owns module load, plug-in lifecycle, processing, parameters, state, and
// transport. Windowing lives in the platform TUs (clap_editor_windows.cpp,
// clap_editor_mac.mm), exactly as the VST3 and VST2 bridges split it.
//
// Realtime contract: every buffer handed to the plug-in is allocated in
// `setup()`. The process functions do no allocation, take no lock except the
// small bounded parameter mutex, and never log unless a debug env var was set
// at startup.

#include "clap_processor_internal.hpp"

#include <cstdint>
#include <sstream>

#if defined(_WIN32)
#include <windows.h>
#else
// Every POSIX platform loads the module with `dlopen`, macOS included: a
// `.clap` there is a bundle, so CoreFoundation resolves the executable inside
// it (`mac_executable_path`) and `dlopen` loads that path. Selecting
// CoreFoundation *instead of* dlfcn.h on Apple left `dlopen`/`dlsym`/
// `dlclose`/`dlerror` undeclared and broke the macOS build.
#include <dlfcn.h>
#if defined(__APPLE__)
#include <CoreFoundation/CoreFoundation.h>
#endif
#endif

namespace {

std::atomic<unsigned long long> g_next_editor_handle{1};

SphereDauxClapProcessor *processor_for(const clap_host_t *host) {
  return host ? static_cast<SphereDauxClapProcessor *>(host->host_data)
              : nullptr;
}

double clamp01(double value) {
  return value < 0.0 ? 0.0 : (value > 1.0 ? 1.0 : value);
}

void append_json_escaped(std::string &out, const char *value, size_t max_len) {
  if (!value) {
    return;
  }
  for (size_t i = 0; i < max_len && value[i] != '\0'; ++i) {
    const char raw = value[i];
    const auto ch = static_cast<unsigned char>(raw);
    switch (raw) {
    case '"':
      out += "\\\"";
      break;
    case '\\':
      out += "\\\\";
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
        char buf[8];
        std::snprintf(buf, sizeof(buf), "\\u%04x", ch);
        out += buf;
      } else {
        out.push_back(raw);
      }
    }
  }
}

// ── Module loading ──────────────────────────────────────────────────────────

#if defined(_WIN32)

std::wstring widen_utf8(const char *value) {
  if (!value || !*value)
    return {};
  const int needed = MultiByteToWideChar(CP_UTF8, 0, value, -1, nullptr, 0);
  if (needed <= 0)
    return {};
  std::wstring out(static_cast<size_t>(needed - 1), L'\0');
  MultiByteToWideChar(CP_UTF8, 0, value, -1, out.data(), needed);
  return out;
}

#elif defined(__APPLE__)

/// A macOS `.clap` is a bundle; its executable lives in `Contents/MacOS/<name>`.
std::string mac_executable_path(const std::string &bundle_path) {
  CFStringRef path_str = CFStringCreateWithCString(
      kCFAllocatorDefault, bundle_path.c_str(), kCFStringEncodingUTF8);
  if (!path_str) {
    return bundle_path;
  }
  CFURLRef url = CFURLCreateWithFileSystemPath(kCFAllocatorDefault, path_str,
                                               kCFURLPOSIXPathStyle, true);
  CFRelease(path_str);
  if (!url) {
    return bundle_path;
  }
  CFBundleRef bundle = CFBundleCreate(kCFAllocatorDefault, url);
  CFRelease(url);
  if (!bundle) {
    return bundle_path;
  }
  CFURLRef exe = CFBundleCopyExecutableURL(bundle);
  std::string result = bundle_path;
  if (exe) {
    char buffer[2048] = {};
    if (CFURLGetFileSystemRepresentation(
            exe, true, reinterpret_cast<UInt8 *>(buffer), sizeof(buffer))) {
      result = buffer;
    }
    CFRelease(exe);
  }
  CFRelease(bundle);
  return result;
}

#endif

// ── clap_host callbacks ─────────────────────────────────────────────────────

const void *CLAP_ABI host_get_extension(const clap_host_t *host,
                                        const char *extension_id);

void CLAP_ABI host_request_restart(const clap_host_t *host) {
  if (auto *p = processor_for(host)) {
    p->restart_requested.store(true, std::memory_order_release);
  }
}

void CLAP_ABI host_request_process(const clap_host_t *host) {
  if (auto *p = processor_for(host)) {
    p->process_requested.store(true, std::memory_order_release);
  }
}

void CLAP_ABI host_request_callback(const clap_host_t *host) {
  if (auto *p = processor_for(host)) {
    p->callback_requested.store(true, std::memory_order_release);
  }
}

// clap.log
void CLAP_ABI host_log(const clap_host_t *, clap_log_severity severity,
                       const char *msg) {
  // Warnings and worse always surface; anything chattier is gated so a noisy
  // plug-in cannot flood the log in a normal session.
  if (severity >= CLAP_LOG_WARNING || daux_clap_debug()) {
    std::fprintf(stderr, "[clap-plugin] severity=%d %s\n",
                 static_cast<int>(severity), msg ? msg : "");
  }
}

// clap.thread-check
//
// Answered from real thread identity rather than a constant: plug-ins use this
// to assert their own threading rules, and a host that always says "yes, main
// thread" turns a genuine violation into silent corruption.
bool CLAP_ABI host_is_main_thread(const clap_host_t *host) {
  auto *p = processor_for(host);
  return p && std::this_thread::get_id() == p->main_thread_id;
}

bool CLAP_ABI host_is_audio_thread(const clap_host_t *host) {
  auto *p = processor_for(host);
  if (!p) {
    return false;
  }
  // The audio thread is whichever thread last entered process(); it is stable
  // for the life of a stream and is never the thread that created the
  // instance.
  return p->audio_thread_known.load(std::memory_order_acquire) &&
         std::this_thread::get_id() == p->audio_thread_id;
}

// clap.latency
void CLAP_ABI host_latency_changed(const clap_host_t *host) {
  if (auto *p = processor_for(host)) {
    if (p->ext_latency && p->plugin) {
      p->reported_latency.store(
          static_cast<int>(p->ext_latency->get(p->plugin)),
          std::memory_order_release);
    }
  }
}

// clap.params
void CLAP_ABI host_params_rescan(const clap_host_t *, clap_param_rescan_flags) {
}
void CLAP_ABI host_params_clear(const clap_host_t *, clap_id,
                                clap_param_clear_flags) {}
void CLAP_ABI host_params_request_flush(const clap_host_t *) {}

// clap.state
void CLAP_ABI host_state_mark_dirty(const clap_host_t *) {
  // Project dirtiness is tracked by the app from user gestures, not by the
  // plug-in; nothing to do, but the extension has to exist or plug-ins that
  // require it refuse to load.
}

// clap.gui
void CLAP_ABI host_gui_resize_hints_changed(const clap_host_t *) {}

bool CLAP_ABI host_gui_request_resize(const clap_host_t *host, uint32_t width,
                                      uint32_t height) {
  auto *p = processor_for(host);
  if (!p || width == 0 || height == 0) {
    return false;
  }
  // Publish for the shell to pick up on the UI thread — never resize a window
  // from whatever thread this arrives on.
  p->pending_main_shell_w.store(static_cast<int>(width),
                                std::memory_order_relaxed);
  p->pending_main_shell_h.store(static_cast<int>(height),
                                std::memory_order_relaxed);
  p->pending_main_shell_resize.store(true, std::memory_order_release);
  // Host-owned view path has its own consumer, so it gets its own slot —
  // whichever of the two is live drains only what belongs to it.
  p->view_host_resize_w.store(static_cast<int>(width),
                              std::memory_order_relaxed);
  p->view_host_resize_h.store(static_cast<int>(height),
                              std::memory_order_relaxed);
  p->view_host_resize_pending.store(true, std::memory_order_release);
  return true;
}

bool CLAP_ABI host_gui_request_show(const clap_host_t *) { return false; }
bool CLAP_ABI host_gui_request_hide(const clap_host_t *) { return false; }

void CLAP_ABI host_gui_closed(const clap_host_t *host, bool /*was_destroyed*/) {
  if (auto *p = processor_for(host)) {
    p->embed_user_closed.store(true, std::memory_order_release);
  }
}

const clap_host_log_t kHostLog{host_log};
const clap_host_thread_check_t kHostThreadCheck{host_is_main_thread,
                                                host_is_audio_thread};
const clap_host_latency_t kHostLatency{host_latency_changed};
const clap_host_params_t kHostParams{host_params_rescan, host_params_clear,
                                     host_params_request_flush};
const clap_host_state_t kHostState{host_state_mark_dirty};
const clap_host_gui_t kHostGui{host_gui_resize_hints_changed,
                               host_gui_request_resize, host_gui_request_show,
                               host_gui_request_hide, host_gui_closed};

#if defined(__linux__)
// clap.posix-fd-support / clap.timer-support: only meaningful where a plug-in
// GUI needs the host to drive its event loop (Linux — see
// clap_editor_linux.cpp). Windows/Cocoa editors have their own native message
// pump and never query these.
const clap_host_posix_fd_support_t kHostPosixFdSupport{
    clap_host_register_fd_linux, clap_host_modify_fd_linux,
    clap_host_unregister_fd_linux};
const clap_host_timer_support_t kHostTimerSupport{
    clap_host_register_timer_linux, clap_host_unregister_timer_linux};
#endif

const void *CLAP_ABI host_get_extension(const clap_host_t *,
                                        const char *extension_id) {
  if (!extension_id) {
    return nullptr;
  }
  if (std::strcmp(extension_id, CLAP_EXT_LOG) == 0)
    return &kHostLog;
  if (std::strcmp(extension_id, CLAP_EXT_THREAD_CHECK) == 0)
    return &kHostThreadCheck;
  if (std::strcmp(extension_id, CLAP_EXT_LATENCY) == 0)
    return &kHostLatency;
  if (std::strcmp(extension_id, CLAP_EXT_PARAMS) == 0)
    return &kHostParams;
  if (std::strcmp(extension_id, CLAP_EXT_STATE) == 0)
    return &kHostState;
  if (std::strcmp(extension_id, CLAP_EXT_GUI) == 0)
    return &kHostGui;
#if defined(__linux__)
  if (std::strcmp(extension_id, CLAP_EXT_POSIX_FD_SUPPORT) == 0)
    return &kHostPosixFdSupport;
  if (std::strcmp(extension_id, CLAP_EXT_TIMER_SUPPORT) == 0)
    return &kHostTimerSupport;
#endif
  // Everything else is genuinely unsupported. Returning null is the contract.
  return nullptr;
}

// ── Event list adapters ─────────────────────────────────────────────────────

uint32_t CLAP_ABI in_events_size(const clap_input_events_t *list) {
  auto *p = static_cast<SphereDauxClapProcessor *>(list->ctx);
  return p ? static_cast<uint32_t>(p->event_count) : 0;
}

const clap_event_header_t *CLAP_ABI in_events_get(
    const clap_input_events_t *list, uint32_t index) {
  auto *p = static_cast<SphereDauxClapProcessor *>(list->ctx);
  if (!p || index >= static_cast<uint32_t>(p->event_count)) {
    return nullptr;
  }
  return &p->event_storage[index].header;
}

bool CLAP_ABI out_events_try_push(const clap_output_events_t *,
                                  const clap_event_header_t *) {
  // Plug-in-produced events (parameter gestures, note output) are not routed
  // anywhere yet. Reporting failure is honest: accepting them would claim the
  // host delivered something it dropped.
  return false;
}

// ── State streams ───────────────────────────────────────────────────────────

struct ClapWriteStreamCtx {
  std::vector<unsigned char> *bytes;
};

int64_t CLAP_ABI stream_write(const clap_ostream_t *stream, const void *buffer,
                              uint64_t size) {
  auto *ctx = static_cast<ClapWriteStreamCtx *>(stream->ctx);
  if (!ctx || !ctx->bytes || !buffer) {
    return -1;
  }
  const auto *src = static_cast<const unsigned char *>(buffer);
  ctx->bytes->insert(ctx->bytes->end(), src, src + size);
  return static_cast<int64_t>(size);
}

struct ClapReadStreamCtx {
  const unsigned char *data;
  uint64_t size;
  uint64_t offset;
};

int64_t CLAP_ABI stream_read(const clap_istream_t *stream, void *buffer,
                             uint64_t size) {
  auto *ctx = static_cast<ClapReadStreamCtx *>(stream->ctx);
  if (!ctx || !buffer) {
    return -1;
  }
  const uint64_t remaining = ctx->size - ctx->offset;
  const uint64_t n = size < remaining ? size : remaining;
  if (n > 0) {
    std::memcpy(buffer, ctx->data + ctx->offset, static_cast<size_t>(n));
    ctx->offset += n;
  }
  return static_cast<int64_t>(n);
}

} // namespace

extern "C" unsigned long long clap_next_editor_handle(void) {
  return g_next_editor_handle.fetch_add(1, std::memory_order_relaxed);
}

// ── Setup ───────────────────────────────────────────────────────────────────

void SphereDauxClapProcessor::cache_port_topology() {
  audio_input_bus_count = 0;
  audio_output_bus_count = 0;
  main_audio_input_channel_count = 0;
  main_audio_output_channel_count = 0;
  bridge_audio_output_channel_count = 0;
  audio_output_bus_channel_counts.fill(0);
  audio_input_bus_channel_counts.fill(0);

  if (ext_audio_ports) {
    const int in_count =
        std::min<int>(static_cast<int>(ext_audio_ports->count(plugin, true)),
                      kMaxBridgeBuses);
    const int out_count =
        std::min<int>(static_cast<int>(ext_audio_ports->count(plugin, false)),
                      kMaxBridgeBuses);
    audio_input_bus_count = in_count;
    audio_output_bus_count = out_count;

    for (int i = 0; i < in_count; ++i) {
      clap_audio_port_info_t info{};
      if (ext_audio_ports->get(plugin, static_cast<uint32_t>(i), true, &info)) {
        const int channels = std::min<int>(static_cast<int>(info.channel_count),
                                           kMaxBridgeChannels);
        audio_input_bus_channel_counts[i] = channels;
        if (i == 0) {
          main_audio_input_channel_count = channels;
        }
      }
    }
    for (int i = 0; i < out_count; ++i) {
      clap_audio_port_info_t info{};
      if (ext_audio_ports->get(plugin, static_cast<uint32_t>(i), false, &info)) {
        const int channels = std::min<int>(static_cast<int>(info.channel_count),
                                           kMaxBridgeChannels);
        audio_output_bus_channel_counts[i] = channels;
        if (i == 0) {
          main_audio_output_channel_count = channels;
        }
        bridge_audio_output_channel_count = std::min(
            kMaxBridgeChannels, bridge_audio_output_channel_count + channels);
      }
    }
  }

  if (bridge_audio_output_channel_count <= 0) {
    bridge_audio_output_channel_count = main_audio_output_channel_count;
  }

  event_input_bus_count = 0;
  note_port_accepts_clap_dialect = false;
  note_port_accepts_midi = false;
  if (ext_note_ports) {
    const uint32_t note_in = ext_note_ports->count(plugin, true);
    if (note_in > 0) {
      event_input_bus_count = 1;
      clap_note_port_info_t info{};
      if (ext_note_ports->get(plugin, 0, true, &info)) {
        note_port_accepts_clap_dialect =
            (info.supported_dialects & CLAP_NOTE_DIALECT_CLAP) != 0;
        note_port_accepts_midi =
            (info.supported_dialects & CLAP_NOTE_DIALECT_MIDI) != 0;
      }
    }
  }
}

void SphereDauxClapProcessor::cache_param_ranges() {
  param_ranges.clear();
  if (!ext_params) {
    return;
  }
  const uint32_t count = ext_params->count(plugin);
  param_ranges.reserve(count);
  for (uint32_t i = 0; i < count; ++i) {
    clap_param_info_t info{};
    if (!ext_params->get_info(plugin, i, &info)) {
      continue;
    }
    param_ranges.push_back(
        ClapParamRange{info.id, info.min_value, info.max_value});
  }
}

void SphereDauxClapProcessor::allocate_audio_scratch(int frames) {
  const int f = std::max(1, std::min(frames, kMaxProcessFrames));

  int total_in_channels = 0;
  for (int i = 0; i < audio_input_bus_count; ++i) {
    total_in_channels += std::max(0, audio_input_bus_channel_counts[i]);
  }
  int total_out_channels = 0;
  for (int i = 0; i < audio_output_bus_count; ++i) {
    total_out_channels += std::max(0, audio_output_bus_channel_counts[i]);
  }
  total_in_channels = std::max(0, total_in_channels);
  total_out_channels = std::max(1, total_out_channels);

  input_storage.assign(static_cast<size_t>(total_in_channels) * f, 0.f);
  output_storage.assign(static_cast<size_t>(total_out_channels) * f, 0.f);
  input_channel_ptrs.assign(static_cast<size_t>(total_in_channels), nullptr);
  output_channel_ptrs.assign(static_cast<size_t>(total_out_channels), nullptr);
  for (int c = 0; c < total_in_channels; ++c) {
    input_channel_ptrs[c] = input_storage.data() + static_cast<size_t>(c) * f;
  }
  for (int c = 0; c < total_out_channels; ++c) {
    output_channel_ptrs[c] = output_storage.data() + static_cast<size_t>(c) * f;
  }

  input_buffers.assign(static_cast<size_t>(std::max(0, audio_input_bus_count)),
                       clap_audio_buffer_t{});
  output_buffers.assign(
      static_cast<size_t>(std::max(0, audio_output_bus_count)),
      clap_audio_buffer_t{});

  int cursor = 0;
  for (int i = 0; i < audio_input_bus_count; ++i) {
    const int channels = std::max(0, audio_input_bus_channel_counts[i]);
    input_buffers[i].data32 =
        channels > 0 ? input_channel_ptrs.data() + cursor : nullptr;
    input_buffers[i].data64 = nullptr;
    input_buffers[i].channel_count = static_cast<uint32_t>(channels);
    input_buffers[i].latency = 0;
    input_buffers[i].constant_mask = 0;
    cursor += channels;
  }
  cursor = 0;
  for (int i = 0; i < audio_output_bus_count; ++i) {
    const int channels = std::max(0, audio_output_bus_channel_counts[i]);
    output_buffers[i].data32 =
        channels > 0 ? output_channel_ptrs.data() + cursor : nullptr;
    output_buffers[i].data64 = nullptr;
    output_buffers[i].channel_count = static_cast<uint32_t>(channels);
    output_buffers[i].latency = 0;
    output_buffers[i].constant_mask = 0;
    cursor += channels;
  }

  allocated_frames = f;
}

bool SphereDauxClapProcessor::setup(double sr) {
  sample_rate = sr > 0.0 ? sr : 44100.0;

  if (!plugin->init(plugin)) {
    clap_set_last_error("clap_plugin->init() returned false");
    return false;
  }
  plugin_initialized = true;

  ext_audio_ports = static_cast<const clap_plugin_audio_ports_t *>(
      plugin->get_extension(plugin, CLAP_EXT_AUDIO_PORTS));
  ext_note_ports = static_cast<const clap_plugin_note_ports_t *>(
      plugin->get_extension(plugin, CLAP_EXT_NOTE_PORTS));
  ext_params = static_cast<const clap_plugin_params_t *>(
      plugin->get_extension(plugin, CLAP_EXT_PARAMS));
  ext_state = static_cast<const clap_plugin_state_t *>(
      plugin->get_extension(plugin, CLAP_EXT_STATE));
  ext_gui = static_cast<const clap_plugin_gui_t *>(
      plugin->get_extension(plugin, CLAP_EXT_GUI));
  ext_latency = static_cast<const clap_plugin_latency_t *>(
      plugin->get_extension(plugin, CLAP_EXT_LATENCY));

  cache_port_topology();
  cache_param_ranges();
  allocate_audio_scratch(kMaxProcessFrames);

  event_storage.assign(static_cast<size_t>(kMaxEvents), ClapEventCell{});
  event_count = 0;
  in_events.ctx = this;
  in_events.size = in_events_size;
  in_events.get = in_events_get;
  out_events.ctx = this;
  out_events.try_push = out_events_try_push;

  transport = {};
  transport.header.size = sizeof(clap_event_transport_t);
  transport.header.time = 0;
  transport.header.space_id = CLAP_CORE_EVENT_SPACE_ID;
  transport.header.type = CLAP_EVENT_TRANSPORT;
  transport.header.flags = 0;
  transport.tempo = 120.0;
  transport.tsig_num = 4;
  transport.tsig_denom = 4;
  transport.flags = CLAP_TRANSPORT_HAS_TEMPO |
                    CLAP_TRANSPORT_HAS_BEATS_TIMELINE |
                    CLAP_TRANSPORT_HAS_TIME_SIGNATURE;

  if (!plugin->activate(plugin, sample_rate, 1,
                        static_cast<uint32_t>(kMaxProcessFrames))) {
    clap_set_last_error("clap_plugin->activate() returned false");
    return false;
  }
  activated = true;

  if (!plugin->start_processing(plugin)) {
    clap_set_last_error("clap_plugin->start_processing() returned false");
    return false;
  }
  processing = true;

  if (ext_latency) {
    reported_latency.store(static_cast<int>(ext_latency->get(plugin)),
                           std::memory_order_release);
  }
  if (ext_gui) {
    editor_resizable = ext_gui->can_resize && ext_gui->can_resize(plugin);
  }

  std::fprintf(stderr,
               "[SphereCLAP] setup path=\"%s\" id=\"%s\" sr=%.0f inBuses=%d "
               "outBuses=%d mainIn=%d mainOut=%d params=%zu noteIn=%d "
               "clapDialect=%d gui=%d state=%d latency=%d\n",
               plugin_path.c_str(), plugin_id.c_str(), sample_rate,
               audio_input_bus_count, audio_output_bus_count,
               main_audio_input_channel_count, main_audio_output_channel_count,
               param_ranges.size(), event_input_bus_count,
               note_port_accepts_clap_dialect ? 1 : 0, ext_gui ? 1 : 0,
               ext_state ? 1 : 0, reported_latency.load());

  return true;
}

void SphereDauxClapProcessor::shutdown() {
  processor_valid.store(false, std::memory_order_release);

#if defined(_WIN32)
  close_editor_window();
#elif defined(__APPLE__)
  clap_close_editor_mac(this);
#endif

  if (plugin) {
    if (processing) {
      plugin->stop_processing(plugin);
      processing = false;
    }
    if (activated) {
      plugin->deactivate(plugin);
      activated = false;
    }
    plugin->destroy(plugin);
    plugin = nullptr;
    plugin_initialized = false;
  }

  if (entry && entry_initialized) {
    entry->deinit();
    entry_initialized = false;
  }
  entry = nullptr;

  if (module) {
#if defined(_WIN32)
    FreeLibrary(module);
#else
    dlclose(module);
#endif
    module = nullptr;
  }
}

// ── Parameters ──────────────────────────────────────────────────────────────

void SphereDauxClapProcessor::enqueue_param(clap_id id, double absolute_value) {
  std::lock_guard<std::mutex> lock(pending_mutex);
  for (int i = 0; i < pending_count; ++i) {
    if (pending_buf[i].id == id) {
      pending_buf[i].value = absolute_value;
      return;
    }
  }
  if (pending_count < kMaxPending) {
    pending_buf[pending_count++] = {id, absolute_value};
  }
}

// ── Events ──────────────────────────────────────────────────────────────────

void SphereDauxClapProcessor::build_events(
    const SphereDauxClapMidiEvent *events, int count) {
  event_count = 0;

  // Parameter changes first at time 0, then notes/MIDI at their own offsets.
  // CLAP requires the list to be sorted by `time`, and every parameter change
  // this block lands at 0.
  {
    std::lock_guard<std::mutex> lock(pending_mutex);
    for (int i = 0; i < pending_count && event_count < kMaxEvents; ++i) {
      auto &cell = event_storage[event_count++];
      cell.param = {};
      cell.param.header.size = sizeof(clap_event_param_value_t);
      cell.param.header.time = 0;
      cell.param.header.space_id = CLAP_CORE_EVENT_SPACE_ID;
      cell.param.header.type = CLAP_EVENT_PARAM_VALUE;
      cell.param.header.flags = 0;
      cell.param.param_id = pending_buf[i].id;
      cell.param.cookie = nullptr;
      cell.param.note_id = -1;
      cell.param.port_index = -1;
      cell.param.channel = -1;
      cell.param.key = -1;
      cell.param.value = pending_buf[i].value;
    }
    pending_count = 0;
  }

  if (!events || count <= 0 || event_input_bus_count <= 0) {
    return;
  }

  const int n = std::min(count, kMaxEvents - event_count);
  for (int i = 0; i < n; ++i) {
    const auto &src = events[i];
    const auto channel = static_cast<int16_t>(src.channel & 0x0F);
    const auto time = static_cast<uint32_t>(src.sample_offset);

    if (src.kind == 0 || src.kind == 1) {
      if (note_port_accepts_clap_dialect) {
        auto &cell = event_storage[event_count++];
        cell.note = {};
        cell.note.header.size = sizeof(clap_event_note_t);
        cell.note.header.time = time;
        cell.note.header.space_id = CLAP_CORE_EVENT_SPACE_ID;
        cell.note.header.type =
            src.kind == 1 ? CLAP_EVENT_NOTE_ON : CLAP_EVENT_NOTE_OFF;
        cell.note.header.flags = 0;
        cell.note.note_id = -1;
        cell.note.port_index = 0;
        cell.note.channel = channel;
        cell.note.key = static_cast<int16_t>(src.pitch & 0x7F);
        cell.note.velocity = clamp01(static_cast<double>(src.velocity));
        continue;
      }
      if (!note_port_accepts_midi) {
        continue;
      }
      auto &cell = event_storage[event_count++];
      cell.midi = {};
      cell.midi.header.size = sizeof(clap_event_midi_t);
      cell.midi.header.time = time;
      cell.midi.header.space_id = CLAP_CORE_EVENT_SPACE_ID;
      cell.midi.header.type = CLAP_EVENT_MIDI;
      cell.midi.header.flags = 0;
      cell.midi.port_index = 0;
      const int velocity = std::max(
          0, std::min(127, static_cast<int>(src.velocity * 127.f + 0.5f)));
      cell.midi.data[0] = static_cast<uint8_t>(
          (src.kind == 1 ? 0x90 : 0x80) | static_cast<uint8_t>(channel));
      cell.midi.data[1] = static_cast<uint8_t>(src.pitch & 0x7F);
      cell.midi.data[2] = static_cast<uint8_t>(velocity);
      continue;
    }

    if (src.kind != 2 || !note_port_accepts_midi) {
      // CLAP has no first-class controller event, so a plug-in whose note port
      // does not accept the MIDI dialect simply cannot receive CC.
      continue;
    }

    auto &cell = event_storage[event_count++];
    cell.midi = {};
    cell.midi.header.size = sizeof(clap_event_midi_t);
    cell.midi.header.time = time;
    cell.midi.header.space_id = CLAP_CORE_EVENT_SPACE_ID;
    cell.midi.header.type = CLAP_EVENT_MIDI;
    cell.midi.header.flags = 0;
    cell.midi.port_index = 0;
    const double normalized = clamp01(static_cast<double>(src.velocity));
    if (src.pitch == 128) {
      cell.midi.data[0] =
          static_cast<uint8_t>(0xD0 | static_cast<uint8_t>(channel));
      cell.midi.data[1] =
          static_cast<uint8_t>(std::min(127, static_cast<int>(normalized * 127.0 + 0.5)));
      cell.midi.data[2] = 0;
    } else if (src.pitch == 129) {
      const int bend = std::max(
          0, std::min(16383, static_cast<int>(normalized * 16383.0 + 0.5)));
      cell.midi.data[0] =
          static_cast<uint8_t>(0xE0 | static_cast<uint8_t>(channel));
      cell.midi.data[1] = static_cast<uint8_t>(bend & 0x7F);
      cell.midi.data[2] = static_cast<uint8_t>((bend >> 7) & 0x7F);
    } else {
      cell.midi.data[0] =
          static_cast<uint8_t>(0xB0 | static_cast<uint8_t>(channel));
      cell.midi.data[1] = static_cast<uint8_t>(src.pitch & 0x7F);
      cell.midi.data[2] =
          static_cast<uint8_t>(std::min(127, static_cast<int>(normalized * 127.0 + 0.5)));
    }
  }

  // CLAP requires ascending `time`. Parameter events are all at 0 and were
  // added first, so only the note/MIDI tail can be out of order.
  std::stable_sort(event_storage.begin(), event_storage.begin() + event_count,
                   [](const ClapEventCell &a, const ClapEventCell &b) {
                     return a.header.time < b.header.time;
                   });

  if (daux_clap_midi_debug()) {
    std::fprintf(stderr, "[clap-midi] events=%d noteIn=%d clapDialect=%d\n",
                 event_count, event_input_bus_count,
                 note_port_accepts_clap_dialect ? 1 : 0);
  }
}

// ── Processing ──────────────────────────────────────────────────────────────

bool SphereDauxClapProcessor::process_planar(
    const float *in_l, const float *in_r, int frames,
    const SphereDauxClapMidiEvent *events, int midi_event_count) {
  if (!plugin || !processing || frames <= 0 || frames > allocated_frames) {
    return false;
  }

  // Latch the audio thread on the first block so `clap.thread-check` can answer
  // from real identity. Relaxed store then release flag: any reader that sees
  // the flag also sees the id.
  if (!audio_thread_known.load(std::memory_order_acquire)) {
    audio_thread_id = std::this_thread::get_id();
    audio_thread_known.store(true, std::memory_order_release);
  }

  build_events(events, midi_event_count);

  // Fill the main input port from the host's stereo pair: channel 0 takes L,
  // channel 1 takes R, further channels and further ports stay silent (the
  // engine has no sidechain routing yet).
  double input_peak = 0.0;
  const int main_in_channels =
      audio_input_bus_count > 0 ? audio_input_bus_channel_counts[0] : 0;
  for (size_t c = 0; c < input_channel_ptrs.size(); ++c) {
    float *dst = input_channel_ptrs[c];
    if (!dst) {
      continue;
    }
    const float *src = nullptr;
    if (static_cast<int>(c) < main_in_channels) {
      src = (c == 0) ? in_l : (c == 1 ? in_r : nullptr);
    }
    if (src) {
      for (int i = 0; i < frames; ++i) {
        const float v = src[i];
        dst[i] = v;
        const double a = std::abs(static_cast<double>(v));
        if (a > input_peak) {
          input_peak = a;
        }
      }
    } else {
      std::memset(dst, 0, sizeof(float) * static_cast<size_t>(frames));
    }
  }

  // A plug-in that leaves an output channel untouched would otherwise expose
  // the previous block, so clear before process.
  for (float *dst : output_channel_ptrs) {
    if (dst) {
      std::memset(dst, 0, sizeof(float) * static_cast<size_t>(frames));
    }
  }

  clap_process_t process{};
  process.steady_time = static_cast<int64_t>(steady_time);
  process.frames_count = static_cast<uint32_t>(frames);
  process.transport = &transport;
  process.audio_inputs = input_buffers.empty() ? nullptr : input_buffers.data();
  process.audio_inputs_count = static_cast<uint32_t>(input_buffers.size());
  process.audio_outputs =
      output_buffers.empty() ? nullptr : output_buffers.data();
  process.audio_outputs_count = static_cast<uint32_t>(output_buffers.size());
  process.in_events = &in_events;
  process.out_events = &out_events;

  const clap_process_status status = plugin->process(plugin, &process);
  if (status == CLAP_PROCESS_ERROR) {
    return false;
  }

  steady_time += static_cast<uint64_t>(frames);
  ++process_count;
  last_input_peak = input_peak;
  return true;
}

bool SphereDauxClapProcessor::preferred_gui_size(int *width, int *height) {
  if (!ext_gui || !ext_gui->get_size || !gui_created) {
    return false;
  }
  uint32_t w = 0;
  uint32_t h = 0;
  if (!ext_gui->get_size(plugin, &w, &h) || w == 0 || h == 0) {
    return false;
  }
  if (width) {
    *width = static_cast<int>(w);
  }
  if (height) {
    *height = static_cast<int>(h);
  }
  return true;
}

// ── C API ───────────────────────────────────────────────────────────────────

extern "C" {

int sphere_daux_clap_bridge_probe(void) { return 0x434C4150; } // 'CLAP'

const char *sphere_daux_clap_last_error(void) {
  return g_clap_last_error.c_str();
}

SphereDauxClapProcessor *sphere_daux_clap_create(const char *plugin_path,
                                                 const char *class_id,
                                                 double sample_rate) {
  clap_set_last_error({});
  if (!plugin_path || !*plugin_path) {
    clap_set_last_error("CLAP create: empty module path");
    return nullptr;
  }

  auto *p = new SphereDauxClapProcessor();
  // Whatever thread creates the instance is this plug-in's CLAP main thread.
  p->main_thread_id = std::this_thread::get_id();
  p->plugin_path = plugin_path;
  p->plugin_id = class_id ? class_id : "";
  p->sample_rate = sample_rate > 0.0 ? sample_rate : 44100.0;

#if defined(_WIN32)
  const std::wstring wide_path = widen_utf8(plugin_path);
  p->module = LoadLibraryExW(wide_path.c_str(), nullptr,
                             LOAD_WITH_ALTERED_SEARCH_PATH);
  if (!p->module) {
    std::ostringstream err;
    err << "LoadLibraryEx failed (error " << GetLastError() << ") for "
        << plugin_path;
    clap_set_last_error(err.str());
    delete p;
    return nullptr;
  }
  p->entry = reinterpret_cast<const clap_plugin_entry_t *>(
      GetProcAddress(p->module, "clap_entry"));
#else
#if defined(__APPLE__)
  const std::string executable = mac_executable_path(plugin_path);
#else
  const std::string executable = plugin_path;
#endif
  p->module = dlopen(executable.c_str(), RTLD_NOW | RTLD_LOCAL);
  if (!p->module) {
    std::ostringstream err;
    err << "dlopen failed for " << executable << ": "
        << (dlerror() ? dlerror() : "unknown error");
    clap_set_last_error(err.str());
    delete p;
    return nullptr;
  }
  p->entry = reinterpret_cast<const clap_plugin_entry_t *>(
      dlsym(p->module, "clap_entry"));
#endif

  if (!p->entry || !p->entry->init || !p->entry->deinit ||
      !p->entry->get_factory) {
    clap_set_last_error("Module exports no usable `clap_entry`");
    p->shutdown();
    delete p;
    return nullptr;
  }
  if (!clap_version_is_compatible(p->entry->clap_version)) {
    std::ostringstream err;
    err << "CLAP version " << p->entry->clap_version.major << "."
        << p->entry->clap_version.minor << "." << p->entry->clap_version.revision
        << " is not compatible with this host";
    clap_set_last_error(err.str());
    p->shutdown();
    delete p;
    return nullptr;
  }
  if (!p->entry->init(plugin_path)) {
    clap_set_last_error("clap_entry->init() returned false");
    p->shutdown();
    delete p;
    return nullptr;
  }
  p->entry_initialized = true;

  const auto *factory = static_cast<const clap_plugin_factory_t *>(
      p->entry->get_factory(CLAP_PLUGIN_FACTORY_ID));
  if (!factory || !factory->get_plugin_count || !factory->get_plugin_descriptor ||
      !factory->create_plugin) {
    clap_set_last_error("CLAP plug-in factory is unavailable");
    p->shutdown();
    delete p;
    return nullptr;
  }

  // Resolve the requested plug-in id; an empty id selects the first entry, which
  // is what a single-plug-in module reports anyway.
  std::string resolved_id = p->plugin_id;
  if (resolved_id.empty()) {
    const uint32_t count = factory->get_plugin_count(factory);
    if (count == 0) {
      clap_set_last_error("CLAP module declares no plug-ins");
      p->shutdown();
      delete p;
      return nullptr;
    }
    const auto *descriptor = factory->get_plugin_descriptor(factory, 0);
    if (!descriptor || !descriptor->id) {
      clap_set_last_error("CLAP plug-in descriptor has no id");
      p->shutdown();
      delete p;
      return nullptr;
    }
    resolved_id = descriptor->id;
    p->plugin_id = resolved_id;
  }

  p->host = {};
  p->host.clap_version = CLAP_VERSION;
  p->host.host_data = p;
  p->host.name = "Futureboard Studio";
  p->host.vendor = "Futureboard";
  p->host.url = "";
  p->host.version = "1.0";
  p->host.get_extension = host_get_extension;
  p->host.request_restart = host_request_restart;
  p->host.request_process = host_request_process;
  p->host.request_callback = host_request_callback;

  p->plugin = factory->create_plugin(factory, &p->host, resolved_id.c_str());
  if (!p->plugin) {
    std::ostringstream err;
    err << "CLAP factory could not create plug-in id \"" << resolved_id << "\"";
    clap_set_last_error(err.str());
    p->shutdown();
    delete p;
    return nullptr;
  }

  if (!p->setup(p->sample_rate)) {
    p->shutdown();
    delete p;
    return nullptr;
  }
  return p;
}

void sphere_daux_clap_destroy(SphereDauxClapProcessor *processor) {
  if (!processor) {
    return;
  }
  processor->shutdown();
  delete processor;
}

int sphere_daux_clap_is_valid(SphereDauxClapProcessor *p) {
  return (p && p->processor_valid.load(std::memory_order_acquire)) ? 1 : 0;
}

int sphere_daux_clap_process_stereo_block_with_midi(
    SphereDauxClapProcessor *p, const float *in_l, const float *in_r,
    float *out_l, float *out_r, int frames,
    const SphereDauxClapMidiEvent *events, int event_count) {
  if (!p || !out_l || !out_r || frames <= 0) {
    return 0;
  }
  if (!p->processor_valid.load(std::memory_order_acquire)) {
    return 0;
  }
  if (!p->process_planar(in_l, in_r, frames, events, event_count)) {
    return 0;
  }

  const int available = static_cast<int>(p->output_channel_ptrs.size());
  const float *left = available > 0 ? p->output_channel_ptrs[0] : nullptr;
  const float *right = available > 1 ? p->output_channel_ptrs[1] : left;
  if (!left) {
    return 0;
  }

  double output_peak = 0.0;
  double difference_peak = 0.0;
  for (int i = 0; i < frames; ++i) {
    const float l = left[i];
    const float r = right ? right[i] : l;
    out_l[i] = l;
    out_r[i] = r;
    output_peak = std::max(output_peak,
                           std::max(std::abs(static_cast<double>(l)),
                                    std::abs(static_cast<double>(r))));
    if (in_l && in_r) {
      difference_peak =
          std::max(difference_peak,
                   std::max(std::abs(static_cast<double>(l - in_l[i])),
                            std::abs(static_cast<double>(r - in_r[i]))));
    }
  }
  p->last_output_peak = output_peak;
  p->last_difference_peak = difference_peak;
  return 1;
}

int sphere_daux_clap_process_stereo_block(SphereDauxClapProcessor *p,
                                          const float *in_l, const float *in_r,
                                          float *out_l, float *out_r,
                                          int frames) {
  return sphere_daux_clap_process_stereo_block_with_midi(
      p, in_l, in_r, out_l, out_r, frames, nullptr, 0);
}

int sphere_daux_clap_process_stereo_sample(SphereDauxClapProcessor *p,
                                           float in_l, float in_r, float *out_l,
                                           float *out_r) {
  if (!out_l || !out_r) {
    return 0;
  }
  float l_in = in_l;
  float r_in = in_r;
  float l_out = 0.f;
  float r_out = 0.f;
  const int ok = sphere_daux_clap_process_stereo_block_with_midi(
      p, &l_in, &r_in, &l_out, &r_out, 1, nullptr, 0);
  *out_l = ok ? l_out : in_l;
  *out_r = ok ? r_out : in_r;
  return ok;
}

int sphere_daux_clap_process_main_output_block_with_midi(
    SphereDauxClapProcessor *p, const float *in_l, const float *in_r,
    float *out_interleaved, int frames, int output_channels,
    const SphereDauxClapMidiEvent *events, int event_count) {
  if (!p || !out_interleaved || frames <= 0 || output_channels <= 0) {
    return 0;
  }
  if (!p->processor_valid.load(std::memory_order_acquire)) {
    return 0;
  }
  if (!p->process_planar(in_l, in_r, frames, events, event_count)) {
    return 0;
  }

  const int available = static_cast<int>(p->output_channel_ptrs.size());
  double output_peak = 0.0;
  for (int i = 0; i < frames; ++i) {
    for (int c = 0; c < output_channels; ++c) {
      float v = 0.f;
      if (c < available && p->output_channel_ptrs[c]) {
        v = p->output_channel_ptrs[c][i];
      }
      out_interleaved[static_cast<size_t>(i) * output_channels + c] = v;
      const double a = std::abs(static_cast<double>(v));
      if (a > output_peak) {
        output_peak = a;
      }
    }
  }
  p->last_output_peak = output_peak;
  return 1;
}

int sphere_daux_clap_event_input_bus_count(SphereDauxClapProcessor *p) {
  return p ? p->event_input_bus_count : 0;
}

int sphere_daux_clap_audio_input_bus_count(SphereDauxClapProcessor *p) {
  return p ? p->audio_input_bus_count : 0;
}

int sphere_daux_clap_audio_output_bus_count(SphereDauxClapProcessor *p) {
  return p ? p->audio_output_bus_count : 0;
}

int sphere_daux_clap_main_audio_input_channel_count(
    SphereDauxClapProcessor *p) {
  return p ? p->main_audio_input_channel_count : 0;
}

int sphere_daux_clap_main_audio_output_channel_count(
    SphereDauxClapProcessor *p) {
  return p ? p->main_audio_output_channel_count : 0;
}

int sphere_daux_clap_bridge_audio_output_channel_count(
    SphereDauxClapProcessor *p) {
  return p ? p->bridge_audio_output_channel_count : 0;
}

int sphere_daux_clap_output_bus_channel_counts(SphereDauxClapProcessor *p,
                                               int *out_counts, int max_count) {
  if (!p || !out_counts || max_count <= 0) {
    return 0;
  }
  const int n = std::min(p->audio_output_bus_count, max_count);
  for (int i = 0; i < n; ++i) {
    out_counts[i] = p->audio_output_bus_channel_counts[i];
  }
  return n;
}

unsigned long long sphere_daux_clap_process_count(SphereDauxClapProcessor *p) {
  return p ? p->process_count : 0;
}

double sphere_daux_clap_last_input_peak(SphereDauxClapProcessor *p) {
  return p ? p->last_input_peak : 0.0;
}

double sphere_daux_clap_last_output_peak(SphereDauxClapProcessor *p) {
  return p ? p->last_output_peak : 0.0;
}

double sphere_daux_clap_last_difference_peak(SphereDauxClapProcessor *p) {
  return p ? p->last_difference_peak : 0.0;
}

void sphere_daux_clap_set_param(SphereDauxClapProcessor *p,
                                unsigned int param_id, double value) {
  if (!p) {
    return;
  }
  const auto id = static_cast<clap_id>(param_id);
  const ClapParamRange *range = p->range_for(id);
  if (!range) {
    // An id the plug-in never declared: dropping it is correct, and enqueuing
    // it would push a value the plug-in cannot interpret.
    return;
  }
  // The bridge API is normalized; CLAP parameters are absolute. Converting on
  // the caller's thread keeps the audio path free of range lookups.
  p->enqueue_param(id, range->denormalize(value));
}

int sphere_daux_clap_get_latency_samples(SphereDauxClapProcessor *p) {
  if (!p) {
    return 0;
  }
  return std::max(0, p->reported_latency.load(std::memory_order_acquire));
}

void sphere_daux_clap_set_process_context(SphereDauxClapProcessor *p,
                                          double tempo, int time_sig_num,
                                          int time_sig_den,
                                          long long project_time_samples,
                                          double ppq, double bar_ppq,
                                          int playing, int recording) {
  if (!p) {
    return;
  }
  auto &t = p->transport;
  t.tempo = tempo > 0.0 ? tempo : 120.0;
  t.tempo_inc = 0.0;
  t.tsig_num = static_cast<uint16_t>(time_sig_num > 0 ? time_sig_num : 4);
  t.tsig_denom = static_cast<uint16_t>(time_sig_den > 0 ? time_sig_den : 4);
  // CLAP beat/second positions are fixed-point (`clap_beattime` /
  // `clap_sectime`), so the quarter-note positions convert rather than assign.
  t.song_pos_beats =
      static_cast<clap_beattime>(std::llround(ppq * CLAP_BEATTIME_FACTOR));
  t.bar_start =
      static_cast<clap_beattime>(std::llround(bar_ppq * CLAP_BEATTIME_FACTOR));
  t.bar_number = 0;
  const double seconds = p->sample_rate > 0.0
                             ? static_cast<double>(project_time_samples) /
                                   p->sample_rate
                             : 0.0;
  t.song_pos_seconds =
      static_cast<clap_sectime>(std::llround(seconds * CLAP_SECTIME_FACTOR));
  t.loop_start_beats = 0;
  t.loop_end_beats = 0;
  t.loop_start_seconds = 0;
  t.loop_end_seconds = 0;

  uint32_t flags = CLAP_TRANSPORT_HAS_TEMPO | CLAP_TRANSPORT_HAS_BEATS_TIMELINE |
                   CLAP_TRANSPORT_HAS_SECONDS_TIMELINE |
                   CLAP_TRANSPORT_HAS_TIME_SIGNATURE;
  if (playing) {
    flags |= CLAP_TRANSPORT_IS_PLAYING;
  }
  if (recording) {
    flags |= CLAP_TRANSPORT_IS_RECORDING;
  }
  t.flags = flags;
}

// ── State ───────────────────────────────────────────────────────────────────

int sphere_daux_clap_get_state(SphereDauxClapProcessor *p,
                               unsigned char **out_component,
                               int *out_component_len,
                               unsigned char **out_controller,
                               int *out_controller_len) {
  if (!out_component || !out_component_len || !out_controller ||
      !out_controller_len) {
    return 0;
  }
  *out_component = nullptr;
  *out_component_len = 0;
  // CLAP has a single state stream; the caller's packing tolerates an empty
  // second blob.
  *out_controller = nullptr;
  *out_controller_len = 0;
  if (!p || !p->plugin || !p->ext_state || !p->ext_state->save) {
    return 0;
  }

  std::vector<unsigned char> bytes;
  ClapWriteStreamCtx ctx{&bytes};
  clap_ostream_t stream{};
  stream.ctx = &ctx;
  stream.write = stream_write;
  if (!p->ext_state->save(p->plugin, &stream)) {
    return 0;
  }

  // A zero-length state is valid (the plug-in is at defaults), so allocate at
  // least one byte rather than returning a null pointer the caller would read
  // as failure.
  auto *buffer = static_cast<unsigned char *>(std::malloc(bytes.size() + 1));
  if (!buffer) {
    return 0;
  }
  if (!bytes.empty()) {
    std::memcpy(buffer, bytes.data(), bytes.size());
  }
  buffer[bytes.size()] = 0;
  *out_component = buffer;
  *out_component_len = static_cast<int>(bytes.size());
  return 1;
}

int sphere_daux_clap_set_state(SphereDauxClapProcessor *p,
                               const unsigned char *component_data,
                               int component_len,
                               const unsigned char *controller_data,
                               int controller_len) {
  (void)controller_data;
  (void)controller_len;
  if (!p || !p->plugin || !p->ext_state || !p->ext_state->load ||
      !component_data || component_len < 0) {
    return 0;
  }
  ClapReadStreamCtx ctx{component_data, static_cast<uint64_t>(component_len), 0};
  clap_istream_t stream{};
  stream.ctx = &ctx;
  stream.read = stream_read;
  return p->ext_state->load(p->plugin, &stream) ? 1 : 0;
}

void sphere_daux_clap_state_free(unsigned char *data) { std::free(data); }

// ── Parameter enumeration ───────────────────────────────────────────────────

char *sphere_daux_clap_list_parameters_json(SphereDauxClapProcessor *p) {
  if (!p || !p->plugin) {
    return nullptr;
  }

  std::string json = "[";
  if (p->ext_params) {
    const uint32_t count = p->ext_params->count(p->plugin);
    bool first = true;
    for (uint32_t i = 0; i < count; ++i) {
      clap_param_info_t info{};
      if (!p->ext_params->get_info(p->plugin, i, &info)) {
        continue;
      }
      if (!first) {
        json += ",";
      }
      first = false;

      double absolute = info.default_value;
      if (p->ext_params->get_value) {
        p->ext_params->get_value(p->plugin, info.id, &absolute);
      }
      const ClapParamRange range{info.id, info.min_value, info.max_value};

      json += "{\"id\":";
      json += std::to_string(static_cast<unsigned int>(info.id));
      json += ",\"title\":\"";
      append_json_escaped(json, info.name, sizeof(info.name));
      json += "\",\"short_title\":\"\",\"unit\":\"";
      // CLAP has no unit string; `module` is the closest structural label and
      // is what a generic view can usefully group by.
      append_json_escaped(json, info.module, sizeof(info.module));
      json += "\",\"automatable\":";
      json += (info.flags & CLAP_PARAM_IS_AUTOMATABLE) ? "true" : "false";
      json += ",\"hidden\":";
      json += (info.flags & CLAP_PARAM_IS_HIDDEN) ? "true" : "false";
      json += ",\"read_only\":";
      json += (info.flags & CLAP_PARAM_IS_READONLY) ? "true" : "false";
      json += ",\"value_normalized\":";
      json += std::to_string(range.normalize(absolute));
      json += "}";
    }
  }
  json += "]";

  auto *buffer = static_cast<char *>(std::malloc(json.size() + 1));
  if (!buffer) {
    return nullptr;
  }
  std::memcpy(buffer, json.c_str(), json.size() + 1);
  return buffer;
}

void sphere_daux_clap_parameters_json_free(char *data) { std::free(data); }

// ── Editor metadata (platform-independent parts) ────────────────────────────

void sphere_daux_clap_set_editor_title(SphereDauxClapProcessor *p,
                                       const char *title) {
  if (!p) {
    return;
  }
  p->editor_title = title ? title : "";
}

void sphere_daux_clap_embed_set_instance_label(SphereDauxClapProcessor *p,
                                               const char *instance_id) {
  if (!p) {
    return;
  }
  p->embed_instance_label = instance_id ? instance_id : "";
}

int sphere_daux_clap_editor_resizable(SphereDauxClapProcessor *p) {
  return (p && p->editor_resizable) ? 1 : 0;
}

int sphere_daux_clap_embed_host_kind(SphereDauxClapProcessor *p) {
  if (!p || !p->embed_mode) {
    return -1;
  }
  return p->embed_host_kind;
}

int sphere_daux_clap_embed_take_user_close(SphereDauxClapProcessor *p) {
  if (!p) {
    return 0;
  }
  return p->embed_user_closed.exchange(false, std::memory_order_acq_rel) ? 1 : 0;
}

int sphere_daux_clap_take_pending_shell_resize(SphereDauxClapProcessor *p,
                                               int *out_width,
                                               int *out_height) {
  if (!p) {
    return 0;
  }
  if (!p->pending_main_shell_resize.exchange(false,
                                             std::memory_order_acq_rel)) {
    return 0;
  }
  if (out_width) {
    *out_width = p->pending_main_shell_w.load(std::memory_order_relaxed);
  }
  if (out_height) {
    *out_height = p->pending_main_shell_h.load(std::memory_order_relaxed);
  }
  return 1;
}

int sphere_daux_clap_embed_content_size(SphereDauxClapProcessor *p,
                                        int *out_width, int *out_height) {
  if (!p || p->embed_content_w <= 0 || p->embed_content_h <= 0) {
    return 0;
  }
  if (out_width) {
    *out_width = p->embed_content_w;
  }
  if (out_height) {
    *out_height = p->embed_content_h;
  }
  return 1;
}

int sphere_daux_clap_prepare_editor_view(SphereDauxClapProcessor *p,
                                         int *out_width, int *out_height) {
  if (!p) {
    return 0;
  }
  return p->preferred_gui_size(out_width, out_height) ? 1 : 0;
}

void sphere_daux_clap_embed_set_waiting_stage(SphereDauxClapProcessor *p,
                                              const char *stage) {
  (void)p;
  (void)stage;
  // A CLAP GUI is created and parented synchronously, so there is no
  // multi-stage async bring-up to report. Kept so the Rust surface is identical
  // across backends.
}

} // extern "C"
