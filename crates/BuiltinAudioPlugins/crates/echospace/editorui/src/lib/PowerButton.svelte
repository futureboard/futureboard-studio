<script lang="ts">
  type Props = {
    value: boolean
    onchange: (value: boolean) => void
  }

  const { value, onchange }: Props = $props()
</script>

<button
  type="button"
  class="power"
  class:on={value}
  role="switch"
  aria-checked={value}
  aria-label="Power"
  title={value ? 'On — click to bypass' : 'Bypassed — click to turn on'}
  onclick={() => onchange(!value)}
>
  <svg viewBox="0 0 24 24" aria-hidden="true">
    <path d="M12 4v7.5" />
    <path d="M7.05 7.4a7 7 0 1 0 9.9 0" />
  </svg>
</button>

<style>
  .power {
    display: grid;
    place-items: center;
    flex: none;
    width: 2rem;
    height: 2rem;
    border: 1px solid var(--border-strong);
    border-radius: 50%;
    background: var(--surface);
    color: var(--text-faint);
    cursor: pointer;
    transition:
      color 120ms ease,
      border-color 120ms ease,
      box-shadow 120ms ease;
  }

  .power:hover {
    color: var(--text);
  }

  .power:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--accent-dim);
  }

  .power.on {
    border-color: var(--accent-dim);
    background: var(--accent-fill);
    color: var(--accent-bright);
  }

  svg {
    width: 0.95rem;
    height: 0.95rem;
    fill: none;
    stroke: currentColor;
    stroke-width: 2.4;
    stroke-linecap: round;
  }
</style>
