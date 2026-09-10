// au_host.mm — Audio Unit runtime for the plug-in host process (macOS).
//
// See `include/sphere_au_host.h` for the contract. Notes that matter here:
//
//   * Host-provided buffers. The unit is asked not to allocate its own output
//     buffers so a block can render straight into preallocated planes, but the
//     request is advisory — after `AudioUnitRender` the data is read back
//     through the buffer list, which is where a unit that ignored us puts it.
//   * Input arrives through a render callback, not a buffer we hand over. The
//     callback may be invoked more than once per render (units are free to pull
//     in sub-slices), so it walks a cursor over the block instead of assuming
//     one call per block.
//   * Everything the render path touches is sized at open time. No allocation,
//     no Objective-C messaging, no locking below `sphere_au_render`.

#import <AppKit/AppKit.h>
#import <AudioToolbox/AUCocoaUIView.h>
#import <AudioToolbox/AudioToolbox.h>
#import <AudioToolbox/AudioUnitUtilities.h>
#import <CoreFoundation/CoreFoundation.h>
#import <dispatch/dispatch.h>

#include "sphere_au_host.h"

#include "sphere_daux_editor_chrome.h"
#include "sphere_daux_editor_shell_mac.h"

#include <algorithm>
#include <cmath>
#include <cstdio>
#include <cstring>
#include <string>
#include <vector>

namespace {

/// Longest we wait for the asynchronous instantiation path (AUv3 app
/// extensions). Control thread only, during a load command.
constexpr double kInstantiateTimeoutSeconds = 10.0;

/// Plain-value range for one global parameter, kept next to the render path so
/// normalized automation can be denormalized without a control-thread lookup.
struct ParamRange {
  AudioUnitParameterID id;
  float min;
  float max;
  bool quantized;
};

void set_error(char* error, size_t error_len, const std::string& message) {
  if (error == nullptr || error_len == 0) {
    return;
  }
  const size_t copy = std::min(message.size(), error_len - 1);
  std::memcpy(error, message.data(), copy);
  error[copy] = '\0';
}

std::string status_message(const char* what, OSStatus status) {
  char buffer[160];
  std::snprintf(buffer, sizeof(buffer), "%s failed (OSStatus %d)", what, static_cast<int>(status));
  return std::string(buffer);
}

std::string cf_string_to_utf8(CFStringRef value) {
  if (value == nullptr) {
    return {};
  }
  char buffer[256];
  if (!CFStringGetCString(value, buffer, sizeof(buffer), kCFStringEncodingUTF8)) {
    return {};
  }
  return std::string(buffer);
}

void copy_fixed(char* dest, size_t dest_len, const std::string& source) {
  if (dest == nullptr || dest_len == 0) {
    return;
  }
  const size_t copy = std::min(source.size(), dest_len - 1);
  std::memcpy(dest, source.data(), copy);
  dest[copy] = '\0';
}

const char* unit_label(AudioUnitParameterUnit unit) {
  switch (unit) {
    case kAudioUnitParameterUnit_Percent:
    case kAudioUnitParameterUnit_EqualPowerCrossfade:
      return "%";
    case kAudioUnitParameterUnit_Seconds:
      return "s";
    case kAudioUnitParameterUnit_SampleFrames:
      return "smp";
    case kAudioUnitParameterUnit_Phase:
    case kAudioUnitParameterUnit_Degrees:
      return "deg";
    case kAudioUnitParameterUnit_Hertz:
      return "Hz";
    case kAudioUnitParameterUnit_Cents:
    case kAudioUnitParameterUnit_AbsoluteCents:
      return "cent";
    case kAudioUnitParameterUnit_RelativeSemiTones:
      return "st";
    case kAudioUnitParameterUnit_MIDINoteNumber:
      return "note";
    case kAudioUnitParameterUnit_MIDIController:
      return "cc";
    case kAudioUnitParameterUnit_Decibels:
    case kAudioUnitParameterUnit_LinearGain:
      return "dB";
    case kAudioUnitParameterUnit_Octaves:
      return "oct";
    case kAudioUnitParameterUnit_BPM:
      return "BPM";
    case kAudioUnitParameterUnit_Beats:
      return "beats";
    case kAudioUnitParameterUnit_Milliseconds:
      return "ms";
    case kAudioUnitParameterUnit_Ratio:
      return ":1";
    case kAudioUnitParameterUnit_Pan:
      return "pan";
    case kAudioUnitParameterUnit_Meters:
      return "m";
    default:
      return "";
  }
}

bool parse_component_id(const char* component_id, AudioComponentDescription& out_desc) {
  if (component_id == nullptr || component_id[0] == '\0') {
    return false;
  }
  unsigned type = 0;
  unsigned subtype = 0;
  unsigned manufacturer = 0;
  if (std::sscanf(component_id, "au:%x:%x:%x", &type, &subtype, &manufacturer) != 3) {
    return false;
  }
  out_desc = AudioComponentDescription{};
  out_desc.componentType = static_cast<OSType>(type);
  out_desc.componentSubType = static_cast<OSType>(subtype);
  out_desc.componentManufacturer = static_cast<OSType>(manufacturer);
  return true;
}

bool type_accepts_midi(OSType type) {
  return type == kAudioUnitType_MusicDevice || type == kAudioUnitType_MusicEffect;
}

bool type_is_instrument(OSType type) {
  return type == kAudioUnitType_MusicDevice || type == kAudioUnitType_Generator;
}

/// Instantiate synchronously when the component allows it, otherwise fall back
/// to the asynchronous API that also covers AUv3 app extensions. Control thread
/// only: the fallback spins the main run loop (or waits on a semaphore when the
/// caller is not the main thread) because the completion handler is delivered on
/// the main queue.
AudioComponentInstance instantiate_component(AudioComponent component, std::string& error) {
  AudioComponentInstance instance = nullptr;
  OSStatus status = AudioComponentInstanceNew(component, &instance);
  if (status == noErr && instance != nullptr) {
    return instance;
  }

  __block AudioComponentInstance async_instance = nullptr;
  __block OSStatus async_status = noErr;
  __block bool finished = false;
  dispatch_semaphore_t signal = dispatch_semaphore_create(0);
  AudioComponentInstantiate(
      component,
      kAudioComponentInstantiation_LoadOutOfProcess,
      ^(AudioComponentInstance created, OSStatus created_status) {
        async_instance = created;
        async_status = created_status;
        finished = true;
        dispatch_semaphore_signal(signal);
      });

  if (CFRunLoopGetCurrent() == CFRunLoopGetMain()) {
    const CFAbsoluteTime deadline = CFAbsoluteTimeGetCurrent() + kInstantiateTimeoutSeconds;
    while (!finished && CFAbsoluteTimeGetCurrent() < deadline) {
      CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.02, true);
    }
  } else {
    dispatch_semaphore_wait(
        signal,
        dispatch_time(DISPATCH_TIME_NOW,
                      static_cast<int64_t>(kInstantiateTimeoutSeconds * NSEC_PER_SEC)));
  }

