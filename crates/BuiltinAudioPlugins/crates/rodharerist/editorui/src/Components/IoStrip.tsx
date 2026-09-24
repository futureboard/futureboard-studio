import { useEffect, useRef } from "react";
import { postClearClip } from "../bridge";
import {
  INPUT_TRIM,
  OUTPUT_TRIM,
  calibrationFor,
  calibrationLabel,
  formatTrim,
  linearToDbfs,
} from "../globals";
import { clearLocalClip, getFrame, subscribeMeters } from "../state/meters";
import { LevelMeter } from "./LevelMeter";

/**
 * Sticky clip indicator. Like {@link LevelMeter} it paints via refs so it does
 * not re-render on meter frames. Clicking resets both the DSP's latched flag and
 * the local view.
 */
function ClipIndicator({ side }: { side: "in" | "out" }) {
  const ref = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    const paint = () => {
      const frame = getFrame();
      const clipped = side === "in" ? frame.inClip : frame.outClip;
      ref.current?.classList.toggle("clipped", clipped);
      if (ref.current) {
        ref.current.setAttribute("aria-pressed", String(clipped));
      }
    };
    paint();
    return subscribeMeters(paint);
  }, [side]);

  return (
    <button
      ref={ref}
      type="button"
      className="clip-led"
      aria-pressed="false"
      title="Clip indicator — click to reset"
      onClick={() => {
        postClearClip();
        clearLocalClip();
      }}
    >
      CLIP
    </button>
  );
}

/**
 * NAM input calibration readout. A capture's gain and voicing depend on the
 * level it is fed, so the band the input currently sits in is shown next to the
 * input trim rather than buried in an advanced panel.
 */
function CalibrationStatus() {
  const ref = useRef<HTMLSpanElement>(null);

  useEffect(() => {
    const paint = () => {
      const frame = getFrame();
      const state = calibrationFor(linearToDbfs(frame.inPeak), frame.inClip);
      const el = ref.current;
      if (!el) return;
      el.textContent = calibrationLabel[state];
      el.dataset.state = state;
    };
    paint();
    return subscribeMeters(paint);
  }, []);

  return (
    <span
      ref={ref}
      className="calib"
      data-state="silent"
      title="Input calibration — where the incoming level sits for a NAM capture"
    />
  );
}

/** A compact bipolar trim control. Drag, wheel, double-click to reset to 0 dB. */
function TrimControl({
  spec,
  value,
  onChange,
}: {
  spec: typeof INPUT_TRIM;
  value: number;
  onChange: (id: string, value: number) => void;
}) {
  const trackRef = useRef<HTMLDivElement>(null);
  const draggingRef = useRef(false);

  const range = spec.max - spec.min;
  const pct = (value - spec.min) / range;

  const setFromClientX = (clientX: number, fine: boolean) => {
    const el = trackRef.current;
    if (!el) return;
    const rect = el.getBoundingClientRect();
    if (rect.width <= 0) return;
    const p = Math.max(0, Math.min(1, (clientX - rect.left) / rect.width));
    const raw = spec.min + p * range;
    // Shift constrains the move to a tenth of the gesture, around the current
    // value, for fine adjustment.
    onChange(spec.id, fine ? value + (raw - value) * 0.1 : raw);
  };

  useEffect(() => {
    const el = trackRef.current;
    if (!el) return;
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      const step = e.shiftKey ? spec.step : spec.step * 10;
      const next = value + (e.deltaY < 0 ? step : -step);
      onChange(spec.id, Math.max(spec.min, Math.min(spec.max, next)));
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
  }, [onChange, spec, value]);

  return (
    <div className="trim">
      <span className="trim-label">{spec.name}</span>
      <div
        ref={trackRef}
        className="trim-track"
        role="slider"
        tabIndex={0}
        aria-label={spec.displayName}
        aria-valuemin={spec.min}
        aria-valuemax={spec.max}
        aria-valuenow={value}
        aria-valuetext={`${formatTrim(value)} dB`}
        title={`${spec.displayName} — drag (Shift = fine), double-click for 0 dB`}
        onPointerDown={(e) => {
          e.preventDefault();
          e.currentTarget.setPointerCapture(e.pointerId);
          draggingRef.current = true;
          setFromClientX(e.clientX, e.shiftKey);
        }}
        onPointerMove={(e) => {
          if (draggingRef.current) setFromClientX(e.clientX, e.shiftKey);
        }}
        onPointerUp={(e) => {
          draggingRef.current = false;
          if (e.currentTarget.hasPointerCapture(e.pointerId)) {
            e.currentTarget.releasePointerCapture(e.pointerId);
          }
        }}
        onPointerCancel={() => {
          draggingRef.current = false;
        }}
        onDoubleClick={() => onChange(spec.id, 0)}
        onKeyDown={(e) => {
          const step = e.shiftKey ? spec.step : spec.step * 10;
          if (e.key === "ArrowRight" || e.key === "ArrowUp") {
            e.preventDefault();
            onChange(spec.id, Math.min(spec.max, value + step));
          } else if (e.key === "ArrowLeft" || e.key === "ArrowDown") {
            e.preventDefault();
            onChange(spec.id, Math.max(spec.min, value - step));
          } else if (e.key === "Home") {
            e.preventDefault();
            onChange(spec.id, 0);
          }
        }}
      >
        {/* Unity tick: a bipolar trim needs a visible 0 dB reference. */}
        <span className="trim-unity" aria-hidden />
        <div
          className="trim-fill"
          style={{
            left: `${Math.min(pct, 0.5) * 100}%`,
            width: `${Math.abs(pct - 0.5) * 100}%`,
          }}
        />
        <div className="trim-thumb" style={{ left: `${pct * 100}%` }} />
      </div>
      <span className="trim-value">{formatTrim(value)}</span>
    </div>
  );
}

export type IoStripProps = {
  side: "in" | "out";
  trim: number;
  compact?: boolean;
  onTrimChange: (id: string, value: number) => void;
  /** Output side only: global plugin bypass. */
  globalBypass?: boolean;
  onToggleGlobalBypass?: () => void;
};

/**
 * Global gain staging for one end of the chain. Always visible — gain staging is
 * never hidden behind a menu.
 */
export function IoStrip({
  side,
  trim,
  compact,
  onTrimChange,
  globalBypass,
  onToggleGlobalBypass,
}: IoStripProps) {
  const isInput = side === "in";
  return (
    <section
      className={`io-strip io-${side}${compact ? " compact" : ""}`}
      aria-label={isInput ? "Input" : "Output"}
    >
      <header className="io-strip-head">
        <span className="io-strip-title">{isInput ? "Input" : "Output"}</span>
        <ClipIndicator side={side} />
      </header>

      <LevelMeter side={side} label={isInput ? "In" : "Out"} />

      <TrimControl
        spec={isInput ? INPUT_TRIM : OUTPUT_TRIM}
        value={trim}
        onChange={onTrimChange}
      />

      {isInput ? (
        <CalibrationStatus />
      ) : (
        <button
          type="button"
          className={`io-bypass${globalBypass ? " engaged" : ""}`}
          aria-pressed={!!globalBypass}
          onClick={onToggleGlobalBypass}
          title="Global bypass — pass the input through untouched"
        >
          <span className="led" aria-hidden />
          {globalBypass ? "Bypassed" : "Active"}
        </button>
      )}
    </section>
  );
}
