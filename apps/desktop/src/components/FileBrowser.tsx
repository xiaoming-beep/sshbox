import { useCallback, useEffect, useRef, useState } from "react";
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
  if (path === "/" || path === ".") return "/";
  const cut = path.lastIndexOf("/");
  if (cut <= 0) return "/";
  return path.slice(0, cut);
}

function joinPath(base: string, name: string): string {
  return `${base.replace(/\/+$/, "")}/${name.replace(/^\/+/, "")}`;
}

/** 大文件分块转 base64，避免 String.fromCharCode 爆栈 */
function bytesToBase64(bytes: Uint8Array): string {
  let binary = "";
  const chunk = 0x8000;
  for (let index = 0; index < bytes.length; index += chunk) {
    binary += String.fromCharCode(...bytes.subarray(index, index + chunk));
  }
  return btoa(binary);
}

/** 单个文件上限（后端还有连接级 maxWriteBytes） */
const MAX_UPLOAD_BYTES = 32 * 1024 * 1024;

export default function FileBrowser({ sessionId, onClose }: Props) {
  const [path, setPath] = useState("/");
  const [entries, setEntries] = useState<SftpEntry[]>([]);
  const [history, setHistory] = useState<string[]>([]);
  const [selected, setSelected] = useState<SftpEntry | null>(null);
  const [writeRoot, setWriteRoot] = useState<string | null>(null);
  const [overwrite, setOverwrite] = useState(false);
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const fileInput = useRef<HTMLInputElement | null>(null);
  const dirInput = useRef<HTMLInputElement | null>(null);

  /** 读目录；返回规范化后的绝对路径（失败返回 null） */
  const load = useCallback(
    async (target: string): Promise<string | null> => {
      setLoading(true);
      setError(null);
      setNotice(null);
      setSelected(null);
      try {
        const listing = await api.sftpList(sessionId, target);
        setEntries(listing.entries);
        setPath(listing.dir);
        return listing.dir;
      } catch (e) {
        setError(errorText(e));
        return null;
      } finally {
        setLoading(false);
      }
    },
    [sessionId],
  );

  /** 切换目录并记入历史（刷新用 load，不写历史） */
  const navigate = useCallback(
    async (target: string) => {
      const from = path;
      const dir = await load(target);
      if (dir && dir !== from) {
        setHistory((prev) => [...prev, from]);
        setSelected(null);
      }
    },
    [load, path],
  );

  const goBack = useCallback(() => {
    setHistory((prev) => {
      if (prev.length === 0) return prev;
      const target = prev[prev.length - 1];
      void load(target);
      return prev.slice(0, -1);
    });
  }, [load]);

  useEffect(() => {
    void load(".");
    setHistory([]);
    setWriteRoot(null);
    setOverwrite(false);
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

  const grantHere = useCallback(async () => {
    setError(null);
    try {
      const next = writeRoot === path ? null : path;
      const granted = await api.setWriteRoot(sessionId, next);
      setWriteRoot(granted);
      setNotice(granted ? `已允许写入 ${granted}` : "已撤销写入权限");
    } catch (e) {
      setError(errorText(e));
    }
  }, [path, sessionId, writeRoot]);

  const uploadFiles = useCallback(
    async (files: File[]) => {
      if (files.length === 0) return;
      setBusy(true);
      setError(null);

      let done = 0;
      const failures: string[] = [];

      try {
        for (const [index, file] of files.entries()) {
          const relative = (file as File & { webkitRelativePath?: string }).webkitRelativePath;
          const name = relative && relative.length > 0 ? relative : file.name;
          setNotice(`上传中 ${index + 1}/${files.length}：${name}`);

          if (file.size > MAX_UPLOAD_BYTES) {
            failures.push(`${name}：超过单文件上限 ${formatSize(MAX_UPLOAD_BYTES)}`);
            continue;
          }

          const target = joinPath(path, name);

          // 目录上传时先补齐父目录（mkdir -p，已存在会被忽略）
          const segments = name.split("/").slice(0, -1);
          if (segments.length > 0) {
            let current = path;
            for (const segment of segments) {
              current = joinPath(current, segment);
              try {
                await api.sftpMkdir(sessionId, current);
              } catch {
                /* 目录已存在 */
              }
            }
          }

          try {
            const buffer = await file.arrayBuffer();
            const base64 = bytesToBase64(new Uint8Array(buffer));
            await api.sftpWrite(sessionId, target, base64, overwrite);
            done += 1;
          } catch (e) {
            failures.push(`${name}：${errorText(e)}`);
          }
        }

        if (failures.length > 0) {
          setError(`完成 ${done} 个，失败 ${failures.length} 个：\n${failures.slice(0, 5).join("\n")}`);
          setNotice(null);
        } else {
          setNotice(`已上传 ${done} 个文件`);
        }
        await load(path);
      } finally {
        setBusy(false);
        if (fileInput.current) fileInput.current.value = "";
        if (dirInput.current) dirInput.current.value = "";
      }
    },
    [load, overwrite, path, sessionId],
  );

  const writableHere =
    writeRoot !== null &&
    (path === writeRoot || path.startsWith(`${writeRoot.replace(/\/+$/, "")}/`));

  const segments = path.split("/").filter(Boolean);

  return (
    <aside className="file-drawer">
      <div className="fb-head">
        <span>文件</span>
        <div className="fb-actions">
          <button
            className="btn-ghost"
            onClick={goBack}
            disabled={history.length === 0}
            title={history.length > 0 ? `返回 ${history[history.length - 1]}` : "没有可返回的目录"}
          >
            ←
          </button>
          <button className="btn-ghost" onClick={() => void navigate(parentOf(path))} title="上一级">
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

      <div className="fb-crumbs">
        <button className="fb-crumb" onClick={() => void navigate("/")} title="根目录">
          /
        </button>
        {segments.map((segment, index) => {
          const target = `/${segments.slice(0, index + 1).join("/")}`;
          return (
            <span key={target} className="fb-crumb-wrap">
              <button className="fb-crumb" onClick={() => void navigate(target)} title={target}>
                {segment}
              </button>
              {index < segments.length - 1 && <span className="fb-sep">/</span>}
            </span>
          );
        })}
      </div>

      <div className="fb-tools">
        <button
          className={`btn-ghost ${writeRoot === path ? "active" : ""}`}
          onClick={() => void grantHere()}
          title="把当前目录设为可写范围（可随时撤销）"
        >
          {writeRoot === path ? "✓ 可写" : "允许写入"}
        </button>
        <button
          className="btn-ghost"
          onClick={() => fileInput.current?.click()}
          disabled={!writableHere || busy}
          title={writableHere ? "上传文件到当前目录" : "先允许写入此目录"}
        >
          传文件
        </button>
        <button
          className="btn-ghost"
          onClick={() => dirInput.current?.click()}
          disabled={!writableHere || busy}
          title={writableHere ? "上传整个文件夹（含子目录）" : "先允许写入此目录"}
        >
          传目录
        </button>
        <label className="fb-overwrite" title="默认不覆盖已存在的文件">
          <input
            type="checkbox"
            checked={overwrite}
            onChange={(event) => setOverwrite(event.target.checked)}
          />
          覆盖
        </label>

        <input
          ref={fileInput}
          type="file"
          multiple
          style={{ display: "none" }}
          onChange={(event) => void uploadFiles(Array.from(event.target.files ?? []))}
        />
        <input
          ref={dirInput}
          type="file"
          multiple
          // @ts-expect-error webkitdirectory 不是标准属性，但 Chromium 支持
          webkitdirectory=""
          style={{ display: "none" }}
          onChange={(event) => void uploadFiles(Array.from(event.target.files ?? []))}
        />
      </div>

      {writeRoot && (
        <div className="fb-scope" title={writeRoot}>
          可写范围：{writeRoot}
        </div>
      )}

      <div className="fb-list">
        {loading && <p className="hint fb-hint">读取中…</p>}
        {!loading && entries.length === 0 && !error && <p className="hint fb-hint">空目录</p>}
        {entries.map((entry) => (
          <div
            key={entry.path}
            className={`fb-row ${selected?.path === entry.path ? "selected" : ""}`}
            onClick={() => {
              if (entry.isDir) void navigate(entry.path);
              else setSelected(entry);
            }}
            onDoubleClick={() => {
              if (!entry.isDir) void download(entry);
            }}
            title={entry.isDir ? "单击进入" : "双击下载"}
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
