// The live half of a built-in's editor: what the insert measures, as the
// server streams it, with the history and ballistics kept between frames.
// A port of the native editors' `Live` (components/plugin_live.rs).
//
// An insert measures only while an editor shows it: `useLive` tells the
// server when the first editor opens and the last one closes. Readings never
// go through React state; canvases read them on their own animation frame.

import { useEffect } from 'react'
import type { ImageFrame, LevelFrame, TelemetryFrame } from '../protocol.ts'
import { onOpen, onTelemetry, request } from '../store.ts'

/** Frames of level history kept: ten seconds at the telemetry rate. */
export const HISTORY = 300
/** Frames a meter's held peak covers: about 1.2 s. */
export const HOLD_FRAMES = 36
/** The level floor meters and the history draw down to, dBFS. */
export const LEVEL_FLOOR_DB = -48
/** 0 VU on the output scale, dBFS RMS. */
export const VU_REFERENCE_DBFS = -18
/** The VU needle's time constant: 300 ms to 99 %. */
const NEEDLE_TAU_SEC = 0.3 / 4.6
/** Vectorscope frames kept for persistence. */
export const SCOPE_FRAMES = 6
/** Angle bins across the polar level display's half circle. */
export const POLAR_BINS = 61
const POLAR_HOLD = 0.86
/** A reading older than this is stale: the insert stopped (bypassed, or the
 *  device stopped). Meters park. */
const STALE_MS = 500

/** The analyser's scale (builtin_dsp_core::spectrum). */
export const SPECTRUM_FLOOR_DB = -100
export const SPECTRUM_CEIL_DB = 0
export const SPECTRUM_MIN_HZ = 20
export const SPECTRUM_MAX_HZ = 20000

export function toDb(level: number): number {
  return level <= 1e-6 ? -120 : 20 * Math.log10(level)
}

export interface HistoryPoint {
  inDb: number
  outDb: number
  reductionDb: number
  /** Rack position 0's input level. A single-stage built-in may carry its
   *  own reading there: WayGate's key (`waygate::KEY_SLOT`). */
  slotInDb: number
  /** Whether rack position 0's output level is lit: WayGate's detector. */
  slotLit: boolean
}

export class Live {
  /** The newest level frame; null while none is current. */
  private latest: LevelFrame | null = null
  private latestAt = 0
  private history: HistoryPoint[] = Array.from({ length: HISTORY }, () => ({
    inDb: -120,
    outDb: -120,
    reductionDb: 0,
    slotInDb: -120,
    slotLit: false,
  }))
  private write = 0
  /** Points of history filled. */
  count = 0
  needleReduction = 0
  needleOutput = -20
  /** The input spectrum, dB per bin; null until a frame arrives. */
  spectrum: Float32Array | null = null
  bandReduction: number[] | null = null
  /** Recent image frames, oldest first. */
  images: ImageFrame[] = []
  /** The vectorscope's auto-gain. */
  scopeGain = 1
  /** The polar level display's rays, held and falling back. */
  polar = new Float32Array(POLAR_BINS)
  /** WhiteSharp's newest readings block, and when it came. */
  pitch: number[] | null = null
  pitchAt = 0
  /** Bumped on every frame: lets a painter skip work when nothing moved. */
  version = 0
  private lastTake = 0

  /** The current level frame, or null when none arrived lately. */
  get frame(): LevelFrame | null {
    return this.latest && performance.now() - this.latestAt < STALE_MS ? this.latest : null
  }

  take(frame: TelemetryFrame) {
    const now = performance.now()
    const dt = this.lastTake ? Math.min(0.25, (now - this.lastTake) / 1000) : 0.033
    this.lastTake = now
    if (frame.levels) this.push(frame.levels, dt, now)
    if (frame.spectrum) {
      const bins = new Float32Array(frame.spectrum.length)
      frame.spectrum.forEach((q, i) => {
        bins[i] = SPECTRUM_FLOOR_DB + (q / 255) * (SPECTRUM_CEIL_DB - SPECTRUM_FLOOR_DB)
      })
      this.spectrum = bins
    }
    if (frame.band_reduction) this.bandReduction = frame.band_reduction
    if (frame.image) this.takeImage(frame.image)
    if (frame.pitch) {
      this.pitch = frame.pitch
      this.pitchAt = now
    }
    this.version++
  }

