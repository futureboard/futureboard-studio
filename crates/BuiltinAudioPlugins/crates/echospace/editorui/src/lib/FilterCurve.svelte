<script lang="ts">
  import type { EchoParams } from '../bridge'
  import { filterMagnitude } from '../model'

  type Props = { params: EchoParams }
  const { params }: Props = $props()

  /**
   * The feedback path's tone, the way the repeats hear it: the low and high
   * cut sit inside the loop, so the nth repeat has been through them n times.
   * Each curve is the same `filterMagnitude` the echo display dulls its bars
   * with, raised to the pass count — a model of the settings, not a measured
   * response.
   */
  const PASSES = [1, 2, 4] as const
  const MIN_HZ = 20
  const MAX_HZ = 20_000
  const FLOOR_DB = -30
  const W = 240
  const H = 80
  const STEPS = 96

  const span = Math.log(MAX_HZ / MIN_HZ)
  const xOf = (hz: number) => (Math.log(hz / MIN_HZ) / span) * W
  const yOf = (db: number) => 3 + Math.min(Math.max(db / FLOOR_DB, 0), 1) * (H - 6)

  const curves = $derived(
    PASSES.map((pass) => {
      let d = ''
      for (let i = 0; i <= STEPS; i++) {
        const hz = MIN_HZ * Math.exp((i / STEPS) * span)
        const magnitude = filterMagnitude(params, hz) ** pass
        const db = 20 * Math.log10(Math.max(magnitude, 1e-6))
        d += `${i === 0 ? 'M' : 'L'}${xOf(hz).toFixed(1)} ${yOf(db).toFixed(1)}`
      }
      return { pass, d }
    }),
  )

  const GRID_HZ = [100, 1000, 10_000]
  const lowX = $derived((xOf(params.lowCutHz) / W) * 100)
  const highX = $derived((xOf(params.highCutHz) / W) * 100)
</script>

<figure class="curve" aria-label="Tone of each repeat">
  <div class="plot">
    <svg viewBox="0 0 {W} {H}" preserveAspectRatio="none" aria-hidden="true">
      {#each GRID_HZ as hz (hz)}
        <line class="grid" x1={xOf(hz)} x2={xOf(hz)} y1="0" y2={H} />
      {/each}
      <line class="grid" x1="0" x2={W} y1={yOf(-12)} y2={yOf(-12)} />
      {#each [...curves].reverse() as curve (curve.pass)}
        <path class="pass p{curve.pass}" d={curve.d} />
      {/each}
    </svg>
    <span class="cut" style="left: {lowX}%"></span>
    <span class="cut" style="left: {highX}%"></span>
  </div>
  <figcaption>
    <span class="axis">
      {#each GRID_HZ as hz (hz)}
        <span style="left: {(xOf(hz) / W) * 100}%"
          >{hz >= 1000 ? `${hz / 1000}k` : hz}</span
        >
      {/each}
    </span>
    <span class="key">
      <span class="swatch p1"></span>1st
      <span class="swatch p2"></span>2nd
      <span class="swatch p4"></span>4th repeat
    </span>
  </figcaption>
</figure>

<style>
  .curve {
    display: flex;
    flex-direction: column;
    gap: 0.2rem;
    width: 100%;
    min-width: 0;
    margin: 0;
  }

  .plot {
    position: relative;
    height: 4.2rem;
    overflow: hidden;
    border: 1px solid var(--border);
    border-radius: var(--radius-sm);
    background: linear-gradient(180deg, var(--stage-top), var(--stage-bottom));
  }

  svg {
    display: block;
    width: 100%;
    height: 100%;
  }

  .grid {
    stroke: var(--grid);
    stroke-width: 1;
    vector-effect: non-scaling-stroke;
  }

  .pass {
    fill: none;
    stroke: var(--accent-bright);
    stroke-width: 1.6;
    stroke-linejoin: round;
    vector-effect: non-scaling-stroke;
  }

  .pass.p2 {
    stroke-opacity: 0.55;
  }

  .pass.p4 {
    stroke-opacity: 0.28;
  }

  .cut {
    position: absolute;
    top: 0;
    bottom: 0;
    width: 0;
    border-left: 1px dashed rgba(255, 255, 255, 0.18);
  }

  figcaption {
    display: flex;
    flex-direction: column;
    gap: 0.25rem;
  }

  .axis {
    position: relative;
    height: 0.7rem;
  }

  .axis span {
    position: absolute;
    top: 0;
    transform: translateX(-50%);
    color: var(--text-faint);
    font-size: 0.58rem;
  }

  .key {
    display: flex;
    align-items: center;
    gap: 0.3rem;
    color: var(--text-faint);
    font-size: 0.6rem;
    white-space: nowrap;
  }

  .swatch {
    width: 0.7rem;
    height: 2px;
    margin-left: 0.2rem;
    border-radius: 1px;
    background: var(--accent-bright);
  }

  .swatch.p2 {
    opacity: 0.55;
  }

  .swatch.p4 {
    opacity: 0.28;
  }
</style>
