import { useEffect, useRef } from "react";
import * as d3 from "d3";
import type { History } from "../lib/stream";

const HEIGHT = 200;
const FLOOR = -40;
const CEIL = 0;
/** The server keeps 300 points at 0.1 s. */
const SPAN_SECONDS = 30;

/** The last 30 s of short-term loudness against the target, drawn with D3. */
export default function LoudnessHistory({
  history,
  target,
}: {
  history: History | null;
  target: number;
}) {
  const svg = useRef<SVGSVGElement>(null);

  useEffect(() => {
    const el = svg.current;
    if (!el) return;
    const width = el.clientWidth || 600;
    const margin = { top: 10, right: 12, bottom: 22, left: 40 };
    const root = d3.select(el).attr("viewBox", `0 0 ${width} ${HEIGHT}`);
    root.selectAll("*").remove();

    const points = history?.shortTerm ?? [];
    const step = history?.step ?? 0.1;
    // Always the full window, so the trace grows in from the right instead
    // of stretching what has arrived across the whole width.
    const x = d3
      .scaleLinear()
      .domain([-SPAN_SECONDS, 0])
      .range([margin.left, width - margin.right]);
    const y = d3
      .scaleLinear()
      .domain([FLOOR, CEIL])
      .range([HEIGHT - margin.bottom, margin.top])
      .clamp(true);

    root
      .append("g")
      .attr("class", "axis")
      .attr("transform", `translate(0,${HEIGHT - margin.bottom})`)
      .call(
        d3
          .axisBottom(x)
          .ticks(6)
          .tickFormat((d) => `${d}s`),
      );
    root
      .append("g")
      .attr("class", "axis")
      .attr("transform", `translate(${margin.left},0)`)
      .call(d3.axisLeft(y).ticks(5));
    root
      .append("g")
      .attr("class", "grid")
      .selectAll("line")
      .data(y.ticks(5))
      .join("line")
      .attr("x1", margin.left)
      .attr("x2", width - margin.right)
      .attr("y1", (d) => y(d))
      .attr("y2", (d) => y(d));

    root
      .append("line")
      .attr("class", "target-line")
      .attr("x1", margin.left)
      .attr("x2", width - margin.right)
      .attr("y1", y(target))
      .attr("y2", y(target));
    root
      .append("text")
      .attr("class", "target-label")
      .attr("x", width - margin.right - 4)
      .attr("y", y(target) - 5)
      .attr("text-anchor", "end")
      .text(`Target ${target} LUFS`);

    if (points.length < 2) {
      root
        .append("text")
        .attr("class", "empty")
        .attr("x", width / 2)
        .attr("y", HEIGHT / 2)
        .attr("text-anchor", "middle")
        .text("Play something in Studio");
      return;
    }
    const data = points.map((value, index) => ({
      t: (index - (points.length - 1)) * step,
      value: Math.max(value, FLOOR),
    }));
    const area = d3
      .area<{ t: number; value: number }>()
      .x((d) => x(d.t))
      .y0(y(FLOOR))
      .y1((d) => y(d.value))
      .curve(d3.curveMonotoneX);
    const line = d3
      .line<{ t: number; value: number }>()
      .x((d) => x(d.t))
      .y((d) => y(d.value))
      .curve(d3.curveMonotoneX);
    root.append("path").datum(data).attr("class", "history-area").attr("d", area);
    root.append("path").datum(data).attr("class", "history-line").attr("d", line);
  }, [history, target]);

  return <svg ref={svg} className="chart" height={HEIGHT} role="img" aria-label="Short-term loudness history" />;
}
