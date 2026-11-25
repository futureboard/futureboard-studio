import { useEffect, useRef, useState } from "react";
import { animate } from "animejs";
import { fmt, useAnimationFrame, type Stream } from "../lib/stream";

interface Readings {
  momentary: number | null;
  shortTerm: number | null;
  integrated: number | null;
  range: number | null;
  truePeak: number | null;
  live: boolean;
}

const EMPTY: Readings = {
  momentary: null,
  shortTerm: null,
  integrated: null,
  range: null,
  truePeak: null,
  live: false,
};

/** Big loudness readouts. The fast values (momentary, short-term) follow the
 * stream directly; the slow ones tween with anime.js when they change, so a
 * jump reads as movement rather than a flicker. */
export default function LoudnessPanel({ stream, target }: { stream: Stream; target: number }) {
  const [readings, setReadings] = useState<Readings>(EMPTY);
  const lastUpdate = useRef(0);

  // Ten readouts a second is all an eye can follow.
  useAnimationFrame((time) => {
    if (time - lastUpdate.current < 100) return;
    lastUpdate.current = time;
    const frame = stream.frame.current;
    if (!frame) return;
    setReadings({ ...frame.loudness, live: frame.live });
  });

  const delta = readings.integrated == null ? null : readings.integrated - target;
  const overPeak = readings.truePeak != null && readings.truePeak > -1;

  return (
    <div className="loudness">
      <div className="loudness-main">
        <Readout label="Integrated" unit="LUFS" value={readings.integrated} tween big />
        <div className={`delta ${delta == null ? "" : Math.abs(delta) <= 1 ? "ok" : "off"}`}>
          {delta == null ? "Waiting for audio" : `${delta > 0 ? "+" : ""}${delta.toFixed(1)} LU vs target`}
        </div>
      </div>
      <div className="loudness-side">
        <Readout label="Momentary" unit="LUFS" value={readings.momentary} />
        <Readout label="Short-term" unit="LUFS" value={readings.shortTerm} />
        <Readout label="Range" unit="LU" value={readings.range} tween />
        <Readout label="True peak" unit="dBTP" value={readings.truePeak} tween warn={overPeak} />
      </div>
      <button className="reset" type="button" onClick={stream.resetLoudness}>
        Reset
      </button>
    </div>
  );
}

function Readout({
  label,
  unit,
  value,
  tween = false,
  big = false,
  warn = false,
}: {
  label: string;
  unit: string;
  value: number | null;
  tween?: boolean;
  big?: boolean;
  warn?: boolean;
}) {
  const number = useRef<HTMLSpanElement>(null);
  const shown = useRef<{ value: number }>({ value: value ?? 0 });

  useEffect(() => {
    const el = number.current;
    if (!el) return;
    if (value == null) {
      el.textContent = "—";
      return;
    }
    if (!tween || el.textContent === "—") {
      shown.current.value = value;
      el.textContent = fmt(value);
      return;
    }
    const animation = animate(shown.current, {
      value,
      duration: 380,
      ease: "outQuad",
      onUpdate: () => {
        el.textContent = fmt(shown.current.value);
      },
    });
    return () => {
      animation.pause();
    };
  }, [value, tween]);

  return (
    <div className={`readout ${big ? "readout-big" : ""} ${warn ? "readout-warn" : ""}`}>
      <span className="readout-label">{label}</span>
      <span className="readout-value">
        <span ref={number}>—</span>
        <span className="readout-unit">{unit}</span>
      </span>
    </div>
  );
}
