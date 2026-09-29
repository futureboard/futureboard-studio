<script lang="ts">
  import { connectBridge, postParam, type VerbParams } from './bridge'
  import { PARAMS, modeToWire, type Mode, type ParamId } from './params'
  import {
    DEFAULT_PARAMS,
    FACTORY_PRESETS,
    matchingPresetIndex,
    postAllParams,
  } from './presets'
  import DecayView from './lib/DecayView.svelte'
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
  let params = $state<VerbParams>({ ...DEFAULT_PARAMS })

  let connected = $state(false)
  let preset = $state<number | null>(0)

  $effect(() =>
    connectBridge(
      (next) => {
        params = next
        preset = matchingPresetIndex(next)
      },
      (isConnected) => {
        connected = isConnected
      },
    ),
  )

  function set(id: ParamId, value: number) {
    params[id] = value
    postParam(id, value)
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
</script>

<div class="app" class:bypassed={!params.power}>
  <header class="topbar">
    <div class="brand">
      <img class="logo" src={logo} alt="VerbSpace" />
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
    <section class="panel spaces" aria-label="Space">
      <h2>Space</h2>
      <ModeSelect value={params.mode} onchange={setMode} />
    </section>

    <section class="stage" aria-label="Decay">
      <DecayView {params} />
    </section>

    <section class="panel tail" aria-label="Tail">
      <h2>Tail</h2>
      <div class="tail-knobs">
        <Knob
          spec={PARAMS.decaySec}
          value={params.decaySec}
          onchange={(v) => set('decaySec', v)}
          size="lg"
          alert={params.freeze}
          disabled={params.freeze}
        />
        <Knob
          spec={PARAMS.size}
          value={params.size}
          onchange={(v) => set('size', v)}
          size="lg"
        />
      </div>
    </section>
  </main>

  <section class="rack" aria-label="Controls">
    <div class="group" role="group" aria-label="Early">
      <h2 class="group-title">Early</h2>
      <div class="knobs" style="--count: 2">
        <Knob
          spec={PARAMS.predelayMs}
          value={params.predelayMs}
          onchange={(v) => set('predelayMs', v)}
        />
        <Knob
          spec={PARAMS.diffusion}
          value={params.diffusion}
          onchange={(v) => set('diffusion', v)}
        />
      </div>
    </div>

    <div class="group" role="group" aria-label="Tone">
      <h2 class="group-title">Tone</h2>
      <div class="knobs" style="--count: 4">
        <Knob
          spec={PARAMS.damping}
          value={params.damping}
          onchange={(v) => set('damping', v)}
          disabled={params.freeze}
        />
        <Knob
          spec={PARAMS.bassMult}
          value={params.bassMult}
          onchange={(v) => set('bassMult', v)}
          tone="alt"
          disabled={params.freeze}
        />
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
      </div>
    </div>

    <div class="group" role="group" aria-label="Motion">
      <h2 class="group-title">Motion</h2>
      <div class="knobs" style="--count: 3">
        <Knob
          spec={PARAMS.modDepth}
          value={params.modDepth}
          onchange={(v) => set('modDepth', v)}
        />
        <Knob
          spec={PARAMS.modRateHz}
          value={params.modRateHz}
          onchange={(v) => set('modRateHz', v)}
        />
        <Knob
          spec={PARAMS.width}
          value={params.width}
          onchange={(v) => set('width', v)}
        />
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
    width: clamp(7rem, 11.5vw, 9.25rem);
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

  /* ---- main: space, display, tail -------------------------------------- */

  .main {
    display: grid;
    grid-template-columns:
      minmax(12rem, 14.5rem)
      minmax(0, 1fr)
      minmax(8.5rem, 10.5rem);
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

  .spaces {
    padding-inline: var(--space-2);
    overflow-y: auto;
  }

  .spaces h2 {
    padding-inline: var(--space-1);
  }

  h2 {
    color: var(--text-muted);
    font-size: 0.66rem;
    font-weight: 700;
    letter-spacing: 0.1em;
    text-transform: uppercase;
  }

  .tail-knobs {
    display: flex;
    flex: 1;
    flex-direction: column;
    align-items: center;
    justify-content: space-evenly;
    gap: var(--space-2);
    min-height: 0;
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
      minmax(0, 4fr)
      minmax(0, 3fr)
      minmax(0, 2.4fr);
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
      grid-template-columns: minmax(10rem, 12rem) minmax(0, 1fr);
      grid-template-rows: minmax(0, 1fr) auto;
    }

    .spaces {
      grid-row: 1 / span 2;
    }

    .tail {
      grid-column: 2;
      flex-direction: row;
      align-items: center;
    }

    .tail-knobs {
      flex-direction: row;
    }

    .rack {
      grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
    }

    .group:nth-child(3),
    .group:nth-child(4) {
      border-top: 1px solid var(--border);
    }

    .group:nth-child(3) {
      border-left: 0;
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
  }
</style>
