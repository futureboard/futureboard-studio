<script lang="ts">
  import { DIVISION_LABELS, divisionMs } from '../params'

  type Props = {
    label: string
    /** Index into `DIVISION_LABELS`; the wire value Rust indexes with. */
    value: number
    /** Transport tempo, for the millisecond readout under the note. */
    tempoBpm: number
    onchange: (value: number) => void
    disabled?: boolean
  }

  const { label, value, tempoBpm, onchange, disabled = false }: Props = $props()

  const last = DIVISION_LABELS.length - 1
  const index = $derived(Math.min(Math.max(Math.round(value), 0), last))
  const ms = $derived(divisionMs(index, tempoBpm))

  /**
   * The readout is what the delay line is really running at, tempo included —
   * a note name alone hides the moment a long division clamps against the
   * line's 4 s ceiling.
   */
  const readout = $derived(
    ms >= 1000 ? `${(ms / 1000).toFixed(2)} s` : `${Math.round(ms)} ms`,
  )

  function step(delta: number) {
    const next = index + delta
    if (next < 0 || next > last) return
    onchange(next)
  }

  function onKeyDown(event: KeyboardEvent) {
    if (event.key === 'ArrowUp' || event.key === 'ArrowRight') {
      event.preventDefault()
      step(1)
    } else if (event.key === 'ArrowDown' || event.key === 'ArrowLeft') {
      event.preventDefault()
      step(-1)
    }
  }

  function onWheel(event: WheelEvent) {
    if (disabled) return
    event.preventDefault()
    step(event.deltaY < 0 ? 1 : -1)
  }
</script>

<div class="division" class:disabled>
  <div class="row" onwheel={onWheel}>
    <button
      type="button"
      class="step"
      aria-label="Shorter {label} division"
      disabled={disabled || index === 0}
      onclick={() => step(-1)}
    >
      <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M10 3.5 5.5 8l4.5 4.5" /></svg>
    </button>
    <label class="select">
      <span class="note">{DIVISION_LABELS[index]}</span>
      <select
        aria-label="{label} division"
        {disabled}
        value={index}
        onkeydown={onKeyDown}
        onchange={(event) =>
          onchange(Number((event.currentTarget as HTMLSelectElement).value))}
      >
        {#each DIVISION_LABELS as name, option (name)}
          <option value={option}>{name}</option>
        {/each}
      </select>
    </label>
    <button
      type="button"
      class="step"
      aria-label="Longer {label} division"
      disabled={disabled || index === last}
      onclick={() => step(1)}
    >
      <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M6 3.5 10.5 8 6 12.5" /></svg>
    </button>
  </div>
  <div class="readout">{readout}</div>
</div>

<style>
  .division {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 0.35rem;
    width: 100%;
    min-width: 0;
  }

  .division.disabled {
    opacity: 0.4;
  }

  .row {
    display: grid;
    grid-template-columns: 1.4rem minmax(0, 1fr) 1.4rem;
    align-items: stretch;
    width: 100%;
    height: 2.6rem;
    overflow: hidden;
    border: 1px solid var(--border-strong);
    border-radius: var(--radius-sm);
    background: var(--inset);
    box-shadow: inset 0 1px 2px rgba(0, 0, 0, 0.5);
  }

  .step {
    display: grid;
    place-items: center;
    color: var(--text-muted);
    cursor: pointer;
  }

  .step svg {
    width: 0.75rem;
    height: 0.75rem;
    fill: none;
    stroke: currentColor;
    stroke-width: 1.8;
    stroke-linecap: round;
    stroke-linejoin: round;
  }

  .step:hover:not(:disabled) {
    color: var(--text);
    background: var(--surface-hi);
  }

  .step:disabled {
    cursor: default;
    opacity: 0.3;
  }

  .select {
    position: relative;
    display: grid;
    place-items: center;
    min-width: 0;
  }

  .note {
    overflow: hidden;
    width: 100%;
    color: var(--lane, var(--accent-bright));
    font-size: 1.05rem;
    font-weight: 700;
    text-align: center;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  /* The real control sits invisibly on top so the native picker (and its
     keyboard handling) does the work, with our own face drawn underneath —
     the same trick `PresetControl` uses. */
  .select select {
    position: absolute;
    inset: 0;
    width: 100%;
    border: 0;
    opacity: 0;
    cursor: pointer;
    background: var(--surface);
    color: var(--text);
  }

  .select select:disabled {
    cursor: default;
  }

  .readout {
    color: var(--text-muted);
    font-size: 0.72rem;
    font-weight: 600;
  }
</style>
