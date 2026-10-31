//! SQLite 存储层：连接、文件夹、操作日志统一落库，替代原来的 JSON 文件。
//!
//! 并发模型：`rusqlite::Connection` 内含 `RefCell`，既非 `Send` 也非 `Sync`，
//! 不能放进 tokio 异步任务。因此这里采用「单后台线程 + 命令通道」：
//! - 所有 SQLite 读写都由一条专用 `std::thread` 串行执行；
//! - 调用方通过 `mpsc::Sender` 发命令、通过 `mpsc::Receiver` 取结果；
//! - 这个 `Db` 句柄本身是 `Send + Sync`，可被 `Arc` 共享、进 `tokio::spawn`。
//!
//! 写操作对调用方而言是同步阻塞的（等后台线程返回），但调用点都是低频的
//! 连接/文件夹管理，不会成为瓶颈；日志走批量插入，同样由后台线程完成。

use std::path::Path;
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection as SqliteConnection, OptionalExtension};

use crate::config::Connection;
use crate::error::{Error, Result};

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 操作日志保留天数
pub const LOG_RETENTION_DAYS: i64 = 7;

/// 发给后台线程的请求。响应经 `Responder` 发回调用方。
enum DbRequest {
    ListConnections(Responder<Vec<Connection>>),
    GetConnection(String, Responder<Option<Connection>>),
    UpsertConnection(Connection, Responder<()>),
    DeleteConnection(String, Responder<bool>),
    ListFolders(Responder<Vec<crate::config::Folder>>),
    CreateFolder(Option<String>, String, Responder<String>),
    RenameFolder(String, String, Responder<bool>),
    DeleteFolder(String, Responder<bool>),
    MoveFolder(String, Option<String>, Responder<bool>),
    SetFolderCollapsed(String, bool, Responder<bool>),
    SetConnectionFolder(String, Option<String>, Responder<bool>),
    InsertCommands(Vec<crate::manager::CommandRecord>, Responder<()>),
    InsertFileOps(Vec<crate::manager::FileOpRecord>, Responder<()>),
    PruneLogs(Responder<()>),
    #[allow(dead_code)]
    Shutdown,
}

/// 单向响应通道（后台线程 → 调用方）
type Responder<T> = Sender<std::result::Result<T, String>>;

/// 数据库句柄。`Clone` 只是复制发送端，共享同一条后台线程。
#[derive(Clone)]
pub struct Db {
    tx: Arc<Sender<DbRequest>>,
}

