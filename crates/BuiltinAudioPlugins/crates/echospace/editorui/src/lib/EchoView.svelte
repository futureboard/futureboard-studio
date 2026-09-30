<script lang="ts">
  import type { EchoParams } from '../bridge'
  import { echoModel, filterMagnitude, tapTimes, type Lane } from '../model'
  import {
    DEFAULT_TEMPO_BPM,
    DIVISION_LABELS,
    MAX_TEMPO_BPM,
    MIN_TEMPO_BPM,
    MODE_LABELS,
    clamp,
  } from '../params'

  type Props = {
    params: EchoParams
    /** Transport tempo the synced tap times are derived from. */
    tempoBpm?: number
  }
  const { params, tempoBpm = DEFAULT_TEMPO_BPM }: Props = $props()

  let canvas: HTMLCanvasElement | undefined = $state()
  let host: HTMLDivElement | undefined = $state()
  let cssWidth = $state(0)
  let cssHeight = $state(0)

  const MAX_WINDOW_SEC = 6
  /** Tone the per-pass dulling is measured at. */
  const TONE_REF_HZ = 3000
  const MAX_DULL = 0.55
  const FLOOR_DB = -60
  const WINDOW_FLOOR_DB = -40
  /** Number echo marks 1..N so the first hits read as a countable sequence. */
  const NUMBERED_PASSES = 4

  const model = $derived(echoModel(params, FLOOR_DB, 64, tempoBpm))
  const times = $derived(tapTimes(params, tempoBpm))

  const windowSec = $derived.by(() => {
    const floor = 10 ** (WINDOW_FLOOR_DB / 20)
    const audible = model.taps.filter((tap) => tap.amplitude >= floor)
    const last = audible.length > 0 ? audible[audible.length - 1]!.time : 0
    return Math.min(MAX_WINDOW_SEC, Math.max(0.25, last * 1.15))
  })

  function cssVar(name: string, fallback: string): string {
    if (!host) return fallback
    const value = getComputedStyle(host).getPropertyValue(name).trim()
    return value || fallback
  }

  function rgb(hex: string): [number, number, number] | null {
    const match = /^#?([0-9a-f]{6})$/i.exec(hex.trim())
    if (!match?.[1]) return null
    const int = parseInt(match[1], 16)
    return [(int >> 16) & 255, (int >> 8) & 255, int & 255]
  }

  function alpha(hex: string, a: number): string {
    const c = rgb(hex)
    return c ? `rgba(${c[0]}, ${c[1]}, ${c[2]}, ${a})` : hex
  }

  /** `from` pulled `t` of the way toward `to`, at alpha `a`. */
  function blend(from: string, to: string, t: number, a: number): string {
    const f = rgb(from)
    const g = rgb(to)
    if (!f || !g) return alpha(from, a)
    const k = Math.min(Math.max(t, 0), 1)
    const mix = (i: number) => Math.round(f[i]! + (g[i]! - f[i]!) * k)
    return `rgba(${mix(0)}, ${mix(1)}, ${mix(2)}, ${a})`
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
    const laneColor: Record<Lane, string> = params.freeze
      ? { left: warn, right: warn }
      : {
          left: cssVar('--accent-bright', '#72b1fa'),
          right: cssVar('--accent-alt-bright', '#bdbdbd'),
        }
    const grid = cssVar('--grid', '#262626')
    const gridStrong = cssVar('--grid-strong', '#333333')
    const faint = cssVar('--text-faint', '#6e6e6e')
    const muted = cssVar('--text-muted', '#a3a3a3')
    const text = cssVar('--text', '#e8e8e8')

    // Two lanes around a centre line: the left channel's repeats rise from it,
    // the right channel's hang below it. Ping-pong reads as a zig-zag between
    // them; stereo as two independent rhythms; mono as a mirror.
    const padL = 30
    const padR = 16
    const padT = 44
    const padB = 28
    const w = Math.max(cssWidth - padL - padR, 1)
    const h = Math.max(cssHeight - padT - padB, 1)
    const GAP = 3
    const mid = padT + h / 2
    const laneH = Math.max(h / 2 - GAP, 1)
    const baseOf = (lane: Lane) => (lane === 'left' ? mid - GAP : mid + GAP)
    const dirOf = (lane: Lane) => (lane === 'left' ? -1 : 1)

    const x = (t: number) => padL + (t / windowSec) * w
    // A full-level bar stops short of the lane's edge so its pass number
    // still fits inside the plot.
    const LABEL_ROOM = 13
    const reachMax = Math.max(laneH - LABEL_ROOM, 1)
    const reach = (amplitude: number) => {
      if (amplitude <= 0) return 0
      const db = 20 * Math.log10(Math.min(amplitude, 1))
      if (db <= FLOOR_DB) return 0
      return reachMax * (1 - db / FLOOR_DB)
    }

    // ---- lanes ----------------------------------------------------------
    ctx.fillStyle = alpha(laneColor.left, 0.03)
    ctx.fillRect(padL, mid - GAP - laneH, w, laneH)
    ctx.fillStyle = alpha(laneColor.right, 0.03)
    ctx.fillRect(padL, mid + GAP, w, laneH)

    // Loudness guides, unlabelled: -20 and -40 dB in each lane.
    ctx.lineWidth = 1
    ctx.strokeStyle = alpha(faint, 0.14)
    ctx.setLineDash([2, 4])
    for (const lane of ['left', 'right'] as const) {
      for (const db of [-20, -40]) {
        const gy = Math.round(baseOf(lane) + dirOf(lane) * reach(10 ** (db / 20))) + 0.5
        ctx.beginPath()
        ctx.moveTo(padL, gy)
        ctx.lineTo(padL + w, gy)
        ctx.stroke()
      }
    }
    ctx.setLineDash([])

    // ---- time grid ------------------------------------------------------
    // Synced lines are note lengths, so they get the beat grid they land on;
    // free lines get a plain time ruler.
    ctx.font = '600 10px system-ui, sans-serif'
    ctx.textAlign = 'center'
    ctx.textBaseline = 'top'
    const rulerY = padT + h + 9
    const vline = (gx: number, color: string) => {
      ctx.strokeStyle = color
      ctx.beginPath()
      ctx.moveTo(gx, padT)
      ctx.lineTo(gx, padT + h)
      ctx.stroke()
    }

    if (params.sync) {
      const bpm = Number.isFinite(tempoBpm)
        ? clamp(tempoBpm, MIN_TEMPO_BPM, MAX_TEMPO_BPM)
        : DEFAULT_TEMPO_BPM
      const beat = 60 / bpm
      const beatPx = (beat / windowSec) * w
      if (beatPx >= 56) {
        for (let n = 1; n * beat * 0.25 < windowSec; n++) {
          if (n % 4 === 0) continue
          vline(Math.round(x(n * beat * 0.25)) + 0.5, alpha(faint, 0.07))
        }
      }
      const every = beatPx < 14 ? 4 : beatPx < 26 ? 2 : 1
      for (let n = 1; n * beat < windowSec; n++) {
        if (n % every !== 0) continue
        const gx = Math.round(x(n * beat)) + 0.5
        vline(gx, n % 4 === 0 ? gridStrong : grid)
        ctx.fillStyle = n % 4 === 0 ? muted : faint
        ctx.fillText(String(n), gx, rulerY)
      }
      ctx.textAlign = 'right'
      ctx.fillStyle = faint
      ctx.fillText('beats', padL + w, rulerY)
    } else {
      const tickSec =
        windowSec <= 0.5 ? 0.1 : windowSec <= 1.5 ? 0.25 : windowSec <= 4 ? 0.5 : 1
      for (let t = tickSec; t < windowSec - tickSec * 0.3; t += tickSec) {
        const gx = Math.round(x(t)) + 0.5
        vline(gx, grid)
        ctx.fillStyle = faint
        ctx.fillText(
          t < 1 ? `${Math.round(t * 1000)} ms` : `${Number(t.toFixed(2))} s`,
          gx,
          rulerY,
        )
      }
    }

    // Centre line.
    ctx.strokeStyle = alpha(faint, 0.45)
    ctx.beginPath()
    ctx.moveTo(padL, Math.round(mid) + 0.5)
    ctx.lineTo(padL + w, Math.round(mid) + 0.5)
    ctx.stroke()

    // Lane names.
    ctx.font = '700 10px system-ui, sans-serif'
    ctx.textAlign = 'center'
    ctx.textBaseline = 'middle'
    ctx.fillStyle = laneColor.left
    ctx.fillText('L', padL / 2, mid - GAP - laneH / 2)
    ctx.fillStyle = laneColor.right
    ctx.fillText('R', padL / 2, mid + GAP + laneH / 2)

    // ---- the dry sound --------------------------------------------------
    const dryX = Math.round(x(0))
    ctx.fillStyle = alpha(text, 0.85)
    ctx.beginPath()
    ctx.roundRect(dryX, mid - GAP - laneH, 3, laneH * 2 + GAP * 2, 1.5)
    ctx.fill()
    ctx.font = '600 10px system-ui, sans-serif'
    ctx.textAlign = 'left'
    ctx.textBaseline = 'top'
    ctx.fillStyle = muted
    ctx.fillText('Dry', dryX, rulerY)

    // ---- repeats --------------------------------------------------------
    const visible = model.taps.filter((tap) => tap.time <= windowSec)
    const byLane: Record<Lane, typeof visible> = { left: [], right: [] }
    for (const tap of visible) byLane[tap.lane].push(tap)

    // Bars thin out as the pattern gets dense, so neighbours never merge.
    // Repeats a few pixels apart are one cluster (drawn side by side below),
    // not a density to size every other bar by.
    const CLUSTER_PX = 6
    let closestPx = Infinity
    for (const lane of ['left', 'right'] as const) {
      const list = byLane[lane]
      for (let i = 1; i < list.length; i++) {
        const gapPx = ((list[i]!.time - list[i - 1]!.time) / windowSec) * w
        if (gapPx >= CLUSTER_PX) closestPx = Math.min(closestPx, gapPx)
      }
    }
    const barW = Number.isFinite(closestPx)
      ? Math.min(Math.max(closestPx * 0.45, 3), 9)
      : 9

    const perPass = filterMagnitude(params, TONE_REF_HZ)
    const dullTo = faint

    // Feedback envelope behind each lane. A repeat's level depends only on
    // its round trip, `gain^(t / spacing)`, so a lane decays at the pace of
    // the slowest line that reaches it: its own line in stereo and mono, and
    // in ping-pong both lines, since every other pass crosses over.
    const spacing: Record<Lane, number> =
      params.mode === 'pingpong'
        ? {
            left: Math.max(times.left, times.right),
            right: Math.max(times.left, times.right),
          }
        : { left: times.left, right: times.right }
    for (const lane of ['left', 'right'] as const) {
      if (byLane[lane].length === 0) continue
      const base = baseOf(lane)
      const dir = dirOf(lane)
      const step = spacing[lane]
      const steps = Math.max(Math.round(w / 3), 2)
      const region = new Path2D()
      const edge = new Path2D()
      region.moveTo(x(0), base)
      for (let i = 0; i <= steps; i++) {
        const t = (i / steps) * windowSec
        const level = params.freeze || step <= 0 ? 1 : model.gain ** (t / step)
        const py = base + dir * reach(level)
        region.lineTo(x(t), py)
        if (i === 0) edge.moveTo(x(t), py)
        else edge.lineTo(x(t), py)
      }
      region.lineTo(x(windowSec), base)
      region.closePath()
      const fill = ctx.createLinearGradient(0, base + dir * laneH, 0, base)
      fill.addColorStop(0, alpha(laneColor[lane], 0.1))
      fill.addColorStop(1, alpha(laneColor[lane], 0.02))
      ctx.fillStyle = fill
      ctx.fill(region)
      ctx.strokeStyle = alpha(laneColor[lane], 0.22)
      ctx.lineWidth = 1
      ctx.setLineDash([3, 3])
      ctx.stroke(edge)
      ctx.setLineDash([])
    }

    const lastX: Record<Lane, number> = { left: -Infinity, right: -Infinity }
    const clusterSize: Record<Lane, number> = { left: 0, right: 0 }
    for (const tap of visible) {
      const base = baseOf(tap.lane)
      const dir = dirOf(tap.lane)
      const length = reach(tap.amplitude)
      if (length <= 0) continue

      // Two lines landing on (nearly) the same instant in the same lane sit
      // side by side instead of hiding one another.
      const at = x(tap.time)
      const peers = at - lastX[tap.lane] < CLUSTER_PX ? clusterSize[tap.lane] : 0
      if (peers === 0) lastX[tap.lane] = at
      clusterSize[tap.lane] = peers + 1
      const gx = lastX[tap.lane] + peers * (barW + 1)

      const dulled = Math.min(1 - perPass ** tap.pass, MAX_DULL)
      const color = laneColor[tap.lane]
      ctx.fillStyle = blend(color, dullTo, dulled, 0.5 + Math.min(tap.amplitude, 1) * 0.45)
      ctx.beginPath()
      if (dir < 0) {
        ctx.roundRect(gx - barW / 2, base - length, barW, length, [barW / 2, barW / 2, 0, 0])
      } else {
        ctx.roundRect(gx - barW / 2, base, barW, length, [0, 0, barW / 2, barW / 2])
      }
      ctx.fill()

      if (tap.pass <= NUMBERED_PASSES && tap.amplitude > 0.08 && peers === 0) {
        ctx.fillStyle = alpha(text, 0.8)
        ctx.font = '650 9px system-ui, sans-serif'
        ctx.textAlign = 'center'
        ctx.textBaseline = dir < 0 ? 'bottom' : 'top'
        ctx.fillText(String(tap.pass), gx, base + dir * (length + 3))
      }
    }
  }

  $effect(() => {
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

  function time(value: number): string {
    return value >= 1 ? `${value.toFixed(2)} s` : `${Math.round(value * 1000)} ms`
  }

  const mono = $derived(params.mode === 'mono')

  /** Note name in front of a synced side's time, so the legend says *why* the
   *  spacing is what it is. Empty while the line runs on free time. */
  function note(division: number): string {
    if (!params.sync) return ''
    const index = Math.min(
      Math.max(Math.round(division), 0),
      DIVISION_LABELS.length - 1,
    )
    return `${DIVISION_LABELS[index]} · `
  }
</script>

<div class="view" bind:this={host} class:frozen={params.freeze}>
  <canvas bind:this={canvas} style="width: {cssWidth}px; height: {cssHeight}px"
  ></canvas>
  <div class="overlay">
    <div class="title">
      <span class="mode">{MODE_LABELS[params.mode]}</span>
      {#if params.freeze}
        <span class="badge">Frozen — repeats hold</span>
      {:else}
        <span class="sub">Echo pattern · modelled from the settings</span>
      {/if}
    </div>
    <div class="legend">
      <div class="chip">
        <span class="swatch left"></span>
        <span class="key">{mono ? 'L + R' : 'L'}</span>
        <span class="val">{note(params.divisionL)}{time(times.left)}</span>
      </div>
      {#if !mono}
        <div class="chip">
          <span class="swatch right"></span>
          <span class="key">R</span>
          <span class="val">{note(params.divisionR)}{time(times.right)}</span>
        </div>
      {/if}
      <div class="chip">
        <span class="key">Repeats</span>
        <span class="val">{params.freeze ? '∞' : model.passes}</span>
      </div>
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
    gap: 0.35rem;
  }

  .chip {
    display: flex;
    align-items: center;
    gap: 0.35rem;
    padding: 0.28rem 0.5rem;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: var(--overlay-scrim);
    white-space: nowrap;
  }

  .swatch {
    width: 0.45rem;
    height: 0.45rem;
    border-radius: 2px;
  }

  .swatch.left {
    background: var(--accent-bright);
  }

  .swatch.right {
    background: var(--accent-alt-bright);
  }

  .view.frozen .swatch {
    background: var(--warn);
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
    font-size: 0.74rem;
    font-weight: 650;
  }
</style>
