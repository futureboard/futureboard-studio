import { useEffect, useRef, type RefObject } from 'react'
import type { SpectrumFrame } from './bridge'

/**
 * Host analyser overlay, drawn behind the filter and EQ plots.
 *
 * The frame is the signal arriving at the insert — the host captures it
 * before the DSP runs — so it shows what the curve is being set against
 * rather than the result. Painted on a canvas from a ref in its own animation
 * loop, so ~30 Hz analyser frames never re-render the editor. Bins are
 * log-spaced across the frame's own `minHz..maxHz` and mapped onto the plot's
 * axis, so the two never have to agree on constants.
 */
export function SpectrumOverlay({
  frameRef,
  live,
  minHz,
  maxHz,
}: {
  frameRef: RefObject<SpectrumFrame | null>
  live: boolean
  minHz: number
  maxHz: number
}) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null)
  const liveRef = useRef(live)
  useEffect(() => {
    liveRef.current = live
  }, [live])

  useEffect(() => {
    const canvas = canvasRef.current
    const ctx = canvas?.getContext('2d')
    if (!canvas || !ctx) return
    let width = 0
    let height = 0
    const resize = () => {
      const dpr = Math.min(window.devicePixelRatio || 1, 2)
      const rect = canvas.getBoundingClientRect()
      width = Math.max(1, Math.round(rect.width))
      height = Math.max(1, Math.round(rect.height))
      canvas.width = width * dpr
      canvas.height = height * dpr
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0)
    }
    resize()
    const observer = new ResizeObserver(resize)
    observer.observe(canvas)

    // Rise at once, fall slowly, so the display eases between analyser frames.
    let envelope: Float32Array | null = null
    let raf = 0
    const paint = () => {
      raf = requestAnimationFrame(paint)
      ctx.clearRect(0, 0, width, height)
      const frame = liveRef.current ? frameRef.current : null
      if (!frame || frame.bins.length < 2) {
        envelope = null
        return
      }
      const count = frame.bins.length
      if (!envelope || envelope.length !== count) envelope = new Float32Array(count)
      const logMin = Math.log(minHz)
      const logSpan = Math.log(maxHz) - logMin
      const binLogMin = Math.log(frame.minHz)
      const binLogSpan = Math.log(frame.maxHz) - binLogMin
      ctx.beginPath()
      ctx.moveTo(0, height)
      for (let i = 0; i < count; i++) {
        const level = frame.bins[i]! / 255
        envelope[i] = level > envelope[i]! ? level : envelope[i]! * 0.88 + level * 0.12
        const hz = Math.exp(binLogMin + (binLogSpan * i) / (count - 1))
        ctx.lineTo(((Math.log(hz) - logMin) / logSpan) * width, height - envelope[i]! * height)
      }
      ctx.lineTo(width, height)
      ctx.closePath()
      const fill = ctx.createLinearGradient(0, 0, 0, height)
      fill.addColorStop(0, 'rgba(232, 232, 232, 0.16)')
      fill.addColorStop(1, 'rgba(232, 232, 232, 0.02)')
      ctx.fillStyle = fill
      ctx.fill()
    }
    raf = requestAnimationFrame(paint)
    return () => {
      cancelAnimationFrame(raf)
      observer.disconnect()
    }
  }, [frameRef, minHz, maxHz])

  return <canvas ref={canvasRef} aria-hidden="true" className="pointer-events-none absolute inset-0 h-full w-full" />
}
