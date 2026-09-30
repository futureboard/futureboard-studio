/**
 * Dev-only test signal for the browser preview: a 120 BPM groove (kick on
 * every beat, snare on two and four, eighth-note hats over a slow bed),
 * reduced to what one ~30 Hz meter frame reports.
 */

export type InputFrame = {
  /// Linear peak over the frame.
  peak: number
  /// Linear RMS over the frame.
  rms: number
  /// How much of the peak is a fresh hit (0..1), for transient models.
  onset: number
}

const BEAT = 0.5
const SUBSTEPS = 16

function levelAt(t: number) {
  const kickAge = t % BEAT
  const barPos = t % (BEAT * 2)
  const snareAge = barPos >= BEAT ? barPos - BEAT : barPos + BEAT
  const hatAge = t % (BEAT / 2)
  const kick = 0.82 * Math.exp(-kickAge / 0.09)
  const snare = 0.62 * Math.exp(-snareAge / 0.13)
  const hat = 0.2 * Math.exp(-hatAge / 0.025)
  const bed = 0.1 + 0.05 * Math.sin((2 * Math.PI * t) / 8)
  const onset = Math.max(Math.exp(-kickAge / 0.012), Math.exp(-snareAge / 0.012) * 0.8)
  return { level: Math.sqrt(kick * kick + snare * snare + hat * hat + bed * bed), onset }
}

export function grooveFrame(time: number, duration: number): InputFrame {
  let peak = 0
  let energy = 0
  let onset = 0
  for (let i = 0; i < SUBSTEPS; i++) {
    const { level, onset: hit } = levelAt(time + (i / SUBSTEPS) * duration)
    peak = Math.max(peak, level)
    energy += level * level
    onset = Math.max(onset, hit)
  }
  // A drum hit's waveform RMS sits well under its envelope.
  return { peak, rms: Math.sqrt(energy / SUBSTEPS) * 0.5, onset }
}

export const dbToLinear = (db: number) => Math.pow(10, db / 20)
export const linearToDb = (linear: number) => 20 * Math.log10(Math.max(linear, 1e-6))