impl Db {
    /// 打开（或新建）数据库并启动后台线程。文件位于 `root/sshbox.db`。
    pub fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        let path = root.join("sshbox.db");
        let first_time = !path.exists();
        let conn = SqliteConnection::open(&path)
            .map_err(|e| Error::Config(format!("打开数据库失败 {}：{e}", path.display())))?;
        let db = Self::spawn(conn);
        // 首次创建时，从旧 connections.json 迁移（若有）
        if first_time {
            db.migrate_from_json(root);
        }
        Ok(db)
    }

    /// 一次性迁移：把旧的 connections.json 里的连接导入 SQLite。
    /// group 字符串按 "/" 分层，每层建一个文件夹，连接挂到最深层。
    fn migrate_from_json(&self, root: &Path) {
        let json_path = root.join("connections.json");
        if !json_path.exists() {
            return;
        }
        let Ok(text) = std::fs::read_to_string(&json_path) else {
            return;
        };
        let text = text.trim_start_matches('\u{feff}');
        let Ok(value): std::result::Result<serde_json::Value, _> = serde_json::from_str(text) else {
            return;
        };
        let Some(conns) = value.get("connections").and_then(|v| v.as_array()) else {
            return;
        };

        // 路径 → folder_id 缓存，避免重复建文件夹
        let mut path_to_id: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        for item in conns {
            let Some(obj) = item.as_object() else { continue };
            let group = obj
                .get("group")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();

            // 把 group 字符串拆成文件夹链
            let mut folder_id: Option<String> = None;
            if !group.is_empty() {
                let mut acc = String::new();
                for segment in group.split('/').map(|s| s.trim()).filter(|s| !s.is_empty()) {
                    acc = if acc.is_empty() {
                        segment.to_string()
                    } else {
                        format!("{acc}/{segment}")
                    };
                    folder_id = match path_to_id.get(&acc) {
                        Some(id) => Some(id.clone()),
                        None => {
                            let parent = folder_id.clone();
                            let id = self
                                .create_folder(parent.as_deref(), segment)
                                .unwrap_or_else(|_| uuid());
                            path_to_id.insert(acc.clone(), id.clone());
                            Some(id)
                        }
                    };
                }
            }

            // 用旧的 Connection 反序列化，再写进 SQLite
            if let Ok(mut conn) = serde_json::from_value::<Connection>(serde_json::Value::Object(obj.clone())) {
                conn.group = folder_id.unwrap_or_default();
                let _ = self.upsert_connection(&conn);
            }
        }
        eprintln!("[db] 已从 connections.json 迁移连接到 SQLite");
    }

    /// 内存数据库（测试用）
    pub fn open_in_memory() -> Result<Self> {
        let conn = SqliteConnection::open_in_memory()
            .map_err(|e| Error::Config(format!("打开内存数据库失败：{e}")))?;
        Ok(Self::spawn(conn))
    }

    fn spawn(mut conn: SqliteConnection) -> Self {
        init_schema(&conn);
        let (tx, rx) = channel::<DbRequest>();
        let thread_tx = Arc::new(tx);

        std::thread::spawn(move || {
            while let Ok(request) = rx.recv() {
                let shutdown = matches!(request, DbRequest::Shutdown);
                handle_request(&mut conn, request);
                if shutdown {
                    break;
                }
            }
        });

        Self { tx: thread_tx }
    }

    /// 发送请求并等待结果
    fn call<T>(&self, build: impl FnOnce(Responder<T>) -> DbRequest) -> Result<T> {
        let (resp_tx, resp_rx) = channel();
        let request = build(resp_tx);
        self.tx
            .send(request)
            .map_err(|_| Error::Config("数据库后台线程已退出".to_string()))?;
        resp_rx
            .recv()
            .map_err(|_| Error::Config("数据库响应丢失".to_string()))?
            .map_err(Error::Config)
    }

    /// 发一条无需等待的命令（日志批量插入用；失败只打日志不影响主流程）
    fn fire(&self, request: DbRequest) {
        let _ = self.tx.send(request);
    }

    // ------------------------------------------------------------ 连接

    pub fn list_connections(&self) -> Result<Vec<Connection>> {
        self.call(DbRequest::ListConnections)
    }

    pub fn get_connection(&self, id: &str) -> Result<Option<Connection>> {
        self.call(|resp| DbRequest::GetConnection(id.to_string(), resp))
    }

    pub fn upsert_connection(&self, conn: &Connection) -> Result<()> {
        self.call(|resp| DbRequest::UpsertConnection(conn.clone(), resp))
    }

    pub fn delete_connection(&self, id: &str) -> Result<bool> {
        self.call(|resp| DbRequest::DeleteConnection(id.to_string(), resp))
    }

    // ------------------------------------------------------------ 文件夹

    pub fn list_folders(&self) -> Result<Vec<crate::config::Folder>> {
        self.call(DbRequest::ListFolders)
    }

    pub fn create_folder(&self, parent_id: Option<&str>, name: &str) -> Result<String> {
        self.call(|resp| {
            DbRequest::CreateFolder(parent_id.map(|s| s.to_string()), name.to_string(), resp)
        })
    }

    pub fn rename_folder(&self, id: &str, name: &str) -> Result<bool> {
        self.call(|resp| DbRequest::RenameFolder(id.to_string(), name.to_string(), resp))
    }

    pub fn delete_folder(&self, id: &str) -> Result<bool> {
        self.call(|resp| DbRequest::DeleteFolder(id.to_string(), resp))
    }

    pub fn move_folder(&self, id: &str, parent_id: Option<&str>) -> Result<bool> {
        self.call(|resp| {
            DbRequest::MoveFolder(id.to_string(), parent_id.map(|s| s.to_string()), resp)
        })
    }

    pub fn set_folder_collapsed(&self, id: &str, collapsed: bool) -> Result<bool> {
        self.call(|resp| DbRequest::SetFolderCollapsed(id.to_string(), collapsed, resp))
    }

    pub fn set_connection_folder(&self, conn_id: &str, folder_id: Option<&str>) -> Result<bool> {
        self.call(|resp| {
            DbRequest::SetConnectionFolder(
                conn_id.to_string(),
                folder_id.map(|s| s.to_string()),
                resp,
            )
        })
    }

    // ------------------------------------------------------------ 日志

    pub fn insert_commands(&self, records: &[crate::manager::CommandRecord]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        self.call(|resp| DbRequest::InsertCommands(records.to_vec(), resp))
    }

    pub fn insert_file_ops(&self, records: &[crate::manager::FileOpRecord]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        self.call(|resp| DbRequest::InsertFileOps(records.to_vec(), resp))
    }

    /// 触发一次过期日志清理（后台线程执行，不阻塞）
    pub fn prune_logs(&self) {
        let (tx, _rx) = channel::<std::result::Result<(), String>>();
        self.fire(DbRequest::PruneLogs(tx));
    }
}

