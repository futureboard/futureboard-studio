<script lang="ts">
  import type { VerbParams } from '../bridge'
  import { decayModel, diffusionGain, levelDbAt } from '../model'
  import { MODE_LABELS } from '../params'

  type Props = { params: VerbParams }
  const { params }: Props = $props()

  let canvas: HTMLCanvasElement | undefined = $state()
  let host: HTMLDivElement | undefined = $state()
  let cssWidth = $state(0)
  let cssHeight = $state(0)

  /** Floor of the dB axis. Reverb tails are quoted to -60, so is this. */
  const FLOOR_DB = -60

  /** Longest window the axis will show, so a 20 s decay does not squash a
   *  20 ms pre-delay into the first pixel. */
  const MAX_WINDOW_SEC = 8

  const model = $derived(decayModel(params))

  const windowSec = $derived(
    Math.min(
      MAX_WINDOW_SEC,
      Math.max(
        0.35,
        params.predelayMs / 1000 +
          (Number.isFinite(model.longestSec) ? model.longestSec : 2) * 1.08,
      ),
    ),
  )

  function cssVar(name: string, fallback: string): string {
    if (!host) return fallback
    const value = getComputedStyle(host).getPropertyValue(name).trim()
    return value || fallback
  }

  /** `#rrggbb` + alpha -> `rgba(...)`, so one palette drives both the DOM and
   *  the canvas instead of the canvas carrying a second set of literals. */
  function alpha(hex: string, a: number): string {
    const match = /^#?([0-9a-f]{6})$/i.exec(hex.trim())
    if (!match?.[1]) return hex
    const int = parseInt(match[1], 16)
    return `rgba(${(int >> 16) & 255}, ${(int >> 8) & 255}, ${int & 255}, ${a})`
  }

  function draw() {
    if (!canvas || cssWidth <= 0 || cssHeight <= 0) return
    const ctx = canvas.getContext('2d')
    if (!ctx) return

    const dpr = window.devicePixelRatio || 1
    canvas.width = Math.round(cssWidth * dpr)
    canvas.height = Math.round(cssHeight * dpr)
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0)
    ctx.clearRect(0, 0, cssWidth, cssHeight)

    const warn = cssVar('--warn', '#e8b75c')
    const frozen = params.freeze
    const mid = frozen ? warn : cssVar('--accent', '#4d9cf8')
    const midBright = frozen ? warn : cssVar('--accent-bright', '#72b1fa')
    const low = frozen ? warn : cssVar('--accent-alt-bright', '#bdbdbd')
    const high = frozen ? warn : cssVar('--band-high', '#8f8f8f')
    const grid = cssVar('--grid', '#262626')
    const gridStrong = cssVar('--grid-strong', '#333333')
    const faint = cssVar('--text-faint', '#6e6e6e')
    const muted = cssVar('--text-muted', '#a3a3a3')

    const padL = 40
    const padR = 16
    const padT = 50
    const padB = 28
    const w = Math.max(cssWidth - padL - padR, 1)
    const h = Math.max(cssHeight - padT - padB, 1)

    const x = (t: number) => padL + (t / windowSec) * w
    const y = (db: number) =>
      padT + (Math.min(0, Math.max(FLOOR_DB, db)) / FLOOR_DB) * h

    // ---- grid ------------------------------------------------------------
    ctx.font = '600 10px system-ui, sans-serif'
    ctx.textBaseline = 'middle'
    ctx.lineWidth = 1

    for (const db of [0, -12, -24, -36, -48, -60]) {
      const gy = Math.round(y(db)) + 0.5
      ctx.strokeStyle = db === -60 ? gridStrong : grid
      ctx.beginPath()
      ctx.moveTo(padL, gy)
      ctx.lineTo(padL + w, gy)
      ctx.stroke()
      ctx.fillStyle = faint
      ctx.textAlign = 'right'
      ctx.fillText(db === 0 ? '0 dB' : `${db}`, padL - 7, gy)
    }

    const tickSec =
      windowSec <= 0.6 ? 0.1 : windowSec <= 2 ? 0.25 : windowSec <= 5 ? 1 : 2
    ctx.textAlign = 'center'
    ctx.textBaseline = 'top'
    for (let t = tickSec; t < windowSec - tickSec * 0.3; t += tickSec) {
      const gx = Math.round(x(t)) + 0.5
      ctx.strokeStyle = grid
      ctx.beginPath()
      ctx.moveTo(gx, padT)
      ctx.lineTo(gx, padT + h)
      ctx.stroke()
      ctx.fillStyle = faint
      ctx.fillText(
        t < 1 ? `${Math.round(t * 1000)} ms` : `${Number(t.toFixed(2))} s`,
        gx,
        padT + h + 9,
      )
    }

    // ---- pre-delay gap ---------------------------------------------------
    const preSec = params.predelayMs / 1000
    const preX = x(preSec)
    if (preSec > 0) {
      ctx.fillStyle = alpha(faint, 0.08)
      ctx.fillRect(padL, padT, Math.max(preX - padL, 0), h)
      const gx = Math.round(preX) + 0.5
      ctx.strokeStyle = alpha(muted, 0.5)
      ctx.setLineDash([2, 3])
      ctx.beginPath()
      ctx.moveTo(gx, padT)
      ctx.lineTo(gx, padT + h)
      ctx.stroke()
      ctx.setLineDash([])
      if (preX - padL >= 46) {
        ctx.fillStyle = muted
        ctx.textAlign = 'center'
        ctx.textBaseline = 'middle'
        ctx.fillText('Pre-delay', (padL + preX) / 2, padT + h - 12)
      }
    }

    // ---- band envelopes --------------------------------------------------
    // Sampled once per band, then turned into both a stroke and (for the mid
    // band) a closed region. Building the region by appending the stroke path
    // would leave two subpaths, and canvas closes each one on its own — which
    // fills a spurious wedge back to the start point.
    const envelope = (rt60: number): [number, number][] => {
      const points: [number, number][] = []
      const steps = Math.max(Math.round(w / 2), 2)
      for (let i = 0; i <= steps; i++) {
        const t = (i / steps) * windowSec
        if (t < preSec) continue
        const db = levelDbAt(t, preSec, rt60)
        if (db < FLOOR_DB) {
          points.push([x(t), y(FLOOR_DB)])
          break
        }
        points.push([x(t), y(db)])
      }
      return points
    }

    const line = (points: [number, number][]) => {
      const path = new Path2D()
      for (const [index, [px, py]] of points.entries()) {
        if (index === 0) path.moveTo(px, py)
        else path.lineTo(px, py)
      }
      return path
    }

    const midPoints = envelope(model.midSec)

    // Filled mid band first, so the low/high edges read on top of it.
    if (midPoints.length > 1) {
      const first = midPoints[0]!
      const last = midPoints[midPoints.length - 1]!
      const region = new Path2D()
      region.moveTo(first[0], y(FLOOR_DB))
      for (const [px, py] of midPoints) region.lineTo(px, py)
      region.lineTo(last[0], y(FLOOR_DB))
      region.closePath()

      const fill = ctx.createLinearGradient(0, padT, 0, padT + h)
      fill.addColorStop(0, alpha(mid, 0.36))
      fill.addColorStop(0.7, alpha(mid, 0.08))
      fill.addColorStop(1, alpha(mid, 0.02))
      ctx.fillStyle = fill
      ctx.fill(region)
    }

    // ---- early reflections ----------------------------------------------
    // Line arrivals and their first recirculations. Diffusion smears discrete
    // reflections into the tail, so it fades these out as it rises.
    const smear = 1 - diffusionGain(params.mode, params.diffusion) / 0.78
    const tickAlpha = 0.14 + smear * 0.4
    ctx.strokeStyle = alpha(midBright, tickAlpha)
    ctx.lineWidth = 1.25
    for (const delayMs of model.lineDelaysMs) {
      for (let k = 1; k <= 2; k++) {
        const t = preSec + (delayMs * k) / 1000
        if (t > windowSec) break
        const db = levelDbAt(t, preSec, model.midSec)
        if (db < FLOOR_DB) break
        const gx = Math.round(x(t)) + 0.5
        ctx.beginPath()
        ctx.moveTo(gx, padT + h)
        ctx.lineTo(gx, y(db))
        ctx.stroke()
      }
    }

    // ---- band outlines ---------------------------------------------------
    ctx.lineJoin = 'round'
    ctx.lineWidth = 1.5
    ctx.strokeStyle = alpha(low, 0.9)
    ctx.stroke(line(envelope(model.lowSec)))
    ctx.strokeStyle = alpha(high, 0.9)
    ctx.setLineDash([4, 3])
    ctx.stroke(line(envelope(model.highSec)))
    ctx.setLineDash([])

    ctx.lineWidth = 2.25
    ctx.strokeStyle = midBright
    ctx.stroke(line(midPoints))

    // Onset marker where the tail starts.
    ctx.fillStyle = midBright
    ctx.beginPath()
    ctx.arc(preX, y(0), 3.5, 0, Math.PI * 2)
    ctx.fill()
  }

  $effect(() => {
    // Touch the layout inputs so the redraw re-runs when any of them changes;
    // `draw` reads the parameters itself.
    void windowSec
    void cssWidth
    void cssHeight
    draw()
  })

  $effect(() => {
    if (!host) return
    const observer = new ResizeObserver((entries) => {
      const rect = entries[0]?.contentRect
      if (!rect) return
      cssWidth = rect.width
      cssHeight = rect.height
    })
    observer.observe(host)
    return () => observer.disconnect()
  })

  const bands = $derived([
    { key: 'low', label: 'Low', sec: model.lowSec },
    { key: 'mid', label: 'Mid', sec: model.midSec },
    { key: 'high', label: 'High', sec: model.highSec },
  ])

  function seconds(value: number): string {
    if (!Number.isFinite(value)) return '∞'
    return value >= 10 ? value.toFixed(1) : value.toFixed(2)
  }
