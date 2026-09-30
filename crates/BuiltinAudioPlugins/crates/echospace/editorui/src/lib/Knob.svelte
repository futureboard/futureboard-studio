<script lang="ts">
  import {
    clamp,
    format,
    fromNorm,
    toNorm,
    unitFor,
    type ParamSpec,
  } from '../params'

  type Props = {
    spec: ParamSpec
    value: number
    onchange: (value: number) => void
    /** Dial size from the palette: `lg` for the controls a patch is set by,
     *  `sm` where a group has to fit a narrow column. */
    size?: 'sm' | 'md' | 'lg'
    /** Hide the name above the dial where the surrounding card already says
     *  which control this is. The accessible name is kept either way. */
    showLabel?: boolean
    /** `alt` paints the value in the right channel's lane colour. */
    tone?: 'accent' | 'alt'
    alert?: boolean
    disabled?: boolean
  }

  const {
    spec,
    value,
    onchange,
    size = 'md',
    showLabel = true,
    tone = 'accent',
    alert = false,
    disabled = false,
  }: Props = $props()

  const norm = $derived(toNorm(spec, value))
  const uid = `k${Math.random().toString(36).slice(2, 9)}`

  let dragging = $state(false)
  let editing = $state(false)
  let draft = $state('')
  let inputEl: HTMLInputElement | undefined = $state()
  let dragStartY = 0
  let dragStartNorm = 0

  $effect(() => {
    if (editing) {
      inputEl?.focus()
      inputEl?.select()
    }
  })

  /** Pointer travel, in px, for the full range; Shift makes it five times
   *  longer for fine moves. */
  const TRAVEL_PX = 230

  function commit(nextNorm: number) {
    if (disabled) return
    onchange(fromNorm(spec, clamp(nextNorm, 0, 1)))
  }

  function onPointerDown(event: PointerEvent) {
    if (disabled || editing || event.button !== 0) return
    dragging = true
    dragStartY = event.clientY
    dragStartNorm = norm
    ;(event.currentTarget as HTMLElement).setPointerCapture(event.pointerId)
    event.preventDefault()
  }

  function onPointerMove(event: PointerEvent) {
    if (!dragging) return
    const travel = event.shiftKey ? TRAVEL_PX * 5 : TRAVEL_PX
    commit(dragStartNorm + (dragStartY - event.clientY) / travel)
  }

  function onPointerUp(event: PointerEvent) {
    if (!dragging) return
    dragging = false
    ;(event.currentTarget as HTMLElement).releasePointerCapture(event.pointerId)
  }

  function onWheel(event: WheelEvent) {
    if (disabled) return
    event.preventDefault()
    const step = event.shiftKey ? spec.step / 5 : spec.step
    onchange(
      clamp(value + (event.deltaY < 0 ? step : -step), spec.min, spec.max),
    )
  }

  function onKeyDown(event: KeyboardEvent) {
    if (disabled) return
    const step = event.shiftKey ? spec.step / 5 : spec.step
    switch (event.key) {
      case 'ArrowUp':
      case 'ArrowRight':
        onchange(clamp(value + step, spec.min, spec.max))
        break
      case 'ArrowDown':
      case 'ArrowLeft':
        onchange(clamp(value - step, spec.min, spec.max))
        break
      case 'PageUp':
        commit(norm + 0.1)
        break
      case 'PageDown':
        commit(norm - 0.1)
        break
      case 'Home':
        onchange(spec.default)
        break
      default:
        return
    }
    event.preventDefault()
  }

  function startEdit() {
    if (disabled) return
    draft = String(Number(value.toFixed(Math.max(spec.digits, 2))))
    editing = true
  }

  function commitEdit() {
    const parsed = Number(draft.replace(/[^\d.+-]/g, ''))
    if (Number.isFinite(parsed)) {
      onchange(clamp(parsed, spec.min, spec.max))
    }
    editing = false
  }

  // Geometry, in the 100 × 100 viewBox. The sweep runs 270° clockwise from
  // lower left to lower right, so the gap sits at the bottom.
  const START_DEG = -225
  const SWEEP_DEG = 270
  const ARC_R = 43

  function polar(deg: number, radius: number) {
    const rad = (deg * Math.PI) / 180
    return { x: 50 + Math.cos(rad) * radius, y: 50 + Math.sin(rad) * radius }
  }

  function angle(t: number) {
    return START_DEG + SWEEP_DEG * t
  }

  function arc(fromNormValue: number, toNormValue: number, radius: number) {
    const a0 = angle(Math.min(fromNormValue, toNormValue))
    const a1 = angle(Math.max(fromNormValue, toNormValue))
    const p0 = polar(a0, radius)
    const p1 = polar(a1, radius)
    const large = a1 - a0 > 180 ? 1 : 0
    return `M ${p0.x.toFixed(2)} ${p0.y.toFixed(2)} A ${radius} ${radius} 0 ${large} 1 ${p1.x.toFixed(2)} ${p1.y.toFixed(2)}`
  }

  const originNorm = $derived(
    spec.origin === undefined ? 0 : toNorm(spec, spec.origin),
  )
  const pointerInner = $derived(polar(angle(norm), 12))
  const pointerOuter = $derived(polar(angle(norm), 27))
  const head = $derived(polar(angle(norm), ARC_R))
  const defaultDot = $derived(polar(angle(toNorm(spec, spec.default)), 49.5))
