<script lang="ts">
  import { MODE_DIFFUSION_BIAS, MODE_LINE_SCALE } from '../model'
  import { MODES, MODE_HINTS, MODE_LABELS, type Mode } from '../params'

  type Props = {
    value: Mode
    onchange: (mode: Mode) => void
    disabled?: boolean
  }

  const { value, onchange, disabled = false }: Props = $props()

  /**
   * Each space's glyph is drawn from the tank it builds: the rings scale with
   * `line_scale`, and they close up from dotted to solid as
   * `diffusion_bias` rises — so the icons compare the modes the way the DSP
   * does, not the way a room photograph would.
   */
  function rings(mode: Mode): { r: number; dash: string }[] {
    const scale = MODE_LINE_SCALE[mode]
    const bias = MODE_DIFFUSION_BIAS[mode]
    const dash = bias >= 0.4 ? 'none' : bias >= 0.15 ? '3 1.6' : '1.4 2.2'
    return [13, 9, 5].map((r) => ({ r: Math.max(r * scale, 1.2), dash }))
  }
</script>

<div class="spaces" class:disabled role="radiogroup" aria-label="Reverb mode">
  {#each MODES as mode (mode)}
    <button
      type="button"
      role="radio"
      aria-checked={mode === value}
      class:active={mode === value}
      {disabled}
      onclick={() => onchange(mode)}
    >
      <svg viewBox="0 0 32 32" aria-hidden="true">
        {#each rings(mode) as ring, index (index)}
          <circle
            cx="16"
            cy="16"
            r={ring.r}
            stroke-dasharray={ring.dash}
            stroke-opacity={1 - index * 0.28}
          />
        {/each}
        <circle class="core" cx="16" cy="16" r="1.6" />
      </svg>
      <span class="copy">
        <span class="name">{MODE_LABELS[mode]}</span>
        <span class="hint">{MODE_HINTS[mode]}</span>
      </span>
    </button>
  {/each}
</div>

<style>
  .spaces {
    display: flex;
    flex-direction: column;
    gap: 3px;
    min-height: 0;
  }

  button {
    display: grid;
    grid-template-columns: 2.1rem minmax(0, 1fr);
    align-items: center;
    gap: 0.55rem;
    min-width: 0;
    padding: 0.4rem 0.55rem 0.4rem 0.4rem;
    border: 1px solid transparent;
    border-radius: var(--radius-sm);
    color: var(--text-muted);
    text-align: left;
    cursor: pointer;
  }

  button:hover:not(.active):not(:disabled) {
    color: var(--text);
    background: var(--surface-hi);
  }

  button.active {
    border-color: var(--accent-dim);
    background: var(--accent-fill);
    color: var(--accent-text);
  }

  button:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--accent-dim);
  }

  svg {
    width: 2.1rem;
    height: 2.1rem;
    fill: none;
    stroke: currentColor;
    stroke-width: 1.4;
    stroke-linecap: round;
  }

  .core {
    fill: currentColor;
    stroke: none;
  }

  .copy {
    display: flex;
    flex-direction: column;
    gap: 0.12rem;
    min-width: 0;
  }

  .name {
    color: var(--text);
    font-size: 0.8rem;
    font-weight: 650;
  }

  button:not(.active) .name {
    color: inherit;
  }

  .hint {
    overflow: hidden;
    color: var(--text-faint);
    font-size: 0.63rem;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .spaces.disabled button {
    cursor: default;
    opacity: 0.45;
  }

  @media (max-height: 600px) {
    button {
      padding-block: 0.22rem;
    }

    svg {
      width: 1.6rem;
      height: 1.6rem;
    }

    .hint {
      display: none;
    }
  }
</style>
