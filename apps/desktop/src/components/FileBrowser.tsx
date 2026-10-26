import { useCallback, useEffect, useState } from "react";
import { api, errorText, type SftpEntry } from "../api";

interface Props {
  sessionId: string;
  onClose: () => void;
}

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(1)} GB`;
}

function formatTime(seconds: number | null): string {
  if (!seconds) return "";
  const date = new Date(seconds * 1000);
  return `${date.getMonth() + 1}-${String(date.getDate()).padStart(2, "0")} ${String(
    date.getHours(),
  ).padStart(2, "0")}:${String(date.getMinutes()).padStart(2, "0")}`;
}

function parentOf(path: string): string {
  if (path === "/" || path === ".") return path;
  const cut = path.lastIndexOf("/");
  if (cut < 0) return ".";
  if (cut === 0) return "/";
  return path.slice(0, cut);
}

export default function FileBrowser({ sessionId, onClose }: Props) {
  const [path, setPath] = useState(".");
  const [entries, setEntries] = useState<SftpEntry[]>([]);
  const [selected, setSelected] = useState<SftpEntry | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const load = useCallback(
    async (target: string) => {
      setLoading(true);
      setError(null);
      setNotice(null);
      setSelected(null);
      try {
        const list = await api.sftpList(sessionId, target);
        setEntries(list);
        setPath(target);
      } catch (e) {
        setError(errorText(e));
      } finally {
        setLoading(false);
      }
    },
    [sessionId],
  );

  useEffect(() => {
    void load(".");
  }, [load]);

  const download = useCallback(
    async (entry: SftpEntry) => {
      setError(null);
      setNotice(`正在下载 ${entry.name} …`);
      try {
        const saved = await api.sftpDownload(sessionId, entry.path);
        setNotice(`已保存到 ${saved}`);
      } catch (e) {
        setNotice(null);
        setError(errorText(e));
      }
    },
    [sessionId],
  );

  return (
    <aside className="file-drawer">
      <div className="fb-head">
        <span>文件</span>
        <div className="fb-actions">
          <button className="btn-ghost" onClick={() => void load(parentOf(path))} title="上一级">
            ↑
          </button>
          <button className="btn-ghost" onClick={() => void load(path)} title="刷新">
            ⟳
          </button>
          <button className="btn-ghost" onClick={onClose} title="关闭">
            ✕
          </button>
        </div>
      </div>

      <div className="fb-path" title={path}>
        {path}
      </div>

      <div className="fb-list">
        {loading && <p className="hint fb-hint">读取中…</p>}
        {!loading && entries.length === 0 && !error && <p className="hint fb-hint">空目录</p>}
        {entries.map((entry) => (
          <div
            key={entry.path}
            className={`fb-row ${selected?.path === entry.path ? "selected" : ""}`}
            onClick={() => {
              if (entry.isDir) void load(entry.path);
              else setSelected(entry);
            }}
            onDoubleClick={() => {
              if (!entry.isDir) void download(entry);
            }}
            title={entry.isDir ? "双击进入" : "双击下载"}
          >
            <span className="fb-icon">{entry.isDir ? "📁" : "📄"}</span>
            <span className="fb-name">{entry.name}</span>
            <span className="fb-size">{entry.isDir ? "" : formatSize(entry.size)}</span>
            <span className="fb-time">{formatTime(entry.modified)}</span>
            {!entry.isDir && selected?.path === entry.path && (
              <button
                className="fb-dl"
                onClick={(event) => {
                  event.stopPropagation();
                  void download(entry);
                }}
              >
                下载
              </button>
            )}
          </div>
        ))}
      </div>

      {(error || notice) && (
        <div className={`fb-foot ${error ? "err-text" : "ok-text"}`}>{error ?? notice}</div>
      )}
    </aside>
  );
}
