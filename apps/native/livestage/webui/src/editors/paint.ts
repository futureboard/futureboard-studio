// Canvas painters shared by the built-in editors: a port of the native
// editors' painters (components/plugin_live.rs, eq_graph.rs). Everything is
// in CSS pixels; `LiveCanvas` scales the context for the screen.

import type { Live } from './live.ts'
import {
  HISTORY,
  LEVEL_FLOOR_DB,
  SPECTRUM_CEIL_DB,
  SPECTRUM_FLOOR_DB,
  SPECTRUM_MAX_HZ,
  SPECTRUM_MIN_HZ,
  toDb,
} from './live.ts'

export type Ctx = CanvasRenderingContext2D

// ── Theme ───────────────────────────────────────────────────────────────

export interface Theme {
  text: string
  textSecondary: string
  textMuted: string
  textFaint: string
  accent: string
  accentHover: string
  meterBg: string
  meterLow: string
  meterMid: string
  meterHigh: string
  canvas: string
  panel: string
  border: string
  font: string
}

let theme: Theme | null = null

/** The page's colour tokens, read once. */
export function colors(): Theme {
  if (theme) return theme
  const s = getComputedStyle(document.documentElement)
  const v = (name: string) => s.getPropertyValue(name).trim()
  theme = {
    text: v('--text-primary'),
    textSecondary: v('--text-secondary'),
    textMuted: v('--text-muted'),
    textFaint: v('--text-faint'),
    accent: v('--accent'),
    accentHover: v('--accent-hover'),
    meterBg: '#00000047',
    meterLow: v('--meter-low'),
    meterMid: v('--meter-mid'),
    meterHigh: v('--meter-high'),
    canvas: v('--surface-canvas'),
    panel: v('--surface-panel'),
    border: v('--border-normal'),
    font: v('--font') || 'system-ui',
  }
  return theme
}

/** `color` (#rgb, #rrggbb or #rrggbbaa) at `alpha` times its own. */
export function alpha(color: string, a: number): string {
  let hex = color.replace('#', '')
  if (hex.length === 3) hex = [...hex].map((c) => c + c).join('')
  const r = parseInt(hex.slice(0, 2), 16)
  const g = parseInt(hex.slice(2, 4), 16)
  const b = parseInt(hex.slice(4, 6), 16)
  const own = hex.length === 8 ? parseInt(hex.slice(6, 8), 16) / 255 : 1
  return `rgba(${r},${g},${b},${(own * a).toFixed(3)})`
}

// ── Primitives ──────────────────────────────────────────────────────────

/** Native caption sizes (theme::typography). */
export const DENSE_CAPTION = 10
export const UI_XS = 11

export type Align = 'left' | 'center' | 'right'

/** One line of text with its top at `y`. */
export function label(ctx: Ctx, text: string, size: number, color: string, x: number, y: number, align: Align = 'left') {
  if (!text) return
  ctx.font = `${size}px ${colors().font}`
  ctx.fillStyle = color
  ctx.textAlign = align
  ctx.textBaseline = 'top'
  ctx.fillText(text, x, y)
}

export function line(ctx: Ctx, points: [number, number][], width: number, color: string) {
  if (points.length < 2) return
  ctx.beginPath()
  ctx.moveTo(points[0][0], points[0][1])
  for (let i = 1; i < points.length; i++) ctx.lineTo(points[i][0], points[i][1])
  ctx.lineWidth = width
  ctx.strokeStyle = color
  ctx.lineJoin = 'round'
  ctx.lineCap = 'round'
  ctx.stroke()
}

/** The area between `points` and the horizontal line at `baseY`. */
export function area(ctx: Ctx, points: [number, number][], baseY: number, color: string) {
  if (points.length < 2) return
  ctx.beginPath()
  ctx.moveTo(points[0][0], baseY)
  for (const [x, y] of points) ctx.lineTo(x, y)
  ctx.lineTo(points[points.length - 1][0], baseY)
  ctx.closePath()
  ctx.fillStyle = color
  ctx.fill()
}

export function dashed(ctx: Ctx, from: [number, number], to: [number, number], width: number, color: string) {
  ctx.save()
  ctx.setLineDash([4, 4])
  line(ctx, [from, to], width, color)
  ctx.restore()
}

