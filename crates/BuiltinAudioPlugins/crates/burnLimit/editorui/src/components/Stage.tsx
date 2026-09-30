import { useEffect, useRef, type RefObject } from 'react'
import { HISTORY_LENGTH, type MeterHistory } from '../lib/history'
import { clamp } from '../lib/math'

/// The stage's level scale, in dBFS: 0 at the top.
export const STAGE_FLOOR_DB = -48
const GRID_DB = [0, -6, -12, -18, -24, -36]
const GUTTER = 34
const PAD_Y = 10

export type StageMarker = {
  db: number
  label: string
  tone: 'accent' | 'warn' | 'ink'
}

export type StageProps = {
  historyRef: RefObject<MeterHistory>
  /// Horizontal reference lines (threshold, ceiling), redrawn when they move.
  markers: readonly StageMarker[]
  /// Word for the fill hanging from the top — `GR`, `Clip`, `Shape`.
  reductionLabel: string
  /// `false` while bypassed: the history stays, dimmed.
  active: boolean
}

type Palette = Record<'grid' | 'label' | 'input' | 'output' | 'accent' | 'warn' | 'ink', string>

function readPalette(element: Element): Palette {
  const css = getComputedStyle(element)
  const token = (name: string, fallback: string) => css.getPropertyValue(name).trim() || fallback
  return {
    grid: 'rgba(255, 255, 255, 0.06)',
    label: token('--color-ink-4', '#6e6e6e'),
    input: token('--color-ink-3', '#8f8f8f'),
    output: token('--color-ink', '#e8e8e8'),
    accent: token('--color-accent', '#4d9cf8'),
    warn: token('--color-warn', '#e8b75c'),
    ink: token('--color-ink-2', '#bdbdbd'),
  }
}

/**
 * Ten seconds of the insert's own meter frames, newest at the right.
 *
 * The input peak is the grey body, the output peak the bright line, and the
 * reduction hangs from the top on the same dB scale — so a limiter pulling
 * 6 dB reads as a fill reaching the −6 line. Everything drawn is a measured
 * frame from the host; nothing is interpolated between them.
 *
 * Painted on a canvas from the history ring in its own animation loop, and
 * only when a new frame or a new size arrives.
 */
export function Stage({ historyRef, markers, reductionLabel, active }: StageProps) {
  const canvasRef = useRef<HTMLCanvasElement>(null)
  const markersRef = useRef(markers)
  const activeRef = useRef(active)
  const dirty = useRef(true)

  useEffect(() => {
    markersRef.current = markers
    activeRef.current = active
    dirty.current = true
  }, [markers, active])

  useEffect(() => {
    const canvas = canvasRef.current
    const ctx = canvas?.getContext('2d')
    if (!canvas || !ctx) return
    const palette = readPalette(canvas)
    let width = 0
    let height = 0
    const resize = () => {
      const dpr = window.devicePixelRatio || 1
      const rect = canvas.getBoundingClientRect()
      width = Math.max(1, Math.round(rect.width))
      height = Math.max(1, Math.round(rect.height))
      canvas.width = Math.round(width * dpr)
      canvas.height = Math.round(height * dpr)
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0)
      dirty.current = true
    }
    resize()
    const observer = new ResizeObserver(resize)
    observer.observe(canvas)

    let drawnStamp = -1
    let raf = 0
    const paint = () => {
      raf = requestAnimationFrame(paint)
      const history = historyRef.current
      if (!dirty.current && history.stamp === drawnStamp) return
      dirty.current = false
      drawnStamp = history.stamp
      draw(ctx, width, height, history, markersRef.current, reductionLabel, activeRef.current, palette)
    }
    raf = requestAnimationFrame(paint)
    return () => {
      cancelAnimationFrame(raf)
      observer.disconnect()
    }
  }, [historyRef, reductionLabel])

  return <canvas ref={canvasRef} className="stage-canvas" aria-label="Level and reduction history" role="img" />
}