</script>

<div class="view" bind:this={host} class:frozen={params.freeze}>
  <canvas bind:this={canvas} style="width: {cssWidth}px; height: {cssHeight}px"
  ></canvas>
  <div class="overlay">
    <div class="title">
      <span class="mode">{MODE_LABELS[params.mode]}</span>
      {#if params.freeze}
        <span class="badge">Frozen — tail holds</span>
      {:else}
        <span class="sub">Decay by band · modelled from the settings</span>
      {/if}
    </div>
    <div class="legend" aria-label="Decay time (RT60) per band">
      <span class="legend-title">RT60</span>
      {#each bands as band (band.key)}
        <div class="chip {band.key}">
          <span class="swatch"></span>
          <span class="key">{band.label}</span>
          <span class="val"
            >{seconds(band.sec)}{#if Number.isFinite(band.sec)}<span class="s"
                >s</span
              >{/if}</span
          >
        </div>
      {/each}
    </div>
  </div>
</div>

<style>
  .view {
    position: relative;
    flex: 1;
    min-width: 0;
    min-height: 0;
    background: linear-gradient(180deg, var(--stage-top), var(--stage-bottom));
    overflow: hidden;
  }

  .view.frozen {
    box-shadow: inset 0 0 0 1px var(--warn-dim);
  }

  canvas {
    display: block;
  }

  .overlay {
    position: absolute;
    inset: 0.6rem 0.7rem auto 0.8rem;
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: var(--space-3);
    pointer-events: none;
  }

  .title {
    display: flex;
    align-items: baseline;
    gap: 0.55rem;
    min-width: 0;
    padding-top: 0.3rem;
  }

  .mode {
    color: var(--text);
    font-size: 0.82rem;
    font-weight: 700;
    white-space: nowrap;
  }

  .sub {
    overflow: hidden;
    color: var(--text-faint);
    font-size: 0.64rem;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .badge {
    padding: 0.1rem 0.45rem;
    border: 1px solid var(--warn-dim);
    border-radius: 999px;
    background: var(--warn-fill);
    color: var(--warn);
    font-size: 0.64rem;
    font-weight: 650;
    white-space: nowrap;
  }

  .legend {
    display: flex;
    flex: none;
    align-items: center;
    gap: 0.35rem;
  }

  .legend-title {
    color: var(--text-faint);
    font-size: 0.6rem;
    font-weight: 700;
    letter-spacing: 0.08em;
  }

  .chip {
    --band: var(--accent-bright);

    display: flex;
    align-items: center;
    gap: 0.35rem;
    padding: 0.28rem 0.5rem;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: var(--overlay-scrim);
    white-space: nowrap;
  }

  .chip.low {
    --band: var(--accent-alt-bright);
  }

  .chip.high {
    --band: var(--band-high);
  }

  .view.frozen .chip {
    --band: var(--warn);
  }

  .chip.high .swatch {
    height: 0;
    border-top: 2px dashed var(--band);
    border-radius: 0;
    background: none;
  }

  .swatch {
    width: 0.7rem;
    height: 2px;
    border-radius: 1px;
    background: var(--band);
  }

  .key {
    color: var(--text-faint);
    font-size: 0.6rem;
    font-weight: 700;
    letter-spacing: 0.06em;
    text-transform: uppercase;
  }

  .val {
    color: var(--text);
    font-size: 0.78rem;
    font-weight: 650;
  }

  .s {
    margin-left: 1px;
    color: var(--text-faint);
    font-size: 0.62rem;
  }
</style>
