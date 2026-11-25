import { useRef } from "react";
import { useAnimationFrame, type Stream } from "../lib/stream";

const SIZE = 220;

/** Stereo image: a vectorscope (mid up, side across) on a canvas, with the
 * correlation and balance under it. */
export default function Vectorscope({ stream }: { stream: Stream }) {
  const canvas = useRef<HTMLCanvasElement>(null);
  const correlation = useRef<HTMLDivElement>(null);
  const balance = useRef<HTMLSpanElement>(null);
  const corrValue = useRef<HTMLSpanElement>(null);
  const drawn = useRef(-1);

  useAnimationFrame(() => {
    const el = canvas.current;
    const frame = stream.frame.current;
    if (!el || !frame || drawn.current === stream.version.current) return;
    drawn.current = stream.version.current;
    const ctx = el.getContext("2d");
    if (!ctx) return;
    const ratio = window.devicePixelRatio || 1;
    if (el.width !== SIZE * ratio) {
      el.width = SIZE * ratio;
      el.height = SIZE * ratio;
    }
    ctx.setTransform(ratio, 0, 0, ratio, 0, 0);
    // Fade the previous trace instead of clearing: a short persistence.
    ctx.fillStyle = "rgba(13, 14, 17, 0.35)";
    ctx.fillRect(0, 0, SIZE, SIZE);
    const c = SIZE / 2;
    ctx.strokeStyle = "rgba(255,255,255,0.08)";
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(c, 0);
    ctx.lineTo(c, SIZE);
    ctx.moveTo(0, c);
    ctx.lineTo(SIZE, c);
    ctx.moveTo(0, 0);
    ctx.lineTo(SIZE, SIZE);
    ctx.moveTo(SIZE, 0);
    ctx.lineTo(0, SIZE);
    ctx.stroke();

    ctx.fillStyle = "rgba(77, 208, 225, 0.75)";
    const scale = c * 0.9;
    for (const [l, r] of frame.scope) {
      const mid = (l + r) * Math.SQRT1_2;
      const side = (r - l) * Math.SQRT1_2;
      ctx.fillRect(c + side * scale, c - mid * scale, 1.5, 1.5);
    }

    const corr = Math.max(-1, Math.min(1, frame.levels.correlation));
    if (correlation.current) {
      correlation.current.style.left = `${((corr + 1) / 2) * 100}%`;
      correlation.current.classList.toggle("bad", corr < 0);
    }
    if (corrValue.current) corrValue.current.textContent = corr.toFixed(2);
    if (balance.current) {
      const b = frame.levels.balance;
      balance.current.textContent =
        Math.abs(b) < 0.1 ? "Centre" : `${Math.abs(b).toFixed(1)} dB ${b > 0 ? "R" : "L"}`;
    }
  });

  return (
    <div className="scope">
      <canvas ref={canvas} style={{ width: SIZE, height: SIZE }} aria-label="Vectorscope" />
      <div className="corr">
        <div className="corr-label">
          <span>Correlation</span>
          <span ref={corrValue} className="num">
            —
          </span>
        </div>
        <div className="corr-track">
          <span className="corr-tick" style={{ left: "50%" }} />
          <div className="corr-dot" ref={correlation} />
        </div>
        <div className="corr-scale">
          <span>−1</span>
          <span>0</span>
          <span>+1</span>
        </div>
        <div className="corr-label">
          <span>Balance</span>
          <span ref={balance} className="num">
            —
          </span>
        </div>
      </div>
    </div>
  );
}
