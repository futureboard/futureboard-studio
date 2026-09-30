/// Below this linear RMS a correlation is not a reading (about −80 dBFS).
const SILENT_LEVEL = 1e-4

/// Correlation, −1..+1, as a bar growing out from the centre: right of centre
/// (towards +1) is in phase and mono-safe, left of centre means the two sides
/// are fighting and will cancel when summed — drawn in the warning colour.
export function CorrelationMeter({
  value,
  active,
  compact = false,
  label,
}: {
  value: number
  /// `false` while there is too little signal to measure.
  active: boolean
  compact?: boolean
  label?: string
}) {
  const v = Math.max(-1, Math.min(1, active ? value : 0))
  const from = v < 0 ? 50 + v * 50 : 50
  const size = Math.abs(v) * 50
  const colour = v < 0 ? 'var(--color-warn)' : 'var(--color-accent)'
  return (
    <div className="flex min-w-0 flex-col gap-1">
      {!compact && (
        <div className="flex items-baseline justify-between">
          <span className="cap">{label ?? 'Correlation'}</span>
          <span className={`num text-[11px] font-semibold ${active ? 'text-ink' : 'text-ink-4'}`}>
            {active ? `${v >= 0 ? '+' : ''}${v.toFixed(2)}` : '—'}
          </span>
        </div>
      )}
      <div
        className={`relative w-full overflow-hidden rounded-full bg-canvas ${compact ? 'h-1.5' : 'h-2'}`}
        role="meter"
        aria-label={label ?? 'Correlation'}
        aria-valuemin={-1}
        aria-valuemax={1}
        aria-valuenow={active ? Number(v.toFixed(2)) : undefined}
        title={active ? `Correlation ${v.toFixed(2)}` : 'Too quiet to measure'}
      >
        <div className="absolute inset-y-0 w-px bg-white/25" style={{ left: '50%' }} />
        {active && (
          <div
            className="absolute inset-y-0 rounded-full transition-[left,width] duration-100"
            style={{ left: `${from}%`, width: `${size}%`, background: colour }}
          />
        )}
      </div>
      {!compact && (
        <div className="num flex justify-between text-[9px] text-ink-4">
          <span>−1</span>
          <span>0</span>
          <span>+1</span>
        </div>
      )}
    </div>
  )
}

export function isMeasurable(level: number | undefined) {
  return level !== undefined && level > SILENT_LEVEL
}

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
      <div className="relative h-1.5 min-w-0 flex-1 overflow-hidden rounded-full bg-canvas">
        <div
          className="absolute inset-y-0 left-0 rounded-full bg-ink-3 transition-[width] duration-75"
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
