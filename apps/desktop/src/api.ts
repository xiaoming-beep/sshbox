import { invoke } from "@tauri-apps/api/core";

export type AuthKind =
  | { kind: "password" }
  | { kind: "privateKey"; keyPath: string; hasPassphrase: boolean };

export interface Connection {
  id: string;
  name: string;
  host: string;
  port: number;
  user: string;
  auth: AuthKind;
  credentialRef: string;
  shellIntegration: boolean;
  aiAccess: boolean;
  /** 启动命令（代替默认登录 shell），如 bash、tmux new -A -s main */
  shellCommand?: string | null;
  /** 分组路径，如 `自己的服务器/生产`；空表示未分组 */
  group: string;
  tags: string[];
}

export type InputSource = "human" | "ai" | "terminal";

/** 终端里执行过的一条命令（由内核按行切出） */
export interface CommandRecord {
  id: number;
  sessionId: string;
  source: InputSource;
  command: string;
  sentAt: number;
  /** 退出码，仅 OSC 133 生效时可得 */
  exitCode: number | null;
  /** 执行耗时（毫秒） */
  durationMs: number | null;
  /** 敏感输入（密码等），内容已隐去 */
  redacted: boolean;
}

export interface SftpEntry {
  name: string;
  path: string;
  isDir: boolean;
  size: number;
  mode: number | null;
  modified: number | null;
}

export interface DaemonInfo {
  port: number;
  token: string;
  baseUrl: string;
}

export interface SessionInfo {
  id: string;
  connectionId: string;
  name: string;
  host: string;
  port: number;
  user: string;
  connectedAt: number;
}

export interface ProbeInfo {
  fingerprint: string;
  trusted: boolean;
}

export type CmdError =
  | { kind: "hostKeyUnknown"; host: string; port: number; fingerprint: string }
  | { kind: "hostKeyMismatch"; expected: string; actual: string }
  | { kind: "message"; message: string };

export function errorText(err: unknown): string {
  const e = err as Partial<CmdError> | undefined;
  if (!e || typeof e !== "object") return String(err);
  switch (e.kind) {
    case "hostKeyMismatch":
      return [
        "主机指纹不匹配，连接已拒绝。",
        `已记录：${(e as { expected: string }).expected}`,
        `实际：${(e as { actual: string }).actual}`,
        "请核对后在 known_hosts.json 中移除该主机，或确认是否遭到中间人攻击。",
      ].join("\n");
    case "hostKeyUnknown":
      return "首次连接该主机，需要确认主机指纹。";
    case "message":
      return (e as { message: string }).message;
    default:
      return JSON.stringify(err);
  }
}

export const api = {
  listConnections: () => invoke<Connection[]>("list_connections"),
  saveConnection: (conn: Connection) => invoke<Connection>("save_connection", { conn }),
  deleteConnection: (id: string) => invoke<void>("delete_connection", { id }),
  setSecret: (id: string, secret: string) => invoke<void>("set_secret", { id, secret }),
  hasSecret: (id: string) => invoke<boolean>("has_secret", { id }),
  probeConnection: (connectionId: string) =>
    invoke<ProbeInfo>("probe_connection", { connectionId }),
  trustHost: (host: string, port: number, fingerprint: string) =>
    invoke<void>("trust_host", { host, port, fingerprint }),
  forgetHost: (host: string, port: number) => invoke<boolean>("forget_host", { host, port }),

  listSessions: () => invoke<SessionInfo[]>("list_sessions"),
  daemonInfo: () => invoke<DaemonInfo>("daemon_info"),
  openSession: (connectionId: string, cols: number, rows: number) =>
    invoke<SessionInfo>("open_session", { connectionId, cols, rows }),
  input: (id: string, data: string, source: InputSource = "human") =>
    invoke<void>("session_input", { id, data, source }),
  sessionHistory: (id: string) => invoke<CommandRecord[]>("session_history", { id }),
  /** 读取自 cursor 起的输出（终端晚挂载时回放） */
  sessionRead: (id: string, cursor: number) =>
    invoke<{ data: string; cursor: number }>("session_read", { id, cursor }),
  resize: (id: string, cols: number, rows: number) =>
    invoke<void>("session_resize", { id, cols, rows }),
  closeSession: (id: string) => invoke<void>("close_session", { id }),
  sftpList: (id: string, path: string) => invoke<SftpEntry[]>("sftp_list", { id, path }),
  sftpStat: (id: string, path: string) => invoke<SftpEntry>("sftp_stat", { id, path }),
  sftpDownload: (id: string, path: string) => invoke<string>("sftp_download", { id, path }),
  setWriteRoot: (id: string, path: string | null) =>
    invoke<string | null>("set_write_root", { id, path }),
  sftpWrite: (id: string, path: string, base64: string, overwrite = false) =>
    invoke<void>("sftp_write", { id, path, base64, overwrite }),
  sftpMkdir: (id: string, path: string) => invoke<void>("sftp_mkdir", { id, path }),
  sftpRemove: (id: string, path: string) => invoke<void>("sftp_remove", { id, path }),
  sftpRename: (id: string, from: string, to: string) =>
    invoke<void>("sftp_rename", { id, from, to }),
  debugLog: (message: string) => invoke<void>("debug_log", { message }),
};

export function b64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i += 1) out[i] = bin.charCodeAt(i);
  return out;
}

export function blankConnection(): Connection {
  return {
    id: "",
    name: "",
    host: "",
    port: 22,
    user: "",
    auth: { kind: "password" },
    credentialRef: "",
    shellIntegration: true,
    aiAccess: true,
    shellCommand: "",
    group: "",
    tags: [],
  };
}
