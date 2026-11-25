/*
 * RodhareistEngine — C ABI over the Rodhareist guitar multi-effect.
 *
 * Link `rodhareist_engine.dll.lib` (Windows), `librodhareist_engine.so` /
 * `.dylib`, or the static library (`rodhareist_engine.lib` /
 * `librodhareist_engine.a`, with RODHAREIST_STATIC defined).
 *
 * THREADS
 *   An engine has an audio side and a control side.
 *   - Audio: rodhareist_engine_process. One thread at a time (a second call
 *     at once returns RODHAREIST_BUSY). It never allocates, locks or blocks.
 *   - Control: parameters, state, presets, NAM captures, impulse responses,
 *     last error. Any thread, concurrently with processing; control calls
 *     serialize among themselves. Their changes reach the DSP at the start
 *     of the next process call, in the order they were made.
 *   - Any thread: tempo, latency, meters, telemetry.
 *   Create and destroy an engine while nothing else uses it.
 *
 * ERRORS
 *   Calls that can fail return a RodhareistStatus; RODHAREIST_OK is 0. A
 *   failed control call says why in rodhareist_engine_last_error. Nothing
 *   unwinds across this boundary.
 *
 * STRINGS
 *   Strings the library returns are UTF-8 and NUL-terminated. Strings it
 *   fills into a caller's buffer follow one rule: `out_len` receives the
 *   length without the NUL; a null or short buffer returns
 *   RODHAREIST_BUFFER_TOO_SMALL, so call once with a null buffer to size it.
 */

#ifndef RODHAREIST_ENGINE_H
#define RODHAREIST_ENGINE_H

#include <stddef.h>
#include <stdint.h>

#if defined(_WIN32) && !defined(RODHAREIST_STATIC)
#define RODHAREIST_API __declspec(dllimport)
#else
#define RODHAREIST_API
#endif

