import { useEffect, useRef, useState, type RefObject } from 'react'
import type { SpectrumFrame } from '../bridge'
import {
  createSpectrum2dRenderer,
  createSpectrumRenderer,
  type SpectrumRenderer,
} from '../lib/spectrumGl'

export type SpectrumLayerProps = {
  /// Live handle on the newest frame, written by the bridge listener. A ref and
  /// not a value: frames arrive at ~30 Hz, and passing them as props would
  /// rerender the whole graph — every curve, node and label — to repaint one
  /// canvas that can read the value itself.
  frameRef: RefObject<SpectrumFrame | null>
  /// Hidden while the unit is bypassed — the graph greys out with it, and a
  /// live analyser under a dead curve reads as if the plugin were still working.
  visible: boolean
}

/// The analyser overlay: a WebGL canvas sitting under the response SVG, or a
/// Canvas 2D one where this runtime cannot create WebGL.
///
/// The redraw is driven by rAF, so the browser parks it entirely while the
/// editor window is hidden rather than burning frames nobody sees.
///
/// The loop polls but only *draws* when the frame it would draw is not the one
/// already on the canvas. Issuing GL commands is what marks the canvas dirty,
/// and a dirty canvas makes Chromium composite the whole editor surface and
/// hand the host a new shared texture. Drawing unconditionally therefore pinned
/// the browser at the display's frame rate forever — including while idle, and
/// including the ~half of frames where no new analyser data had arrived at all.
export function SpectrumLayer({ frameRef, visible }: SpectrumLayerProps) {
  const canvasRef = useRef<HTMLCanvasElement>(null)
  const rendererRef = useRef<SpectrumRenderer | null>(null)
  const visibleRef = useRef(visible)
  /// Which backend draws. Starts on WebGL; drops to 2D for good if WebGL
  /// cannot be created. The canvas is keyed on this, so the fallback gets a
  /// fresh element — one that tried WebGL may refuse a 2D context.
  const [backend, setBackend] = useState<'webgl' | '2d'>('webgl')

  /// Mirrored into a ref so the rAF loop can read it without the effect below
  /// re-running — rebuilding the GL context to flip a boolean would drop the
  /// context and reallocate the drawing buffer on every toggle.
  useEffect(() => {
    visibleRef.current = visible
  }, [visible])

  useEffect(() => {
    const canvas = canvasRef.current
    if (!canvas) return
    const renderer =
      backend === 'webgl' ? createSpectrumRenderer(canvas) : createSpectrum2dRenderer(canvas)
    if (!renderer) {
      // No WebGL here: redraw the same measured data with Canvas 2D.
      if (backend === 'webgl') {
        console.warn('[equz8] WebGL unavailable; analyser falls back to Canvas 2D')
        setBackend('2d')
      }
      return
    }
    rendererRef.current = renderer

    // `undefined` is "nothing drawn yet", which is distinct from the `null`
    // that means "drawn, and deliberately blank". The first tick always draws.
    let drawn: SpectrumFrame | null | undefined = undefined

    const observer = new ResizeObserver(() => {
      renderer.resize()
      // Resizing a canvas clears its backing store, so the frame that was on
      // screen is gone even though the ref still points at it. Forget it, or
      // the skip below would leave the overlay blank until new data arrives.
      drawn = undefined
    })
    observer.observe(canvas)

    let raf = 0
    const tick = () => {
      // The bridge replaces this ref with a fresh object per analyser frame, so
      // identity is a sound "is there anything new" test.
      const next = visibleRef.current ? frameRef.current : null
      if (next !== drawn) {
        renderer.draw(next)
        drawn = next
      }
      raf = requestAnimationFrame(tick)
    }
    raf = requestAnimationFrame(tick)

    return () => {
      cancelAnimationFrame(raf)
      observer.disconnect()
      renderer.dispose()
      rendererRef.current = null
    }
    // `frameRef` is a stable ref object owned by the editor root, so this
    // effect runs once per backend: the context is built on mount (or on the
    // one switch to 2D) and torn down on unmount, never in between.
  }, [frameRef, backend])

  return <canvas key={backend} ref={canvasRef} className="spectrum" aria-hidden="true" />
}