  if (!finished) {
    error = "component instantiation timed out";
    return nullptr;
  }
  if (async_status != noErr || async_instance == nullptr) {
    error = status_message("AudioComponentInstantiate", async_status != noErr ? async_status : status);
    return nullptr;
  }
  return async_instance;
}

UInt32 element_count(AudioUnit unit, AudioUnitScope scope) {
  UInt32 count = 0;
  UInt32 size = sizeof(count);
  if (AudioUnitGetProperty(unit, kAudioUnitProperty_ElementCount, scope, 0, &count, &size) != noErr) {
    return 0;
  }
  return count;
}

AudioStreamBasicDescription make_asbd(double sample_rate, unsigned int channels) {
  AudioStreamBasicDescription asbd{};
  asbd.mSampleRate = sample_rate;
  asbd.mFormatID = kAudioFormatLinearPCM;
  asbd.mFormatFlags = static_cast<AudioFormatFlags>(kAudioFormatFlagsNativeFloatPacked) |
                      static_cast<AudioFormatFlags>(kAudioFormatFlagIsNonInterleaved);
  asbd.mBitsPerChannel = 32;
  asbd.mChannelsPerFrame = channels;
  asbd.mFramesPerPacket = 1;
  asbd.mBytesPerFrame = sizeof(Float32);
  asbd.mBytesPerPacket = sizeof(Float32);
  return asbd;
}

/// Ask for deinterleaved float32 at `channels`, and report what the unit
/// actually ended up with. A unit that refuses the requested layout keeps its
/// own; the caller adapts rather than failing the load.
AudioStreamBasicDescription negotiate_format(
    AudioUnit unit,
    AudioUnitScope scope,
    double sample_rate,
    unsigned int channels) {
  const AudioStreamBasicDescription desired = make_asbd(sample_rate, channels);
  AudioUnitSetProperty(
      unit, kAudioUnitProperty_StreamFormat, scope, 0, &desired, sizeof(desired));

  AudioStreamBasicDescription actual{};
  UInt32 size = sizeof(actual);
  if (AudioUnitGetProperty(unit, kAudioUnitProperty_StreamFormat, scope, 0, &actual, &size) !=
      noErr) {
    return desired;
  }
  if (std::fabs(actual.mSampleRate - sample_rate) > 0.001) {
    actual.mSampleRate = sample_rate;
    AudioUnitSetProperty(
        unit, kAudioUnitProperty_StreamFormat, scope, 0, &actual, sizeof(actual));
    size = sizeof(actual);
    AudioUnitGetProperty(unit, kAudioUnitProperty_StreamFormat, scope, 0, &actual, &size);
  }
  return actual;
}

bool format_is_planar(const AudioStreamBasicDescription& asbd) {
  return (asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved) != 0;
}

}  // namespace

/// One live Audio Unit and every buffer its render path needs.
struct SphereAuInstance {
  AudioUnit unit = nullptr;
  AudioComponentDescription desc{};
  double sample_rate = 48000.0;
  unsigned int max_frames = 0;
  unsigned int input_channels = 0;
  unsigned int output_channels = 0;
  bool output_planar = true;
  bool has_input_bus = false;
  bool accepts_midi = false;
  bool is_instrument = false;
  bool initialized = false;

  // Cocoa editor objects are retained explicitly so their lifetime is
  // independent of local ARC scopes. The editor is host-owned; closing it does
  // not dispose the Audio Unit used by the audio producer.
  void* editor_window = nullptr;
  void* editor_view = nullptr;
  void* editor_delegate = nullptr;
  bool editor_user_closed = false;

  /// Deinterleaved output planes, `max_frames` apart. Also backs an interleaved
  /// layout when the unit insisted on one.
  std::vector<float> output_scratch;
  /// `AudioBufferList` with room for `output_channels` buffers.
  std::vector<unsigned char> buffer_list_storage;

  /// The block currently being rendered, walked by the input callback.
  const float* in_l = nullptr;
  const float* in_r = nullptr;
  unsigned int in_frames = 0;
  unsigned int in_cursor = 0;

  SphereAuTransport transport{};
  /// Render timeline, which must advance monotonically regardless of transport.
  Float64 render_sample_time = 0.0;

  std::vector<SphereAuParameterInfo> parameters;
  std::vector<ParamRange> ranges;

  AudioBufferList* buffer_list() {
    return reinterpret_cast<AudioBufferList*>(buffer_list_storage.data());
  }
};