export function dot(ctx: Ctx, x: number, y: number, r: number, color: string) {
  ctx.beginPath()
  ctx.arc(x, y, r, 0, Math.PI * 2)
  ctx.fillStyle = color
  ctx.fill()
}

export function rect(ctx: Ctx, x: number, y: number, w: number, h: number, color: string, radius = 0) {
  ctx.fillStyle = color
  if (radius > 0) {
    ctx.beginPath()
    ctx.roundRect(x, y, w, h, radius)
    ctx.fill()
  } else {
    ctx.fillRect(x, y, w, h)
  }
}

export function dbText(db: number): string {
  if (db <= -119) return '−∞'
  if (Math.abs(db) < 0.05) return '0.0'
  return db.toFixed(1)
}

// ── Frequency axis (eq_graph.rs) ────────────────────────────────────────

export const FREQ_MIN = 20
export const FREQ_MAX = 20000

export function freqFraction(hz: number): number {
  const h = Math.min(FREQ_MAX, Math.max(FREQ_MIN, hz))
  return Math.log(h / FREQ_MIN) / Math.log(FREQ_MAX / FREQ_MIN)
}

export function freqAtFraction(f: number): number {
  return FREQ_MIN * Math.pow(FREQ_MAX / FREQ_MIN, Math.min(1, Math.max(0, f)))
}

// ── The history ─────────────────────────────────────────────────────────

export interface Marker {
  db: number
  label: string
  color: string
}

/** The last ten seconds, newest at the right: input peaks as a shaded area,
 *  the reduction hanging from the top, the output peak as a line, and
 *  `markers` across it. */
export function paintHistory(
  ctx: Ctx,
  w: number,
  h: number,
  live: Live,
  markers: Marker[] = [],
  reductionLabel = 'GR',
  bypassed = false,
) {
  const c = colors()
  const GUTTER = 34
  const PAD_Y = 10
  const plotW = Math.max(1, w - GUTTER)
  const plotH = Math.max(1, h - 2 * PAD_Y)
  const yAt = (db: number) => PAD_Y + Math.min(1, Math.max(0, db / LEVEL_FLOOR_DB)) * plotH
  const xAt = (age: number) => plotW - (age * plotW) / (HISTORY - 1)
  for (const db of [0, -6, -12, -18, -24, -36]) {
    const y = yAt(db)
    rect(ctx, 0, y, plotW, 1, alpha(c.text, 0.06))
    label(ctx, db.toFixed(0), DENSE_CAPTION, c.textFaint, plotW + 6, y - 6)
  }
  const a = bypassed ? 0.45 : 1
  if (live.count > 1) {
    const ages = Array.from({ length: live.count }, (_, i) => i)
    const input = ages.map((age): [number, number] => [xAt(age), yAt(live.point(age).inDb)])
    area(ctx, input, PAD_Y + plotH, alpha(c.textMuted, 0.22 * a))
    line(ctx, input, 1, alpha(c.textMuted, 0.7 * a))
    const reduction = ages.map((age): [number, number] => [xAt(age), yAt(-live.point(age).reductionDb)])
    if (reduction.some(([, y]) => y > yAt(0) + 0.5)) {
      area(ctx, reduction, yAt(0), alpha(c.accent, 0.3 * a))
      line(ctx, reduction, 1.25, alpha(c.accent, a))
    }
    const output = ages.map((age): [number, number] => [xAt(age), yAt(live.point(age).outDb)])
    line(ctx, output, 1.25, alpha(c.text, 0.9 * a))
  }
  for (const marker of markers) {
    const y = yAt(marker.db)
    const tagY = y - 14 < 4 ? y + 3 : y - 14
    dashed(ctx, [0, y], [plotW, y], 1, alpha(marker.color, 0.85))
    label(ctx, marker.label, DENSE_CAPTION, marker.color, 8, tagY)
  }
  label(ctx, `In · Out · ${reductionLabel}`, DENSE_CAPTION, c.textFaint, 8, h - 16)
}

// ── Level meters ────────────────────────────────────────────────────────

export type MeterColumn =
  | { kind: 'input' }
  | { kind: 'output' }
  /** The reduction, filling down from the top over 24 dB; its readout
   *  prefixed with `sign`. */
  | { kind: 'reduction'; caption: string; sign: string }

/** Vertical bars side by side, each with its caption, a held-peak tick and
 *  the held value. */
