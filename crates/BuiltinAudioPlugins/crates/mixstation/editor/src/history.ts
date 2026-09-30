import type { MeterFrame } from './bridge'

/// Columns the scrolling stage keeps: ten seconds at the ~30 Hz telemetry rate.
export const HISTORY_LENGTH = 300

/**
 * Fixed-size ring of the host's meter frames, oldest first when read.
 *
 * Written from the bridge listener and read by the stage's paint loop; it is
 * never React state, so 30 Hz telemetry never re-renders the control tree.
 */
export class MeterHistory {
  readonly inPeak = new Float32Array(HISTORY_LENGTH)
  readonly outPeak = new Float32Array(HISTORY_LENGTH)
  readonly reduction = new Float32Array(HISTORY_LENGTH)
  /// Next slot to write.
  write = 0
  /// Frames held, up to `HISTORY_LENGTH`.
  count = 0
  /// Bumped on every change, so a painter can skip unchanged frames.
  stamp = 0

  push(frame: MeterFrame) {
    this.inPeak[this.write] = frame.inPeak
    this.outPeak[this.write] = frame.outPeak
    this.reduction[this.write] = Math.max(0, frame.gainReductionDb)
    this.write = (this.write + 1) % HISTORY_LENGTH
    this.count = Math.min(this.count + 1, HISTORY_LENGTH)
    this.stamp += 1
  }

  clear() {
    this.inPeak.fill(0)
    this.outPeak.fill(0)
    this.reduction.fill(0)
    this.write = 0
    this.count = 0
    this.stamp += 1
  }

  /// Ring index of the `age`-th newest frame (0 = newest).
  indexFromNewest(age: number) {
    return (this.write - 1 - age + HISTORY_LENGTH * 2) % HISTORY_LENGTH
  }
}
