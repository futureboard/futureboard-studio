// Cross-platform VST2 runtime core for the DAUx bridge.
//
// Owns module load, AEffect lifecycle, processing, parameters, state, and
// transport. Windowing lives in the platform TUs (vst2_editor_windows.cpp,
// vst2_editor_mac.mm) exactly as the VST3 bridge splits it.
//
// Realtime contract: every buffer this file hands to the plug-in is allocated
// in `create()` / `setup()`. The process functions do no allocation, take no
// lock except the small bounded parameter mutex (uncontended in steady state),
// and never log unless a debug env var was set at startup.

#include "vst2_processor_internal.hpp"

#include <cerrno>
#include <cstdint>
#include <sstream>

#if defined(_WIN32)
#include <windows.h>
#elif defined(__APPLE__)
#include <CoreFoundation/CoreFoundation.h>
#else
#include <dlfcn.h>
#endif

namespace {

/// Set while a module's entry point runs so `audioMasterCurrentId` can answer
/// with the shell sub-plug-in the host asked for, before any AEffect exists.
thread_local SphereDauxVst2Processor *g_instantiating = nullptr;

std::atomic<unsigned long long> g_next_editor_handle{1};

/// Recover the owning processor from an AEffect. `resvd1` is reserved for the
/// host by the VST2 ABI, so it is the canonical back-pointer slot.
SphereDauxVst2Processor *processor_for(AEffect *effect) {
  if (!effect)
    return g_instantiating;
  auto *owner =
      reinterpret_cast<SphereDauxVst2Processor *>(effect->resvd1);
  return owner ? owner : g_instantiating;
}

/// Parse the `class_id` shell selector. Accepts `"vst2:<decimal>"`, a bare
/// decimal, or a 4-character FourCC. Returns 0 when empty/unparseable, which
/// means "the module's default plug-in".
int32_t parse_shell_unique_id(const char *class_id) {
  if (!class_id)
    return 0;
  std::string value(class_id);
  // Trim surrounding whitespace.
  const auto begin = value.find_first_not_of(" \t\r\n");
  if (begin == std::string::npos)
    return 0;
  const auto end = value.find_last_not_of(" \t\r\n");
  value = value.substr(begin, end - begin + 1);
  if (value.rfind("vst2:", 0) == 0)
    value = value.substr(5);
  if (value.empty())
    return 0;

  const bool all_digits =
      value.find_first_not_of("0123456789-") == std::string::npos;
  if (all_digits) {
    errno = 0;
    const long long parsed = std::strtoll(value.c_str(), nullptr, 10);
    if (errno == 0)
      return static_cast<int32_t>(parsed);
    return 0;
  }
  if (value.size() == 4) {
    return (static_cast<int32_t>(static_cast<unsigned char>(value[0])) << 24) |
           (static_cast<int32_t>(static_cast<unsigned char>(value[1])) << 16) |
           (static_cast<int32_t>(static_cast<unsigned char>(value[2])) << 8) |
           static_cast<int32_t>(static_cast<unsigned char>(value[3]));
  }
  return 0;
}

// ── Module loading ──────────────────────────────────────────────────────────

#if defined(_WIN32)

std::wstring widen_utf8(const char *value) {
  if (!value || !*value)
    return {};
  const int needed =
      MultiByteToWideChar(CP_UTF8, 0, value, -1, nullptr, 0);
  if (needed <= 0)
    return {};
  std::wstring out(static_cast<size_t>(needed - 1), L'\0');
  MultiByteToWideChar(CP_UTF8, 0, value, -1, out.data(), needed);
  return out;
}

/// Read the PE machine field without executing the image, so a 32-bit plug-in
/// produces a precise diagnostic instead of a generic LoadLibrary failure.
/// Returns false when the file could not be inspected at all.
bool read_pe_machine(const std::wstring &path, unsigned short *out_machine) {
  HANDLE file = CreateFileW(path.c_str(), GENERIC_READ, FILE_SHARE_READ,
                            nullptr, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL,
                            nullptr);
  if (file == INVALID_HANDLE_VALUE)
    return false;
  bool ok = false;
  IMAGE_DOS_HEADER dos{};
  DWORD read = 0;
  if (ReadFile(file, &dos, sizeof(dos), &read, nullptr) &&
      read == sizeof(dos) && dos.e_magic == IMAGE_DOS_SIGNATURE) {
    if (SetFilePointer(file, dos.e_lfanew, nullptr, FILE_BEGIN) !=
        INVALID_SET_FILE_POINTER) {
      DWORD signature = 0;
      IMAGE_FILE_HEADER coff{};
      if (ReadFile(file, &signature, sizeof(signature), &read, nullptr) &&
          read == sizeof(signature) && signature == IMAGE_NT_SIGNATURE &&
          ReadFile(file, &coff, sizeof(coff), &read, nullptr) &&
          read == sizeof(coff)) {
        *out_machine = coff.Machine;
        ok = true;
      }
    }
  }
  CloseHandle(file);
  return ok;
}

#elif defined(__APPLE__)

/// macOS VST2 plug-ins are `.vst` bundles. Returns a retained CFBundleRef.
void *load_mac_bundle(const char *path) {
  CFStringRef path_str = CFStringCreateWithCString(kCFAllocatorDefault, path,
                                                   kCFStringEncodingUTF8);
  if (!path_str)
    return nullptr;
  CFURLRef url = CFURLCreateWithFileSystemPath(
      kCFAllocatorDefault, path_str, kCFURLPOSIXPathStyle, true);
  CFRelease(path_str);
  if (!url)
    return nullptr;
  CFBundleRef bundle = CFBundleCreate(kCFAllocatorDefault, url);
  CFRelease(url);
  if (!bundle)
    return nullptr;
  if (!CFBundleLoadExecutable(bundle)) {
    CFRelease(bundle);
    return nullptr;
  }
  return bundle;
}

void *mac_bundle_symbol(void *handle, const char *name) {
  auto bundle = static_cast<CFBundleRef>(handle);
  CFStringRef symbol = CFStringCreateWithCString(kCFAllocatorDefault, name,
                                                 kCFStringEncodingUTF8);
  if (!symbol)
    return nullptr;
  void *fn = CFBundleGetFunctionPointerForName(bundle, symbol);
  CFRelease(symbol);
  return fn;
}

#endif

#if defined(_WIN32) || defined(__APPLE__)
/// Resolve the VST2 entry point. Modern modules export `VSTPluginMain`;
/// pre-2.4 modules export `main` (or `main_macho` on old macOS bundles).
VstPluginMainProc resolve_entry_point(SphereDauxVst2Processor *p) {
  static const char *kNames[] = {"VSTPluginMain", "main", "main_macho",
                                 "main_plugin"};
  for (const char *name : kNames) {
#if defined(_WIN32)
    auto fn =
        reinterpret_cast<VstPluginMainProc>(GetProcAddress(p->module, name));
#else
    auto fn =
        reinterpret_cast<VstPluginMainProc>(mac_bundle_symbol(p->module, name));
#endif
    if (fn)
      return fn;
  }
  return nullptr;
}
#endif

double clamp01(double value) {
  if (value < 0.0)
    return 0.0;
  if (value > 1.0)
    return 1.0;
  return value;
}

void append_json_escaped(std::string &out, const std::string &value) {
  for (const char raw : value) {
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

} // namespace

// ── audioMaster ─────────────────────────────────────────────────────────────

intptr_t vst2_audio_master(AEffect *effect, int32_t opcode, int32_t index,
                           intptr_t value, void *ptr, float opt) {
  (void)index;
  (void)value;
  (void)opt;
  auto *p = processor_for(effect);

  switch (opcode) {
  case audioMasterVersion:
    return 2400;

  case audioMasterCurrentId:
    // Shell selector: answered from the "currently instantiating" processor,
    // because the plug-in asks for it from inside its entry point.
    return p ? p->shell_unique_id : 0;

  case audioMasterGetTime:
    // Realtime: returns the transport snapshot written by
    // sphere_daux_vst2_set_process_context before this block. No allocation,
    // no lock.
    return p ? reinterpret_cast<intptr_t>(&p->time_info) : 0;

  case audioMasterGetSampleRate:
    return p ? static_cast<intptr_t>(p->sample_rate) : 44100;

  case audioMasterGetBlockSize:
    return SphereDauxVst2Processor::kMaxProcessFrames;

  case audioMasterGetInputLatency:
  case audioMasterGetOutputLatency:
    return 0;

  case audioMasterGetCurrentProcessLevel:
    return kVstProcessLevelRealtime;

  case audioMasterGetAutomationState:
    return 0; // unsupported / off

  case audioMasterAutomate:
    // The plug-in's own GUI moved a parameter. VST2 applies the value to the
    // instance itself, so there is nothing to forward to a separate processor
    // (unlike VST3's split component/controller). Only noted, so the host can
    // tell the studio the saved state may be stale: one atomic store,
    // realtime-safe.
    if (p)
      p->state_touched.store(true, std::memory_order_release);
    return 0;

  case audioMasterEndEdit:
    if (p)
      p->state_touched.store(true, std::memory_order_release);
    return 1;

  // `audioMasterUpdateDisplay` is deliberately not counted: plug-ins send it
  // for meters and labels as well, and it would leave the project permanently
  // unsaved.
  case audioMasterBeginEdit:
  case audioMasterUpdateDisplay:
  case audioMasterIdle:
    return 1;

  case audioMasterIOChanged:
    // Latency and/or IO count changed. The host re-reads initialDelay via
    // get_latency_samples on the control thread; nothing to do here.
    return 1;

  case audioMasterSizeWindow:
    // Plug-in-driven editor resize. Publish for the shell to pick up on the UI
    // thread — never resize a window from whatever thread this arrives on.
    if (p && index > 0 && value > 0) {
      p->pending_main_shell_w = static_cast<int>(index);
      p->pending_main_shell_h = static_cast<int>(value);
      p->pending_main_shell_resize.store(true, std::memory_order_release);
      // Host-owned view path has its own consumer, so it gets its own slot —
      // whichever of the two is live drains only what belongs to it.
      p->view_host_resize_w = static_cast<int>(index);
      p->view_host_resize_h = static_cast<int>(value);
      p->view_host_resize_pending.store(true, std::memory_order_release);
      return 1;
    }
    return 0;

  case audioMasterGetVendorString:
    if (ptr) {
      std::snprintf(static_cast<char *>(ptr), 64, "%s", "Futureboard");
      return 1;
    }
    return 0;

  case audioMasterGetProductString:
    if (ptr) {
      std::snprintf(static_cast<char *>(ptr), 64, "%s", "Futureboard Studio");
      return 1;
    }
    return 0;

  case audioMasterGetVendorVersion:
    return 1;

  case audioMasterGetLanguage:
    return 1; // kVstLangEnglish

  case audioMasterCanDo: {
    if (!ptr)
      return 0;
    const std::string feature(static_cast<const char *>(ptr));
    if (feature == "sendVstEvents" || feature == "sendVstMidiEvent" ||
        feature == "sendVstTimeInfo" || feature == "sizeWindow" ||
        feature == "startStopProcess" || feature == "supplyIdle" ||
        feature == "shellCategory") {
      return 1;
    }
    // Explicitly unsupported: offline processing, file selectors, MIDI output
    // routing. Answering -1 stops plug-ins from probing further.
    return -1;
  }

  case audioMasterProcessEvents:
    // Plug-in MIDI output. Not routed anywhere yet — accepting it silently
    // would claim a feature the engine does not implement, so report
    // unsupported.
    return 0;

  default:
    return 0;
  }
}

// ── Setup / teardown ────────────────────────────────────────────────────────

void SphereDauxVst2Processor::allocate_audio_scratch(int input_channels_needed,
                                                     int output_channels_needed,
                                                     int frames) {
  const int in_ch =
      std::max(1, std::min(input_channels_needed, kMaxBridgeChannels));
  const int out_ch =
      std::max(1, std::min(output_channels_needed, kMaxBridgeChannels));
  const int f = std::max(1, std::min(frames, kMaxProcessFrames));

  input_storage.assign(static_cast<size_t>(in_ch) * f, 0.f);
  output_storage.assign(static_cast<size_t>(out_ch) * f, 0.f);
  for (int c = 0; c < in_ch; ++c)
    input_channels[c] = input_storage.data() + static_cast<size_t>(c) * f;
  for (int c = in_ch; c < kMaxBridgeChannels; ++c)
    input_channels[c] = nullptr;
  for (int c = 0; c < out_ch; ++c)
    output_channels[c] = output_storage.data() + static_cast<size_t>(c) * f;
  for (int c = out_ch; c < kMaxBridgeChannels; ++c)
    output_channels[c] = nullptr;

  allocated_input_channels = in_ch;
  allocated_output_channels = out_ch;
  allocated_frames = f;
}

bool SphereDauxVst2Processor::setup(double sr) {
  sample_rate = sr > 0.0 ? sr : 44100.0;

  if (!effect || effect->magic != kEffectMagic) {
    vst2_set_last_error("VST2 entry point returned an invalid AEffect");
    return false;
  }
  if ((effect->flags & effFlagsCanReplacing) == 0 ||
      effect->processReplacing == nullptr) {
    vst2_set_last_error("VST2 plug-in does not implement processReplacing "
                        "(pre-2.4 accumulating process is not supported)");
    return false;
  }

  // Host back-pointer for audioMaster. Must be set before effOpen, because
  // plug-ins call back into the host from inside open().
  effect->resvd1 = reinterpret_cast<intptr_t>(this);

  num_inputs = std::max(0, effect->numInputs);
  num_outputs = std::max(0, effect->numOutputs);
  num_params = std::max(0, effect->numParams);
  is_synth = (effect->flags & effFlagsIsSynth) != 0;
  has_editor = (effect->flags & effFlagsHasEditor) != 0;
  uses_chunks = (effect->flags & effFlagsProgramChunks) != 0;

  dispatch(effOpen);
  opened = true;

  dispatch(effSetSampleRate, 0, 0, nullptr, static_cast<float>(sample_rate));
  dispatch(effSetBlockSize, 0, kMaxProcessFrames);
  // Explicitly select single precision — the engine graph is f32 throughout.
  dispatch(effSetProcessPrecision, 0, kVstProcessPrecision32);

  // VST2 exposes flat channel counts, not buses. Present bus 0 as the full
  // channel set so the multi-out mixer sees one bus per stereo pair the same
  // way it does for VST3.
  main_audio_input_channel_count = std::min(num_inputs, kMaxBridgeChannels);
  main_audio_output_channel_count = std::min(num_outputs, kMaxBridgeChannels);
  audio_input_bus_count = num_inputs > 0 ? 1 : 0;
  audio_output_bus_channel_counts.fill(0);
  bridge_audio_output_channel_count = main_audio_output_channel_count;

  if (main_audio_output_channel_count <= 0) {
    audio_output_bus_count = 0;
  } else if (main_audio_output_channel_count <= 2) {
    audio_output_bus_count = 1;
    audio_output_bus_channel_counts[0] = main_audio_output_channel_count;
  } else {
    // Multi-out instrument: split into stereo pairs, matching how VST3
    // multi-out instruments report one bus per mixer route. A trailing odd
    // channel becomes a mono bus.
    int remaining = main_audio_output_channel_count;
    int bus = 0;
    while (remaining > 0 && bus < kMaxBridgeBuses) {
      const int channels = remaining >= 2 ? 2 : 1;
      audio_output_bus_channel_counts[bus] = channels;
      remaining -= channels;
      ++bus;
    }
    audio_output_bus_count = bus;
  }

  event_input_bus_count =
      (is_synth || can_do("receiveVstMidiEvent") || can_do("receiveVstEvents"))
          ? 1
          : 0;

  editor_resizable = has_editor && can_do("sizeWindow");

  allocate_audio_scratch(main_audio_input_channel_count,
                         main_audio_output_channel_count, kMaxProcessFrames);

  // VstEvents header + one pointer per possible event, allocated once.
  events_block.assign(sizeof(VstEvents) +
                          sizeof(VstEvent *) * (kMaxMidiEvents + 1),
                      0);

  time_info = {};
  time_info.sampleRate = sample_rate;
  time_info.tempo = 120.0;
  time_info.timeSigNumerator = 4;
  time_info.timeSigDenominator = 4;
  time_info.flags = kVstTempoValid | kVstTimeSigValid | kVstPpqPosValid |
                    kVstBarsValid;

  dispatch(effMainsChanged, 0, 1);
  dispatch(effStartProcess);
  processing = true;

  std::fprintf(stderr,
               "[SphereVST2] setup path=\"%s\" sr=%.0f inputs=%d outputs=%d "
               "params=%d synth=%d editor=%d chunks=%d outputBuses=%d "
               "eventInputBuses=%d latency=%d\n",
               plugin_path.c_str(), sample_rate, num_inputs, num_outputs,
               num_params, is_synth ? 1 : 0, has_editor ? 1 : 0,
               uses_chunks ? 1 : 0, audio_output_bus_count,
               event_input_bus_count, effect->initialDelay);

  return true;
}

void SphereDauxVst2Processor::shutdown() {
  processor_valid.store(false, std::memory_order_release);

#if defined(_WIN32)
  close_editor_window();
#elif defined(__APPLE__)
  vst2_close_editor_mac(this);
#endif

  if (effect) {
    if (processing) {
      dispatch(effStopProcess);
      dispatch(effMainsChanged, 0, 0);
      processing = false;
    }
    if (opened) {
      dispatch(effClose);
      opened = false;
    }
    // effClose destroys the AEffect; the plug-in owns it, so it must not be
    // freed here.
    effect = nullptr;
  }

  if (module) {
#if defined(_WIN32)
    FreeLibrary(module);
#elif defined(__APPLE__)
    CFRelease(static_cast<CFBundleRef>(module));
#else
    dlclose(module);
#endif
    module = nullptr;
  }
}

// ── Parameters ──────────────────────────────────────────────────────────────

void SphereDauxVst2Processor::enqueue_param(unsigned int index, float value) {
  std::lock_guard<std::mutex> lock(pending_mutex);
  for (int i = 0; i < pending_count; ++i) {
    if (pending_buf[i].index == index) {
      pending_buf[i].value = value;
      return;
    }
  }
  if (pending_count < kMaxPending)
    pending_buf[pending_count++] = {index, value};
}

void SphereDauxVst2Processor::apply_pending_params() {
  // Copy out under the lock, then call the plug-in outside it: setParameter can
  // re-enter the host (audioMasterAutomate) and must never run while the
  // pending mutex is held.
  std::array<PendingParam, kMaxPending> local{};
  int count = 0;
  {
    std::lock_guard<std::mutex> lock(pending_mutex);
    count = pending_count;
    for (int i = 0; i < count; ++i)
      local[i] = pending_buf[i];
    pending_count = 0;
  }
  if (!effect || !effect->setParameter)
    return;
  for (int i = 0; i < count; ++i) {
    effect->setParameter(effect, static_cast<int32_t>(local[i].index),
                         local[i].value);
  }
}

// ── MIDI ────────────────────────────────────────────────────────────────────

void SphereDauxVst2Processor::prepare_midi_events(
    const SphereDauxVst2MidiEvent *events, int count) {
  midi_event_count = 0;
  if (!events || count <= 0 || event_input_bus_count <= 0)
    return;

  const int n = std::min(count, kMaxMidiEvents);
  for (int i = 0; i < n; ++i) {
    const auto &src = events[i];
    auto &dst = midi_events[midi_event_count];
    dst = {};
    dst.type = kVstMidiType;
    dst.byteSize = sizeof(VstMidiEvent);
    dst.deltaFrames = static_cast<int32_t>(src.sample_offset);
    dst.flags = kVstMidiEventIsRealtime;

    const auto channel = static_cast<unsigned char>(src.channel & 0x0F);
    switch (src.kind) {
    case 1: { // NoteOn
      const int velocity =
          std::max(0, std::min(127, static_cast<int>(src.velocity * 127.f +
                                                     0.5f)));
      dst.midiData[0] = static_cast<char>(0x90 | channel);
      dst.midiData[1] = static_cast<char>(src.pitch & 0x7F);
      dst.midiData[2] = static_cast<char>(velocity);
      break;
    }
    case 0: { // NoteOff
      const int velocity =
          std::max(0, std::min(127, static_cast<int>(src.velocity * 127.f +
                                                     0.5f)));
      dst.midiData[0] = static_cast<char>(0x80 | channel);
      dst.midiData[1] = static_cast<char>(src.pitch & 0x7F);
      dst.midiData[2] = static_cast<char>(velocity);
      break;
    }
    case 2: {
      // ControlChange. `pitch` carries the controller number in the VST3
      // encoding the engine already uses: 0..127 = CC, 128 = channel
      // aftertouch, 129 = pitch bend. VST2 takes raw MIDI, so each maps to its
      // own status byte rather than to a parameter.
      const float normalized = clamp01(static_cast<double>(src.velocity));
      if (src.pitch == 128) {
        dst.midiData[0] = static_cast<char>(0xD0 | channel);
        dst.midiData[1] = static_cast<char>(
            std::min(127, static_cast<int>(normalized * 127.f + 0.5f)));
        dst.midiData[2] = 0;
      } else if (src.pitch == 129) {
        const int bend = std::max(
            0, std::min(16383, static_cast<int>(normalized * 16383.f + 0.5f)));
        dst.midiData[0] = static_cast<char>(0xE0 | channel);
        dst.midiData[1] = static_cast<char>(bend & 0x7F);
        dst.midiData[2] = static_cast<char>((bend >> 7) & 0x7F);
      } else {
        dst.midiData[0] = static_cast<char>(0xB0 | channel);
        dst.midiData[1] = static_cast<char>(src.pitch & 0x7F);
        dst.midiData[2] = static_cast<char>(
            std::min(127, static_cast<int>(normalized * 127.f + 0.5f)));
      }
      break;
    }
    default:
      continue;
    }
    ++midi_event_count;
  }

  if (midi_event_count <= 0)
    return;

  // Sort by delta so the plug-in receives an ordered block.
  std::sort(midi_events.begin(), midi_events.begin() + midi_event_count,
            [](const VstMidiEvent &a, const VstMidiEvent &b) {
              return a.deltaFrames < b.deltaFrames;
            });

  auto *block = reinterpret_cast<VstEvents *>(events_block.data());
  block->numEvents = midi_event_count;
  block->reserved = 0;
  for (int i = 0; i < midi_event_count; ++i) {
    block->events[i] = reinterpret_cast<VstEvent *>(&midi_events[i]);
  }

  dispatch(effProcessEvents, 0, 0, block);

  if (daux_vst2_midi_debug()) {
    std::fprintf(stderr, "[vst2-midi] delivered events=%d eventBus=%d\n",
                 midi_event_count, event_input_bus_count);
  }
}

// ── Processing ──────────────────────────────────────────────────────────────

bool SphereDauxVst2Processor::process_planar(
    const float *in_l, const float *in_r, int frames,
    const SphereDauxVst2MidiEvent *events, int event_count) {
  if (!effect || !effect->processReplacing || !processing)
    return false;
  if (frames <= 0 || frames > allocated_frames)
    return false;

  apply_pending_params();
  prepare_midi_events(events, event_count);

  // Fill the plug-in's input channels from the host's stereo pair: channel 0
  // takes L, channel 1 takes R, any further inputs are silent (the engine has
  // no sidechain routing yet).
  double input_peak = 0.0;
  for (int c = 0; c < allocated_input_channels; ++c) {
    float *dst = input_channels[c];
    if (!dst)
      continue;
    const float *src = (c == 0) ? in_l : (c == 1 ? in_r : nullptr);
    if (src) {
      for (int i = 0; i < frames; ++i) {
        const float v = src[i];
        dst[i] = v;
        const double a = std::abs(static_cast<double>(v));
        if (a > input_peak)
          input_peak = a;
      }
    } else {
      std::memset(dst, 0, sizeof(float) * static_cast<size_t>(frames));
    }
  }

  // processReplacing overwrites its outputs, but a plug-in that leaves a
  // channel untouched would otherwise expose the previous block, so clear
  // first.
  for (int c = 0; c < allocated_output_channels; ++c) {
    if (output_channels[c])
      std::memset(output_channels[c], 0,
                  sizeof(float) * static_cast<size_t>(frames));
  }

  effect->processReplacing(effect, input_channels.data(),
                           output_channels.data(),
                           static_cast<int32_t>(frames));

  ++process_count;
  last_input_peak = input_peak;
  return true;
}

// ── C API ───────────────────────────────────────────────────────────────────

extern "C" {

int sphere_daux_vst2_bridge_probe(void) { return 0x56533200; }

unsigned long long vst2_next_editor_handle(void) {
  return g_next_editor_handle.fetch_add(1, std::memory_order_relaxed);
}

const char *sphere_daux_vst2_last_error(void) {
  return g_vst2_last_error.c_str();
}

SphereDauxVst2Processor *sphere_daux_vst2_create(const char *plugin_path,
                                                 const char *class_id,
                                                 double sample_rate) {
  vst2_set_last_error({});
  if (!plugin_path || !*plugin_path) {
    vst2_set_last_error("VST2 create: empty module path");
    return nullptr;
  }

  auto *p = new SphereDauxVst2Processor();
  p->plugin_path = plugin_path;
  p->shell_unique_id = parse_shell_unique_id(class_id);
  p->sample_rate = sample_rate > 0.0 ? sample_rate : 44100.0;

#if defined(_WIN32)
  const std::wstring wide_path = widen_utf8(plugin_path);
  unsigned short machine = 0;
  if (read_pe_machine(wide_path, &machine) &&
      machine != IMAGE_FILE_MACHINE_AMD64 &&
      machine != IMAGE_FILE_MACHINE_ARM64) {
    std::ostringstream err;
    err << "VST2 plug-in is not 64-bit (PE machine 0x" << std::hex << machine
        << "). Futureboard hosts 64-bit VST2 only.";
    vst2_set_last_error(err.str());
    delete p;
    return nullptr;
  }
  // LOAD_WITH_ALTERED_SEARCH_PATH lets the plug-in find sibling DLLs in its own
  // folder, which many VST2 bundles rely on.
  p->module = LoadLibraryExW(wide_path.c_str(), nullptr,
                             LOAD_WITH_ALTERED_SEARCH_PATH);
  if (!p->module) {
    std::ostringstream err;
    err << "LoadLibraryEx failed (error " << GetLastError() << ") for "
        << plugin_path;
    vst2_set_last_error(err.str());
    delete p;
    return nullptr;
  }
#elif defined(__APPLE__)
  p->module = load_mac_bundle(plugin_path);
  if (!p->module) {
    std::ostringstream err;
    err << "CFBundle load failed for " << plugin_path
        << " (missing executable, or not a 64-bit build)";
    vst2_set_last_error(err.str());
    delete p;
    return nullptr;
  }
#else
  // Linux VST2 is out of scope: no module loader, so fail with a precise
  // reason instead of pretending to try.
  vst2_set_last_error("VST2 hosting is not supported on this platform");
  delete p;
  return nullptr;
#endif

#if defined(_WIN32) || defined(__APPLE__)
  auto entry = resolve_entry_point(p);
  if (!entry) {
    vst2_set_last_error(
        "Module exports no VST2 entry point (VSTPluginMain / main)");
    p->shutdown();
    delete p;
    return nullptr;
  }

  // The plug-in calls audioMasterCurrentId from inside the entry point, before
  // any AEffect exists, so the shell selector is published thread-locally.
  g_instantiating = p;
  AEffect *effect = entry(&vst2_audio_master);
  g_instantiating = nullptr;

  if (!effect || effect->magic != kEffectMagic) {
    vst2_set_last_error("VST2 entry point returned no valid AEffect");
    p->shutdown();
    delete p;
    return nullptr;
  }
  p->effect = effect;

  if (!p->setup(p->sample_rate)) {
    p->shutdown();
    delete p;
    return nullptr;
  }
  return p;
#endif
}

void sphere_daux_vst2_destroy(SphereDauxVst2Processor *processor) {
  if (!processor)
    return;
  processor->shutdown();
  delete processor;
}

int sphere_daux_vst2_is_valid(SphereDauxVst2Processor *processor) {
  return (processor &&
          processor->processor_valid.load(std::memory_order_acquire))
             ? 1
             : 0;
}

int sphere_daux_vst2_process_stereo_block_with_midi(
    SphereDauxVst2Processor *p, const float *in_l, const float *in_r,
    float *out_l, float *out_r, int frames,
    const SphereDauxVst2MidiEvent *events, int event_count) {
  if (!p || !out_l || !out_r || frames <= 0)
    return 0;
  if (!p->processor_valid.load(std::memory_order_acquire))
    return 0;
  if (!p->process_planar(in_l, in_r, frames, events, event_count))
    return 0;

  const float *left = p->output_channels[0];
  const float *right =
      p->allocated_output_channels > 1 ? p->output_channels[1] : left;
  if (!left)
    return 0;

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

int sphere_daux_vst2_process_stereo_block(SphereDauxVst2Processor *p,
                                          const float *in_l, const float *in_r,
                                          float *out_l, float *out_r,
                                          int frames) {
  return sphere_daux_vst2_process_stereo_block_with_midi(
      p, in_l, in_r, out_l, out_r, frames, nullptr, 0);
}

int sphere_daux_vst2_process_stereo_sample(SphereDauxVst2Processor *p,
                                           float in_l, float in_r,
                                           float *out_l, float *out_r) {
  if (!out_l || !out_r)
    return 0;
  float l_in = in_l;
  float r_in = in_r;
  float l_out = 0.f;
  float r_out = 0.f;
  const int ok = sphere_daux_vst2_process_stereo_block_with_midi(
      p, &l_in, &r_in, &l_out, &r_out, 1, nullptr, 0);
  *out_l = ok ? l_out : in_l;
  *out_r = ok ? r_out : in_r;
  return ok;
}

int sphere_daux_vst2_process_main_output_block_with_midi(
    SphereDauxVst2Processor *p, const float *in_l, const float *in_r,
    float *out_interleaved, int frames, int output_channels,
    const SphereDauxVst2MidiEvent *events, int event_count) {
  if (!p || !out_interleaved || frames <= 0 || output_channels <= 0)
    return 0;
  if (!p->processor_valid.load(std::memory_order_acquire))
    return 0;
  if (!p->process_planar(in_l, in_r, frames, events, event_count))
    return 0;

  const int available = p->allocated_output_channels;
  double output_peak = 0.0;
  for (int i = 0; i < frames; ++i) {
    for (int c = 0; c < output_channels; ++c) {
      float v = 0.f;
      if (c < available && p->output_channels[c])
        v = p->output_channels[c][i];
      out_interleaved[static_cast<size_t>(i) * output_channels + c] = v;
      const double a = std::abs(static_cast<double>(v));
      if (a > output_peak)
        output_peak = a;
    }
  }
  p->last_output_peak = output_peak;
  return 1;
}

int sphere_daux_vst2_event_input_bus_count(SphereDauxVst2Processor *p) {
  return p ? p->event_input_bus_count : 0;
}

int sphere_daux_vst2_audio_input_bus_count(SphereDauxVst2Processor *p) {
  return p ? p->audio_input_bus_count : 0;
}

int sphere_daux_vst2_audio_output_bus_count(SphereDauxVst2Processor *p) {
  return p ? p->audio_output_bus_count : 0;
}

int sphere_daux_vst2_main_audio_input_channel_count(
    SphereDauxVst2Processor *p) {
  return p ? p->main_audio_input_channel_count : 0;
}

int sphere_daux_vst2_main_audio_output_channel_count(
    SphereDauxVst2Processor *p) {
  return p ? p->main_audio_output_channel_count : 0;
}

int sphere_daux_vst2_bridge_audio_output_channel_count(
    SphereDauxVst2Processor *p) {
  return p ? p->bridge_audio_output_channel_count : 0;
}

int sphere_daux_vst2_output_bus_channel_counts(SphereDauxVst2Processor *p,
                                               int *out_counts, int max_count) {
  if (!p || !out_counts || max_count <= 0)
    return 0;
  const int n = std::min(p->audio_output_bus_count, max_count);
  for (int i = 0; i < n; ++i)
    out_counts[i] = p->audio_output_bus_channel_counts[i];
  return n;
}

unsigned long long sphere_daux_vst2_process_count(SphereDauxVst2Processor *p) {
  return p ? p->process_count : 0;
}

double sphere_daux_vst2_last_input_peak(SphereDauxVst2Processor *p) {
  return p ? p->last_input_peak : 0.0;
}

double sphere_daux_vst2_last_output_peak(SphereDauxVst2Processor *p) {
  return p ? p->last_output_peak : 0.0;
}

double sphere_daux_vst2_last_difference_peak(SphereDauxVst2Processor *p) {
  return p ? p->last_difference_peak : 0.0;
}

void sphere_daux_vst2_set_param(SphereDauxVst2Processor *p,
                                unsigned int param_id, double value) {
  if (!p || static_cast<int>(param_id) >= p->num_params)
    return;
  p->enqueue_param(param_id, static_cast<float>(clamp01(value)));
}

int sphere_daux_vst2_get_latency_samples(SphereDauxVst2Processor *p) {
  if (!p || !p->effect)
    return 0;
  return std::max(0, p->effect->initialDelay);
}

void sphere_daux_vst2_set_process_context(SphereDauxVst2Processor *p,
                                          double tempo, int time_sig_num,
                                          int time_sig_den,
                                          long long project_time_samples,
                                          double ppq, double bar_ppq,
                                          int playing, int recording) {
  if (!p)
    return;
  // Control-thread write, once per block, before process(). The plug-in reads
  // it back through audioMasterGetTime during process on the same thread.
  auto &info = p->time_info;
  info.samplePos = static_cast<double>(project_time_samples);
  info.sampleRate = p->sample_rate;
  info.nanoSeconds = 0.0;
  info.ppqPos = ppq;
  info.tempo = tempo > 0.0 ? tempo : 120.0;
  info.barStartPos = bar_ppq;
  info.cycleStartPos = 0.0;
  info.cycleEndPos = 0.0;
  info.timeSigNumerator = time_sig_num > 0 ? time_sig_num : 4;
  info.timeSigDenominator = time_sig_den > 0 ? time_sig_den : 4;
  info.smpteOffset = 0;
  info.smpteFrameRate = 0;
  info.samplesToNextClock = 0;
  int flags = kVstTempoValid | kVstTimeSigValid | kVstPpqPosValid |
              kVstBarsValid;
  if (playing)
    flags |= kVstTransportPlaying;
  if (recording)
    flags |= kVstTransportRecording;
  info.flags = flags;
}

// ── State ───────────────────────────────────────────────────────────────────

int sphere_daux_vst2_get_state(SphereDauxVst2Processor *p,
                               unsigned char **out_component,
                               int *out_component_len,
                               unsigned char **out_controller,
                               int *out_controller_len) {
  if (!out_component || !out_component_len || !out_controller ||
      !out_controller_len)
    return 0;
  *out_component = nullptr;
  *out_component_len = 0;
  // VST2 has no split controller; the caller's packing tolerates an empty blob.
  *out_controller = nullptr;
  *out_controller_len = 0;
  if (!p || !p->effect)
    return 0;

  if (p->uses_chunks) {
    void *chunk = nullptr;
    // index 0 = bank (all programs), which is what a project should persist.
    const auto size = p->dispatch(effGetChunk, 0, 0, &chunk);
    if (size > 0 && chunk) {
      auto *buffer = static_cast<unsigned char *>(
          std::malloc(static_cast<size_t>(size)));
      if (!buffer)
        return 0;
      std::memcpy(buffer, chunk, static_cast<size_t>(size));
      *out_component = buffer;
      *out_component_len = static_cast<int>(size);
      return 1;
    }
    // Fall through to the parameter vector: a chunk-capable plug-in that
    // returns nothing is still worth persisting by value.
  }

  const int count = std::max(0, p->num_params);
  const size_t bytes = sizeof(kVst2ParamStateMagic) + sizeof(uint32_t) +
                       sizeof(float) * static_cast<size_t>(count);
  auto *buffer = static_cast<unsigned char *>(std::malloc(bytes));
  if (!buffer)
    return 0;
  std::memcpy(buffer, kVst2ParamStateMagic, sizeof(kVst2ParamStateMagic));
  const auto count_u32 = static_cast<uint32_t>(count);
  std::memcpy(buffer + sizeof(kVst2ParamStateMagic), &count_u32,
              sizeof(count_u32));
  auto *values = reinterpret_cast<float *>(buffer + sizeof(kVst2ParamStateMagic) +
                                           sizeof(uint32_t));
  for (int i = 0; i < count; ++i) {
    values[i] = p->effect->getParameter ? p->effect->getParameter(p->effect, i)
                                        : 0.f;
  }
  *out_component = buffer;
  *out_component_len = static_cast<int>(bytes);
  return 1;
}

int sphere_daux_vst2_set_state(SphereDauxVst2Processor *p,
                               const unsigned char *component_data,
                               int component_len,
                               const unsigned char *controller_data,
                               int controller_len) {
  (void)controller_data;
  (void)controller_len;
  if (!p || !p->effect || !component_data || component_len <= 0)
    return 0;

  const size_t header = sizeof(kVst2ParamStateMagic) + sizeof(uint32_t);
  const bool is_param_vector =
      static_cast<size_t>(component_len) >= header &&
      std::memcmp(component_data, kVst2ParamStateMagic,
                  sizeof(kVst2ParamStateMagic)) == 0;

  if (is_param_vector) {
    uint32_t count = 0;
    std::memcpy(&count, component_data + sizeof(kVst2ParamStateMagic),
                sizeof(count));
    const size_t available =
        (static_cast<size_t>(component_len) - header) / sizeof(float);
    const auto n = static_cast<int>(
        std::min<size_t>(std::min<size_t>(count, available),
                         static_cast<size_t>(std::max(0, p->num_params))));
    const auto *values =
        reinterpret_cast<const float *>(component_data + header);
    if (p->effect->setParameter) {
      for (int i = 0; i < n; ++i)
        p->effect->setParameter(p->effect, i, values[i]);
    }
    return 1;
  }

  if (!p->uses_chunks)
    return 0;
  // Plug-ins are inconsistent about effSetChunk's return value (many return 0
  // on success), so a non-zero reply is not required to call this applied.
  p->dispatch(effSetChunk, 0, static_cast<intptr_t>(component_len),
              const_cast<unsigned char *>(component_data));
  return 1;
}

void sphere_daux_vst2_state_free(unsigned char *data) { std::free(data); }

// ── Parameter enumeration ───────────────────────────────────────────────────

char *sphere_daux_vst2_list_parameters_json(SphereDauxVst2Processor *p) {
  if (!p || !p->effect)
    return nullptr;

  std::string json = "[";
  const int count = std::max(0, p->num_params);
  for (int i = 0; i < count; ++i) {
    if (i > 0)
      json += ",";
    std::string title = p->dispatch_string(effGetParamName, i);
    if (title.empty()) {
      char fallback[32];
      std::snprintf(fallback, sizeof(fallback), "Param %d", i + 1);
      title = fallback;
    }
    const std::string unit = p->dispatch_string(effGetParamLabel, i);

    VstParameterProperties props{};
    const bool has_props =
        p->dispatch(effGetParameterProperties, i, 0, &props) != 0;
    std::string short_title;
    if (has_props) {
      char short_buf[9] = {};
      std::memcpy(short_buf, props.shortLabel, sizeof(props.shortLabel));
      short_title = short_buf;
    }

    // VST2 has no read-only parameter concept, and `effCanBeAutomated` cannot
    // distinguish "not automatable" from "opcode not implemented". Treating a
    // non-zero reply as automatable is the long-standing host convention
    // (JUCE does the same), so plug-ins are built against it.
    const bool automatable = p->dispatch(effCanBeAutomated, i) != 0;
    const float value =
        p->effect->getParameter ? p->effect->getParameter(p->effect, i) : 0.f;

    json += "{\"id\":";
    json += std::to_string(i);
    json += ",\"title\":\"";
    append_json_escaped(json, title);
    json += "\",\"short_title\":\"";
    append_json_escaped(json, short_title);
    json += "\",\"unit\":\"";
    append_json_escaped(json, unit);
    json += "\",\"automatable\":";
    json += automatable ? "true" : "false";
    json += ",\"hidden\":false,\"read_only\":false,\"value_normalized\":";
    json += std::to_string(clamp01(static_cast<double>(value)));
    json += "}";
  }
  json += "]";

  auto *buffer = static_cast<char *>(std::malloc(json.size() + 1));
  if (!buffer)
    return nullptr;
  std::memcpy(buffer, json.c_str(), json.size() + 1);
  return buffer;
}

void sphere_daux_vst2_parameters_json_free(char *data) { std::free(data); }

// ── Editor metadata (platform-independent parts) ────────────────────────────

void sphere_daux_vst2_set_editor_title(SphereDauxVst2Processor *p,
                                       const char *title) {
  if (!p)
    return;
  p->editor_title = title ? title : "";
}

void sphere_daux_vst2_embed_set_instance_label(SphereDauxVst2Processor *p,
                                               const char *instance_id) {
  if (!p)
    return;
  p->embed_instance_label = instance_id ? instance_id : "";
}

int sphere_daux_vst2_editor_resizable(SphereDauxVst2Processor *p) {
  return (p && p->editor_resizable) ? 1 : 0;
}

int sphere_daux_vst2_embed_host_kind(SphereDauxVst2Processor *p) {
  if (!p || !p->embed_mode)
    return -1;
  return p->embed_host_kind;
}

int sphere_daux_vst2_embed_take_user_close(SphereDauxVst2Processor *p) {
  if (!p)
    return 0;
  return p->embed_user_closed.exchange(false, std::memory_order_acq_rel) ? 1
                                                                        : 0;
}

int sphere_daux_vst2_take_state_touched(SphereDauxVst2Processor *p) {
  if (!p)
    return 0;
  return p->state_touched.exchange(false, std::memory_order_acq_rel) ? 1 : 0;
}

int sphere_daux_vst2_take_pending_shell_resize(SphereDauxVst2Processor *p,
                                               int *out_width,
                                               int *out_height) {
  if (!p)
    return 0;
  if (!p->pending_main_shell_resize.exchange(false, std::memory_order_acq_rel))
    return 0;
  if (out_width)
    *out_width = p->pending_main_shell_w;
  if (out_height)
    *out_height = p->pending_main_shell_h;
  return 1;
}

int sphere_daux_vst2_embed_content_size(SphereDauxVst2Processor *p,
                                        int *out_width, int *out_height) {
  if (!p || p->embed_content_w <= 0 || p->embed_content_h <= 0)
    return 0;
  if (out_width)
    *out_width = p->embed_content_w;
  if (out_height)
    *out_height = p->embed_content_h;
  return 1;
}

/// Query the plug-in's preferred editor size without attaching. Safe to call
/// before `embed_editor`; used by the shell to size the window up front.
int sphere_daux_vst2_prepare_editor_view(SphereDauxVst2Processor *p,
                                         int *out_width, int *out_height) {
  if (!p || !p->effect || !p->has_editor)
    return 0;
  ERect *rect = nullptr;
  p->dispatch(effEditGetRect, 0, 0, &rect);
  if (!rect)
    return 0;
  const int w = rect->right - rect->left;
  const int h = rect->bottom - rect->top;
  if (w <= 0 || h <= 0)
    return 0;
  if (out_width)
    *out_width = w;
  if (out_height)
    *out_height = h;
  return 1;
}

void sphere_daux_vst2_embed_set_waiting_stage(SphereDauxVst2Processor *p,
                                              const char *stage) {
  (void)p;
  (void)stage;
  // The VST2 editor attaches synchronously in effEditOpen — there is no
  // multi-stage async bring-up to report, unlike a VST3 WebView editor. Kept
  // so the Rust surface is identical across backends.
}

} // extern "C"