static void mark_au_editor_user_closed(SphereAuInstance* instance) {
  if (instance != nullptr) {
    instance->editor_user_closed = true;
  }
}

@interface SphereAuEditorWindowDelegate : NSObject <NSWindowDelegate>
@property(nonatomic, assign) SphereAuInstance* instance;
@end

@implementation SphereAuEditorWindowDelegate
- (void)windowWillClose:(NSNotification*)notification {
  (void)notification;
  mark_au_editor_user_closed(self.instance);
}
@end

namespace {

OSStatus render_input(
    void* refcon,
    AudioUnitRenderActionFlags* flags,
    const AudioTimeStamp* timestamp,
    UInt32 bus,
    UInt32 frames,
    AudioBufferList* data) {
  (void)timestamp;
  (void)bus;
  auto* self = static_cast<SphereAuInstance*>(refcon);
  if (self == nullptr || data == nullptr) {
    return kAudio_ParamError;
  }

  const unsigned int available =
      self->in_cursor < self->in_frames ? self->in_frames - self->in_cursor : 0;
  const unsigned int usable = std::min<unsigned int>(frames, available);
  const unsigned int offset = self->in_cursor;
  self->in_cursor += usable;

  if (usable == 0 && flags != nullptr) {
    *flags |= kAudioUnitRenderAction_OutputIsSilence;
  }

  for (UInt32 buffer_index = 0; buffer_index < data->mNumberBuffers; ++buffer_index) {
    AudioBuffer& buffer = data->mBuffers[buffer_index];
    auto* out = static_cast<float*>(buffer.mData);
    if (out == nullptr) {
      continue;
    }
    const UInt32 channels = std::max<UInt32>(buffer.mNumberChannels, 1);
    if (channels == 1) {
      // Deinterleaved: one plane per channel, in bus channel order.
      const float* source = buffer_index == 0 ? self->in_l : self->in_r;
      if (buffer_index > 1 || source == nullptr) {
        std::memset(out, 0, sizeof(float) * frames);
        continue;
      }
      std::memcpy(out, source + offset, sizeof(float) * usable);
      if (usable < frames) {
        std::memset(out + usable, 0, sizeof(float) * (frames - usable));
      }
      continue;
    }
    // Interleaved: fill the first two channels, silence any extras.
    std::memset(out, 0, sizeof(float) * frames * channels);
    for (unsigned int frame = 0; frame < usable; ++frame) {
      float* slot = out + static_cast<size_t>(frame) * channels;
      if (self->in_l != nullptr) {
        slot[0] = self->in_l[offset + frame];
      }
      if (channels > 1 && self->in_r != nullptr) {
        slot[1] = self->in_r[offset + frame];
      }
    }
  }
  return noErr;
}

OSStatus host_beat_and_tempo(void* refcon, Float64* out_beat, Float64* out_tempo) {
  auto* self = static_cast<SphereAuInstance*>(refcon);
  if (self == nullptr) {
    return kAudio_ParamError;
  }
  if (out_beat != nullptr) {
    *out_beat = self->transport.ppq_position;
  }
  if (out_tempo != nullptr) {
    *out_tempo = self->transport.tempo_bpm > 0.0 ? self->transport.tempo_bpm : 120.0;
  }
  return noErr;
}

OSStatus host_musical_time(
    void* refcon,
    UInt32* out_delta_to_next_beat,
    Float32* out_time_sig_numerator,
    UInt32* out_time_sig_denominator,
    Float64* out_measure_downbeat) {
  auto* self = static_cast<SphereAuInstance*>(refcon);
  if (self == nullptr) {
    return kAudio_ParamError;
  }
  if (out_delta_to_next_beat != nullptr) {
    // Samples until the next whole beat, from the engine's PPQ position.
    const double tempo = self->transport.tempo_bpm > 0.0 ? self->transport.tempo_bpm : 120.0;
    const double samples_per_beat = self->sample_rate * 60.0 / tempo;
    const double into_beat = self->transport.ppq_position - std::floor(self->transport.ppq_position);
    *out_delta_to_next_beat =
        static_cast<UInt32>(std::max(0.0, (1.0 - into_beat) * samples_per_beat));
  }
  if (out_time_sig_numerator != nullptr) {
    *out_time_sig_numerator = static_cast<Float32>(
        self->transport.time_sig_num > 0 ? self->transport.time_sig_num : 4);
  }
  if (out_time_sig_denominator != nullptr) {
    *out_time_sig_denominator = self->transport.time_sig_den > 0 ? self->transport.time_sig_den : 4;
  }
  if (out_measure_downbeat != nullptr) {
    *out_measure_downbeat = self->transport.bar_position_ppq;
  }
  return noErr;
}

OSStatus host_transport_state(
    void* refcon,
    Boolean* out_playing,
    Boolean* out_changed,
    Float64* out_sample_in_timeline,
    Boolean* out_cycling,
    Float64* out_cycle_start,
    Float64* out_cycle_end) {
  auto* self = static_cast<SphereAuInstance*>(refcon);
  if (self == nullptr) {
    return kAudio_ParamError;
  }
  if (out_playing != nullptr) {
    *out_playing = self->transport.playing != 0;
  }
  if (out_changed != nullptr) {
    *out_changed = false;
  }
  if (out_sample_in_timeline != nullptr) {
    *out_sample_in_timeline = static_cast<Float64>(self->transport.project_time_samples);
  }
  if (out_cycling != nullptr) {
    *out_cycling = false;
  }
  if (out_cycle_start != nullptr) {
    *out_cycle_start = 0.0;
  }
  if (out_cycle_end != nullptr) {
    *out_cycle_end = 0.0;
  }
  return noErr;
}

OSStatus host_transport_state2(
    void* refcon,
    Boolean* out_playing,
    Boolean* out_recording,
    Boolean* out_changed,
    Float64* out_sample_in_timeline,
    Boolean* out_cycling,
    Float64* out_cycle_start,
    Float64* out_cycle_end) {
  auto* self = static_cast<SphereAuInstance*>(refcon);
  if (out_recording != nullptr) {
    *out_recording = self != nullptr && self->transport.recording != 0;
  }
  return host_transport_state(
      refcon, out_playing, out_changed, out_sample_in_timeline, out_cycling, out_cycle_start,
      out_cycle_end);
}

void collect_parameters(SphereAuInstance* self) {
  UInt32 size = 0;
  Boolean writable = false;
  if (AudioUnitGetPropertyInfo(
          self->unit, kAudioUnitProperty_ParameterList, kAudioUnitScope_Global, 0, &size,
          &writable) != noErr ||
      size < sizeof(AudioUnitParameterID)) {
    return;
  }

  std::vector<AudioUnitParameterID> ids(size / sizeof(AudioUnitParameterID));
  if (AudioUnitGetProperty(
          self->unit, kAudioUnitProperty_ParameterList, kAudioUnitScope_Global, 0, ids.data(),
          &size) != noErr) {
    return;
  }
  ids.resize(size / sizeof(AudioUnitParameterID));

  self->parameters.reserve(ids.size());
  self->ranges.reserve(ids.size());
  for (AudioUnitParameterID id : ids) {
    AudioUnitParameterInfo info{};
    UInt32 info_size = sizeof(info);
    if (AudioUnitGetProperty(
            self->unit, kAudioUnitProperty_ParameterInfo, kAudioUnitScope_Global, id, &info,
            &info_size) != noErr) {
      continue;
    }

    std::string name;
    if ((info.flags & kAudioUnitParameterFlag_HasCFNameString) != 0 && info.cfNameString != nullptr) {
      name = cf_string_to_utf8(info.cfNameString);
      if ((info.flags & kAudioUnitParameterFlag_CFNameRelease) != 0) {
        CFRelease(info.cfNameString);
      }
    }
    if (name.empty()) {
      name.assign(info.name, strnlen(info.name, sizeof(info.name)));
    }
    if (name.empty()) {
      char fallback[32];
      std::snprintf(fallback, sizeof(fallback), "Param %u", static_cast<unsigned>(id));
      name = fallback;
    }

    std::string unit = unit_label(info.unit);
    if (info.unit == kAudioUnitParameterUnit_CustomUnit && info.unitName != nullptr) {
      unit = cf_string_to_utf8(info.unitName);
      CFRelease(info.unitName);
    }

    const float min = info.minValue;
    const float max = info.maxValue;
    const float span = max - min;
    const bool writable_param = (info.flags & kAudioUnitParameterFlag_IsWritable) != 0;

    SphereAuParameterInfo out{};
    out.id = id;
    copy_fixed(out.name, sizeof(out.name), name);
    copy_fixed(out.unit, sizeof(out.unit), unit);
    out.normalized_default =
        std::fabs(span) > 1e-9f ? std::clamp((info.defaultValue - min) / span, 0.0f, 1.0f) : 0.0f;
    out.automatable =
        writable_param && (info.flags & kAudioUnitParameterFlag_NonRealTime) == 0 ? 1 : 0;
    out.read_only = writable_param ? 0 : 1;
    out.hidden = (info.flags & kAudioUnitParameterFlag_ExpertMode) != 0 ? 1 : 0;
    self->parameters.push_back(out);

    ParamRange range{};
    range.id = id;
    range.min = min;
    range.max = max;
    range.quantized = info.unit == kAudioUnitParameterUnit_Indexed ||
                      info.unit == kAudioUnitParameterUnit_Boolean;
    self->ranges.push_back(range);
  }

  std::sort(self->ranges.begin(), self->ranges.end(), [](const ParamRange& a, const ParamRange& b) {
    return a.id < b.id;
  });
}

}  // namespace