  private push(frame: LevelFrame, dt: number, now: number) {
    this.history[this.write] = {
      inDb: toDb(frame.in_peak),
      outDb: toDb(frame.out_peak),
      reductionDb: Math.max(0, frame.gain_reduction_db),
      slotInDb: toDb(frame.slot_in_peak[0] ?? 0),
      slotLit: (frame.slot_out_peak[0] ?? 0) >= 0.5,
    }
    this.write = (this.write + 1) % HISTORY
    this.count = Math.min(HISTORY, this.count + 1)
    const follow = 1 - Math.exp(-dt / NEEDLE_TAU_SEC)
    const output = toDb(frame.out_rms) - VU_REFERENCE_DBFS
    this.needleReduction += (Math.max(0, frame.gain_reduction_db) - this.needleReduction) * follow
    this.needleOutput += (Math.max(-30, output) - this.needleOutput) * follow
    this.latest = frame
    this.latestAt = now
  }

  private takeImage(frame: ImageFrame) {
    if (this.images.length === SCOPE_FRAMES) this.images.shift()
    let peak = 0
    for (const s of frame.scope) peak = Math.max(peak, Math.abs(s))
    const target = peak > 1e-4 ? Math.min(16, 0.82 / peak) : this.scopeGain
    // Quick to back off a loud passage, slow to grow into a quiet one.
    const rate = target < this.scopeGain ? 0.5 : 0.04
    this.scopeGain = Math.min(16, Math.max(1, this.scopeGain + (target - this.scopeGain) * rate))
    const fresh = new Float32Array(POLAR_BINS)
    for (let i = 0; i + 1 < frame.scope.length; i += 2) {
      const [angle, radius] = polarOf(frame.scope[i], frame.scope[i + 1])
      const bin = Math.min(POLAR_BINS - 1, Math.max(0, Math.round((angle / Math.PI + 0.5) * (POLAR_BINS - 1))))
      fresh[bin] = Math.max(fresh[bin], radius)
    }
    for (let i = 0; i < POLAR_BINS; i++) this.polar[i] = Math.max(fresh[i], this.polar[i] * POLAR_HOLD)
    this.images.push(frame)
  }

  /** The newest image frame. */
  get image(): ImageFrame | null {
    return this.images.length ? this.images[this.images.length - 1] : null
  }

  /** The history point `age` frames before the newest. */
  point(age: number): HistoryPoint {
    return this.history[(this.write + HISTORY - 1 - age) % HISTORY]
  }

  /** The highest of a reading over the hold window. */
  held(read: (p: HistoryPoint) => number): number {
    let best = -Infinity
    for (let age = 0; age < Math.min(this.count, HOLD_FRAMES); age++) best = Math.max(best, read(this.point(age)))
    return best
  }
}

/** A left/right pair as a polar point folded into the upper half plane:
 *  `[angle, radius]`, the angle from −π/2 (out of phase) through −π/4 (left
 *  only), 0 (mono) and π/4 (right only) to π/2. */
export function polarOf(left: number, right: number): [number, number] {
  let mid = (left + right) * Math.SQRT1_2
  let side = (right - left) * Math.SQRT1_2
  if (mid < 0) {
    mid = -mid
    side = -side
  }
  return [Math.atan2(side, mid), Math.sqrt(mid * mid + side * side)]
}

// ── Which inserts are watched ───────────────────────────────────────────

const lives = new Map<number, { live: Live; watchers: number }>()

function entryFor(insert: number) {
  let entry = lives.get(insert)
  if (!entry) {
    entry = { live: new Live(), watchers: 0 }
    lives.set(insert, entry)
  }
  return entry
}

onTelemetry((insert, frame) => lives.get(insert)?.live.take(frame))
// A reconnect starts the server's watch list from nothing.
onOpen(() => {
  for (const [insert, entry] of lives) {
    if (entry.watchers > 0) void request({ cmd: 'watch_insert', insert, watch: true })
  }
})

/** The insert's live readings, measured for as long as the calling
 *  component is mounted. */
export function useLive(insert: number): Live {
  const entry = entryFor(insert)
  useEffect(() => {
    // Again here: a remount (StrictMode, a fast close and reopen) may run
    // after the last cleanup dropped the entry.
    const current = entryFor(insert)
    current.watchers++
    if (current.watchers === 1) void request({ cmd: 'watch_insert', insert, watch: true })
    return () => {
      current.watchers--
      // The entry stays (a few kilobytes): a remount finds the object it
      // rendered with.
      if (current.watchers === 0) void request({ cmd: 'watch_insert', insert, watch: false })
    }
  }, [insert])
  return entry.live
}
