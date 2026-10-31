import { useEffect, useMemo, useState } from "react";
import { api, type Connection, type Folder, type SessionInfo } from "../api";

interface Props {
  connections: Connection[];
  folders: Folder[];
  sessions: SessionInfo[];
  servicePort: number | null;
  onShowService: () => void;
  onConnect: (conn: Connection) => void;
  onCreate: () => void;
  /** 在某个分组文件夹下新建连接（folder_id 预填） */
  onCreateInFolder: (folderId: string) => void;
  /** 在某个分组文件夹下新建子文件夹 */
  onCreateSubfolder: (parentId: string) => void;
  /** 重命名分组（id 是文件夹 id） */
  onRenameFolder: (id: string) => void;
  /** 删除分组文件夹 */
  onDeleteFolder: (id: string) => void;
  onEdit: (conn: Connection) => void;
  onDelete: (conn: Connection) => void;
}

interface TreeNode {
  folder: Folder;
  children: TreeNode[];
  conns: Connection[];
}

interface CtxMenu {
  x: number;
  y: number;
  folderId: string;
}

/** 按 parent_id 把文件夹列表构建成树（连接按 folder_id 挂到对应节点） */
function buildTree(folders: Folder[], connections: Connection[]): {
  tree: TreeNode[];
  ungrouped: Connection[];
} {
  const byId = new Map<string, TreeNode>();
  for (const folder of folders) {
    byId.set(folder.id, { folder, children: [], conns: [] });
  }

  const ungrouped: Connection[] = [];
  for (const conn of connections) {
    if (conn.group && byId.has(conn.group)) {
      byId.get(conn.group)!.conns.push(conn);
    } else {
      ungrouped.push(conn);
    }
  }

  const byName = (a: { name: string }, b: { name: string }) =>
    a.name.localeCompare(b.name, "zh-CN");
  ungrouped.sort(byName);

  const roots: TreeNode[] = [];
  for (const node of byId.values()) {
    node.conns.sort(byName);
    if (node.folder.parentId && byId.has(node.folder.parentId)) {
      byId.get(node.folder.parentId)!.children.push(node);
    } else {
      roots.push(node);
    }
  }
  for (const node of byId.values()) {
    node.children.sort((a, b) => a.folder.name.localeCompare(b.folder.name, "zh-CN"));
  }
  roots.sort((a, b) => a.folder.name.localeCompare(b.folder.name, "zh-CN"));
  return { tree: roots, ungrouped };
}

/** 节点下连接总数（含所有子孙文件夹） */
function countConns(node: TreeNode): number {
  return node.conns.length + node.children.reduce((sum, child) => sum + countConns(child), 0);
}

