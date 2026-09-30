const FLOOR_DB = -60

function toUnit(linear: number) {
  if (!(linear > 0)) return 0
  const db = 20 * Math.log10(linear)
  return Math.max(0, Math.min(1, (db - FLOOR_DB) / -FLOOR_DB))
}

/// One horizontal level bar: RMS filled, peak as a tick, dBFS on a −60..0
/// scale.
export function LevelMeter({ label, peak, rms }: { label: string; peak: number; rms: number }) {
  const peakDb = peak > 0 ? 20 * Math.log10(peak) : -Infinity
  return (
    <div className="flex items-center gap-2">
      <span className="cap w-7 shrink-0">{label}</span>
      <div className="relative h-1.5 min-w-0 flex-1 overflow-hidden bg-canvas">
        <div
          className="absolute inset-y-0 left-0 bg-ink-3 transition-[width] duration-75"
          style={{ width: `${toUnit(rms) * 100}%` }}
        />
        <div
          className="absolute inset-y-0 w-0.5"
          style={{
            left: `calc(${toUnit(peak) * 100}% - 1px)`,
            background: peak >= 1 ? 'var(--color-danger)' : 'var(--color-ink)',
          }}
        />
      </div>
      <span className="num w-10 shrink-0 text-right text-[10px] text-ink-3">
        {Number.isFinite(peakDb) && peakDb > FLOOR_DB ? peakDb.toFixed(1) : '−∞'}
      </span>
    </div>
  )
}

/// Full scale of every reduction meter, in dB.
export const REDUCTION_SCALE_DB = 24

/// Gain reduction as a bar growing from the right — the direction gain is
/// taken away in — on a 0..24 dB scale.
export function ReductionMeter({
  label,
  reductionDb,
  active = true,
}: {
  label: string
  reductionDb: number
  /// `false` when the stage is not running (bypassed, or the other mode).
  active?: boolean
}) {
  const db = active ? Math.max(0, reductionDb) : 0
  const unit = Math.min(1, db / REDUCTION_SCALE_DB)
  return (
    <div className="flex items-center gap-2">
      <span className="cap w-7 shrink-0">{label}</span>
      <div
        className="relative h-1.5 min-w-0 flex-1 overflow-hidden bg-canvas"
        role="meter"
        aria-label={`${label} gain reduction`}
        aria-valuemin={0}
        aria-valuemax={REDUCTION_SCALE_DB}
        aria-valuenow={Number(db.toFixed(1))}
      >
        <div
          className="absolute inset-y-0 right-0 bg-accent transition-[width] duration-75"
          style={{ width: `${unit * 100}%` }}
        />
      </div>
      <span className={`num w-10 shrink-0 text-right text-[10px] ${db >= 0.05 ? 'text-ink-2' : 'text-ink-4'}`}>
        {active ? (db >= 0.05 ? `−${db.toFixed(1)}` : '0.0') : '—'}
      </span>
    </div>
  )
}