export function paintMeters(ctx: Ctx, w: number, h: number, live: Live, columns: MeterColumn[], bypassed = false) {
  const c = colors()
  const CAPTION_H = 16
  const READOUT_H = 18
  const BAR_W = 12
  const columnW = w / Math.max(1, columns.length)
  const barTop = CAPTION_H
  const barH = Math.max(1, h - CAPTION_H - READOUT_H)
  const has = live.frame !== null && live.count > 0
  columns.forEach((column, i) => {
    const centre = columnW * (i + 0.5)
    const caption = column.kind === 'input' ? 'In' : column.kind === 'output' ? 'Out' : column.caption
    label(ctx, caption, DENSE_CAPTION, c.textMuted, centre, 0, 'center')
    const x = centre - BAR_W / 2
    rect(ctx, x, barTop, BAR_W, barH, c.meterBg)
    let unit: number, hold: number, text: string
    if (column.kind === 'reduction') {
      const read = (p: { reductionDb: number }) => (bypassed ? 0 : p.reductionDb)
      const now = has ? read(live.point(0)) : 0
      const held = has ? live.held(read) : 0
      text = !has ? '—' : held >= 0.05 ? `${column.sign}${held.toFixed(1)}` : '0.0'
      unit = Math.min(1, Math.max(0, now / 24))
      hold = Math.min(1, Math.max(0, held / 24))
      rect(ctx, x, barTop, BAR_W, unit * barH, c.accent)
      if (hold > 0) rect(ctx, x, barTop + hold * barH - 1, BAR_W, 1.5, c.accentHover)
    } else {
      const read = (p: { inDb: number; outDb: number }) => (column.kind === 'input' ? p.inDb : p.outDb)
      const now = has ? read(live.point(0)) : -120
      const held = has ? live.held(read) : -120
      const toUnit = (db: number) => Math.min(1, Math.max(0, (db - LEVEL_FLOOR_DB) / -LEVEL_FLOOR_DB))
      unit = toUnit(now)
      hold = toUnit(held)
      text = has ? dbText(held) : '—'
      const top = barTop + (1 - unit) * barH
      rect(ctx, x, top, BAR_W, barTop + barH - top, unit >= 1 ? c.meterHigh : c.textMuted)
      if (hold > 0) rect(ctx, x, barTop + (1 - hold) * barH, BAR_W, 1.5, c.text)
    }
    label(ctx, text, UI_XS, c.textSecondary, centre, barTop + barH + 3, 'center')
  })
}

// ── The VU meter ────────────────────────────────────────────────────────

export interface VuFace {
  top: string
  bottom: string
  ink: string
  hot: string
  needle: string
}

/** Warm cream paper, brown ink: the optical leveller. */
export const VU_CREAM: VuFace = { top: '#fff7e5', bottom: '#e7d2aa', ink: '#403629', hot: '#a92f1c', needle: '#160f08' }
/** Deep blue, white ink: the FET limiter. */
export const VU_BLUE: VuFace = { top: '#3a72ad', bottom: '#1a4574', ink: '#f2f6fa', hot: '#d8492f', needle: '#f5f2ec' }

export function vuTinted(top: string, bottom: string, ink: string, hot: string): VuFace {
  return { top, bottom, ink, hot, needle: ink }
}

/** What the VU reads: gain reduction (0 dB at the right, `full` dB at the
 *  left), or output level in VU (−20 to +3). */
export type VuScale = { kind: 'reduction'; full: number } | { kind: 'output' }

function vuPosition(scale: VuScale, value: number): number {
  const unit =
    scale.kind === 'reduction'
      ? 1 - Math.pow(Math.min(1, Math.max(0, value / scale.full)), 0.6)
      : Math.min(1, Math.max(0, (value + 20) / 23))
  return unit * 2 - 1
}

/** A moving-coil meter: its face, the scale, and the needle on the live
 *  reading — parked at rest while no reading arrives. */