fn init_schema(conn: &SqliteConnection) {
    let _ = conn.execute_batch(
        r#"
        PRAGMA journal_mode = WAL;
        PRAGMA foreign_keys = ON;
        PRAGMA busy_timeout = 3000;

        CREATE TABLE IF NOT EXISTS folders (
            id         TEXT PRIMARY KEY,
            parent_id  TEXT REFERENCES folders(id) ON DELETE CASCADE,
            name       TEXT NOT NULL,
            collapsed  INTEGER NOT NULL DEFAULT 0,
            sort       INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS connections (
            id               TEXT PRIMARY KEY,
            name             TEXT NOT NULL,
            host             TEXT NOT NULL,
            port             INTEGER NOT NULL DEFAULT 22,
            user             TEXT NOT NULL,
            auth_kind        TEXT NOT NULL,
            key_path         TEXT,
            has_passphrase   INTEGER NOT NULL DEFAULT 0,
            credential_ref   TEXT NOT NULL DEFAULT '',
            shell_integration INTEGER NOT NULL DEFAULT 1,
            ai_access        INTEGER NOT NULL DEFAULT 1,
            shell_command    TEXT,
            folder_id        TEXT REFERENCES folders(id) ON DELETE SET NULL,
            tags             TEXT NOT NULL DEFAULT '[]',
            sftp_write_paths TEXT NOT NULL DEFAULT '[]',
            sftp_max_write_bytes INTEGER NOT NULL DEFAULT 0,
            extra            TEXT NOT NULL DEFAULT '{}',
            created_at       INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS commands (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id  TEXT NOT NULL,
            source      TEXT NOT NULL,
            command     TEXT NOT NULL,
            sent_at     INTEGER NOT NULL,
            exit_code   INTEGER,
            duration_ms INTEGER,
            redacted    INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_commands_session ON commands(session_id);
        CREATE INDEX IF NOT EXISTS idx_commands_sent_at ON commands(sent_at);

        CREATE TABLE IF NOT EXISTS file_ops (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            source     TEXT NOT NULL,
            op         TEXT NOT NULL,
            path       TEXT NOT NULL,
            bytes      INTEGER NOT NULL DEFAULT 0,
            ok         INTEGER NOT NULL DEFAULT 0,
            detail     TEXT,
            at         INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_file_ops_session ON file_ops(session_id);
        CREATE INDEX IF NOT EXISTS idx_file_ops_at ON file_ops(at);

        CREATE TABLE IF NOT EXISTS session_meta (
            id           TEXT PRIMARY KEY,
            connection_id TEXT NOT NULL,
            name         TEXT NOT NULL,
            host         TEXT NOT NULL,
            port         INTEGER NOT NULL,
            user         TEXT NOT NULL,
            connected_at INTEGER NOT NULL
        );
        "#,
    );
}

fn handle_request(conn: &SqliteConnection, request: DbRequest) {
    match request {
        DbRequest::ListConnections(resp) => respond(resp, list_connections(conn)),
        DbRequest::GetConnection(id, resp) => respond(resp, get_connection(conn, &id)),
        DbRequest::UpsertConnection(c, resp) => respond(resp, upsert_connection(conn, &c)),
        DbRequest::DeleteConnection(id, resp) => respond(resp, delete_connection(conn, &id)),
        DbRequest::ListFolders(resp) => respond(resp, list_folders(conn)),
        DbRequest::CreateFolder(parent, name, resp) => {
            respond(resp, create_folder(conn, parent.as_deref(), &name))
        }
        DbRequest::RenameFolder(id, name, resp) => {
            respond(resp, rename_folder(conn, &id, &name))
        }
        DbRequest::DeleteFolder(id, resp) => respond(resp, delete_folder(conn, &id)),
        DbRequest::MoveFolder(id, parent, resp) => {
            respond(resp, move_folder(conn, &id, parent.as_deref()))
        }
        DbRequest::SetFolderCollapsed(id, collapsed, resp) => {
            respond(resp, set_folder_collapsed(conn, &id, collapsed))
        }
        DbRequest::SetConnectionFolder(id, folder, resp) => {
            respond(resp, set_connection_folder(conn, &id, folder.as_deref()))
        }
        DbRequest::InsertCommands(records, resp) => {
            respond(resp, insert_commands(conn, &records))
        }
        DbRequest::InsertFileOps(records, resp) => {
            respond(resp, insert_file_ops(conn, &records))
        }
        DbRequest::PruneLogs(_resp) => {
            let _ = prune_logs(conn);
        }
        DbRequest::Shutdown => {}
    }
}

fn respond<T>(tx: Sender<std::result::Result<T, String>>, result: std::result::Result<T, rusqlite::Error>) {
    let _ = tx.send(result.map_err(|e| e.to_string()));
}

// ---------------------------------------------------------------- 具体 SQL

fn row_to_connection(row: &rusqlite::Row) -> std::result::Result<Connection, rusqlite::Error> {
    let auth_kind: String = row.get("auth_kind")?;
    let auth = match auth_kind.as_str() {
        "privateKey" => crate::config::AuthKind::PrivateKey {
            key_path: row.get::<_, Option<String>>("key_path")?.unwrap_or_default(),
            has_passphrase: row.get::<_, i64>("has_passphrase")? != 0,
        },
        _ => crate::config::AuthKind::Password,
    };

    let tags: String = row.get("tags")?;
    let sftp_paths: String = row.get("sftp_write_paths")?;
    let sftp_max: i64 = row.get("sftp_max_write_bytes")?;
    let extra: String = row.get("extra")?;

    let sftp = if sftp_max > 0 || sftp_paths != "[]" {
        Some(crate::config::SftpPolicy {
            write_paths: serde_json::from_str(&sftp_paths).unwrap_or_default(),
            max_write_bytes: sftp_max as u64,
        })
    } else {
        None
    };

    Ok(Connection {
        id: row.get("id")?,
        name: row.get("name")?,
        host: row.get("host")?,
        port: row.get("port")?,
        user: row.get("user")?,
        auth,
        credential_ref: row.get("credential_ref")?,
        shell_integration: row.get::<_, i64>("shell_integration")? != 0,
        ai_access: row.get::<_, i64>("ai_access")? != 0,
        shell_command: row.get("shell_command")?,
        group: row.get::<_, Option<String>>("folder_id")?.unwrap_or_default(),
        tags: serde_json::from_str(&tags).unwrap_or_default(),
        extra: serde_json::from_str(&extra).unwrap_or_default(),
        sftp,
    })
}

const CONN_COLS: &str = "id, name, host, port, user, auth_kind, key_path, has_passphrase,
        credential_ref, shell_integration, ai_access, shell_command,
        folder_id, tags, sftp_write_paths, sftp_max_write_bytes, extra";

fn list_connections(conn: &SqliteConnection) -> std::result::Result<Vec<Connection>, rusqlite::Error> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {CONN_COLS} FROM connections ORDER BY name COLLATE NOCASE"
    ))?;
    let rows = stmt.query_map([], row_to_connection)?;
    rows.collect()
}

fn get_connection(
    conn: &SqliteConnection,
    id: &str,
) -> std::result::Result<Option<Connection>, rusqlite::Error> {
    conn.query_row(
        &format!("SELECT {CONN_COLS} FROM connections WHERE id = ?1"),
        params![id],
        row_to_connection,
    )
    .optional()
}

fn upsert_connection(
    conn: &SqliteConnection,
    c: &Connection,
) -> std::result::Result<(), rusqlite::Error> {
    let auth_kind = match &c.auth {
        crate::config::AuthKind::Password => "password",
        crate::config::AuthKind::PrivateKey { .. } => "privateKey",
    };
    let (key_path, has_passphrase) = match &c.auth {
        crate::config::AuthKind::Password => (None, false),
        crate::config::AuthKind::PrivateKey {
            key_path,
            has_passphrase,
        } => (Some(key_path.clone()), *has_passphrase),
    };
    let tags = serde_json::to_string(&c.tags).unwrap_or_else(|_| "[]".into());
    let (sftp_paths, sftp_max) = match &c.sftp {
        Some(policy) => (
            serde_json::to_string(&policy.write_paths).unwrap_or_else(|_| "[]".into()),
            policy.max_write_bytes,
        ),
        None => ("[]".to_string(), 0u64),
    };
    let extra = serde_json::to_string(&c.extra).unwrap_or_else(|_| "{}".into());
    let folder_id = if c.group.trim().is_empty() {
        None
    } else {
        Some(c.group.trim().to_string())
    };

    conn.execute(
        "INSERT INTO connections (
            id, name, host, port, user, auth_kind, key_path, has_passphrase,
            credential_ref, shell_integration, ai_access, shell_command,
            folder_id, tags, sftp_write_paths, sftp_max_write_bytes, extra, created_at
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
            ?13, ?14, ?15, ?16, ?17, ?18
         )
         ON CONFLICT(id) DO UPDATE SET
            name = excluded.name,
            host = excluded.host,
            port = excluded.port,
            user = excluded.user,
            auth_kind = excluded.auth_kind,
            key_path = excluded.key_path,
            has_passphrase = excluded.has_passphrase,
            credential_ref = excluded.credential_ref,
            shell_integration = excluded.shell_integration,
            ai_access = excluded.ai_access,
            shell_command = excluded.shell_command,
            folder_id = excluded.folder_id,
            tags = excluded.tags,
            sftp_write_paths = excluded.sftp_write_paths,
            sftp_max_write_bytes = excluded.sftp_max_write_bytes,
            extra = excluded.extra",
        params![
            c.id,
            c.name,
            c.host,
            c.port,
            c.user,
            auth_kind,
            key_path,
            has_passphrase as i64,
            c.credential_ref,
            c.shell_integration as i64,
            c.ai_access as i64,
            c.shell_command,
            folder_id,
            tags,
            sftp_paths,
            sftp_max as i64,
            extra,
            now_millis(),
        ],
    )?;
    Ok(())
}

