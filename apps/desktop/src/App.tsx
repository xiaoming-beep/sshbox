import { useCallback, useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { api, blankConnection, errorText, type Connection, type DaemonInfo, type Folder, type SessionInfo } from "./api";
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

/** 命令面板高度（localStorage 持久化，所有会话共用） */
const PANEL_HEIGHT_KEY = "sshbox.cmdPanelHeight";
const PANEL_HEIGHT_MIN = 60;

function loadPanelHeight(): number {
  const raw = Number(localStorage.getItem(PANEL_HEIGHT_KEY));
  return Number.isFinite(raw) && raw >= PANEL_HEIGHT_MIN ? raw : 156;
}

export default function App() {
  const [connections, setConnections] = useState<Connection[]>([]);
  const [folders, setFolders] = useState<Folder[]>([]);
  const [sessions, setSessions] = useState<SessionInfo[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  const [editing, setEditing] = useState<{ conn: Connection; isNew: boolean } | null>(null);
  const [pending, setPending] = useState<PendingHost | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [daemon, setDaemon] = useState<DaemonInfo | null>(null);
  const [showService, setShowService] = useState(false);
  const [showFiles, setShowFiles] = useState(false);
  const [deleting, setDeleting] = useState<Connection | null>(null);
  const [panelHeight, setPanelHeight] = useState(loadPanelHeight);
  /** 正在重命名的文件夹：id 是 folder id，name 是编辑中的新名字 */
  const [renaming, setRenaming] = useState<{ id: string; name: string } | null>(null);
  /** 正在新建子分组：parentId 是父文件夹 id */
  const [creatingSubfolder, setCreatingSubfolder] = useState<string | null>(null);
  /** 新建子分组时输入的名字 */
  const [subfolderName, setSubfolderName] = useState("");
  /** 正在删除的文件夹 */
  const [deletingFolder, setDeletingFolder] = useState<Folder | null>(null);

  const refresh = useCallback(async () => {
    try {
      const [conns, dirs] = await Promise.all([
        api.listConnections(),
        api.listFolders(),
      ]);
      setConnections(conns);
      setFolders(dirs);
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
    localStorage.setItem(PANEL_HEIGHT_KEY, String(panelHeight));
  }, [panelHeight]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      if (deleting) setDeleting(null);
      else if (deletingFolder) setDeletingFolder(null);
      else if (creatingSubfolder) setCreatingSubfolder(null);
      else if (renaming) setRenaming(null);
      else if (pending) setPending(null);
      else if (showService) setShowService(false);
      else if (editing) setEditing(null);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [deleting, deletingFolder, creatingSubfolder, renaming, pending, showService, editing]);

  /** 拖拽命令面板分隔条：向上拖增高，下限 60px、上限容器 70% */
  const startPanelDrag = useCallback((e: React.PointerEvent<HTMLDivElement>) => {
    e.preventDefault();
    const bar = e.currentTarget;
    const container = bar.parentElement; // session-view
    const startY = e.clientY;
    const startHeight = panelHeight;
    bar.classList.add("dragging");
    document.body.classList.add("cmd-resizing");
    bar.setPointerCapture(e.pointerId);

    const onMove = (ev: PointerEvent) => {
      const max = container ? container.clientHeight * 0.7 : 600;
      const next = Math.min(Math.max(startHeight + (startY - ev.clientY), PANEL_HEIGHT_MIN), max);
      setPanelHeight(Math.round(next));
    };
    const onUp = () => {
      bar.classList.remove("dragging");
      document.body.classList.remove("cmd-resizing");
      bar.removeEventListener("pointermove", onMove);
      bar.removeEventListener("pointerup", onUp);
    };
    bar.addEventListener("pointermove", onMove);
    bar.addEventListener("pointerup", onUp);
  }, [panelHeight]);

  /** 重命名文件夹：直接改 folders 表一行（连接通过 folder_id 外键自动跟随） */
  const confirmRename = useCallback(async () => {
    if (!renaming) return;
    const newName = renaming.name.trim();
    if (!newName) {
      setNotice("分组名不能为空");
      return;
    }
    setRenaming(null);
    try {
      await api.renameFolder(renaming.id, newName);
      await refresh();
      setNotice("已重命名分组");
    } catch (e) {
      setNotice(errorText(e));
    }
  }, [renaming, refresh]);

  /** 新建子分组（空文件夹） */
  const confirmCreateSubfolder = useCallback(async () => {
    if (!creatingSubfolder) return;
    const name = subfolderName.trim();
    if (!name) {
      setNotice("分组名不能为空");
      return;
    }
    setCreatingSubfolder(null);
    setSubfolderName("");
    try {
      await api.createFolder(creatingSubfolder, name);
      await refresh();
      setNotice("已新建子分组");
    } catch (e) {
      setNotice(errorText(e));
    }
  }, [creatingSubfolder, refresh]);

  /** 删除文件夹（子文件夹级联删除，连接置回未分组） */
  const confirmDeleteFolder = useCallback(async () => {
    if (!deletingFolder) return;
    const folder = deletingFolder;
    setDeletingFolder(null);
    try {
      await api.deleteFolder(folder.id);
      await refresh();
      setNotice(`已删除分组「${folder.name}」`);
    } catch (e) {
      setNotice(errorText(e));
    }
  }, [deletingFolder, refresh]);

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

  return (
    <div className="app">
      <Sidebar
        connections={connections}
        folders={folders}
        sessions={sessions}
        onConnect={connect}
        onCreate={() => setEditing({ conn: blankConnection(), isNew: true })}
        onCreateInFolder={(folderId) =>
          setEditing({ conn: { ...blankConnection(), group: folderId }, isNew: true })
        }
        onCreateSubfolder={(parentId) => setCreatingSubfolder(parentId)}
        onRenameFolder={(id) => {
          const folder = folders.find((f) => f.id === id);
          setRenaming({ id, name: folder?.name ?? "" });
        }}
        onDeleteFolder={(id) => {
          const folder = folders.find((f) => f.id === id);
          if (folder) setDeletingFolder(folder);
        }}
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
              <div
                className="cmd-splitter"
                onPointerDown={startPanelDrag}
                title="拖动调整命令面板高度"
              />
              <CommandPanel sessionId={session.id} height={panelHeight} />
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
          folders={folders}
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

      {renaming && (
        <div
          className="modal-backdrop"
          onMouseDown={(e) => {
            if (e.target === e.currentTarget) setRenaming(null);
          }}
        >
          <div className="modal" style={{ width: 420 }}>
            <div className="modal-head">
              <h3>重命名分组</h3>
              <button className="modal-close" onClick={() => setRenaming(null)} title="关闭">
                ✕
              </button>
            </div>
            <p className="hint">
              重命名分组。该分组下的连接和子分组会自动跟随（通过文件夹 id 关联）。
            </p>
            <div className="form-row">
              <label>新名称</label>
              <input
                autoFocus
                value={renaming.name}
                onChange={(e) => setRenaming({ ...renaming, name: e.target.value })}
                onKeyDown={(e) => {
                  if (e.key === "Enter") void confirmRename();
                }}
              />
            </div>
            <div className="modal-actions">
              <button className="btn" onClick={() => setRenaming(null)}>
                取消
              </button>
              <button className="btn primary" onClick={() => void confirmRename()}>
                重命名
              </button>
            </div>
          </div>
        </div>
      )}

      {creatingSubfolder && (
        <div
          className="modal-backdrop"
          onMouseDown={(e) => {
            if (e.target === e.currentTarget) setCreatingSubfolder(null);
          }}
        >
          <div className="modal" style={{ width: 420 }}>
            <div className="modal-head">
              <h3>新建子分组</h3>
              <button className="modal-close" onClick={() => setCreatingSubfolder(null)} title="关闭">
                ✕
              </button>
            </div>
            <p className="hint">在选中的分组下新建一个空文件夹（之后可往里面添加连接）。</p>
            <div className="form-row">
              <label>分组名</label>
              <input
                autoFocus
                value={subfolderName}
                placeholder="例如：生产"
                onChange={(e) => setSubfolderName(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") void confirmCreateSubfolder();
                }}
              />
            </div>
            <div className="modal-actions">
              <button className="btn" onClick={() => setCreatingSubfolder(null)}>
                取消
              </button>
              <button className="btn primary" onClick={() => void confirmCreateSubfolder()}>
                新建
              </button>
            </div>
          </div>
        </div>
      )}

      {deletingFolder && (
        <div
          className="modal-backdrop"
          onMouseDown={(e) => {
            if (e.target === e.currentTarget) setDeletingFolder(null);
          }}
        >
          <div className="modal" style={{ width: 420 }}>
            <div className="modal-head">
              <h3>删除分组</h3>
              <button className="modal-close" onClick={() => setDeletingFolder(null)} title="关闭">
                ✕
              </button>
            </div>
            <p className="hint">
              确定删除分组「{deletingFolder.name}」吗？子分组会级联删除，其中的连接会移到「未分组」。
            </p>
            <div className="modal-actions">
              <button className="btn" onClick={() => setDeletingFolder(null)}>
                取消
              </button>
              <button className="btn danger" onClick={() => void confirmDeleteFolder()}>
                删除
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
