// The desktop mixer's fader taper (apps/native/livestage/src/fader_law.rs):
// unity three quarters up, the working range spread out, the bottom quarter
// falling away to silence. Kept identical so a fader sits at the same height
// on both.

import { MAX_FADER_DB, MIN_FADER_DB } from './protocol.ts'

const KNOTS: [number, number][] = [
  [0, MIN_FADER_DB],
  [0.1, -60],
  [0.25, -40],
  [0.5, -20],
  [0.65, -10],
  [0.75, 0],
  [1, MAX_FADER_DB],
]

const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v))

export function positionToDb(position: number): number {
  const p = clamp(position, 0, 1)
  for (let i = 1; i < KNOTS.length; i++) {
    const [p0, d0] = KNOTS[i - 1]
    const [p1, d1] = KNOTS[i]
    if (p <= p1) return d0 + ((d1 - d0) * (p - p0)) / (p1 - p0)
  }
  return MAX_FADER_DB
}

export function dbToPosition(db: number): number {
  const d = clamp(db, MIN_FADER_DB, MAX_FADER_DB)
  for (let i = 1; i < KNOTS.length; i++) {
    const [p0, d0] = KNOTS[i - 1]
    const [p1, d1] = KNOTS[i]
    if (d <= d1) return p0 + ((p1 - p0) * (d - d0)) / (d1 - d0)
  }
  return 1
}

export function formatDb(db: number): string {
  if (db <= MIN_FADER_DB + 0.05) return '-∞'
  if (db > 0.05) return `+${db.toFixed(1)}`
  return db.toFixed(1)
}

export function formatPan(pan: number): string {
  const amount = Math.round(Math.abs(pan) * 100)
  if (amount === 0) return 'C'
  return `${pan < 0 ? 'L' : 'R'}${amount}`
}

/** Meter height for a linear peak: linear in dB from -60 to 0, as the
 *  desktop meters draw it. */
export const METER_FLOOR_DB = -60
export function meterFraction(level: number): number {
  if (level <= 0) return 0
  const db = 20 * Math.log10(Math.min(level, 1))
  return clamp((db - METER_FLOOR_DB) / -METER_FLOOR_DB, 0, 1)
}
