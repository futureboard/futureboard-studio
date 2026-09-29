<script lang="ts">
  import { connectBridge, postParam, type EchoParams } from './bridge'
  import {
    DEFAULT_PARAMS,
    FACTORY_PRESETS,
    matchingPresetIndex,
    postAllParams,
  } from './presets'
  import {
    DEFAULT_TEMPO_BPM,
    MODE_HINTS,
    PARAMS,
    modeToWire,
    type Mode,
    type ParamId,
  } from './params'
  import DivisionSelect from './lib/DivisionSelect.svelte'
  import EchoView from './lib/EchoView.svelte'
  import FilterCurve from './lib/FilterCurve.svelte'
  import Knob from './lib/Knob.svelte'
  import ModeSelect from './lib/ModeSelect.svelte'
  import PowerButton from './lib/PowerButton.svelte'
  import PresetControl from './lib/PresetControl.svelte'
  import Toggle from './lib/Toggle.svelte'
  import logo from './assets/logo.svg'

  /**
   * Local view of the parameters. Rust is the authority: this starts at the
   * schema defaults, is replaced wholesale by `selectInstance`, and is only
   * moved locally to keep a drag responsive — every change is posted in the
   * same tick, and the host's value wins on the next selection.
   */
  let params = $state<EchoParams>({ ...DEFAULT_PARAMS })

  let connected = $state(false)
  let preset = $state<number | null>(0)

  /**
   * Transport tempo, republished by the host about once a second. A synced
   * delay time is a note length, so everything that prints milliseconds — the
   * division readouts and the echo picture — is derived from this rather than
   * from an assumed 120 BPM.
   */
  let tempoBpm = $state(DEFAULT_TEMPO_BPM)

  $effect(() =>
    connectBridge(
      (next) => {
        params = next
        preset = matchingPresetIndex(next)
      },
      (isConnected) => {
        connected = isConnected
      },
      (bpm) => {
        tempoBpm = bpm
      },
    ),
  )

  function set(id: ParamId, value: number) {
    params[id] = value
    postParam(id, value)
    preset = null
  }

  /**
   * Edit one side of the delay, carrying the other side with it while Link is
   * on. Rust mirrors the same way on the wire index, so the two agree whether
   * the edit arrives from here or from automation; posting both ids keeps this
   * view honest rather than assuming what the DSP did with the first one.
   */
  function setSide(side: 'L' | 'R', value: number) {
    const id: ParamId = side === 'L' ? 'timeMsL' : 'timeMsR'
    const other: ParamId = side === 'L' ? 'timeMsR' : 'timeMsL'
    set(id, value)
    if (params.link) set(other, value)
  }

  function setDivision(side: 'L' | 'R', value: number) {
    const id = side === 'L' ? 'divisionL' : 'divisionR'
    const other = side === 'L' ? 'divisionR' : 'divisionL'
    params[id] = value
    postParam(id, value)
    if (params.link) {
      params[other] = value
      postParam(other, value)
    }
    preset = null
  }

  function setSync(on: boolean) {
    params.sync = on
    postParam('sync', on ? 1 : 0)
    preset = null
  }

  /**
   * Turning Link on pulls the right side onto the left, the same snap
   * `ipc::apply_link` does — otherwise the lit toggle would sit over two
   * different times until the next edit.
   */
  function setLink(on: boolean) {
    params.link = on
    if (on) {
      params.timeMsR = params.timeMsL
      params.divisionR = params.divisionL
    }
    postParam('link', on ? 1 : 0)
    preset = null
  }

  function setMode(mode: Mode) {
    params.mode = mode
    postParam('mode', modeToWire(mode))
    preset = null
  }

  function setFlag(id: 'power' | 'freeze', value: boolean) {
    params[id] = value
    postParam(id, value ? 1 : 0)
    if (id === 'freeze') preset = null
  }

  function loadPreset(index: number) {
    const wrapped =
      ((index % FACTORY_PRESETS.length) + FACTORY_PRESETS.length) %
      FACTORY_PRESETS.length
    const next = { ...FACTORY_PRESETS[wrapped]!.params }
    params = next
    preset = wrapped
    postAllParams(next)
  }

  // Mono sums the input and reads both rings from the left tap, so the right
  // time and the cross amount have nothing to act on. Disabled rather than
  // hidden: the control still shows the value the DSP will use again on the
  // next mode change.
  const monoCollapsed = $derived(params.mode === 'mono')