export function paintVu(ctx: Ctx, w: number, h: number, live: Live, face: VuFace, scale: VuScale, title: string) {
  const SWEEP = 0.7
  const gradient = ctx.createLinearGradient(0, 0, 0, h)
  gradient.addColorStop(0, face.top)
  gradient.addColorStop(1, face.bottom)
  ctx.fillStyle = gradient
  ctx.beginPath()
  ctx.roundRect(0, 0, w, h, 6)
  ctx.fill()
  const radius = Math.min(w * 0.6, h * 0.95)
  const pivot: [number, number] = [w * 0.5, h * 0.2 + radius]
  const at = (position: number, r: number): [number, number] => {
    const angle = position * SWEEP
    return [pivot[0] + r * Math.sin(angle), pivot[1] - r * Math.cos(angle)]
  }
  let ticks: number[], labelled: number[], hotFrom: number | null
  if (scale.kind === 'reduction' && scale.full > 20) {
    ticks = [0, 1.5, 3, 4.5, 6, 9, 12, 18, 24]
    labelled = [0, 3, 6, 12, 24]
    hotFrom = null
  } else if (scale.kind === 'reduction') {
    ticks = [0, 1, 2, 3, 4, 5, 6, 8, 10, 15, 20]
    labelled = [0, 2, 4, 6, 10, 20]
    hotFrom = null
  } else {
    ticks = [-20, -10, -7, -5, -3, -2, -1, 0, 1, 2, 3]
    labelled = [-20, -10, -7, -5, -3, 0, 3]
    hotFrom = 0
  }
  const arc = Array.from({ length: 41 }, (_, i) => at(i / 20 - 1, radius))
  line(ctx, arc, 1.2, alpha(face.ink, 0.8))
  if (hotFrom !== null) {
    const start = vuPosition(scale, hotFrom)
    const hot = Array.from({ length: 13 }, (_, i) => at(start + ((1 - start) * i) / 12, radius + 2))
    line(ctx, hot, 4, face.hot)
  }
  for (const tick of ticks) {
    const position = vuPosition(scale, tick)
    const major = labelled.includes(tick)
    const length = major ? 9 : 5
    const color = hotFrom !== null && tick > hotFrom ? face.hot : face.ink
    line(ctx, [at(position, radius), at(position, radius + length)], major ? 1.4 : 1, color)
    if (major) {
      const [x, y] = at(position, radius + length + 10)
      const text = scale.kind === 'output' && tick > 0 ? `+${tick}` : `${tick}`
      label(ctx, text, DENSE_CAPTION, color, x, y - 7, 'center')
    }
  }
  label(ctx, title, DENSE_CAPTION, alpha(face.ink, 0.85), w * 0.5, pivot[1] - radius * 0.46, 'center')

  const frame = live.frame
  const value = scale.kind === 'reduction' ? live.needleReduction : live.needleOutput
  const position = frame ? vuPosition(scale, value) : vuPosition(scale, scale.kind === 'reduction' ? 0 : -20)
  const needle = frame ? face.needle : alpha(face.needle, 0.45)
  const tip = at(position, radius + 6)
  const below = pivot[1] - (h - 4)
  const base = at(position, Math.max(radius * 0.16, below / Math.cos(position * SWEEP)))
  line(ctx, [base, tip], 1.8, needle)
  if (!frame) {
    label(ctx, 'no signal', DENSE_CAPTION, alpha(face.ink, 0.6), w * 0.5, pivot[1] - radius * 0.46 + 16, 'center')
  }
  if (frame?.out_clip) {
    rect(ctx, w - 42, 8, 34, 15, face.hot, 3)
    label(ctx, 'CLIP', DENSE_CAPTION, '#ffffff', w - 25, 10, 'center')
  }
}

// ── Transfer displays ───────────────────────────────────────────────────

/** The plot a transfer curve sits in, leaving room for axis labels:
 *  `[x0, y0, w, h]`. */
export function transferPlot(w: number, h: number): [number, number, number, number] {
  const [left, right, top, bottom] = [30, 10, 26, 20]
  return [left, top, Math.max(1, w - left - right), Math.max(1, h - top - bottom)]
}

/** The operating point on a transfer curve over `rangeDb` to 0 on both axes:
 *  the input peak against `outputDb(input)`, traced up from the floor.
 *  `inputOffsetDb` is how far the metered input sits above what enters the
 *  plug-in (a limiter that meters after its drive). */