fn delete_connection(conn: &SqliteConnection, id: &str) -> std::result::Result<bool, rusqlite::Error> {
    let n = conn.execute("DELETE FROM connections WHERE id = ?1", params![id])?;
    Ok(n > 0)
}

fn list_folders(conn: &SqliteConnection) -> std::result::Result<Vec<crate::config::Folder>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, parent_id, name, collapsed, sort, created_at
         FROM folders ORDER BY sort, name COLLATE NOCASE",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(crate::config::Folder {
            id: row.get("id")?,
            parent_id: row.get("parent_id")?,
            name: row.get("name")?,
            collapsed: row.get::<_, i64>("collapsed")? != 0,
            sort: row.get("sort")?,
            created_at: row.get("created_at")?,
        })
    })?;
    rows.collect()
}

fn create_folder(
    conn: &SqliteConnection,
    parent_id: Option<&str>,
    name: &str,
) -> std::result::Result<String, rusqlite::Error> {
    let id = uuid();
    conn.execute(
        "INSERT INTO folders (id, parent_id, name, collapsed, sort, created_at)
         VALUES (?1, ?2, ?3, 0, 0, ?4)",
        params![id, parent_id, name, now_millis()],
    )?;
    Ok(id)
}

fn rename_folder(
    conn: &SqliteConnection,
    id: &str,
    name: &str,
) -> std::result::Result<bool, rusqlite::Error> {
    let n = conn.execute("UPDATE folders SET name = ?1 WHERE id = ?2", params![name, id])?;
    Ok(n > 0)
}

