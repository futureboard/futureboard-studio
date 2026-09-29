import { filterMagnitude, type Pad } from '../lib/pads'

const W = 120
const H = 44
const MIN_HZ = 20
const MAX_HZ = 20_000
const TOP_DB = 18
const FLOOR_DB = -30
const STEPS = 64

/// The pad's filter response, so the Cutoff and Resonance knobs have a shape
/// to move. Flat when the filter is off.
export function FilterCurve({ pad }: { pad: Pad }) {
  const span = Math.log(MAX_HZ / MIN_HZ)
  const y = (db: number) => ((TOP_DB - Math.max(FLOOR_DB, Math.min(TOP_DB, db))) / (TOP_DB - FLOOR_DB)) * H
  let d = ''
  for (let i = 0; i <= STEPS; i++) {
    const hz = MIN_HZ * Math.exp((i / STEPS) * span)
    const db = 20 * Math.log10(Math.max(filterMagnitude(pad, hz), 1e-6))
    d += `${i === 0 ? 'M' : 'L'}${((i / STEPS) * W).toFixed(1)} ${y(db).toFixed(1)}`
  }
  return (
    <svg className={`filter-curve ${pad.filterMode === 'off' ? 'is-off' : ''}`} viewBox={`0 0 ${W} ${H}`} aria-hidden="true">
      <line x1={0} x2={W} y1={y(0)} y2={y(0)} className="filter-zero" />
      <path d={d} className="filter-line" />
    </svg>
  )
}