</script>

<div class="app" class:bypassed={!params.power}>
  <header class="topbar">
    <div class="brand">
      <img class="logo" src={logo} alt="EchoSpace" />
      <span
        class="status"
        class:connected
        title={connected ? 'Linked to the DSP' : 'Preview — no DSP attached'}
      >
        <span class="status-dot"></span>
        {connected ? 'Live' : 'Preview'}
      </span>
    </div>

    <PresetControl
      {preset}
      names={FACTORY_PRESETS.map((entry) => entry.name)}
      onchange={loadPreset}
      onprevious={() => loadPreset((preset ?? 0) - 1)}
      onnext={() => loadPreset((preset ?? -1) + 1)}
    />

    <div class="actions">
      <Toggle
        label="Freeze"
        tone="warn"
        value={params.freeze}
        onchange={(v) => setFlag('freeze', v)}
      />
      <PowerButton
        value={params.power}
        onchange={(v) => setFlag('power', v)}
      />
    </div>
  </header>

  <main class="main">
    <section class="panel timing" aria-label="Timing">
      <div class="panel-head">
        <h2>Timing</h2>
        <Toggle compact label="Tempo Sync" value={params.sync} onchange={setSync} />
      </div>

      <ModeSelect value={params.mode} onchange={setMode} />
      <p class="hint">{MODE_HINTS[params.mode]}</p>

      <div class="taps">
        <div class="tap left">
          <span class="lane-badge">{monoCollapsed ? 'L + R' : 'L'}</span>
          {#if params.sync}
            <DivisionSelect
              label="Left"
              value={params.divisionL}
              {tempoBpm}
              onchange={(v) => setDivision('L', v)}
            />
          {:else}
            <Knob
              spec={PARAMS.timeMsL}
              value={params.timeMsL}
              onchange={(v) => setSide('L', v)}
              showLabel={false}
              size="lg"
            />
          {/if}
        </div>

        <button
          type="button"
          class="link"
          class:active={params.link && !monoCollapsed}
          role="switch"
          aria-checked={params.link}
          aria-label="Link left and right"
          title={params.link
            ? 'Linked — both sides move together'
            : 'Link both sides'}
          disabled={monoCollapsed}
          onclick={() => setLink(!params.link)}
        >
          <svg viewBox="0 0 24 24" aria-hidden="true">
            {#if params.link}
              <path d="M9.5 14.5 14.5 9.5" />
              <path d="M11 6.5 12.6 4.9a4 4 0 0 1 5.7 5.7L16.7 12.2" />
              <path d="M13 17.5 11.4 19.1a4 4 0 0 1-5.7-5.7L7.3 11.8" />
            {:else}
              <path d="M11 6.5 12.6 4.9a4 4 0 0 1 5.7 5.7L16.7 12.2" />
              <path d="M13 17.5 11.4 19.1a4 4 0 0 1-5.7-5.7L7.3 11.8" />
            {/if}
          </svg>
          <span>Link</span>
        </button>

        <div class="tap right" class:off={monoCollapsed}>
          <span class="lane-badge">R</span>
          {#if params.sync}
            <DivisionSelect
              label="Right"
              value={params.divisionR}
              {tempoBpm}
              onchange={(v) => setDivision('R', v)}
              disabled={monoCollapsed}
            />
          {:else}
            <Knob
              spec={PARAMS.timeMsR}
              value={params.timeMsR}
              onchange={(v) => setSide('R', v)}
              showLabel={false}
              size="lg"
              tone="alt"
              disabled={monoCollapsed}
            />
          {/if}
        </div>
      </div>

      <p class="tempo">
        {#if params.sync}
          Following the transport at <strong>{Math.round(tempoBpm)} BPM</strong>
        {:else}
          Free time — turn on Tempo Sync to lock to note lengths
        {/if}
      </p>
    </section>

    <section class="stage" aria-label="Echo pattern">
      <EchoView {params} {tempoBpm} />
    </section>
  </main>

  <section class="rack" aria-label="Controls">
    <div class="group" role="group" aria-label="Feedback">
      <h2 class="group-title">Feedback</h2>
      <div class="knobs" style="--count: 2">
        <Knob
          spec={PARAMS.feedback}
          value={params.feedback}
          onchange={(v) => set('feedback', v)}
          alert={params.freeze}
          disabled={params.freeze}
        />
        <Knob
          spec={PARAMS.crossFeedback}
          value={params.crossFeedback}
          onchange={(v) => set('crossFeedback', v)}
          disabled={monoCollapsed}
        />
      </div>
    </div>

    <div class="group tone" role="group" aria-label="Repeat tone">
      <h2 class="group-title">Repeat Tone</h2>
      <div class="tone-body">
        <FilterCurve {params} />
        <div class="knobs" style="--count: 3">
          <Knob
            spec={PARAMS.lowCutHz}
            value={params.lowCutHz}
            onchange={(v) => set('lowCutHz', v)}
          />
          <Knob
            spec={PARAMS.highCutHz}
            value={params.highCutHz}
            onchange={(v) => set('highCutHz', v)}
          />
          <Knob
            spec={PARAMS.saturation}
            value={params.saturation}
            onchange={(v) => set('saturation', v)}
          />
        </div>
      </div>
    </div>

    <div class="group output" role="group" aria-label="Output">
      <h2 class="group-title">Output</h2>
      <div class="knobs" style="--count: 2">
        <Knob
          spec={PARAMS.mix}
          value={params.mix}
          onchange={(v) => set('mix', v)}
          size="lg"
        />
        <Knob
          spec={PARAMS.outputDb}
          value={params.outputDb}
          onchange={(v) => set('outputDb', v)}
        />
      </div>
    </div>
  </section>
</div>

<style>
  .app {
    display: grid;
    grid-template-rows: var(--topbar-height) minmax(0, 1fr) auto;
    height: 100%;
    min-height: 0;
    background: var(--panel);
  }

  /* ---- top bar -------------------------------------------------------- */

  .topbar {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto minmax(0, 1fr);
    align-items: center;
    gap: var(--space-3);
    min-width: 0;
    padding: 0 var(--space-3) 0 var(--space-4);
    border-bottom: 1px solid var(--border);
    background: rgba(0, 0, 0, 0.22);
  }

  .brand {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    min-width: 0;
  }

  .logo {
    display: block;
    width: clamp(7.25rem, 12vw, 9.5rem);
    max-width: 100%;
    height: auto;
  }

  .status {
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
    flex: none;
    color: var(--text-faint);
    font-size: 0.6rem;
    font-weight: 700;
    letter-spacing: 0.08em;
    text-transform: uppercase;
  }

  .status-dot {
    width: 0.4rem;
    height: 0.4rem;
    border-radius: 50%;
    background: rgba(255, 255, 255, 0.14);
  }

  .status.connected {
    color: var(--text-muted);
  }

  .status.connected .status-dot {
    background: var(--accent);
  }

  .actions {
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: var(--space-2);
    min-width: 0;
  }

  /* ---- main: timing + display ---------------------------------------- */

  .main {
    display: grid;
    grid-template-columns: minmax(15.5rem, 19.5rem) minmax(0, 1fr);
    gap: var(--space-3);
    min-height: 0;
    padding: var(--space-3) var(--space-3) 0;
  }

  .panel {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    min-width: 0;
    min-height: 0;
    padding: var(--space-3);
    border: 1px solid var(--border);
    border-radius: var(--radius);
    background: var(--surface);
    box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.03);
  }

  .panel-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-2);
  }

  h2 {
    color: var(--text-muted);
    font-size: 0.66rem;
    font-weight: 700;
    letter-spacing: 0.1em;
    text-transform: uppercase;
  }

  .hint {
    min-height: 1.6em;
    color: var(--text-faint);
    font-size: 0.66rem;
    line-height: 1.35;
  }

  .taps {
    display: grid;
    grid-template-columns: minmax(0, 1fr) 2.4rem minmax(0, 1fr);
    align-items: stretch;
    gap: var(--space-1);
    flex: 1;
    min-height: 0;
  }

  .tap {
    --lane: var(--accent-bright);

    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: var(--space-2);
    min-width: 0;
    padding: var(--space-2) var(--space-2) var(--space-3);
    border: 1px solid var(--border);
    border-top: 2px solid color-mix(in srgb, var(--lane) 55%, transparent);
    border-radius: var(--radius-sm);
    background: var(--panel);
  }

  .tap.right {
    --lane: var(--accent-alt-bright);
  }

  .tap.off {
    opacity: 0.55;
  }

  .lane-badge {
    color: var(--lane);
    font-size: 0.7rem;
    font-weight: 800;
    letter-spacing: 0.08em;
  }

  .link {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 0.2rem;
    border: 1px solid transparent;
    border-radius: var(--radius-sm);
    color: var(--text-faint);
    font-size: 0.58rem;
    font-weight: 700;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    cursor: pointer;
  }

  .link svg {
    width: 1.15rem;
    height: 1.15rem;
    fill: none;
    stroke: currentColor;
    stroke-width: 2;
    stroke-linecap: round;
  }

  .link:hover:not(:disabled) {
    color: var(--text);
    background: var(--surface-hi);
  }

  .link.active {
    border-color: var(--accent-dim);
    background: var(--accent-fill);
    color: var(--accent-bright);
  }

  .link:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--accent-dim);
  }

  .link:disabled {
    cursor: default;
    opacity: 0.35;
  }

  .tempo {
    color: var(--text-faint);
    font-size: 0.64rem;
    text-align: center;
  }

  .tempo strong {
    color: var(--text-muted);
    font-weight: 650;
  }

  .stage {
    display: flex;
    min-width: 0;
    min-height: 0;
    overflow: hidden;
    border: 1px solid var(--border);
    border-radius: var(--radius);
    background: var(--stage-bottom);
    box-shadow: inset 0 1px 0 rgba(255, 255, 255, 0.03);
  }

  /* ---- rack ----------------------------------------------------------- */

  .rack {
    display: grid;
    grid-template-columns:
      minmax(0, 2fr)
      minmax(0, 4.6fr)
      minmax(0, 2.3fr);
    margin: var(--space-3);
    overflow: hidden;
    border: 1px solid var(--border);
    border-radius: var(--radius);
    background: var(--surface);
  }

  .group {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    min-width: 0;
    padding: var(--space-2) var(--space-3) var(--space-3);
  }

  .group + .group {
    border-left: 1px solid var(--border);
  }

  .group-title {
    color: var(--text-faint);
  }

  .knobs {
    display: grid;
    grid-template-columns: repeat(var(--count), minmax(0, 1fr));
    justify-items: center;
    align-items: end;
    gap: var(--space-2);
    min-width: 0;
    flex: 1;
  }

  .tone-body {
    display: grid;
    grid-template-columns: minmax(7rem, 1fr) minmax(0, 1.5fr);
    align-items: center;
    gap: var(--space-3);
    flex: 1;
    min-width: 0;
  }

  .output {
    background: var(--surface-hi);
  }

  /* Power off: the header stays live so it can be turned back on; the rest
     reads as parked. */
  .app.bypassed .main,
  .app.bypassed .rack {
    opacity: 0.42;
  }

  @media (max-width: 860px) {
    .main {
      grid-template-columns: minmax(0, 1fr);
      grid-template-rows: auto minmax(9rem, 1fr);
      overflow-y: auto;
    }

    .rack {
      grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
    }

    .group.tone {
      grid-column: 1 / -1;
      grid-row: 2;
      border-left: 0;
      border-top: 1px solid var(--border);
    }
  }

  @media (max-height: 600px) {
    .main {
      gap: var(--space-2);
      padding: var(--space-2) var(--space-2) 0;
    }

    .panel {
      padding: var(--space-2);
    }

    .rack {
      margin: var(--space-2);
    }

    .hint {
      display: none;
    }
  }
</style>
