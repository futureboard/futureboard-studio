// The Studio API server's WebSocket (`/ws`): a `frame` about 30 times a
// second and a `history` once a second. See `crates/SphereUIComponents/src/
// api_server.rs` for the protocol.

import { useEffect, useRef, useState } from "react";

export interface Loudness {
  momentary: number | null;
  shortTerm: number | null;
  integrated: number | null;
  range: number | null;
  truePeak: number | null;
}

export interface Frame {
  type: "frame";
  seq: number;
  live: boolean;
  sampleRate: number;
  loudness: Loudness;
  levels: {
    rms: [number, number];
    peak: [number, number];
    correlation: number;
    balance: number;
  };
  /** 256 log-spaced bins, 20 Hz–20 kHz, −90..0 dB. */
  spectrum: number[];
  peaks: number[];
  /** Vectorscope pairs, −1..1. */
  scope: [number, number][];
}

export interface History {
  type: "history";
  /** Seconds between points. */
  step: number;
  /** Short-term loudness, oldest first. */
  shortTerm: number[];
}

export type Connection = "connecting" | "open" | "closed";

export interface Stream {
  connection: Connection;
  /** The newest frame, held in a ref so a frame does not re-render React. */
  frame: React.RefObject<Frame | null>;
  history: History | null;
  /** Bumped on each frame, for views that draw in an animation frame. */
  version: React.RefObject<number>;
  resetLoudness: () => void;
}

function socketUrl(): string {
  const scheme = location.protocol === "https:" ? "wss:" : "ws:";
  return `${scheme}//${location.host}/ws`;
}

export function useStudioStream(): Stream {
  const [connection, setConnection] = useState<Connection>("connecting");
  const [history, setHistory] = useState<History | null>(null);
  const frame = useRef<Frame | null>(null);
  const version = useRef(0);
  const socket = useRef<WebSocket | null>(null);

  useEffect(() => {
    let closed = false;
    let retry = 0;
    let timer: ReturnType<typeof setTimeout> | undefined;

    const connect = () => {
      setConnection("connecting");
      const ws = new WebSocket(socketUrl());
      socket.current = ws;
      ws.onopen = () => {
        retry = 0;
        setConnection("open");
      };
      ws.onmessage = (event) => {
        let message: Frame | History;
        try {
          message = JSON.parse(event.data as string);
        } catch {
          return;
        }
        if (message.type === "frame") {
          frame.current = message;
          version.current += 1;
        } else if (message.type === "history") {
          setHistory(message);
        }
      };
      ws.onclose = () => {
        socket.current = null;
        if (closed) return;
        setConnection("closed");
        // Studio closed, or the server was turned off: keep trying, slower.
        retry = Math.min(retry + 1, 6);
        timer = setTimeout(connect, 250 * 2 ** retry);
      };
    };
    connect();
    return () => {
      closed = true;
      if (timer) clearTimeout(timer);
      socket.current?.close();
    };
  }, []);

  const resetLoudness = () => {
    socket.current?.send(JSON.stringify({ type: "reset-loudness" }));
  };

  return { connection, frame, history, version, resetLoudness };
}

/** Run `draw` once per animation frame while mounted. */
export function useAnimationFrame(draw: (time: number) => void) {
  const latest = useRef(draw);
  latest.current = draw;
  useEffect(() => {
    let id = 0;
    const tick = (time: number) => {
      latest.current(time);
      id = requestAnimationFrame(tick);
    };
    id = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(id);
  }, []);
}

export const fmt = (value: number | null | undefined, digits = 1): string =>
  value == null || !Number.isFinite(value) ? "—" : value.toFixed(digits);