fn delete_folder(conn: &SqliteConnection, id: &str) -> std::result::Result<bool, rusqlite::Error> {
    let n = conn.execute("DELETE FROM folders WHERE id = ?1", params![id])?;
    Ok(n > 0)
}

fn move_folder(
    conn: &SqliteConnection,
    id: &str,
    parent_id: Option<&str>,
) -> std::result::Result<bool, rusqlite::Error> {
    let n = conn.execute(
        "UPDATE folders SET parent_id = ?1 WHERE id = ?2",
        params![parent_id, id],
    )?;
    Ok(n > 0)
}

fn set_folder_collapsed(
    conn: &SqliteConnection,
    id: &str,
    collapsed: bool,
) -> std::result::Result<bool, rusqlite::Error> {
    let n = conn.execute(
        "UPDATE folders SET collapsed = ?1 WHERE id = ?2",
        params![collapsed as i64, id],
    )?;
    Ok(n > 0)
}

fn set_connection_folder(
    conn: &SqliteConnection,
    conn_id: &str,
    folder_id: Option<&str>,
) -> std::result::Result<bool, rusqlite::Error> {
    let n = conn.execute(
        "UPDATE connections SET folder_id = ?1 WHERE id = ?2",
        params![folder_id, conn_id],
    )?;
    Ok(n > 0)
}

