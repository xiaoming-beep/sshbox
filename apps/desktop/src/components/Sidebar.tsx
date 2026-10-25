import { useMemo, useState } from "react";
import type { Connection, SessionInfo } from "../api";

interface Props {
  connections: Connection[];
  sessions: SessionInfo[];
  servicePort: number | null;
  onShowService: () => void;
  onConnect: (conn: Connection) => void;
  onCreate: () => void;
  onEdit: (conn: Connection) => void;
  onDelete: (conn: Connection) => void;
}

interface Group {
  name: string;
  conns: Connection[];
}

/** 一级分组：group 字符串就是文件夹名，空表示未分组 */
function groupConnections(connections: Connection[]) {
  const buckets = new Map<string, Connection[]>();
  const ungrouped: Connection[] = [];

  for (const conn of connections) {
    const name = (conn.group ?? "").trim();
    if (!name) {
      ungrouped.push(conn);
    } else {
      const list = buckets.get(name) ?? [];
      list.push(conn);
      buckets.set(name, list);
    }
  }

  const byName = (a: Connection, b: Connection) => a.name.localeCompare(b.name, "zh-CN");
  ungrouped.sort(byName);

  const groups: Group[] = Array.from(buckets, ([name, conns]) => ({
    name,
    conns: conns.sort(byName),
  })).sort((a, b) => a.name.localeCompare(b.name, "zh-CN"));

  return { ungrouped, groups };
}

export default function Sidebar({
  connections,
  sessions,
  servicePort,
  onShowService,
  onConnect,
  onCreate,
  onEdit,
  onDelete,
}: Props) {
  const { ungrouped, groups } = useMemo(() => groupConnections(connections), [connections]);
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const openIds = useMemo(() => new Set(sessions.map((s) => s.connectionId)), [sessions]);

  const toggle = (name: string) => {
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(name)) next.delete(name);
      else next.add(name);
      return next;
    });
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

  return (
    <aside className="sidebar">
      <div className="sidebar-head">
        <span>连接</span>
        <button className="btn-ghost" onClick={onCreate}>
          + 新建
        </button>
      </div>

      <div className="conn-list">
        {connections.length === 0 && (
          <p className="hint" style={{ padding: "0 8px" }}>
            还没有连接。点「+ 新建」添加第一台主机。
          </p>
        )}

        {ungrouped.map((conn) => renderConn(conn, 0))}

        {groups.map((group) => {
          const isCollapsed = collapsed.has(group.name);
          return (
            <div key={group.name}>
              <div
                className="folder-row"
                style={{ paddingLeft: 8 }}
                onClick={() => toggle(group.name)}
                title={isCollapsed ? "展开" : "折叠"}
              >
                <span className="caret">{isCollapsed ? "▸" : "▾"}</span>
                <span className="folder-icon">{isCollapsed ? "📁" : "📂"}</span>
                <span className="folder-name">{group.name}</span>
                <span className="folder-count">{group.conns.length}</span>
              </div>
              {!isCollapsed && group.conns.map((conn) => renderConn(conn, 1))}
            </div>
          );
        })}
      </div>

      <div className="sidebar-foot" onClick={onShowService} title="查看本地服务地址与访问令牌">
        <span className="foot-dot" />
        <span>本地服务 {servicePort ?? "…"}</span>
      </div>
    </aside>
  );
}