export default function Sidebar({
  connections,
  folders,
  sessions,
  servicePort,
  onShowService,
  onConnect,
  onCreate,
  onCreateInFolder,
  onCreateSubfolder,
  onRenameFolder,
  onDeleteFolder,
  onEdit,
  onDelete,
}: Props) {
  const { ungrouped, tree } = useMemo(
    () => buildTree(folders, connections),
    [folders, connections],
  );
  const [menu, setMenu] = useState<CtxMenu | null>(null);
  const openIds = useMemo(() => new Set(sessions.map((s) => s.connectionId)), [sessions]);

  /** 折叠状态：以文件夹自带的 collapsed 为初始值，运行时变更写回后端持久化 */
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  useEffect(() => {
    setCollapsed(
      new Set(folders.filter((f) => f.collapsed).map((f) => f.id)),
    );
  }, [folders]);

  const isCollapsed = (folder: Folder) => collapsed.has(folder.id);

  const toggle = (folder: Folder) => {
    const nextCollapsed = !collapsed.has(folder.id);
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (nextCollapsed) next.add(folder.id);
      else next.delete(folder.id);
      return next;
    });
    // 写回后端，下次启动恢复折叠状态
    void api.setFolderCollapsed(folder.id, nextCollapsed).catch(() => {});
  };

  const renderConn = (conn: Connection, depth: number) => (
    <div
      key={conn.id}
      className={`conn-item ${openIds.has(conn.id) ? "is-open" : ""}`}
      style={{ paddingLeft: 8 + depth * 18 }}
      title={`${conn.user}@${conn.host}:${conn.port}`}
      onDoubleClick={() => onConnect(conn)}
    >
      <span className="host-icon">🖥️</span>
      <div className="conn-meta" onClick={() => onConnect(conn)}>
        <span className="conn-name">{conn.name}</span>
        <span className="conn-sub">
          {conn.user}@{conn.host}:{conn.port}
        </span>
      </div>
      {openIds.has(conn.id) && <span className="conn-dot" />}
      <button
        className="icon-btn"
        title="编辑"
        onClick={(e) => {
          e.stopPropagation();
          onEdit(conn);
        }}
      >
        ✎
      </button>
      <button
        className="icon-btn"
        title="删除"
        onClick={(e) => {
          e.stopPropagation();
          onDelete(conn);
        }}
      >
        ✕
      </button>
    </div>
  );

  const renderNode = (node: TreeNode, depth: number): React.ReactNode => {
    const { folder } = node;
    const collapsedNow = isCollapsed(folder);
    return (
      <div key={folder.id}>
        <div
          className="folder-row"
          style={{ paddingLeft: 8 + depth * 14 }}
          onClick={() => toggle(folder)}
          onContextMenu={(e) => {
            e.preventDefault();
            setMenu({ x: e.clientX, y: e.clientY, folderId: folder.id });
          }}
          title={`${folder.name}（右键：新建连接 / 新建子分组 / 重命名 / 删除）`}
        >
          <span className="caret">{collapsedNow ? "▸" : "▾"}</span>
          <span className="folder-icon">{collapsedNow ? "📁" : "📂"}</span>
          <span className="folder-name">{folder.name}</span>
          <span className="folder-count">{countConns(node)}</span>
        </div>
        {!collapsedNow && (
          <>
            {node.children.map((child) => renderNode(child, depth + 1))}
            {node.conns.map((conn) => renderConn(conn, depth + 1))}
          </>
        )}
      </div>
    );
  };

  return (
    <aside className="sidebar">
      <div className="sidebar-head">
        <span>连接</span>
        <button className="btn-ghost" onClick={onCreate}>
          + 新建
        </button>
      </div>

      <div className="conn-list">
        {connections.length === 0 && folders.length === 0 && (
          <p className="hint" style={{ padding: "0 8px" }}>
            还没有连接。点「+ 新建」添加第一台主机。
          </p>
        )}

        {ungrouped.map((conn) => renderConn(conn, 0))}
        {tree.map((node) => renderNode(node, 0))}
      </div>

      <div className="sidebar-foot" onClick={onShowService} title="查看本地服务地址与访问令牌">
        <span className="foot-dot" />
        <span>本地服务 {servicePort ?? "…"}</span>
      </div>

      {menu && (
        <>
          <div
            className="ctx-overlay"
            onMouseDown={() => setMenu(null)}
            onContextMenu={(e) => {
              e.preventDefault();
              setMenu(null);
            }}
          />
          <div
            className="ctx-menu"
            style={{
              left: Math.min(menu.x, window.innerWidth - 160),
              top: Math.min(menu.y, window.innerHeight - 130),
            }}
          >
            <button
              className="ctx-item"
              onClick={() => {
                setMenu(null);
                onCreateInFolder(menu.folderId);
              }}
            >
              在此新建连接
            </button>
            <button
              className="ctx-item"
              onClick={() => {
                setMenu(null);
                onCreateSubfolder(menu.folderId);
              }}
            >
              新建子分组
            </button>
            <button
              className="ctx-item"
              onClick={() => {
                setMenu(null);
                onRenameFolder(menu.folderId);
              }}
            >
              重命名分组
            </button>
            <button
              className="ctx-item"
              onClick={() => {
                setMenu(null);
                onDeleteFolder(menu.folderId);
              }}
            >
              删除分组
            </button>
          </div>
        </>
      )}
    </aside>
  );
}