</script>

<div
  class="knob {size} {tone}"
  class:disabled
  class:alert
  class:dragging
>
  {#if showLabel}
    <div class="label">{spec.label}</div>
  {/if}
  <div
    class="dial"
    role="slider"
    tabindex={disabled ? -1 : 0}
    aria-label={spec.label}
    aria-valuemin={spec.min}
    aria-valuemax={spec.max}
    aria-valuenow={value}
    aria-valuetext="{format(spec, value)} {unitFor(spec, value)}"
    aria-disabled={disabled}
    title="{spec.label} — drag vertically, Shift for fine, double-click to reset"
    onpointerdown={onPointerDown}
    onpointermove={onPointerMove}
    onpointerup={onPointerUp}
    onpointercancel={onPointerUp}
    onwheel={onWheel}
    onkeydown={onKeyDown}
    ondblclick={() => !disabled && onchange(spec.default)}
  >
    <svg viewBox="0 0 100 100" aria-hidden="true">
      <defs>
        <radialGradient id="{uid}-face" cx=".36" cy=".28" r=".85">
          <stop offset="0" stop-color="var(--knob-face-hi)" />
          <stop offset="1" stop-color="var(--knob-face)" />
        </radialGradient>
        <linearGradient id="{uid}-bevel" x1="0" x2="0" y1="0" y2="1">
          <stop offset="0" stop-color="#fff" stop-opacity=".2" />
          <stop offset=".5" stop-color="#fff" stop-opacity="0" />
          <stop offset="1" stop-color="#000" stop-opacity=".35" />
        </linearGradient>
      </defs>

      <path class="track" d={arc(0, 1, ARC_R)} />
      {#if Math.abs(norm - originNorm) > 0.001}
        <path class="fill" d={arc(originNorm, norm, ARC_R)} />
      {/if}
      <circle class="head" cx={head.x} cy={head.y} r="3.1" />
      <circle class="notch" cx={defaultDot.x} cy={defaultDot.y} r="1.5" />

      <circle class="face" cx="50" cy="50" r="33" fill="url(#{uid}-face)" />
      <circle
        class="bevel"
        cx="50"
        cy="50"
        r="32.3"
        stroke="url(#{uid}-bevel)"
      />
      <line
        class="pointer"
        x1={pointerInner.x}
        y1={pointerInner.y}
        x2={pointerOuter.x}
        y2={pointerOuter.y}
      />
    </svg>
  </div>

  {#if editing}
    <input
      class="input"
      bind:this={inputEl}
      bind:value={draft}
      aria-label="{spec.label} value"
      onblur={commitEdit}
      onkeydown={(event) => {
        if (event.key === 'Enter') commitEdit()
        if (event.key === 'Escape') editing = false
      }}
    />
  {:else}
    <button
      type="button"
      class="readout"
      {disabled}
      title="Click to type a value"
      onclick={startEdit}
    >
      <span class="value">{format(spec, value)}</span>
      <span class="unit">{unitFor(spec, value)}</span>
    </button>
  {/if}
</div>

<style>
  .knob {
    --dial: var(--knob-md);
    --tone: var(--accent);
    --tone-bright: var(--accent-bright);

    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 0.28rem;
    width: 100%;
    max-width: 6rem;
    min-width: 0;
  }

  .knob.sm {
    --dial: var(--knob-sm);
  }

  .knob.lg {
    --dial: var(--knob-lg);
  }

  .knob.alt {
    --tone: var(--accent-alt);
    --tone-bright: var(--accent-alt-bright);
  }

  .knob.alert {
    --tone: var(--warn);
    --tone-bright: var(--warn);
  }

  .label {
    max-width: 100%;
    overflow: hidden;
    color: var(--text-muted);
    font-size: 0.66rem;
    font-weight: 650;
    letter-spacing: 0.04em;
    text-align: center;
    text-overflow: ellipsis;
    text-transform: uppercase;
    white-space: nowrap;
  }

  .knob.lg .label {
    color: var(--text);
    font-size: 0.7rem;
  }

  .dial {
    width: var(--dial);
    height: var(--dial);
    border-radius: 50%;
    cursor: ns-resize;
    touch-action: none;
    outline: none;
  }

  .knob.disabled .dial {
    cursor: default;
    opacity: 0.34;
  }

  svg {
    display: block;
    width: 100%;
    height: 100%;
    overflow: visible;
  }

  .track {
    fill: none;
    stroke: var(--knob-track);
    stroke-width: 5;
    stroke-linecap: round;
  }

  .fill {
    fill: none;
    stroke: var(--tone);
    stroke-width: 5;
    stroke-linecap: round;
    transition: stroke 120ms ease;
  }

  .head {
    fill: var(--tone-bright);
    stroke: var(--base);
    stroke-width: 1.2;
  }

  .notch {
    fill: rgba(255, 255, 255, 0.3);
  }

  .face {
    stroke: var(--knob-edge);
    stroke-width: 1.2;
  }

  .bevel {
    fill: none;
    stroke-width: 1.3;
  }

  .pointer {
    stroke: var(--knob-pointer);
    stroke-width: 3.4;
    stroke-linecap: round;
  }

  .knob:not(.disabled) .dial:hover .fill,
  .knob.dragging .fill {
    stroke: var(--tone-bright);
  }

  .knob:not(.disabled) .dial:hover .face,
  .knob.dragging .face {
    stroke: color-mix(in srgb, var(--tone) 45%, transparent);
  }

  .readout {
    display: flex;
    align-items: baseline;
    justify-content: center;
    gap: 0.18rem;
    max-width: 100%;
    min-height: 1.3rem;
    padding: 0.12rem 0.35rem;
    border: 1px solid transparent;
    border-radius: var(--radius-sm);
    cursor: text;
    white-space: nowrap;
  }

  .readout:hover:not(:disabled) {
    border-color: var(--border);
    background: rgba(255, 255, 255, 0.04);
  }

  .readout:disabled {
    cursor: default;
    opacity: 0.5;
  }

  .value {
    color: var(--text);
    font-size: 0.8rem;
    font-weight: 600;
  }

  .knob.dragging .value {
    color: var(--tone-bright);
  }

  .knob.lg .value {
    font-size: 0.9rem;
  }

  .unit {
    color: var(--text-faint);
    font-size: 0.62rem;
  }

  .input {
    width: 4.4rem;
    min-height: 1.3rem;
    padding: 0.12rem 0.3rem;
    border: 1px solid var(--tone);
    border-radius: var(--radius-sm);
    outline: none;
    background: var(--inset);
    color: var(--text);
    font-size: 0.8rem;
    font-weight: 600;
    text-align: center;
    user-select: text;
  }
</style>
