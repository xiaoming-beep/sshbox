import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { api, type CommandRecord } from "../api";

interface Props {
  sessionId: string;
}

function timeOf(ms: number): string {
  return new Date(ms).toLocaleTimeString("zh-CN", { hour12: false });
}

function durationOf(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  return `${Math.floor(ms / 60_000)}m${Math.round((ms % 60_000) / 1000)}s`;
}

/** 会话下方的命令流水：人类与 AI 的输入都按行记在这里 */
export default function CommandPanel({ sessionId }: Props) {
  const [items, setItems] = useState<CommandRecord[]>([]);

  useEffect(() => {
    setItems([]);
    void api
      .sessionHistory(sessionId)
      .then(setItems)
      .catch(() => undefined);

    const unlisteners: Array<() => void> = [];

    void listen<CommandRecord>("ssh://command", (event) => {
      if (event.payload.sessionId !== sessionId) return;
      setItems((prev) => [...prev, event.payload].slice(-200));
    }).then((fn) => unlisteners.push(fn));

    // 命令结束时回填退出码与耗时（OSC 133）
    void listen<CommandRecord>("ssh://command-finished", (event) => {
      if (event.payload.sessionId !== sessionId) return;
      setItems((prev) =>
        prev.map((item) => (item.id === event.payload.id ? event.payload : item)),
      );
    }).then((fn) => unlisteners.push(fn));

    return () => unlisteners.forEach((fn) => fn());
  }, [sessionId]);

  return (
    <div className="cmd-panel">
      <div className="cmd-head">
        <span>命令流水</span>
        <span className="hint">{items.length} 条</span>
      </div>

      <div className="cmd-list">
        {items.length === 0 && (
          <p className="hint" style={{ padding: "2px 10px" }}>
            终端里输入命令并回车，会记录在这里（AI 的输入也会）。
          </p>
        )}
        {items.map((item) => (
          <div key={item.id} className="cmd-row">
            <span className="cmd-time">{timeOf(item.sentAt)}</span>
            <span className={`cmd-src ${item.source}`}>
              {item.source === "ai" ? "AI" : item.source === "terminal" ? "TERM" : "手动"}
            </span>
            <code className="cmd-text">{item.redacted ? "🔒 已隐去" : item.command}</code>
            {item.durationMs != null && (
              <span className="cmd-dur">{durationOf(item.durationMs)}</span>
            )}
            {item.exitCode != null ? (
              <span className={`cmd-exit ${item.exitCode === 0 ? "ok" : "bad"}`}>
                exit {item.exitCode}
              </span>
            ) : (
              <span className="cmd-exit pending">…</span>
            )}
          </div>
        ))}
      </div>
    </div>
  );
}