fn insert_commands(
    conn: &SqliteConnection,
    records: &[crate::manager::CommandRecord],
) -> std::result::Result<(), rusqlite::Error> {
    let mut stmt = conn.prepare(
        "INSERT INTO commands (session_id, source, command, sent_at, exit_code, duration_ms, redacted)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    for r in records {
        stmt.execute(params![
            r.session_id,
            source_str(r.source),
            r.command,
            r.sent_at as i64,
            r.exit_code,
            r.duration_ms.map(|v| v as i64),
            r.redacted as i64,
        ])?;
    }
    Ok(())
}

fn insert_file_ops(
    conn: &SqliteConnection,
    records: &[crate::manager::FileOpRecord],
) -> std::result::Result<(), rusqlite::Error> {
    let mut stmt = conn.prepare(
        "INSERT INTO file_ops (session_id, source, op, path, bytes, ok, detail, at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    for r in records {
        stmt.execute(params![
            r.session_id,
            access_str(r.source),
            r.op,
            r.path,
            r.bytes as i64,
            r.ok as i64,
            r.detail,
            r.at as i64,
        ])?;
    }
    Ok(())
}

fn prune_logs(conn: &SqliteConnection) -> std::result::Result<(), rusqlite::Error> {
    let cutoff = now_millis() - LOG_RETENTION_DAYS * 24 * 3600 * 1000;
    conn.execute("DELETE FROM commands WHERE sent_at < ?1", params![cutoff])?;
    conn.execute("DELETE FROM file_ops WHERE at < ?1", params![cutoff])?;
    Ok(())
}

fn source_str(source: crate::manager::InputSource) -> &'static str {
    match source {
        crate::manager::InputSource::Human => "human",
        crate::manager::InputSource::Ai => "ai",
        crate::manager::InputSource::Terminal => "terminal",
    }
}

fn access_str(source: crate::manager::AccessSource) -> &'static str {
    match source {
        crate::manager::AccessSource::Human => "human",
        crate::manager::AccessSource::Ai => "ai",
    }
}

