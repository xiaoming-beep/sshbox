import { useState } from "react";
import { api, errorText, type Connection } from "../api";

interface Props {
  value: Connection;
  isNew: boolean;
  groups: string[];
  onCancel: () => void;
  onSaved: (conn: Connection) => void;
}

export default function ConnectionDialog({ value, isNew, groups, onCancel, onSaved }: Props) {
  const [form, setForm] = useState<Connection>({ ...value, group: value.group ?? "" });
  const [secret, setSecret] = useState("");
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [ok, setOk] = useState(false);

  const patch = (p: Partial<Connection>) => setForm((f) => ({ ...f, ...p }));

  const switchAuth = (kind: "password" | "privateKey") => {
    patch({
      auth:
        kind === "password"
          ? { kind: "password" }
          : { kind: "privateKey", keyPath: "", hasPassphrase: false },
    });
  };

  async function persist(): Promise<Connection> {
    const saved = await api.saveConnection(form);
    if (secret) await api.setSecret(saved.id, secret);
    return saved;
  }

  async function handleSave() {
    setBusy(true);
    setMessage(null);
    try {
      const saved = await persist();
      onSaved(saved);
    } catch (e) {
      setOk(false);
      setMessage(errorText(e));
    } finally {
      setBusy(false);
    }
  }

  async function handleProbe() {
    setBusy(true);
    setMessage(null);
    try {
      const saved = await persist();
      const info = await api.probeConnection(saved.id);
      setOk(true);
      setMessage(
        info.trusted
          ? `连接成功。主机指纹（已记录）：${info.fingerprint}`
          : `连接成功。新主机指纹：${info.fingerprint}`,
      );
    } catch (e) {
      setOk(false);
      setMessage(errorText(e));
    } finally {
      setBusy(false);
    }
  }

  const auth = form.auth;

  return (
    <div
      className="modal-backdrop"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget && !busy) onCancel();
      }}
    >
      <div className="modal">
        <div className="modal-head">
          <h3>{isNew ? "新建连接" : `编辑连接：${value.name}`}</h3>
          <button className="modal-close" onClick={onCancel} title="关闭" disabled={busy}>
            ✕
          </button>
        </div>

        <div className="form-row">
          <label>名称</label>
          <input
            value={form.name}
            autoFocus
            placeholder="例如：测试机 web-01"
            onChange={(e) => patch({ name: e.target.value })}
          />
        </div>

        <div className="form-row">
          <label>分组（可选，留空则不分组）</label>
          <input
            list="sshbox-groups"
            value={form.group ?? ""}
            placeholder="例如：自己的服务器"
            onChange={(e) => patch({ group: e.target.value })}
          />
          <datalist id="sshbox-groups">
            {groups.map((g) => (
              <option key={g} value={g} />
            ))}
          </datalist>
        </div>

        <div className="form-grid">
          <div className="form-row">
            <label>主机</label>
            <input
              value={form.host}
              placeholder="10.0.0.11 或 example.com"
              onChange={(e) => patch({ host: e.target.value })}
            />
          </div>
          <div className="form-row">
            <label>端口</label>
            <input
              type="number"
              value={form.port}
              onChange={(e) => patch({ port: Number(e.target.value) || 22 })}
            />
          </div>
        </div>

        <div className="form-grid">
          <div className="form-row">
            <label>用户名</label>
            <input
              value={form.user}
              placeholder="deploy"
              onChange={(e) => patch({ user: e.target.value })}
            />
          </div>
          <div className="form-row">
            <label>认证方式</label>
            <select
              value={auth.kind}
              onChange={(e) => switchAuth(e.target.value as "password" | "privateKey")}
            >
              <option value="password">密码</option>
              <option value="privateKey">私钥</option>
            </select>
          </div>
        </div>

        <div className="form-row">
          <label>启动命令（可选，代替默认登录 shell）</label>
          <input
            value={form.shellCommand ?? ""}
            placeholder="例如 bash，或 tmux new -A -s main"
            onChange={(e) => patch({ shellCommand: e.target.value })}
          />
        </div>

        {auth.kind === "privateKey" && (
          <>
            <div className="form-row">
              <label>私钥路径</label>
              <input
                value={auth.keyPath}
                placeholder="C:\Users\you\.ssh\id_ed25519"
                onChange={(e) => patch({ auth: { ...auth, keyPath: e.target.value } })}
              />
            </div>
            <label className="checkbox">
              <input
                type="checkbox"
                checked={auth.hasPassphrase}
                onChange={(e) => patch({ auth: { ...auth, hasPassphrase: e.target.checked } })}
              />
              私钥有口令
            </label>
          </>
        )}

        <div className="form-row">
          <label>
            {auth.kind === "password"
              ? "密码（留空则保留已保存的凭据）"
              : "私钥口令（留空则保留已保存的凭据）"}
          </label>
          <input
            type="password"
            value={secret}
            placeholder="保存到 Windows 凭据管理器"
            onChange={(e) => setSecret(e.target.value)}
          />
        </div>

        <label className="checkbox">
          <input
            type="checkbox"
            checked={form.aiAccess}
            onChange={(e) => patch({ aiAccess: e.target.checked })}
          />
          允许 AI 访问该连接
        </label>

        {message && (
          <p className={`hint ${ok ? "ok-text" : "err-text"}`} style={{ whiteSpace: "pre-wrap" }}>
            {message}
          </p>
        )}

        <div className="modal-actions">
          <button className="btn" onClick={handleProbe} disabled={busy}>
            测试连接
          </button>
          <button className="btn" onClick={onCancel} disabled={busy}>
            取消
          </button>
          <button className="btn primary" onClick={handleSave} disabled={busy}>
            保存
          </button>
        </div>
      </div>
    </div>
  );
}
