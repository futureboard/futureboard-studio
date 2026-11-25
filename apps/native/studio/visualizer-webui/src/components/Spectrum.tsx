import { useEffect, useRef } from "react";
import * as d3 from "d3";
import { useAnimationFrame, type Stream } from "../lib/stream";

const HEIGHT = 240;
const MIN_HZ = 20;
const MAX_HZ = 20_000;
const FLOOR_DB = -90;

/** Frequency of display bin `index` of `count` (the server's log spacing). */
const binHz = (index: number, count: number) =>
  MIN_HZ * (MAX_HZ / MIN_HZ) ** ((index + 0.5) / count);

/** The master spectrum: D3 scales and axes once, the paths redrawn each
 * animation frame from the newest frame (no React render per frame). */
export default function Spectrum({ stream }: { stream: Stream }) {
  const svg = useRef<SVGSVGElement>(null);
  const area = useRef<SVGPathElement | null>(null);
  const peaks = useRef<SVGPathElement | null>(null);
  const scales = useRef<{ x: d3.ScaleLogarithmic<number, number>; y: d3.ScaleLinear<number, number> } | null>(null);
  const drawn = useRef(-1);

  useEffect(() => {
    const el = svg.current;
    if (!el) return;
    const build = () => {
      const width = el.clientWidth || 900;
      const margin = { top: 10, right: 12, bottom: 22, left: 40 };
      const root = d3.select(el).attr("viewBox", `0 0 ${width} ${HEIGHT}`);
      root.selectAll("*").remove();
      const x = d3
        .scaleLog()
        .domain([MIN_HZ, MAX_HZ])
        .range([margin.left, width - margin.right]);
      const y = d3
        .scaleLinear()
        .domain([FLOOR_DB, 0])
        .range([HEIGHT - margin.bottom, margin.top])
        .clamp(true);
      root
        .append("g")
        .attr("class", "grid")
        .selectAll("line")
        .data([50, 100, 200, 500, 1000, 2000, 5000, 10000])
        .join("line")
        .attr("x1", (d) => x(d))
        .attr("x2", (d) => x(d))
        .attr("y1", margin.top)
        .attr("y2", HEIGHT - margin.bottom);
      root
        .append("g")
        .attr("class", "axis")
        .attr("transform", `translate(0,${HEIGHT - margin.bottom})`)
        .call(
          d3
            .axisBottom(x)
            .tickValues([20, 50, 100, 200, 500, 1000, 2000, 5000, 10000, 20000])
            .tickFormat((d) => {
              const hz = Number(d);
              return hz >= 1000 ? `${hz / 1000}k` : `${hz}`;
            }),
        );
      root
        .append("g")
        .attr("class", "axis")
        .attr("transform", `translate(${margin.left},0)`)
        .call(d3.axisLeft(y).ticks(6));
      area.current = root.append("path").attr("class", "spectrum-area").node();
      peaks.current = root.append("path").attr("class", "spectrum-peaks").node();
      scales.current = { x, y };
      drawn.current = -1;
    };
    build();
    const observer = new ResizeObserver(build);
    observer.observe(el);
    return () => observer.disconnect();
  }, []);

  useAnimationFrame(() => {
    const frame = stream.frame.current;
    const s = scales.current;
    if (!frame || !s || drawn.current === stream.version.current) return;
    drawn.current = stream.version.current;
    const count = frame.spectrum.length;
    const points = frame.spectrum.map((db, i) => [binHz(i, count), db] as const);
    const fill = d3
      .area<readonly [number, number]>()
      .x((d) => s.x(d[0]))
      .y0(s.y(FLOOR_DB))
      .y1((d) => s.y(d[1]))
      .curve(d3.curveBasis);
    const hold = d3
      .line<readonly [number, number]>()
      .x((d) => s.x(d[0]))
      .y((d) => s.y(d[1]))
      .curve(d3.curveBasis);
    area.current?.setAttribute("d", fill(points) ?? "");
    peaks.current?.setAttribute(
      "d",
      hold(frame.peaks.map((db, i) => [binHz(i, count), db] as const)) ?? "",
    );
  });

  return <svg ref={svg} className="chart" height={HEIGHT} role="img" aria-label="Spectrum" />;
}