#ifdef __cplusplus
extern "C" {
#endif

/* The ABI this header describes. Compare with rodhareist_abi_version(). */
#define RODHAREIST_ABI_VERSION 1

typedef int32_t RodhareistStatus;
#define RODHAREIST_OK 0
#define RODHAREIST_NULL_POINTER 1
#define RODHAREIST_UNKNOWN_PARAM 2
#define RODHAREIST_INVALID_ARGUMENT 3
#define RODHAREIST_BUFFER_TOO_SMALL 4
#define RODHAREIST_QUEUE_FULL 5
#define RODHAREIST_PARSE_FAILED 6
#define RODHAREIST_LOAD_FAILED 7
#define RODHAREIST_PANIC 8
#define RODHAREIST_NO_SUCH_PRESET 9
#define RODHAREIST_BUSY 10

/* RodhareistParamInfo.flags */
#define RODHAREIST_PARAM_DESCRIBED 1 /* name, unit and range are meaningful */
#define RODHAREIST_PARAM_TOGGLE 2    /* a switch: 0 or 1 */
#define RODHAREIST_PARAM_CHOICE 4    /* one of a list: whole numbers min..max */
#define RODHAREIST_PARAM_ACTION 8    /* writing it acts (clear_clip); no value */

/* The telemetry block: rodhareist_engine_telemetry. */
#define RODHAREIST_TELEMETRY_SLOTS 64
#define RODHAREIST_TELEMETRY_IN_L 0      /* input peak, linear */
#define RODHAREIST_TELEMETRY_IN_R 1
#define RODHAREIST_TELEMETRY_OUT_L 2     /* output peak, linear */
#define RODHAREIST_TELEMETRY_OUT_R 3
#define RODHAREIST_TELEMETRY_IN_RMS_L 4  /* ~300 ms RMS, linear */
#define RODHAREIST_TELEMETRY_IN_RMS_R 5
#define RODHAREIST_TELEMETRY_OUT_RMS_L 6
#define RODHAREIST_TELEMETRY_OUT_RMS_R 7
#define RODHAREIST_TELEMETRY_TUNER_HZ 8      /* 0: no pitch, or tuner off */
#define RODHAREIST_TELEMETRY_TUNER_CLARITY 9 /* 0..1 */

typedef struct RodhareistEngine RodhareistEngine;

/* One wire parameter. The strings live as long as the library is loaded. */
typedef struct RodhareistParamInfo {
    const char *id;
    const char *name;
    const char *unit;
    float min_value;
    float max_value;
    float default_value;
    uint32_t flags;
} RodhareistParamInfo;

/* One factory preset. The strings live as long as the library is loaded. */
typedef struct RodhareistPresetInfo {
    const char *id; /* bank position, e.g. "03B" */
    const char *name;
} RodhareistPresetInfo;

/* Levels after the trims, linear. Clip flags stay set until cleared. */
typedef struct RodhareistMeters {
    float in_peak;
    float in_rms;
    float out_peak;
    float out_rms;
    uint32_t in_clip;
    uint32_t out_clip;
} RodhareistMeters;

typedef struct RodhareistNamInfo {
    double sample_rate;
    uint32_t receptive_field;
    uint32_t submodel_count;
    uint32_t full_rig; /* models amp, cabinet and microphone together */
    uint32_t slimmable;
    char architecture[64];
    char family[64];
} RodhareistNamInfo;

typedef struct RodhareistIrInfo {
    double source_sample_rate;
    uint64_t frames;
    uint32_t latency_samples;
    uint32_t stereo;
    uint32_t truncated; /* longer than the engine keeps, and cut */
} RodhareistIrInfo;

/* ── Library ─────────────────────────────────────────────────────────────── */

RODHAREIST_API uint32_t rodhareist_abi_version(void);
RODHAREIST_API const char *rodhareist_version(void);
RODHAREIST_API const char *rodhareist_status_name(RodhareistStatus status);

/* Wire parameters: indices are stable across versions of the same ABI. */
RODHAREIST_API uint32_t rodhareist_param_count(void);
RODHAREIST_API RodhareistStatus rodhareist_param_info(uint32_t index, RodhareistParamInfo *out);
RODHAREIST_API RodhareistStatus rodhareist_param_index(const char *id, uint32_t *out_index);
/* The cab_model value that plays the loaded impulse response. */
RODHAREIST_API uint32_t rodhareist_cab_model_ir(void);

RODHAREIST_API uint32_t rodhareist_preset_count(void);
RODHAREIST_API RodhareistStatus rodhareist_preset_info(uint32_t index, RodhareistPresetInfo *out);

/* ── Lifecycle ───────────────────────────────────────────────────────────── */

/* A new engine at sample_rate Hz (8000..384000), at the defaults. NULL for a
 * rate out of range. Another rate needs another engine. */
RODHAREIST_API RodhareistEngine *rodhareist_engine_create(double sample_rate);
RODHAREIST_API void rodhareist_engine_destroy(RodhareistEngine *engine);
RODHAREIST_API double rodhareist_engine_sample_rate(const RodhareistEngine *engine);

/* ── Audio side ──────────────────────────────────────────────────────────── */

/* Processes `frames` frames. in_right may be NULL for a mono input (the left
 * feeds both sides); out_right may be NULL to keep only the left. An output
 * may be the same buffer as an input. */
RODHAREIST_API RodhareistStatus rodhareist_engine_process(RodhareistEngine *engine,
                                                          const float *in_left,
                                                          const float *in_right,
                                                          float *out_left,
                                                          float *out_right,
                                                          uint32_t frames);

/* ── Any thread ──────────────────────────────────────────────────────────── */

/* Host tempo for synced delays; applied at the next process call. */
RODHAREIST_API RodhareistStatus rodhareist_engine_set_tempo(RodhareistEngine *engine, double bpm);
/* Delay the engine adds, in samples, as of the last process call. */
RODHAREIST_API uint32_t rodhareist_engine_latency_samples(const RodhareistEngine *engine);
RODHAREIST_API RodhareistStatus rodhareist_engine_meters(const RodhareistEngine *engine,
                                                         RodhareistMeters *out);
/* Copies up to `count` floats of the telemetry block. */
RODHAREIST_API RodhareistStatus rodhareist_engine_telemetry(const RodhareistEngine *engine,
                                                            float *out,
                                                            size_t count);

/* ── Control side ────────────────────────────────────────────────────────── */

/* Values are in the parameter's own units (dB, ms, a model's index…). */
RODHAREIST_API RodhareistStatus rodhareist_engine_set_param(RodhareistEngine *engine,
                                                            uint32_t index,
                                                            float value);
RODHAREIST_API RodhareistStatus rodhareist_engine_set_param_by_id(RodhareistEngine *engine,
                                                                  const char *id,
                                                                  float value);
/* The value the parameter was last set to. */
RODHAREIST_API RodhareistStatus rodhareist_engine_get_param(const RodhareistEngine *engine,
                                                            uint32_t index,
                                                            float *out_value);
/* Clears every tail, meter and clip flag. */
RODHAREIST_API RodhareistStatus rodhareist_engine_reset(RodhareistEngine *engine);

/* The whole state as JSON: parameters, stage path, and the names (not the
 * contents) of the loaded capture and impulse response. */
RODHAREIST_API RodhareistStatus rodhareist_engine_get_state(const RodhareistEngine *engine,
                                                            char *buf,
                                                            size_t capacity,
                                                            size_t *out_len);
/* Restores a state (`len` bytes, no NUL needed). The files it names are not
 * loaded: load them again with load_nam / load_ir. */
RODHAREIST_API RodhareistStatus rodhareist_engine_set_state(RodhareistEngine *engine,
                                                            const char *json,
                                                            size_t len);
/* Power, input trim and the loaded files stay as they are. */
RODHAREIST_API RodhareistStatus rodhareist_engine_load_preset(RodhareistEngine *engine,
                                                              uint32_t index);

/* A .nam capture (`len` bytes of its JSON). Loading does not switch the amp
 * to it: set "tone_engine" to 1 for that. Slow for a large capture. */
RODHAREIST_API RodhareistStatus rodhareist_engine_load_nam(RodhareistEngine *engine,
                                                           const char *json,
                                                           size_t len,
                                                           const char *name,
                                                           uint32_t stereo,
                                                           uint32_t full_rig,
                                                           RodhareistNamInfo *out);
RODHAREIST_API RodhareistStatus rodhareist_engine_unload_nam(RodhareistEngine *engine);
/* An impulse response (`len` bytes of a .wav file). It plays once
 * "cab_model" is set to rodhareist_cab_model_ir(). */
RODHAREIST_API RodhareistStatus rodhareist_engine_load_ir(RodhareistEngine *engine,
                                                          const uint8_t *wav,
                                                          size_t len,
                                                          const char *name,
                                                          RodhareistIrInfo *out);
RODHAREIST_API RodhareistStatus rodhareist_engine_unload_ir(RodhareistEngine *engine);
/* Frees what the audio side has finished with. Call it now and then. */
RODHAREIST_API RodhareistStatus rodhareist_engine_collect_garbage(RodhareistEngine *engine);
/* Why the most recent control call failed; empty when it succeeded. */
RODHAREIST_API RodhareistStatus rodhareist_engine_last_error(const RodhareistEngine *engine,
                                                             char *buf,
                                                             size_t capacity,
                                                             size_t *out_len);

#ifdef __cplusplus
}
#endif

#endif /* RODHAREIST_ENGINE_H */
