import type { SessionInfo } from "../api";

interface Props {
  sessions: SessionInfo[];
  activeId: string | null;
  onSelect: (id: string) => void;
  onClose: (id: string) => void;
}

export default function TerminalTabs({ sessions, activeId, onSelect, onClose }: Props) {
  return (
    <div className="tabs">
      {sessions.map((session) => (
        <div
          key={session.id}
          className={`tab ${session.id === activeId ? "active" : ""}`}
          onClick={() => onSelect(session.id)}
        >
          <span className="tab-name">{session.name}</span>
          <button
            className="tab-close"
            title="断开"
            onClick={(e) => {
              e.stopPropagation();
              onClose(session.id);
            }}
          >
            ✕
          </button>
        </div>
      ))}
    </div>
  );
}
