import { useRef } from "react";
import { useAnimationFrame, type Stream } from "../lib/stream";

const FLOOR_DB = -60;

const toDb = (linear: number) => (linear > 0 ? 20 * Math.log10(linear) : -Infinity);
const toFraction = (db: number) =>
  Number.isFinite(db) ? Math.max(0, Math.min(1, (db - FLOOR_DB) / -FLOOR_DB)) : 0;

/** L/R peak and RMS bars, updated in place each animation frame. */
export default function LevelMeters({ stream }: { stream: Stream }) {
  const bars = useRef<(HTMLDivElement | null)[]>([]);
  const readouts = useRef<(HTMLSpanElement | null)[]>([]);
  const drawn = useRef(-1);

  useAnimationFrame(() => {
    const frame = stream.frame.current;
    if (!frame || drawn.current === stream.version.current) return;
    drawn.current = stream.version.current;
    for (let ch = 0; ch < 2; ch++) {
      const peakDb = toDb(frame.levels.peak[ch]);
      const rmsDb = toDb(frame.levels.rms[ch]);
      const peakBar = bars.current[ch * 2];
      const rmsBar = bars.current[ch * 2 + 1];
      if (peakBar) {
        peakBar.style.transform = `scaleY(${toFraction(peakDb)})`;
        peakBar.classList.toggle("hot", peakDb > -1);
      }
      if (rmsBar) rmsBar.style.transform = `scaleY(${toFraction(rmsDb)})`;
      const readout = readouts.current[ch];
      if (readout) readout.textContent = Number.isFinite(peakDb) ? peakDb.toFixed(1) : "−∞";
    }
  });

  return (
    <div className="meters">
      <h2>Levels</h2>
      <div className="meter-row">
        {["L", "R"].map((name, ch) => (
          <div className="meter" key={name}>
            <div className="meter-track">
              <div className="meter-peak" ref={(el) => void (bars.current[ch * 2] = el)} />
              <div className="meter-rms" ref={(el) => void (bars.current[ch * 2 + 1] = el)} />
            </div>
            <span className="meter-name">{name}</span>
            <span className="num" ref={(el) => void (readouts.current[ch] = el)}>
              −∞
            </span>
          </div>
        ))}
        <div className="meter-scale" aria-hidden>
          {[0, -6, -12, -24, -36, -48, -60].map((db) => (
            <span key={db} style={{ bottom: `${toFraction(db) * 100}%` }}>
              {db}
            </span>
          ))}
        </div>
      </div>
      <p className="meter-note">Peak (light) and RMS (dark), dBFS</p>
    </div>
  );
}
