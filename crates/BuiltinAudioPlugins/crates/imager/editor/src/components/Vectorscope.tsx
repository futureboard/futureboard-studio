import { useEffect, useRef, type RefObject } from 'react'
import type { StereoImageFrame } from '../bridge'

/// Frames kept on screen at once; older ones fade, so movement leaves a short
/// trail rather than a flicker.
const PERSISTENCE = 6
/// The scope scales itself so the loudest recent point sits at this fraction
/// of the radius — a mix at −18 dBFS would otherwise be a dot in the middle.
const TARGET_REACH = 0.82
const MAX_GAIN = 16
/// Per-frame easing of the auto-gain: quick to back off, slow to open up.
const GAIN_DOWN = 0.5
const GAIN_UP = 0.04

export type VectorscopeProps = {
  /// Live handle on the newest frame; written by the bridge at ~30 Hz, read
  /// here in the paint loop so a new frame never re-renders React.
  frameRef: RefObject<StereoImageFrame | null>
}

/// Goniometer of the plugin's output: mid runs up the screen, side across it.
/// A mono signal is a vertical line; a wide one fans out; a signal with one
/// side inverted lies flat. The two diagonals are pure left and pure right.
export function Vectorscope({ frameRef }: VectorscopeProps) {
  const hostRef = useRef<HTMLDivElement>(null)
  const canvasRef = useRef<HTMLCanvasElement>(null)
  const gainRef = useRef<HTMLSpanElement>(null)

  useEffect(() => {
    const host = hostRef.current
    const canvas = canvasRef.current
    if (!host || !canvas) return
    let size = { w: 0, h: 0 }
    const observer = new ResizeObserver((entries) => {
      const rect = entries[0]?.contentRect
      if (rect) size = { w: rect.width, h: rect.height }
      dirty = true
    })
    observer.observe(host)

    const history: number[][] = []
    let gain = 1
    let seen: StereoImageFrame | null = null
    let dirty = true
    let raf = 0

    const tick = () => {
      const frame = frameRef.current
      if (frame && frame !== seen) {
        seen = frame
        history.push(frame.scope)
        if (history.length > PERSISTENCE) history.shift()
        let peak = 0
        for (const value of frame.scope) peak = Math.max(peak, Math.abs(value) / 127)
        const target = peak > 1e-3 ? Math.min(TARGET_REACH / peak, MAX_GAIN) : gain
        gain += (target - gain) * (target < gain ? GAIN_DOWN : GAIN_UP)
        gain = Math.max(1, Math.min(gain, MAX_GAIN))
        if (gainRef.current) gainRef.current.textContent = `×${gain.toFixed(1)}`
        dirty = true
      }
      if (dirty) {
        paint(canvas, size.w, size.h, history, gain)
        dirty = false
      }
      raf = requestAnimationFrame(tick)
    }
    raf = requestAnimationFrame(tick)
    return () => {
      cancelAnimationFrame(raf)
      observer.disconnect()
    }
  }, [frameRef])

  return (
    <div ref={hostRef} className="relative h-full w-full">
      <canvas ref={canvasRef} className="absolute inset-0 block h-full w-full" aria-label="Vectorscope" />
      <span className="cap pointer-events-none absolute top-2 left-2.5">Vectorscope</span>
      <span
        ref={gainRef}
        className="num pointer-events-none absolute top-2 right-2.5 text-[10px] text-ink-4"
        title="The scope scales itself to the signal"
      >
        ×1.0
      </span>
    </div>
  )
}

function paint(canvas: HTMLCanvasElement, w: number, h: number, history: number[][], gain: number) {
  const dpr = window.devicePixelRatio || 1
  const pw = Math.max(Math.round(w * dpr), 1)
  const ph = Math.max(Math.round(h * dpr), 1)
  if (canvas.width !== pw || canvas.height !== ph) {
    canvas.width = pw
    canvas.height = ph
  }
  const ctx = canvas.getContext('2d')
  if (!ctx) return
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0)
  ctx.clearRect(0, 0, w, h)

  const cx = w / 2
  const cy = h / 2 + 6
  const radius = Math.max(Math.min(w, h - 24) / 2 - 10, 10)

  // Guides: the half-disc of possible positions, the mono axis, the side
  // axis, and the two pure-channel diagonals.
  ctx.lineWidth = 1
  ctx.strokeStyle = 'rgba(255, 255, 255, 0.07)'
  ctx.beginPath()
  ctx.arc(cx, cy, radius, 0, Math.PI * 2)
  ctx.stroke()
  ctx.beginPath()
  ctx.arc(cx, cy, radius * 0.5, 0, Math.PI * 2)
  ctx.stroke()
  ctx.strokeStyle = 'rgba(255, 255, 255, 0.12)'
  ctx.beginPath()
  ctx.moveTo(cx, cy - radius)
  ctx.lineTo(cx, cy + radius)
  ctx.moveTo(cx - radius, cy)
  ctx.lineTo(cx + radius, cy)
  const d = radius * Math.SQRT1_2
  ctx.moveTo(cx - d, cy - d)
  ctx.lineTo(cx + d, cy + d)
  ctx.moveTo(cx + d, cy - d)
  ctx.lineTo(cx - d, cy + d)
  ctx.stroke()

  ctx.fillStyle = 'rgba(143, 143, 143, 0.9)'
  ctx.font = '600 9.5px "Mona Sans Variable", system-ui, sans-serif'
  ctx.textAlign = 'center'
  ctx.textBaseline = 'middle'
  ctx.fillText('M', cx, cy - radius - 7)
  ctx.fillText('L', cx - d - 7, cy - d - 7)
  ctx.fillText('R', cx + d + 7, cy - d - 7)
  ctx.fillText('S', cx + radius + 7, cy)

  // Points: newest brightest.
  const scale = (radius * gain) / 127
  history.forEach((scope, age) => {
    const alpha = 0.18 + 0.72 * ((age + 1) / history.length)
    ctx.fillStyle = `rgba(114, 177, 250, ${alpha.toFixed(3)})`
    for (let i = 0; i + 1 < scope.length; i += 2) {
      const left = scope[i]!
      const right = scope[i + 1]!
      const side = (right - left) * Math.SQRT1_2
      const mid = (left + right) * Math.SQRT1_2
      const px = cx + clampReach(side * scale, radius)
      const py = cy - clampReach(mid * scale, radius)
      ctx.fillRect(px - 1, py - 1, 2, 2)
    }
  })
}

function clampReach(value: number, radius: number) {
  return value > radius ? radius : value < -radius ? -radius : value
}
