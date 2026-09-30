<script lang="ts">
  type Props = {
    label: string
    value: boolean
    onchange: (value: boolean) => void
    /** `warn` marks a hold/override state; `accent` marks ordinary engagement. */
    tone?: 'accent' | 'warn'
    disabled?: boolean
    /** Panel-sized: the top bar has room for the full control, a panel
     *  header does not. */
    compact?: boolean
  }

  const {
    label,
    value,
    onchange,
    tone = 'accent',
    disabled = false,
    compact = false,
  }: Props = $props()
</script>

<button
  type="button"
  class="toggle {tone}"
  class:active={value}
  class:compact
  role="switch"
  aria-checked={value}
  aria-label={label}
  {disabled}
  onclick={() => onchange(!value)}
>
  <span class="led"></span>
  <span class="text">{label}</span>
</button>

<style>
  .toggle {
    --tone: var(--accent);
    --tone-dim: var(--accent-dim);
    --tone-fill: var(--accent-fill);

    display: inline-flex;
    align-items: center;
    gap: 0.45rem;
    min-height: 1.9rem;
    padding: 0 0.8rem 0 0.65rem;
    border: 1px solid var(--border-strong);
    border-radius: 999px;
    background: var(--surface);
    color: var(--text-muted);
    font-size: 0.74rem;
    font-weight: 650;
    letter-spacing: 0.02em;
    white-space: nowrap;
    cursor: pointer;
    transition:
      color 120ms ease,
      border-color 120ms ease,
      background 120ms ease;
  }

  .toggle.warn {
    --tone: var(--warn);
    --tone-dim: var(--warn-dim);
    --tone-fill: var(--warn-fill);
  }

  .toggle.compact {
    gap: 0.35rem;
    min-height: 1.5rem;
    padding: 0 0.6rem 0 0.5rem;
    font-size: 0.66rem;
  }

  .toggle:hover:not(:disabled) {
    color: var(--text);
    border-color: rgba(255, 255, 255, 0.2);
  }

  .toggle:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--tone-dim);
  }

  .toggle:disabled {
    cursor: default;
    opacity: 0.4;
  }

  .led {
    flex: none;
    width: 0.45rem;
    height: 0.45rem;
    border-radius: 50%;
    background: rgba(255, 255, 255, 0.12);
    box-shadow: inset 0 1px 1px rgba(0, 0, 0, 0.6);
  }

  .toggle.compact .led {
    width: 0.38rem;
    height: 0.38rem;
  }

  .toggle.active {
    border-color: var(--tone-dim);
    background: var(--tone-fill);
    color: var(--text);
  }

  .toggle.warn.active {
    color: var(--warn);
  }

  .toggle.active .led {
    background: var(--tone);
    box-shadow: none;
  }
</style>
