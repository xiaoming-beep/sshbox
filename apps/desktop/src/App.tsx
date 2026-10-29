import { useCallback, useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { api, blankConnection, errorText, type Connection, type DaemonInfo, type SessionInfo } from "./api";
import Sidebar from "./components/Sidebar";
import TerminalTabs from "./components/TerminalTabs";
import TerminalPane from "./components/TerminalPane";
import CommandPanel from "./components/CommandPanel";
import FileBrowser from "./components/FileBrowser";
import ConnectionDialog from "./components/ConnectionDialog";

interface PendingHost {
  connectionId: string;
  name: string;
  host: string;
  port: number;
  fingerprint: string;
}

export default function App() {
  const [connections, setConnections] = useState<Connection[]>([]);
  const [sessions, setSessions] = useState<SessionInfo[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  const [editing, setEditing] = useState<{ conn: Connection; isNew: boolean } | null>(null);
  const [pending, setPending] = useState<PendingHost | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [daemon, setDaemon] = useState<DaemonInfo | null>(null);
  const [showService, setShowService] = useState(false);
  const [showFiles, setShowFiles] = useState(false);
  const [deleting, setDeleting] = useState<Connection | null>(null);

  const refresh = useCallback(async () => {
    try {
      setConnections(await api.listConnections());
    } catch (e) {
      setNotice(errorText(e));
    }
  }, []);

  useEffect(() => {
    void refresh();
    void api.listSessions().then((list) => {
      setSessions(list);
      setActiveId((cur) => cur ?? list[0]?.id ?? null);
    });

    let unlisten: (() => void) | undefined;
    void listen<{ sessionId: string }>("ssh://closed", (event) => {
      setSessions((prev) => prev.filter((s) => s.id !== event.payload.sessionId));
    }).then((fn) => {
      unlisten = fn;
    });

    return () => unlisten?.();
  }, [refresh]);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void listen<SessionInfo>("ssh://opened", (event) => {
      const info = event.payload;
      setSessions((prev) => (prev.some((s) => s.id === info.id) ? prev : [...prev, info]));
      setActiveId(info.id);
    }).then((fn) => {
      unlisten = fn;
    });
    return () => unlisten?.();
  }, []);

  useEffect(() => {
    if (activeId && !sessions.some((s) => s.id === activeId)) {
      setActiveId(sessions[0]?.id ?? null);
    }
  }, [sessions, activeId]);

  useEffect(() => {
    void api
      .daemonInfo()
      .then(setDaemon)
      .catch(() => undefined);
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      if (deleting) setDeleting(null);
      else if (pending) setPending(null);
      else if (showService) setShowService(false);
      else if (editing) setEditing(null);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [deleting, pending, showService, editing]);

  const connect = useCallback(async (conn: Connection) => {
    setNotice(null);
    try {
      const info = await api.openSession(conn.id, 100, 30);
      setSessions((prev) => [...prev.filter((s) => s.id !== info.id), info]);
      setActiveId(info.id);
    } catch (e) {
      const err = e as {
        kind?: string;
        host?: string;
        port?: number;
        fingerprint?: string;
      };
      if (err?.kind === "hostKeyUnknown") {
        setPending({
          connectionId: conn.id,
          name: conn.name,
          host: err.host ?? conn.host,
          port: err.port ?? conn.port,
          fingerprint: err.fingerprint ?? "",
        });
      } else {
        setNotice(errorText(e));
      }
    }
  }, []);

  const closeSession = useCallback(async (id: string) => {
    try {
      await api.closeSession(id);
    } catch {
      /* 已经断开了 */
    }
    setSessions((prev) => prev.filter((s) => s.id !== id));
  }, []);

  const acceptHost = useCallback(async () => {
    if (!pending) return;
    const target = pending;
    setPending(null);
    try {
      await api.trustHost(target.host, target.port, target.fingerprint);
      const conn = connections.find((c) => c.id === target.connectionId);
      if (conn) await connect(conn);
    } catch (e) {
      setNotice(errorText(e));
    }
  }, [pending, connections, connect]);

  const removeConnection = useCallback(
    async (conn: Connection) => {
      setDeleting(conn);
    },
    [],
  );

  const confirmDelete = useCallback(async () => {
    if (!deleting) return;
    const conn = deleting;
    setDeleting(null);
    try {
      await api.deleteConnection(conn.id);
      await refresh();
    } catch (e) {
      setNotice(errorText(e));
    }
  }, [deleting, refresh]);

  const groups = Array.from(
    new Set(
      connections
        .map((c) => (c.group ?? "").trim())
        .filter((g) => g.length > 0),
    ),
  ).sort((a, b) => a.localeCompare(b, "zh-CN"));

  return (
    <div className="app">
      <Sidebar
        connections={connections}
        sessions={sessions}
        onConnect={connect}
        onCreate={() => setEditing({ conn: blankConnection(), isNew: true })}
        onEdit={(conn) => setEditing({ conn, isNew: false })}
        onDelete={removeConnection}
        servicePort={daemon?.port ?? null}
        onShowService={() => setShowService(true)}
      />

      <main className="main">
        <div className="main-head">
          <TerminalTabs
            sessions={sessions}
            activeId={activeId}
            onSelect={setActiveId}
            onClose={closeSession}
          />
          <button
            className={`btn-ghost files-toggle ${showFiles ? "active" : ""}`}
            onClick={() => setShowFiles((prev) => !prev)}
            disabled={!activeId}
            title={activeId ? "浏览当前会话主机的文件" : "先打开一个会话"}
          >
            文件
          </button>
        </div>
        <div className="main-body">
        <div className="term-area">
          {sessions.length === 0 && (
            <div className="empty">
              <p>从左侧双击一条连接开始</p>
              <p className="hint">密码保存在 Windows 凭据管理器，不会写入配置文件</p>
            </div>
          )}
          {sessions.map((session) => (
            <div
              key={session.id}
              className="session-view"
              style={{ display: session.id === activeId ? "flex" : "none" }}
            >
              <div className="term-wrap">
                <TerminalPane sessionId={session.id} active={session.id === activeId} />
              </div>
              <CommandPanel sessionId={session.id} />
            </div>
          ))}
        </div>
          {showFiles && activeId && (
            <FileBrowser sessionId={activeId} onClose={() => setShowFiles(false)} />
          )}
        </div>
      </main>

      {editing && (
        <ConnectionDialog
          value={editing.conn}
          isNew={editing.isNew}
          groups={groups}
          onCancel={() => setEditing(null)}
          onSaved={async () => {
            setEditing(null);
            await refresh();
          }}
        />
      )}

      {pending && (
        <div
          className="modal-backdrop"
          onMouseDown={(e) => {
            if (e.target === e.currentTarget) setPending(null);
          }}
        >
          <div className="modal">
            <div className="modal-head">
              <h3>确认主机指纹</h3>
              <button className="modal-close" onClick={() => setPending(null)} title="关闭">
                ✕
              </button>
            </div>
            <p className="hint">
              首次连接 {pending.host}:{pending.port}。请与服务器管理员核对以下指纹，
              一致后再信任；不一致说明可能存在中间人攻击。
            </p>
            <code className="fingerprint">{pending.fingerprint}</code>
            <div className="modal-actions">
              <button className="btn" onClick={() => setPending(null)}>
                取消
              </button>
              <button className="btn primary" onClick={acceptHost}>
                信任并保存
              </button>
            </div>
          </div>
        </div>
      )}

      {showService && (
        <div
          className="modal-backdrop"
          onMouseDown={(e) => {
            if (e.target === e.currentTarget) setShowService(false);
          }}
        >
          <div className="modal">
            <div className="modal-head">
              <h3>本地服务</h3>
              <button className="modal-close" onClick={() => setShowService(false)} title="关闭">
                ✕
              </button>
            </div>
            <p className="hint">
              AI 客户端通过下面的地址与本应用通信。令牌等同于密码，请不要外传。
            </p>
            <div className="form-row">
              <label>基础地址</label>
              <input
                readOnly
                value={daemon?.baseUrl ?? "启动中…"}
                onFocus={(e) => e.currentTarget.select()}
              />
            </div>
            <div className="form-row">
              <label>访问令牌</label>
              <input
                readOnly
                value={daemon?.token ?? ""}
                onFocus={(e) => e.currentTarget.select()}
              />
            </div>
            <div className="modal-actions">
              <button
                className="btn"
                onClick={async () => {
                  if (!daemon) return;
                  const text = `SSHBOX_URL=${daemon.baseUrl}\nSSHBOX_TOKEN=${daemon.token}`;
                  try {
                    await navigator.clipboard.writeText(text);
                    setNotice("已复制服务地址与令牌");
                  } catch {
                    setNotice("复制失败，请手动选中上面的文本");
                  }
                }}
              >
                复制
              </button>
              <button className="btn primary" onClick={() => setShowService(false)}>
                关闭
              </button>
            </div>
          </div>
        </div>
      )}

      {deleting && (
        <div
          className="modal-backdrop"
          onMouseDown={(e) => {
            if (e.target === e.currentTarget) setDeleting(null);
          }}
        >
          <div className="modal" style={{ width: 420 }}>
            <div className="modal-head">
              <h3>删除连接</h3>
              <button className="modal-close" onClick={() => setDeleting(null)} title="关闭">
                ✕
              </button>
            </div>
            <p className="hint">
              确定删除连接「{deleting.name}」吗？同时会删除已保存的凭据，此操作不可撤销。
            </p>
            <div className="modal-actions">
              <button className="btn" onClick={() => setDeleting(null)}>
                取消
              </button>
              <button className="btn danger" onClick={confirmDelete}>
                删除
              </button>
            </div>
          </div>
        </div>
      )}

      {notice && (
        <div className="toast" onClick={() => setNotice(null)} title="点击关闭">
          {notice}
        </div>
      )}
    </div>
  );
}
