import { useEffect, useRef, useState } from "react";
import { animate, stagger } from "animejs";
import { useStudioStream } from "../lib/stream";
import LoudnessPanel from "./LoudnessPanel";
import LoudnessHistory from "./LoudnessHistory";
import Spectrum from "./Spectrum";
import Vectorscope from "./Vectorscope";
import LevelMeters from "./LevelMeters";

/** Loudness targets a mix is often delivered at. */
export const TARGETS = [
  { label: "Streaming", lufs: -14 },
  { label: "Apple Music", lufs: -16 },
  { label: "Podcast", lufs: -16 },
  { label: "EBU R128", lufs: -23 },
] as const;

export default function Visualizer() {
  const stream = useStudioStream();
  const [target, setTarget] = useState<number>(-14);
  const root = useRef<HTMLDivElement>(null);

  // The panels come in once, staggered — motion that says the page is ready,
  // not decoration that keeps moving.
  useEffect(() => {
    if (!root.current) return;
    animate(root.current.querySelectorAll(".panel"), {
      opacity: [0, 1],
      translateY: [10, 0],
      delay: stagger(70),
      duration: 520,
      ease: "outCubic",
    });
  }, []);

  return (
    <div className="app" ref={root}>
      <header className="topbar">
        <div className="brand">
          <span className="brand-mark" aria-hidden />
          <span>Futureboard</span>
          <span className="brand-sub">Loudness &amp; Visualizer</span>
        </div>
        <div className="topbar-right">
          <label className="target">
            <span>Target</span>
            <select value={target} onChange={(e) => setTarget(Number(e.target.value))}>
              {TARGETS.map((t) => (
                <option key={t.label} value={t.lufs}>
                  {t.label} ({t.lufs} LUFS)
                </option>
              ))}
            </select>
          </label>
          <ConnectionPill state={stream.connection} />
        </div>
      </header>

      <main className="grid">
        <section className="panel span-2">
          <LoudnessPanel stream={stream} target={target} />
        </section>
        <section className="panel">
          <LevelMeters stream={stream} />
        </section>
        <section className="panel span-2">
          <h2>Short-term loudness — last 30 s</h2>
          <LoudnessHistory history={stream.history} target={target} />
        </section>
        <section className="panel">
          <h2>Stereo image</h2>
          <Vectorscope stream={stream} />
        </section>
        <section className="panel span-3">
          <h2>Spectrum</h2>
          <Spectrum stream={stream} />
        </section>
      </main>

      <footer className="footnote">
        Master bus, after the fader. Read-only, served by Futureboard Studio on this computer.
      </footer>
    </div>
  );
}

function ConnectionPill({ state }: { state: "connecting" | "open" | "closed" }) {
  const dot = useRef<HTMLSpanElement>(null);
  useEffect(() => {
    if (!dot.current || state !== "connecting") return;
    const pulse = animate(dot.current, {
      opacity: [0.35, 1],
      duration: 600,
      alternate: true,
      loop: true,
      ease: "inOutSine",
    });
    return () => {
      pulse.pause();
    };
  }, [state]);
  const label =
    state === "open" ? "Connected" : state === "connecting" ? "Connecting…" : "Studio not reachable";
  return (
    <span className={`pill pill-${state}`} role="status">
      <span className="pill-dot" ref={dot} />
      {label}
    </span>
  );
}