export function paintOperatingPoint(
  ctx: Ctx,
  w: number,
  h: number,
  live: Live,
  rangeDb: number,
  inputOffsetDb: number,
  outputDb: (inDb: number) => number,
) {
  const frame = live.frame
  if (!frame || frame.in_peak <= 1e-5) return
  const [px0, py0, pw, ph] = transferPlot(w, h)
  const clampDb = (db: number) => Math.min(0, Math.max(rangeDb, db))
  const inDb = clampDb(toDb(frame.in_peak) - inputOffsetDb)
  const outDb = clampDb(outputDb(inDb))
  const x = px0 + ((inDb - rangeDb) / -rangeDb) * pw
  const y = py0 + ph - ((outDb - rangeDb) / -rangeDb) * ph
  const c = colors().accentHover
  dashed(ctx, [x, py0 + ph], [x, y], 1, alpha(c, 0.55))
  dashed(ctx, [px0, y], [x, y], 1, alpha(c, 0.55))
  dot(ctx, x, y, 4, c)
}

// ── Spectrum ────────────────────────────────────────────────────────────

/** The insert's input spectrum behind a frequency display, as a soft fill
 *  across the plot `[x0, y0, w, h]`. Nothing until a frame arrives. */
export function paintSpectrum(ctx: Ctx, live: Live, plot: [number, number, number, number]) {
  const bins = live.spectrum
  if (!bins) return
  const [x0, y0, w, h] = plot
  const c = colors()
  const n = bins.length
  const points = Array.from(bins, (db, i): [number, number] => {
    const hz = SPECTRUM_MIN_HZ * Math.pow(SPECTRUM_MAX_HZ / SPECTRUM_MIN_HZ, i / (n - 1))
    const level = Math.min(1, Math.max(0, (db - SPECTRUM_FLOOR_DB) / (SPECTRUM_CEIL_DB - SPECTRUM_FLOOR_DB)))
    return [x0 + freqFraction(hz) * w, y0 + h - level * h]
  })
  area(ctx, points, y0 + h, alpha(c.text, 0.06))
  line(ctx, points, 1, alpha(c.text, 0.16))
}

// ── Bars ────────────────────────────────────────────────────────────────

/** A rack position's two level bars: in above, out below, −48 to 0 dBFS. */
export function paintStageBars(ctx: Ctx, x: number, y: number, w: number, h: number, input: number, output: number) {
  const c = colors()
  const barH = Math.min(3, Math.max(1, (h - 2) / 2))
  const unit = (level: number) => Math.min(1, Math.max(0, (toDb(level) - LEVEL_FLOOR_DB) / -LEVEL_FLOOR_DB))
  for (const [row, level, color] of [
    [0, input, c.textFaint],
    [barH + 2, output, c.accent],
  ] as [number, number, string][]) {
    rect(ctx, x, y + row, w, barH, c.meterBg)
    rect(ctx, x, y + row, w * unit(level), barH, color)
  }
}

/** A horizontal level bar over −60 to 0 dBFS: RMS filled, the peak a tick,
 *  the caption and the peak reading above it. */
export function paintLevelBar(ctx: Ctx, w: number, h: number, caption: string, peak: number | null, rms: number | null) {
  const c = colors()
  const barY = 14
  const barH = Math.min(6, Math.max(3, h - 14))
  rect(ctx, 0, barY, w, barH, c.meterBg)
  const unit = (level: number) => Math.min(1, Math.max(0, (toDb(level) + 60) / 60))
  if (rms !== null) rect(ctx, 0, barY, w * unit(rms), barH, c.textMuted)
  if (peak !== null) rect(ctx, w * unit(peak) - 1, barY, 2, barH, peak >= 1 ? c.meterHigh : c.text)
  label(ctx, caption, DENSE_CAPTION, c.textMuted, 0, 0)
  label(ctx, peak === null ? '—' : dbText(toDb(peak)), DENSE_CAPTION, c.textSecondary, w, 0, 'right')
}

/** A reduction bar growing from the right over 24 dB, its readout over it. */
export function paintReductionBar(ctx: Ctx, w: number, h: number, caption: string, reductionDb: number | null) {
  const c = colors()
  const barY = 14
  const barH = Math.min(6, Math.max(3, h - 14))
  rect(ctx, 0, barY, w, barH, c.meterBg)
  if (reductionDb !== null) {
    const width = w * Math.min(1, Math.max(0, reductionDb / 24))
    rect(ctx, w - width, barY, width, barH, c.accent)
  }
  label(ctx, caption, DENSE_CAPTION, c.textMuted, 0, 0)
  const text = reductionDb === null ? '—' : reductionDb >= 0.05 ? `−${reductionDb.toFixed(1)}` : '0.0'
  label(ctx, text, DENSE_CAPTION, c.textSecondary, w, 0, 'right')
}
