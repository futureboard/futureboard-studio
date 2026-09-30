<script lang="ts">
  type Props = {
    preset: number | null
    names: string[]
    onchange: (index: number) => void
    onprevious: () => void
    onnext: () => void
  }

  const { preset, names, onchange, onprevious, onnext }: Props = $props()
</script>

<div class="preset">
  <button
    type="button"
    class="step"
    aria-label="Previous preset"
    onclick={onprevious}
  >
    <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M10 3.5 5.5 8l4.5 4.5" /></svg>
  </button>
  <label class="select">
    <span class="caption">Preset</span>
    <span class="name" class:custom={preset === null}>
      {preset === null ? 'Custom' : names[preset]}
    </span>
    <svg class="chevron" viewBox="0 0 16 16" aria-hidden="true"
      ><path d="M4.5 6.5 8 10l3.5-3.5" /></svg
    >
    <select
      aria-label="Preset"
      value={preset ?? 'custom'}
      onchange={(event) => {
        const value = (event.currentTarget as HTMLSelectElement).value
        if (value !== 'custom') onchange(Number(value))
      }}
    >
      {#if preset === null}
        <option value="custom">Custom</option>
      {/if}
      {#each names as name, index (name)}
        <option value={index}>{name}</option>
      {/each}
    </select>
  </label>
  <button type="button" class="step" aria-label="Next preset" onclick={onnext}>
    <svg viewBox="0 0 16 16" aria-hidden="true"><path d="M6 3.5 10.5 8 6 12.5" /></svg>
  </button>
</div>

<style>
  .preset {
    display: grid;
    grid-template-columns: 2rem minmax(0, 1fr) 2rem;
    align-items: stretch;
    width: min(100%, 17rem);
    height: 2.3rem;
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

  .step:hover {
    color: var(--text);
    background: var(--surface-hi);
  }

  .step svg,
  .chevron {
    width: 0.8rem;
    height: 0.8rem;
    fill: none;
    stroke: currentColor;
    stroke-width: 1.8;
    stroke-linecap: round;
    stroke-linejoin: round;
  }

  .select {
    position: relative;
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    grid-template-rows: auto auto;
    align-content: center;
    column-gap: 0.4rem;
    min-width: 0;
    padding: 0 0.6rem;
    border-inline: 1px solid var(--border);
  }

  .select:hover {
    background: rgba(255, 255, 255, 0.025);
  }

  .caption {
    grid-column: 1;
    color: var(--text-faint);
    font-size: 0.55rem;
    font-weight: 650;
    letter-spacing: 0.1em;
    text-transform: uppercase;
  }

  .name {
    grid-column: 1;
    overflow: hidden;
    color: var(--text);
    font-size: 0.78rem;
    font-weight: 650;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .name.custom {
    color: var(--accent-text);
  }

  .chevron {
    grid-column: 2;
    grid-row: 1 / span 2;
    align-self: center;
    color: var(--text-faint);
  }

  /* The real control sits invisibly on top so the native picker (and its
     keyboard handling) does the work, with our own face drawn underneath. */
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
</style>