function draw(
  ctx: CanvasRenderingContext2D,
  width: number,
  height: number,
  history: MeterHistory,
  markers: readonly StageMarker[],
  reductionLabel: string,
  active: boolean,
  palette: Palette,
) {
  ctx.clearRect(0, 0, width, height)
  const plotW = Math.max(width - GUTTER, 1)
  const plotH = Math.max(height - PAD_Y * 2, 1)
  const y = (db: number) => PAD_Y + (clamp(db, STAGE_FLOOR_DB, 0) / STAGE_FLOOR_DB) * plotH
  const levelY = (linear: number) => y(linear > 0 ? 20 * Math.log10(linear) : STAGE_FLOOR_DB)
  const step = plotW / (HISTORY_LENGTH - 1)
  const x = (age: number) => plotW - age * step

  // Grid and scale.
  ctx.font = '9.5px "Mona Sans Variable", system-ui, sans-serif'
  ctx.textBaseline = 'middle'
  ctx.textAlign = 'left'
  for (const db of GRID_DB) {
    const gy = Math.round(y(db)) + 0.5
    ctx.strokeStyle = palette.grid
    ctx.lineWidth = 1
    ctx.beginPath()
    ctx.moveTo(0, gy)
    ctx.lineTo(plotW, gy)
    ctx.stroke()
    ctx.fillStyle = palette.label
    ctx.fillText(db === 0 ? '0' : `${db}`, plotW + 8, gy)
  }

  const count = history.count
  ctx.globalAlpha = active ? 1 : 0.45
  if (count > 1) {
    // Input body, from the floor up.
    ctx.beginPath()
    ctx.moveTo(x(count - 1), y(STAGE_FLOOR_DB))
    for (let age = count - 1; age >= 0; age--) {
      ctx.lineTo(x(age), levelY(history.inPeak[history.indexFromNewest(age)]!))
    }
    ctx.lineTo(x(0), y(STAGE_FLOOR_DB))
    ctx.closePath()
    ctx.fillStyle = withAlpha(palette.input, 0.28)
    ctx.fill()

    // Reduction, hanging from the 0 dB line on the same scale.
    ctx.beginPath()
    ctx.moveTo(x(count - 1), y(0))
    for (let age = count - 1; age >= 0; age--) {
      ctx.lineTo(x(age), y(-history.reduction[history.indexFromNewest(age)]!))
    }
    ctx.lineTo(x(0), y(0))
    ctx.closePath()
    ctx.fillStyle = withAlpha(palette.accent, 0.3)
    ctx.fill()
    // The edge only where something is taken off: at rest it would sit on
    // the 0 dB line and read as a reduction that is not happening.
    ctx.beginPath()
    let pen = false
    for (let age = count - 1; age >= 0; age--) {
      const reduction = history.reduction[history.indexFromNewest(age)]!
      if (reduction < 0.05) {
        pen = false
        continue
      }
      const px = x(age)
      const py = y(-reduction)
      if (pen) ctx.lineTo(px, py)
      else ctx.moveTo(px, py)
      pen = true
    }
    ctx.strokeStyle = palette.accent
    ctx.lineWidth = 1.25
    ctx.stroke()

    // Output peak.
    ctx.beginPath()
    for (let age = count - 1; age >= 0; age--) {
      const px = x(age)
      const py = levelY(history.outPeak[history.indexFromNewest(age)]!)
      if (age === count - 1) ctx.moveTo(px, py)
      else ctx.lineTo(px, py)
    }
    ctx.strokeStyle = palette.output
    ctx.lineWidth = 1.25
    ctx.stroke()
  }
  ctx.globalAlpha = 1

  // Reference lines.
  ctx.setLineDash([4, 4])
  for (const marker of markers) {
    const my = Math.round(y(marker.db)) + 0.5
    const colour = palette[marker.tone]
    ctx.strokeStyle = colour
    ctx.lineWidth = 1
    ctx.beginPath()
    ctx.moveTo(0, my)
    ctx.lineTo(plotW, my)
    ctx.stroke()
    ctx.fillStyle = colour
    ctx.textAlign = 'left'
    ctx.fillText(marker.label, 8, my - 8)
  }
  ctx.setLineDash([])

  // Legend.
  ctx.textAlign = 'left'
  ctx.textBaseline = 'bottom'
  ctx.fillStyle = palette.label
  ctx.fillText(`In  ·  Out  ·  ${reductionLabel}`, 8, height - 4)
}

function withAlpha(colour: string, alpha: number) {
  if (colour.startsWith('#') && colour.length === 7) {
    const value = Number.parseInt(colour.slice(1), 16)
    return `rgba(${(value >> 16) & 255}, ${(value >> 8) & 255}, ${value & 255}, ${alpha})`
  }
  return colour
}