extern "C" {

SPHERE_AU_HOST_API SphereAuInstance* sphere_au_open(
    const char* component_id,
    double sample_rate,
    unsigned int max_block_frames,
    char* error,
    size_t error_len) {
  AudioComponentDescription desc{};
  if (!parse_component_id(component_id, desc)) {
    set_error(error, error_len, "malformed component id (expected au:<type>:<subtype>:<manuf>)");
    return nullptr;
  }
  if (sample_rate < 1.0 || max_block_frames == 0) {
    set_error(error, error_len, "invalid sample rate or block size");
    return nullptr;
  }

  AudioComponent component = AudioComponentFindNext(nullptr, &desc);
  if (component == nullptr) {
    set_error(error, error_len, "no installed component matches the id");
    return nullptr;
  }

  std::string instantiate_error;
  AudioComponentInstance unit = instantiate_component(component, instantiate_error);
  if (unit == nullptr) {
    set_error(
        error, error_len,
        instantiate_error.empty() ? "component instantiation failed" : instantiate_error);
    return nullptr;
  }

  auto* self = new SphereAuInstance();
  self->unit = unit;
  self->desc = desc;
  self->sample_rate = sample_rate;
  self->max_frames = max_block_frames;
  self->accepts_midi = type_accepts_midi(desc.componentType);
  self->is_instrument = type_is_instrument(desc.componentType);
  self->transport.tempo_bpm = 120.0;
  self->transport.time_sig_num = 4;
  self->transport.time_sig_den = 4;

  if (element_count(unit, kAudioUnitScope_Output) == 0) {
    set_error(error, error_len, "component has no output bus");
    sphere_au_close(self);
    return nullptr;
  }
  self->has_input_bus = !self->is_instrument && element_count(unit, kAudioUnitScope_Input) > 0;

  UInt32 max_frames_property = max_block_frames;
  AudioUnitSetProperty(
      unit, kAudioUnitProperty_MaximumFramesPerSlice, kAudioUnitScope_Global, 0,
      &max_frames_property, sizeof(max_frames_property));

  // Advisory: lets a block render straight into our own planes. Units that
  // refuse keep allocating, which the render path handles by reading the
  // buffer list back rather than its own pointers.
  UInt32 should_allocate = 0;
  AudioUnitSetProperty(
      unit, kAudioUnitProperty_ShouldAllocateBuffer, kAudioUnitScope_Output, 0, &should_allocate,
      sizeof(should_allocate));

  if (self->has_input_bus) {
    const AudioStreamBasicDescription input_format =
        negotiate_format(unit, kAudioUnitScope_Input, sample_rate, 2);
    self->input_channels = input_format.mChannelsPerFrame;

    AURenderCallbackStruct callback{};
    callback.inputProc = render_input;
    callback.inputProcRefCon = self;
    const OSStatus status = AudioUnitSetProperty(
        unit, kAudioUnitProperty_SetRenderCallback, kAudioUnitScope_Input, 0, &callback,
        sizeof(callback));
    if (status != noErr) {
      set_error(error, error_len, status_message("set input render callback", status));
      sphere_au_close(self);
      return nullptr;
    }
  }

  const AudioStreamBasicDescription output_format =
      negotiate_format(unit, kAudioUnitScope_Output, sample_rate, 2);
  self->output_channels = std::max<unsigned int>(output_format.mChannelsPerFrame, 1);
  self->output_planar = format_is_planar(output_format);

  HostCallbackInfo host_callbacks{};
  host_callbacks.hostUserData = self;
  host_callbacks.beatAndTempoProc = host_beat_and_tempo;
  host_callbacks.musicalTimeLocationProc = host_musical_time;
  host_callbacks.transportStateProc = host_transport_state;
  host_callbacks.transportStateProc2 = host_transport_state2;
  AudioUnitSetProperty(
      unit, kAudioUnitProperty_HostCallbacks, kAudioUnitScope_Global, 0, &host_callbacks,
      sizeof(host_callbacks));

  const OSStatus initialized = AudioUnitInitialize(unit);
  if (initialized != noErr) {
    set_error(error, error_len, status_message("AudioUnitInitialize", initialized));
    sphere_au_close(self);
    return nullptr;
  }
  self->initialized = true;

  // Initialization can renegotiate, so trust the post-init format.
  AudioStreamBasicDescription final_output{};
  UInt32 size = sizeof(final_output);
  if (AudioUnitGetProperty(
          unit, kAudioUnitProperty_StreamFormat, kAudioUnitScope_Output, 0, &final_output, &size) ==
      noErr) {
    self->output_channels = std::max<unsigned int>(final_output.mChannelsPerFrame, 1);
    self->output_planar = format_is_planar(final_output);
  }

  self->output_scratch.assign(
      static_cast<size_t>(self->max_frames) * self->output_channels, 0.0f);
  self->buffer_list_storage.assign(
      sizeof(AudioBufferList) + sizeof(AudioBuffer) * self->output_channels, 0);
  collect_parameters(self);

  std::fprintf(
      stderr,
      "[plugin-host-au] opened %s sr=%.0f max_frames=%u in=%u out=%u planar=%d midi=%d "
      "instrument=%d params=%zu\n",
      component_id, sample_rate, max_block_frames, self->input_channels, self->output_channels,
      self->output_planar ? 1 : 0, self->accepts_midi ? 1 : 0, self->is_instrument ? 1 : 0,
      self->parameters.size());
  return self;
}

SPHERE_AU_HOST_API void sphere_au_close(SphereAuInstance* instance) {
  if (instance == nullptr) {
    return;
  }
  sphere_au_close_editor(instance);
  if (instance->unit != nullptr) {
    if (instance->initialized) {
      AudioUnitUninitialize(instance->unit);
    }
    AudioComponentInstanceDispose(instance->unit);
  }
  delete instance;
}

SPHERE_AU_HOST_API unsigned int sphere_au_output_channels(const SphereAuInstance* instance) {
  return instance != nullptr ? instance->output_channels : 0;
}

SPHERE_AU_HOST_API unsigned int sphere_au_input_channels(const SphereAuInstance* instance) {
  return instance != nullptr ? instance->input_channels : 0;
}

SPHERE_AU_HOST_API int sphere_au_accepts_midi(const SphereAuInstance* instance) {
  return instance != nullptr && instance->accepts_midi ? 1 : 0;
}

SPHERE_AU_HOST_API int sphere_au_is_instrument(const SphereAuInstance* instance) {
  return instance != nullptr && instance->is_instrument ? 1 : 0;
}

SPHERE_AU_HOST_API unsigned int sphere_au_latency_samples(const SphereAuInstance* instance) {
  if (instance == nullptr || instance->unit == nullptr) {
    return 0;
  }
  Float64 seconds = 0.0;
  UInt32 size = sizeof(seconds);
  if (AudioUnitGetProperty(
          instance->unit, kAudioUnitProperty_Latency, kAudioUnitScope_Global, 0, &seconds, &size) !=
          noErr ||
      seconds <= 0.0) {
    return 0;
  }
  return static_cast<unsigned int>(seconds * instance->sample_rate + 0.5);
}

SPHERE_AU_HOST_API unsigned int sphere_au_render(
    SphereAuInstance* instance,
    const float* in_l,
    const float* in_r,
    unsigned int frames,
    float* out_interleaved,
    unsigned int out_channels,
    const SphereAuTransport* transport) {
  if (instance == nullptr || instance->unit == nullptr || !instance->initialized ||
      out_interleaved == nullptr || frames == 0 || out_channels == 0) {
    return 0;
  }
  frames = std::min(frames, instance->max_frames);
  if (transport != nullptr) {
    instance->transport = *transport;
  }
  instance->in_l = in_l;
  instance->in_r = in_r;
  instance->in_frames = frames;
  instance->in_cursor = 0;

  const unsigned int channels = instance->output_channels;
  AudioBufferList* list = instance->buffer_list();
  float* scratch = instance->output_scratch.data();
  if (instance->output_planar) {
    list->mNumberBuffers = channels;
    for (unsigned int channel = 0; channel < channels; ++channel) {
      list->mBuffers[channel].mNumberChannels = 1;
      list->mBuffers[channel].mDataByteSize = sizeof(float) * frames;
      list->mBuffers[channel].mData = scratch + static_cast<size_t>(channel) * instance->max_frames;
    }
  } else {
    list->mNumberBuffers = 1;
    list->mBuffers[0].mNumberChannels = channels;
    list->mBuffers[0].mDataByteSize = sizeof(float) * frames * channels;
    list->mBuffers[0].mData = scratch;
  }

  AudioUnitRenderActionFlags flags = 0;
  AudioTimeStamp timestamp{};
  timestamp.mFlags = kAudioTimeStampSampleTimeValid;
  timestamp.mSampleTime = instance->render_sample_time;
  const OSStatus status =
      AudioUnitRender(instance->unit, &flags, &timestamp, 0, frames, list);
  instance->render_sample_time += frames;
  instance->in_l = nullptr;
  instance->in_r = nullptr;
  if (status != noErr) {
    return 0;
  }

  const unsigned int written = std::min(channels, out_channels);
  if ((flags & kAudioUnitRenderAction_OutputIsSilence) != 0) {
    for (unsigned int frame = 0; frame < frames; ++frame) {
      float* slot = out_interleaved + static_cast<size_t>(frame) * out_channels;
      for (unsigned int channel = 0; channel < written; ++channel) {
        slot[channel] = 0.0f;
      }
    }
    return written;
  }

  if (instance->output_planar) {
    for (unsigned int channel = 0; channel < written; ++channel) {
      const auto* plane = static_cast<const float*>(list->mBuffers[channel].mData);
      if (plane == nullptr) {
        continue;
      }
      for (unsigned int frame = 0; frame < frames; ++frame) {
        out_interleaved[static_cast<size_t>(frame) * out_channels + channel] = plane[frame];
      }
    }
  } else {
    const auto* source = static_cast<const float*>(list->mBuffers[0].mData);
    if (source == nullptr) {
      return 0;
    }
    for (unsigned int frame = 0; frame < frames; ++frame) {
      const float* in_slot = source + static_cast<size_t>(frame) * channels;
      float* out_slot = out_interleaved + static_cast<size_t>(frame) * out_channels;
      for (unsigned int channel = 0; channel < written; ++channel) {
        out_slot[channel] = in_slot[channel];
      }
    }
  }
  return written;
}

SPHERE_AU_HOST_API void sphere_au_set_parameter_normalized(
    SphereAuInstance* instance,
    unsigned int param_id,
    float normalized) {
  if (instance == nullptr || instance->unit == nullptr || instance->ranges.empty()) {
    return;
  }
  const auto found = std::lower_bound(
      instance->ranges.begin(), instance->ranges.end(), param_id,
      [](const ParamRange& range, unsigned int id) { return range.id < id; });
  if (found == instance->ranges.end() || found->id != param_id) {
    return;
  }
  const float clamped = std::clamp(normalized, 0.0f, 1.0f);
  float plain = found->min + clamped * (found->max - found->min);
  if (found->quantized) {
    plain = std::round(plain);
  }
  AudioUnitSetParameter(instance->unit, param_id, kAudioUnitScope_Global, 0, plain, 0);
}

SPHERE_AU_HOST_API void sphere_au_send_midi(
    SphereAuInstance* instance,
    unsigned char status,
    unsigned char data1,
    unsigned char data2,
    unsigned int offset_frames) {
  if (instance == nullptr || instance->unit == nullptr || !instance->accepts_midi) {
    return;
  }
  MusicDeviceMIDIEvent(instance->unit, status, data1, data2, offset_frames);
}

SPHERE_AU_HOST_API void sphere_au_reset(SphereAuInstance* instance) {
  if (instance == nullptr || instance->unit == nullptr) {
    return;
  }
  AudioUnitReset(instance->unit, kAudioUnitScope_Global, 0);
}

SPHERE_AU_HOST_API unsigned int sphere_au_parameter_count(const SphereAuInstance* instance) {
  return instance != nullptr ? static_cast<unsigned int>(instance->parameters.size()) : 0;
}

SPHERE_AU_HOST_API int sphere_au_parameter_info(
    const SphereAuInstance* instance,
    unsigned int index,
    SphereAuParameterInfo* out_info) {
  if (instance == nullptr || out_info == nullptr || index >= instance->parameters.size()) {
    return 0;
  }
  *out_info = instance->parameters[index];
  return 1;
}

SPHERE_AU_HOST_API size_t sphere_au_get_state(
    const SphereAuInstance* instance,
    unsigned char* out,
    size_t capacity) {
  if (instance == nullptr || instance->unit == nullptr) {
    return 0;
  }
  CFPropertyListRef plist = nullptr;
  UInt32 size = sizeof(plist);
  if (AudioUnitGetProperty(
          instance->unit, kAudioUnitProperty_ClassInfo, kAudioUnitScope_Global, 0, &plist, &size) !=
          noErr ||
      plist == nullptr) {
    return 0;
  }
  CFDataRef data =
      CFPropertyListCreateData(kCFAllocatorDefault, plist, kCFPropertyListBinaryFormat_v1_0, 0, nullptr);
  CFRelease(plist);
  if (data == nullptr) {
    return 0;
  }
  const size_t len = static_cast<size_t>(CFDataGetLength(data));
  if (out != nullptr && capacity > 0) {
    CFDataGetBytes(data, CFRangeMake(0, static_cast<CFIndex>(std::min(len, capacity))), out);
  }
  CFRelease(data);
  return len;
}

SPHERE_AU_HOST_API int sphere_au_set_state(
    SphereAuInstance* instance,
    const unsigned char* data,
    size_t len) {
  if (instance == nullptr || instance->unit == nullptr || data == nullptr || len == 0) {
    return 0;
  }
  CFDataRef payload = CFDataCreate(kCFAllocatorDefault, data, static_cast<CFIndex>(len));
  if (payload == nullptr) {
    return 0;
  }
  CFPropertyListRef plist = CFPropertyListCreateWithData(
      kCFAllocatorDefault, payload, kCFPropertyListImmutable, nullptr, nullptr);
  CFRelease(payload);
  if (plist == nullptr) {
    return 0;
  }
  const OSStatus status = AudioUnitSetProperty(
      instance->unit, kAudioUnitProperty_ClassInfo, kAudioUnitScope_Global, 0, &plist,
      sizeof(plist));
  CFRelease(plist);
  if (status != noErr) {
    return 0;
  }
  // An open editor tracks parameters through its own listener, so a restore has
  // to be announced or the UI keeps drawing the pre-restore values.
  AudioUnitParameter changed{};
  changed.mAudioUnit = instance->unit;
  changed.mParameterID = kAUParameterListener_AnyParameter;
  changed.mScope = kAudioUnitScope_Global;
  changed.mElement = 0;
  AUParameterListenerNotify(nullptr, nullptr, &changed);
  return 1;
}

SPHERE_AU_HOST_API unsigned long long sphere_au_open_editor(
    SphereAuInstance* instance,
    const char* title,
    unsigned int preferred_width,
    unsigned int preferred_height,
    unsigned int* out_width,
    unsigned int* out_height) {
  if (out_width != nullptr) {
    *out_width = 0;
  }
  if (out_height != nullptr) {
    *out_height = 0;
  }
  if (instance == nullptr || instance->unit == nullptr) {
    return 0;
  }
  if (![NSThread isMainThread]) {
    __block unsigned long long handle = 0;
    dispatch_sync(dispatch_get_main_queue(), ^{
      handle = sphere_au_open_editor(
          instance, title, preferred_width, preferred_height, out_width, out_height);
    });
    return handle;
  }

  if (instance->editor_window != nullptr) {
    NSWindow* window = (__bridge NSWindow*)instance->editor_window;
    // The unit's own view, not the window's content view — the content view is
    // the shell, and its height includes the chrome.
    NSView* view = (__bridge NSView*)instance->editor_view;
    if (out_width != nullptr) {
      *out_width = static_cast<unsigned int>(std::max<CGFloat>(view.frame.size.width, 1.0));
    }
    if (out_height != nullptr) {
      *out_height = static_cast<unsigned int>(std::max<CGFloat>(view.frame.size.height, 1.0));
    }
    [window makeKeyAndOrderFront:nil];
    [NSApp activateIgnoringOtherApps:YES];
    return reinterpret_cast<unsigned long long>((__bridge void*)window);
  }

  UInt32 cocoa_info_size = 0;
  Boolean writable = false;
  OSStatus status = AudioUnitGetPropertyInfo(
      instance->unit, kAudioUnitProperty_CocoaUI, kAudioUnitScope_Global, 0,
      &cocoa_info_size, &writable);
  if (status != noErr || cocoa_info_size < sizeof(AudioUnitCocoaViewInfo)) {
    std::fprintf(
        stderr,
        "[plugin-host-au] Cocoa editor unavailable property_status=%d bytes=%u\n",
        static_cast<int>(status), cocoa_info_size);
    return 0;
  }

  std::vector<unsigned char> cocoa_info_storage(cocoa_info_size, 0);
  auto* cocoa_info =
      reinterpret_cast<AudioUnitCocoaViewInfo*>(cocoa_info_storage.data());
  status = AudioUnitGetProperty(
      instance->unit, kAudioUnitProperty_CocoaUI, kAudioUnitScope_Global, 0,
      cocoa_info, &cocoa_info_size);
  if (status != noErr || cocoa_info->mCocoaAUViewBundleLocation == nullptr ||
      cocoa_info->mCocoaAUViewClass[0] == nullptr) {
    std::fprintf(
        stderr,
        "[plugin-host-au] Cocoa editor metadata failed status=%d\n",
        static_cast<int>(status));
    return 0;
  }

  NSURL* bundle_url = (__bridge NSURL*)cocoa_info->mCocoaAUViewBundleLocation;
  NSString* factory_name = (__bridge NSString*)cocoa_info->mCocoaAUViewClass[0];
  NSBundle* bundle = [NSBundle bundleWithURL:bundle_url];
  NSError* load_error = nil;
  if (bundle == nil || ![bundle loadAndReturnError:&load_error]) {
    std::fprintf(
        stderr, "[plugin-host-au] Cocoa editor bundle load failed: %s\n",
        load_error.localizedDescription.UTF8String ?: "unknown error");
    return 0;
  }

  Class factory_class = NSClassFromString(factory_name);
  id factory = factory_class != Nil ? [[factory_class alloc] init] : nil;
  if (factory == nil || ![factory conformsToProtocol:@protocol(AUCocoaUIBase)]) {
    std::fprintf(
        stderr, "[plugin-host-au] Cocoa editor factory unavailable class=%s\n",
        factory_name.UTF8String ?: "<unknown>");
    return 0;
  }

  id<AUCocoaUIBase> cocoa_factory = (id<AUCocoaUIBase>)factory;
  NSView* view = [cocoa_factory uiViewForAudioUnit:instance->unit
                                         withSize:NSZeroSize];
  if (view == nil) {
    std::fprintf(stderr, "[plugin-host-au] Cocoa editor factory returned no view\n");
    return 0;
  }

  NSSize size = view.frame.size;
  if (size.width < 32.0 || size.height < 32.0) {
    size = NSMakeSize(
        std::max<unsigned int>(preferred_width, 640),
        std::max<unsigned int>(preferred_height, 360));
    [view setFrameSize:size];
  }
  // The window carries the editor chrome as well as the unit's own view, so
  // its content is taller than the view by exactly that strip. `size` stays the
  // *unit's* size throughout — it is what the caller reports as the editor's
  // dimensions and what the unit itself laid out for.
  const CGFloat chrome_h = sphere_daux_editor_chrome_height();
  NSRect content_rect =
      NSMakeRect(0.0, 0.0, size.width, size.height + chrome_h);
  NSWindowStyleMask style = NSWindowStyleMaskTitled | NSWindowStyleMaskClosable |
                            NSWindowStyleMaskMiniaturizable;
  NSWindow* window = [[NSWindow alloc] initWithContentRect:content_rect
                                                 styleMask:style
                                                   backing:NSBackingStoreBuffered
                                                     defer:NO];
  NSString* window_title = [NSString
      stringWithUTF8String:(title != nullptr && title[0] != '\0') ? title : "Audio Unit"];
  window.title = window_title;
  window.backgroundColor = NSColor.blackColor;
  window.level = NSFloatingWindowLevel;
  window.releasedWhenClosed = NO;
  // The unit's view goes *under* the chrome rather than being the content view
  // itself. It keeps its own size; only where it sits changes.
  NSView* shell = sphere_daux_editor_shell_create(content_rect);
  window.contentView = shell;
  [view setFrame:sphere_daux_editor_shell_plugin_area(shell)];
  view.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
  [shell addSubview:view];
  [window center];

  SphereAuEditorWindowDelegate* delegate = [[SphereAuEditorWindowDelegate alloc] init];
  delegate.instance = instance;
  window.delegate = delegate;

  instance->editor_window = (__bridge_retained void*)window;
  instance->editor_view = (__bridge_retained void*)view;
  instance->editor_delegate = (__bridge_retained void*)delegate;
  instance->editor_user_closed = false;

  [window makeKeyAndOrderFront:nil];
  [NSApp activateIgnoringOtherApps:YES];
  if (out_width != nullptr) {
    *out_width = static_cast<unsigned int>(std::max<CGFloat>(size.width, 1.0));
  }
  if (out_height != nullptr) {
    *out_height = static_cast<unsigned int>(std::max<CGFloat>(size.height, 1.0));
  }
  const auto handle =
      reinterpret_cast<unsigned long long>((__bridge void*)window);
  std::fprintf(
      stderr, "[plugin-host-au] Cocoa editor opened handle=0x%llx size=%ux%u\n",
      handle, out_width != nullptr ? *out_width : 0,
      out_height != nullptr ? *out_height : 0);
  return handle;
}

/// The `NSWindow*` of this unit's editor, as an opaque handle.
///
/// 0 whenever no editor is open. The caller passes it straight to the shared
/// editor-chrome ABI, which addresses the strip by the window it lives in — the
/// same way it is addressed for a VST3, VST2 or CLAP editor, none of whose
/// instance types this one can name.
SPHERE_AU_HOST_API unsigned long long
sphere_au_editor_native_window(SphereAuInstance* instance) {
  if (instance == nullptr) {
    return 0;
  }
  return reinterpret_cast<unsigned long long>(instance->editor_window);
}

SPHERE_AU_HOST_API void sphere_au_close_editor(SphereAuInstance* instance) {
  if (instance == nullptr || instance->editor_window == nullptr) {
    return;
  }
  if (![NSThread isMainThread]) {
    dispatch_async(dispatch_get_main_queue(), ^{ sphere_au_close_editor(instance); });
    return;
  }

  void* window_ptr = instance->editor_window;
  void* view_ptr = instance->editor_view;
  void* delegate_ptr = instance->editor_delegate;
  instance->editor_window = nullptr;
  instance->editor_view = nullptr;
  instance->editor_delegate = nullptr;
  instance->editor_user_closed = false;

  NSWindow* window = (__bridge_transfer NSWindow*)window_ptr;
  window.delegate = nil;
  [window orderOut:nil];
  [window close];
  if (view_ptr != nullptr) {
    NSView* view = (__bridge_transfer NSView*)view_ptr;
    [view removeFromSuperview];
  }
  if (delegate_ptr != nullptr) {
    SphereAuEditorWindowDelegate* delegate =
        (__bridge_transfer SphereAuEditorWindowDelegate*)delegate_ptr;
    delegate.instance = nullptr;
  }
  std::fprintf(stderr, "[plugin-host-au] Cocoa editor closed\n");
}

SPHERE_AU_HOST_API int sphere_au_focus_editor(SphereAuInstance* instance) {
  if (instance == nullptr || instance->editor_window == nullptr) {
    return 0;
  }
  if (![NSThread isMainThread]) {
    dispatch_async(dispatch_get_main_queue(), ^{ sphere_au_focus_editor(instance); });
    return 1;
  }
  NSWindow* window = (__bridge NSWindow*)instance->editor_window;
  [window makeKeyAndOrderFront:nil];
  [NSApp activateIgnoringOtherApps:YES];
  return 1;
}

SPHERE_AU_HOST_API int sphere_au_take_editor_user_close(SphereAuInstance* instance) {
  if (instance == nullptr || !instance->editor_user_closed) {
    return 0;
  }
  instance->editor_user_closed = false;
  sphere_au_close_editor(instance);
  return 1;
}

}  // extern "C"