fn uuid() -> String {
    let mut bytes = [0u8; 16];
    use rand::RngCore;
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuthKind, Connection};

    fn sample_conn(id: &str) -> Connection {
        Connection {
            id: id.to_string(),
            name: "test".to_string(),
            host: "127.0.0.1".to_string(),
            port: 22,
            user: "root".to_string(),
            auth: AuthKind::Password,
            credential_ref: "wincred:conn/test".to_string(),
            shell_integration: true,
            ai_access: true,
            shell_command: None,
            group: String::new(),
            tags: vec![],
            extra: Default::default(),
            sftp: None,
        }
    }

    #[test]
    fn connection_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        // 文件夹必须先存在，连接才能挂到它下面（外键约束）
        let folder = db.create_folder(None, "生产").unwrap();
        let mut conn = sample_conn("a");
        conn.group = folder.clone();
        conn.tags = vec!["prod".to_string()];
        conn.sftp = Some(crate::config::SftpPolicy {
            write_paths: vec!["/tmp".to_string()],
            max_write_bytes: 1024,
        });
        db.upsert_connection(&conn).unwrap();

        let loaded = db.get_connection("a").unwrap().unwrap();
        assert_eq!(loaded.id, "a");
        assert_eq!(loaded.group, folder);
        assert_eq!(loaded.tags, vec!["prod".to_string()]);
        let policy = loaded.sftp.unwrap();
        assert_eq!(policy.write_paths, vec!["/tmp".to_string()]);
        assert_eq!(policy.max_write_bytes, 1024);
    }

    #[test]
    fn private_key_roundtrip() {
        let db = Db::open_in_memory().unwrap();
        let mut conn = sample_conn("k");
        conn.auth = AuthKind::PrivateKey {
            key_path: "C:\\id_ed25519".to_string(),
            has_passphrase: true,
        };
        db.upsert_connection(&conn).unwrap();
        let loaded = db.get_connection("k").unwrap().unwrap();
        match loaded.auth {
            AuthKind::PrivateKey {
                key_path,
                has_passphrase,
            } => {
                assert_eq!(key_path, "C:\\id_ed25519");
                assert!(has_passphrase);
            }
            _ => panic!("expected private key"),
        }
    }

    #[test]
    fn folder_crud() {
        let db = Db::open_in_memory().unwrap();
        let root = db.create_folder(None, "生产").unwrap();
        let child = db.create_folder(Some(&root), "上海").unwrap();
        assert_eq!(db.list_folders().unwrap().len(), 2);

        db.rename_folder(&child, "北京").unwrap();
        assert!(db
            .list_folders()
            .unwrap()
            .iter()
            .any(|f| f.name == "北京"));

        db.set_folder_collapsed(&root, true).unwrap();
        assert!(db.list_folders().unwrap().iter().any(|f| f.collapsed));
    }

    #[test]
    fn log_roundtrip_and_prune() {
        let db = Db::open_in_memory().unwrap();
        let mut rec = crate::manager::CommandRecord {
            id: 1,
            session_id: "s1".to_string(),
            source: crate::manager::InputSource::Human,
            command: "ls".to_string(),
            sent_at: now_millis() as u64,
            exit_code: None,
            duration_ms: None,
            redacted: false,
        };
        db.insert_commands(std::slice::from_ref(&rec)).unwrap();
        // 旧记录
        rec.sent_at = (now_millis() - 8 * 24 * 3600 * 1000) as u64;
        db.insert_commands(std::slice::from_ref(&rec)).unwrap();

        // 等待后台线程处理完再 prune
        std::thread::sleep(std::time::Duration::from_millis(50));
        db.prune_logs();
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    #[test]
    fn migrate_group_path_to_folder_tree() {
        use std::collections::HashMap;
        // 构造临时目录 + 旧格式 connections.json
        let dir = std::env::temp_dir().join(format!("sshbox-migrate-test-{}", uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        let json = r#"{
            "connections": [
                {"id":"a","name":"web-01","host":"10.0.0.1","port":22,"user":"root",
                 "auth":{"kind":"password"},"credentialRef":"wincred:conn/a",
                 "shellIntegration":true,"aiAccess":true,"group":"生产/上海","tags":[]},
                {"id":"b","name":"db-01","host":"10.0.0.2","port":22,"user":"root",
                 "auth":{"kind":"password"},"credentialRef":"wincred:conn/b",
                 "shellIntegration":true,"aiAccess":true,"group":"生产/北京","tags":[]}
            ]
        }"#;
        std::fs::write(dir.join("connections.json"), json).unwrap();

        let db = Db::open(&dir).unwrap();

        // 应该建出 3 个文件夹：生产、生产/上海、生产/北京
        let folders = db.list_folders().unwrap();
        let names: std::collections::HashSet<&str> =
            folders.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains("生产"));
        assert!(names.contains("上海"));
        assert!(names.contains("北京"));

        // 两条连接分别挂到对应的叶子文件夹
        let by_name: HashMap<String, crate::config::Folder> = folders
            .into_iter()
            .map(|f| (f.name.clone(), f))
            .collect();
        let shanghai = &by_name["上海"];
        let beijing = &by_name["北京"];

        let conn_a = db.get_connection("a").unwrap().unwrap();
        let conn_b = db.get_connection("b").unwrap().unwrap();
        assert_eq!(conn_a.group, shanghai.id);
        assert_eq!(conn_b.group, beijing.id);

        std::fs::remove_dir_all(&dir).ok();
    }
}
