import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { api, type CommandRecord, type FileOpRecord } from "../api";

interface Props {
  sessionId: string;
}

type Tab = "commands" | "files";

/** 界面保留的条数；后端命令存 200 条、文件操作存 500 条 */
const COMMAND_LIMIT = 200;
const FILE_LIMIT = 500;

function timeOf(ms: number): string {
  return new Date(ms).toLocaleTimeString("zh-CN", { hour12: false });
}

function durationOf(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  return `${Math.floor(ms / 60_000)}m${Math.round((ms % 60_000) / 1000)}s`;
}

function bytesOf(count: number): string {
  if (count < 1024) return `${count} B`;
  if (count < 1024 * 1024) return `${(count / 1024).toFixed(1)} KB`;
  return `${(count / 1024 / 1024).toFixed(1)} MB`;
}

/** 会话下方的两条流水：命令（按行切分）与文件操作（SFTP 审计） */
export default function CommandPanel({ sessionId }: Props) {
  const [tab, setTab] = useState<Tab>("commands");
  const [items, setItems] = useState<CommandRecord[]>([]);
  const [files, setFiles] = useState<FileOpRecord[]>([]);

  useEffect(() => {
    setItems([]);
    setFiles([]);

    // 首屏选页签：没有命令但有文件操作时，直接看文件（AI 只动文件时不会只看到空面板）
    void Promise.all([
      api.sessionHistory(sessionId).catch(() => [] as CommandRecord[]),
      api.sessionFileHistory(sessionId).catch(() => [] as FileOpRecord[]),
    ]).then(([commands, fileOps]) => {
      // 后端按时间正序返回（旧→新），界面显示新→旧，所以反转
      setItems(commands.slice().reverse());
      setFiles(fileOps.slice().reverse());
      if (commands.length === 0 && fileOps.length > 0) setTab("files");
    });

    const unlisteners: Array<() => void> = [];

    void listen<CommandRecord>("ssh://command", (event) => {
      if (event.payload.sessionId !== sessionId) return;
      // 新的插到最前面，不用滚到底部才能看到
      setItems((prev) => [event.payload, ...prev].slice(0, COMMAND_LIMIT));
    }).then((fn) => unlisteners.push(fn));

    // 命令结束时回填退出码与耗时（OSC 133）
    void listen<CommandRecord>("ssh://command-finished", (event) => {
      if (event.payload.sessionId !== sessionId) return;
      setItems((prev) =>
        prev.map((item) => (item.id === event.payload.id ? event.payload : item)),
      );
    }).then((fn) => unlisteners.push(fn));

    // 文件操作（含被拒的），人与 AI 的都记
    void listen<FileOpRecord>("ssh://file-op", (event) => {
      if (event.payload.sessionId !== sessionId) return;
      setFiles((prev) => [event.payload, ...prev].slice(0, FILE_LIMIT));
    }).then((fn) => unlisteners.push(fn));

    return () => unlisteners.forEach((fn) => fn());
  }, [sessionId]);

  return (
    <div className="cmd-panel">
      <div className="cmd-head">
        <div className="cmd-tabs">
          <button
            type="button"
            className={`cmd-tab ${tab === "commands" ? "active" : ""}`}
            onClick={() => setTab("commands")}
          >
            命令流水 <span className="hint">{items.length}</span>
          </button>
          <button
            type="button"
            className={`cmd-tab ${tab === "files" ? "active" : ""}`}
            onClick={() => setTab("files")}
          >
            文件操作 <span className="hint">{files.length}</span>
          </button>
        </div>
      </div>

      <div className="cmd-list">
        {tab === "commands" && (
          <>
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
          </>
        )}

        {tab === "files" && (
          <>
            {files.length === 0 && (
              <p className="hint" style={{ padding: "2px 10px" }}>
                文件面板或 AI 的 SFTP 操作会记录在这里（被拒绝的也会记）。
              </p>
            )}
            {files.map((item) => (
              <div key={item.id} className="cmd-row">
                <span className="cmd-time">{timeOf(item.at)}</span>
                <span className={`cmd-src ${item.source}`}>
                  {item.source === "ai" ? "AI" : "手动"}
                </span>
                <span className={`cmd-op ${item.ok ? "" : "bad"}`}>{item.op}</span>
                <code className="cmd-text" title={item.path}>
                  {item.path}
                </code>
                {item.bytes > 0 && <span className="cmd-dur">{bytesOf(item.bytes)}</span>}
                {item.ok ? (
                  <span className="cmd-exit ok">✓</span>
                ) : (
                  <span className="cmd-exit bad" title={item.detail ?? ""}>
                    ✕
                  </span>
                )}
              </div>
            ))}
          </>
        )}
      </div>
    </div>
  );
}
