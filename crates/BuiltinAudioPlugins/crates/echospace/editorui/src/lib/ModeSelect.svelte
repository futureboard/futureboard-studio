<script lang="ts">
  import { MODES, MODE_LABELS, type Mode } from '../params'

  type Props = {
    value: Mode
    onchange: (mode: Mode) => void
    disabled?: boolean
  }

  const { value, onchange, disabled = false }: Props = $props()

  /**
   * A small picture of where each mode puts its repeats, in the same terms as
   * the echo display: the upper row is the left lane, the lower row the right,
   * and each dot fades like a decaying repeat. `[x, y, opacity]`.
   */
  const GLYPHS: Record<Mode, [number, number, number][]> = {
    stereo: [
      [4, 4, 1],
      [13, 4, 0.7],
      [22, 4, 0.45],
      [7, 12, 1],
      [19, 12, 0.7],
    ],
    pingpong: [
      [4, 4, 1],
      [10, 12, 0.8],
      [16, 4, 0.62],
      [22, 12, 0.45],
      [28, 4, 0.3],
    ],
    mono: [
      [4, 8, 1],
      [12, 8, 0.72],
      [20, 8, 0.48],
      [28, 8, 0.28],
    ],
  }
</script>

<div class="modes" class:disabled role="radiogroup" aria-label="Delay mode">
  {#each MODES as mode (mode)}
    <button
      type="button"
      role="radio"
      aria-checked={mode === value}
      class:active={mode === value}
      {disabled}
      onclick={() => onchange(mode)}
    >
      <svg viewBox="0 0 32 16" aria-hidden="true">
        {#each GLYPHS[mode] as [cx, cy, opacity], index (index)}
          <circle {cx} {cy} r="2.2" fill-opacity={opacity} />
        {/each}
      </svg>
      <span>{MODE_LABELS[mode]}</span>
    </button>
  {/each}
</div>

<style>
  .modes {
    display: grid;
    grid-template-columns: repeat(3, minmax(0, 1fr));
    gap: 3px;
    padding: 3px;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: var(--inset);
  }

  button {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 0.25rem;
    min-width: 0;
    padding: 0.4rem 0.3rem 0.35rem;
    border: 1px solid transparent;
    border-radius: calc(var(--radius-sm) - 2px);
    color: var(--text-muted);
    font-size: 0.7rem;
    font-weight: 650;
    cursor: pointer;
  }

  button span {
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  svg {
    width: 2rem;
    height: 1rem;
    fill: currentColor;
  }

  button:hover:not(.active):not(:disabled) {
    color: var(--text);
    background: var(--surface);
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

  .modes.disabled button {
    cursor: default;
    opacity: 0.45;
  }
</style>
