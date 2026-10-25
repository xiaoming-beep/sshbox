import { useEffect, useRef } from "react";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { api, b64ToBytes } from "../api";
import "@xterm/xterm/css/xterm.css";

interface Props {
  sessionId: string;
  active: boolean;
}

interface OutputPayload {
  sessionId: string;
  cursor: number;
  data: string;
}

const THEME = {
  background: "#11131a",
  foreground: "#d7dae0",
  cursor: "#7aa2f7",
  cursorAccent: "#11131a",
  selectionBackground: "#2a3040",
  black: "#1a1c24",
  red: "#f7768e",
  green: "#9ece6a",
  yellow: "#e0af68",
  blue: "#7aa2f7",
  magenta: "#bb9af7",
  cyan: "#7dcfff",
  white: "#c0caf5",
  brightBlack: "#414868",
  brightRed: "#f7768e",
  brightGreen: "#9ece6a",
  brightYellow: "#e0af68",
  brightBlue: "#7aa2f7",
  brightMagenta: "#bb9af7",
  brightCyan: "#7dcfff",
  brightWhite: "#ffffff",
};

export default function TerminalPane({ sessionId, active }: Props) {
  const hostRef = useRef<HTMLDivElement | null>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);

  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;

    const term = new Terminal({
      fontFamily: "'Cascadia Mono', Consolas, 'Courier New', monospace",
      fontSize: 13,
      lineHeight: 1.15,
      cursorBlink: true,
      scrollback: 8000,
      theme: THEME,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host);
    termRef.current = term;
    fitRef.current = fit;

    const safeFit = () => {
      try {
        if (host.clientWidth > 0 && host.clientHeight > 0) fit.fit();
      } catch {
        /* 容器尚未布局完（标签未激活等），下一帧或尺寸变化时会重试 */
      }
    };

    // xterm 会通过 onData 回送终端应答（DA/CPR 等），它们不是人类输入：
    // 既不该计入"人类优先"，也不该污染命令文本。
    const isTerminalReply = (data: string) =>
      /^\x1b\[[?>]?[\d;]*[Rcn]$/.test(data) || /^\x1b\[[IO]$/.test(data);

    const dataSub = term.onData((data) => {
      void api.input(sessionId, data, isTerminalReply(data) ? "terminal" : "human");
    });
    const resizeSub = term.onResize(({ cols, rows }) => {
      void api.resize(sessionId, cols, rows);
    });

    // 监听器就位后再做首次 fit：否则第一次尺寸变化会无人接收，
    // PTY 会一直停在 open_session 传入的初始尺寸（tmux 就会画在半空）。
    safeFit();

    // 先订阅、后回放：中间到达的 chunk 进队列，回放完成后按游标补齐
    let cursor = 0;
    let ready = false;
    const queue: OutputPayload[] = [];

    const writeIfNew = (payload: OutputPayload) => {
      if (payload.cursor <= cursor) return;
      cursor = payload.cursor;
      term.write(b64ToBytes(payload.data));
    };

    let unlisten: UnlistenFn | undefined;
    void listen<OutputPayload>("ssh://output", (event) => {
      if (event.payload.sessionId !== sessionId) return;
      if (!ready) {
        queue.push(event.payload);
        return;
      }
      writeIfNew(event.payload);
    }).then((fn) => {
      unlisten = fn;
    });

    void api
      .sessionRead(sessionId, 0)
      .then((snapshot) => {
        if (snapshot.cursor > cursor) {
          cursor = snapshot.cursor;
          term.write(b64ToBytes(snapshot.data));
        }
      })
      .catch(() => undefined)
      .finally(() => {
        ready = true;
        queue.sort((a, b) => a.cursor - b.cursor).forEach(writeIfNew);
        queue.length = 0;
      });

    // 容器尺寸变化（面板挂载/标签切换/窗口拖拽）都要重新 fit，
    // 否则 PTY 会停在初始尺寸，tmux 这类全屏程序就会画在半空。
    let observedWidth = host.clientWidth;
    let observedHeight = host.clientHeight;
    const observer = new ResizeObserver(() => {
        const width = host.clientWidth;
        const height = host.clientHeight;
        if (width === observedWidth && height === observedHeight) return;
        observedWidth = width;
        observedHeight = height;
        safeFit();
    });
    observer.observe(host);

    const onWindowResize = () => safeFit();
    window.addEventListener("resize", onWindowResize);

    return () => {
      observer.disconnect();
      window.removeEventListener("resize", onWindowResize);
      dataSub.dispose();
      resizeSub.dispose();
      unlisten?.();
      term.dispose();
      termRef.current = null;
      fitRef.current = null;
    };
  }, [sessionId]);

  useEffect(() => {
    if (!active) return;
    const raf = requestAnimationFrame(() => {
      try {
        fitRef.current?.fit();
      } catch {
        /* ignore */
      }
      termRef.current?.focus();
    });
    return () => cancelAnimationFrame(raf);
  }, [active]);
  return (
    <div ref={hostRef} className="term-host" style={{ display: active ? "block" : "none" }} />
  );
}
