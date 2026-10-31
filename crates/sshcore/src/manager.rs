//! 会话池（唯一入口）。
//!
//! 界面（Tauri IPC）和将来的 HTTP/MCP 都通过 `SessionManager` 操作会话：
//! - 输入统一走 `input()`，人类与 AI 最终写入同一个 PTY，天然共享终端
//! - 输出走"广播 + 环形缓冲"：界面晚挂载可以回放，AI 也可按游标续读
//! - 每条会话独占一个 tokio 任务，顺序处理输入、输出与命令识别

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::{broadcast, mpsc, RwLock};
use vt100::Parser;

use crate::config::{AppPaths, Connection, ConnectionStore};
use crate::credential;
use crate::db::Db;
use crate::error::{Error, Result};
use crate::known_hosts::KnownHosts;
use crate::osc::{Osc133, OscScanner};
use crate::pty::{self, ChannelEvent, ConnectParams, PtySession, Target};

/// 人类输入后多久之内，AI 的写入会被拒绝
const HUMAN_PRIORITY_WINDOW: Duration = Duration::from_millis(1500);
/// 每条会话保留多少条命令记录
const HISTORY_LIMIT: usize = 200;
/// 每条会话保留多少条文件操作记录
const FILE_HISTORY_LIMIT: usize = 500;
/// 单独的 Escape 后面多等一会：TUI 会把它和紧跟着的字符当成组合键
const ESCAPE_SETTLE: Duration = Duration::from_millis(120);

static FILE_SEQ: AtomicU64 = AtomicU64::new(1);
/// 输出回放缓冲上限
const BACKLOG_LIMIT: usize = 256 * 1024;

static CMD_SEQ: AtomicU64 = AtomicU64::new(1);

/// 日志批量落库间隔（1 分钟）
const FLUSH_INTERVAL: Duration = Duration::from_secs(60);

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 输入来源。人类优先于 AI。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum InputSource {
    Human,
    Ai,
    /// 终端自动应答（DA/CPR 等转义序列），既不算人类输入，也不受人类优先限制
    Terminal,
}

impl InputSource {
    pub fn parse(value: &str) -> Self {
        match value {
            "ai" => Self::Ai,
            "terminal" => Self::Terminal,
            _ => Self::Human,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandRecord {
    pub id: u64,
    pub session_id: String,
    pub source: InputSource,
    pub command: String,
    /// Unix 毫秒时间戳
    pub sent_at: u64,
    /// 退出码（仅 OSC 133 生效时可得）
    pub exit_code: Option<i32>,
    /// 执行耗时（毫秒，仅 OSC 133 生效时可得）
    pub duration_ms: Option<u64>,
    /// 该条是敏感输入（密码等），内容已隐去
    #[serde(default)]
    pub redacted: bool,
}

/// 会话的终端状态（由 OSC 133 与输出流推导）
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalState {
    /// 是否处于提示符（可以输入）
    pub at_prompt: bool,
    /// 是否真的拿到了 OSC 133 标记（false = 注入未生效，at_prompt 不可信）
    pub integration: bool,
    /// 是否在全屏程序（tmux/vim/less 的 alternate screen）
    pub alt_screen: bool,
    /// 最近一条命令的退出码
    pub last_exit: Option<i32>,
}

/// 终端渲染后的屏幕快照（给 AI 看 vim/tmux/top 这类全屏程序的真实画面）
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenSnapshot {
    pub text: String,
    pub rows: u16,
    pub cols: u16,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub at_prompt: bool,
    pub alt_screen: bool,
}

/// 调用来源：界面（人）或 AI（MCP）。AI 受访问范围约束，人类不受限。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AccessSource {
    Human,
    Ai,
}

/// 文件操作审计记录
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileOpRecord {
    pub id: u64,
    pub session_id: String,
    pub source: AccessSource,
    /// list / stat / read / write / mkdir / remove / rename / download / upload
    pub op: String,
    pub path: String,
    pub bytes: u64,
    pub ok: bool,
    pub detail: Option<String>,
    /// Unix 毫秒
    pub at: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub id: String,
    pub connection_id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub connected_at: u64,
}

#[derive(Debug, Clone)]
pub enum SessionEvent {
    Opened(SessionInfo),
    Output {
        session_id: String,
        /// 该 chunk 结束后的累计字节偏移（游标，供客户端去重/续读）
        cursor: u64,
        data: Vec<u8>,
    },
    Command(CommandRecord),
    /// 命令结束（OSC 133 的 D 标记），带上退出码与耗时
    CommandFinished(CommandRecord),
    /// 一次文件操作（含被拒的），供界面实时展示审计
    FileOp(FileOpRecord),
    Closed {
        session_id: String,
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeInfo {
    pub fingerprint: String,
    pub trusted: bool,
}

enum Cmd {
    Input {
        source: InputSource,
        bytes: Vec<u8>,
        /// 单独指定“要计入命令流水”的字节（None = 与 bytes 相同）
        track: Option<Vec<u8>>,
    },
    Resize {
        cols: u32,
        rows: u32,
    },
    Close,
}

/// 环形输出缓冲：保留最近 N 字节，并记录累计偏移
#[derive(Default)]
pub(crate) struct Backlog {
    data: VecDeque<u8>,
    total: u64,
}

impl Backlog {
    fn push(&mut self, chunk: &[u8]) -> u64 {
        self.data.extend(chunk.iter().copied());
        self.total += chunk.len() as u64;
        while self.data.len() > BACKLOG_LIMIT {
            self.data.pop_front();
        }
        self.total
    }

    fn since(&self, cursor: u64) -> (Vec<u8>, u64) {
        let base = self.total - self.data.len() as u64;
        let from = cursor.max(base);
        let skip = (from - base) as usize;
        (self.data.iter().skip(skip).copied().collect(), self.total)
    }
}

pub(crate) struct Entry {
    info: SessionInfo,
    tx: mpsc::UnboundedSender<Cmd>,
    history: Arc<Mutex<Vec<CommandRecord>>>,
    /// 最近一次人类输入的时刻，用于实现"人类优先"
    last_human_input: Arc<Mutex<Option<Instant>>>,
    /// 输出回放缓冲
    backlog: Arc<Mutex<Backlog>>,
    /// 终端状态（at_prompt / alt_screen / 最近退出码）
    state: Arc<Mutex<TerminalState>>,
    /// vt100 解析器：把字节流渲染成屏幕，供 AI 阅读
    screen: Arc<Mutex<Parser>>,
    /// 共享的 SSH 连接句柄（SFTP 等子系统用，不额外开 TCP）
    handle: crate::pty::ConnectionHandle,
    /// 人工在界面上授予以写权限的目录（会话级，随时可撤）
    write_root: Arc<Mutex<Option<String>>>,
    /// 文件操作审计
    file_history: Arc<Mutex<Vec<FileOpRecord>>>,
}

/// 待落库的日志缓冲（命令 + 文件操作），由定时任务批量写入 SQLite
#[derive(Default)]
struct LogBuffer {
    commands: Vec<CommandRecord>,
    file_ops: Vec<FileOpRecord>,
}

pub struct SessionManager {
    paths: AppPaths,
    pub(crate) sessions: RwLock<HashMap<String, Entry>>,
    pub(crate) events: broadcast::Sender<SessionEvent>,
    /// 串行化 open，避免并发调用重复建会话
    open_lock: tokio::sync::Mutex<()>,
    /// SQLite 数据库句柄（连接/文件夹/日志）
    db: Db,
    /// 待落库日志缓冲
    log_buffer: std::sync::Mutex<LogBuffer>,
}

impl SessionManager {
    pub fn new(paths: AppPaths) -> Arc<Self> {
        let (events, _) = broadcast::channel(1024);
        let db = Db::open(&paths.root).expect("打开 SQLite 数据库失败");
        // 启动时清一次过期日志
        let _ = db.prune_logs();

        let manager = Arc::new(Self {
            paths,
            sessions: RwLock::new(HashMap::new()),
            events,
            open_lock: tokio::sync::Mutex::new(()),
            db,
            log_buffer: std::sync::Mutex::new(LogBuffer::default()),
        });

        // 定时任务：每分钟把缓冲日志落库，并顺带清理过期日志
        let flush_handle = Arc::clone(&manager);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(FLUSH_INTERVAL);
            interval.tick().await; // 第一个 tick 立即触发
            loop {
                interval.tick().await;
                flush_handle.flush_logs();
            }
        });

        manager
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    /// 数据库句柄（供连接/文件夹读写）
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// 立即把缓冲里的日志写进 SQLite（定时任务与退出时调用）
    fn flush_logs(&self) {
        let mut buffer = match self.log_buffer.lock() {
            Ok(buffer) => buffer,
            Err(_) => return,
        };
        if !buffer.commands.is_empty() {
            if let Err(e) = self.db.insert_commands(&buffer.commands) {
                eprintln!("[db] 命令日志落库失败：{e}");
            }
            buffer.commands.clear();
        }
        if !buffer.file_ops.is_empty() {
            if let Err(e) = self.db.insert_file_ops(&buffer.file_ops) {
                eprintln!("[db] 文件操作日志落库失败：{e}");
            }
            buffer.file_ops.clear();
        }
        // 顺带清理过期日志（每分钟一次，成本可忽略）
        self.db.prune_logs();
    }

    /// 记录一条命令到缓冲（由 run_session 与 record_file_op 调用）
    fn buffer_command(&self, record: &CommandRecord) {
        if let Ok(mut buffer) = self.log_buffer.lock() {
            buffer.commands.push(record.clone());
        }
    }

    /// 记录一次文件操作到缓冲
    fn buffer_file_op(&self, record: &FileOpRecord) {
        if let Ok(mut buffer) = self.log_buffer.lock() {
            buffer.file_ops.push(record.clone());
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.events.subscribe()
    }

    pub async fn list(&self) -> Vec<SessionInfo> {
        let mut list: Vec<SessionInfo> = self
            .sessions
            .read()
            .await
            .values()
            .map(|e| e.info.clone())
            .collect();
        list.sort_by_key(|s| s.connected_at);
        list
    }

    /// 读取某会话自 `cursor` 之后的输出（晚挂载的终端与 AI 都走这里）
    pub async fn output_since(&self, id: &str, cursor: u64) -> Result<(Vec<u8>, u64)> {
        let guard = self.sessions.read().await;
        let entry = guard
            .get(id)
            .ok_or_else(|| Error::Other(format!("会话不存在：{id}")))?;
        let backlog = entry
            .backlog
            .lock()
            .map_err(|_| Error::Other("输出缓冲锁失败".to_string()))?;
        Ok(backlog.since(cursor))
    }

    /// 当前输出游标（末尾位置），用于“从现在开始等/读”
    pub async fn cursor(&self, id: &str) -> Result<u64> {
        let guard = self.sessions.read().await;
        let entry = guard
            .get(id)
            .ok_or_else(|| Error::Other(format!("会话不存在：{id}")))?;
        Ok(entry.backlog.lock().map(|g| g.total).unwrap_or(0))
    }

    /// 终端渲染后的屏幕快照（纯文本）：给 AI 看 vim/tmux/top 这类全屏程序的真实画面
    pub async fn screen(&self, id: &str) -> Result<ScreenSnapshot> {
        let guard = self.sessions.read().await;
        let entry = guard
            .get(id)
            .ok_or_else(|| Error::Other(format!("会话不存在：{id}")))?;
        let parser = entry
            .screen
            .lock()
            .map_err(|_| Error::Other("屏幕锁失败".to_string()))?;
        let vt_screen = parser.screen();
        let (rows, cols) = vt_screen.size();
        let (cursor_row, cursor_col) = vt_screen.cursor_position();
        let raw = vt_screen.contents();
        let text = trim_screen_text(&raw);

        let state = entry.state.lock().map(|g| g.clone()).unwrap_or_default();
        Ok(ScreenSnapshot {
            text,
            rows,
            cols,
            cursor_row,
            cursor_col,
            at_prompt: state.at_prompt,
            alt_screen: state.alt_screen,
        })
    }

    /// 终端状态：是否在提示符、是否全屏、最近退出码
    pub async fn state(&self, id: &str) -> Result<TerminalState> {        let guard = self.sessions.read().await;
        let entry = guard
            .get(id)
            .ok_or_else(|| Error::Other(format!("会话不存在：{id}")))?;
        Ok(entry.state.lock().map(|g| g.clone()).unwrap_or_default())
    }

    /// 会话的命令历史（最近 `HISTORY_LIMIT` 条）
    pub async fn history(&self, id: &str) -> Result<Vec<CommandRecord>> {
        let guard = self.sessions.read().await;
        let entry = guard
            .get(id)
            .ok_or_else(|| Error::Other(format!("会话不存在：{id}")))?;
        let list = entry.history.lock().map(|g| g.clone()).unwrap_or_default();
        Ok(list)
    }

    /// 取会话的 SSH 连接句柄（SFTP 等子系统复用同一条 TCP 连接）
    async fn connection_handle(&self, id: &str) -> Result<crate::pty::ConnectionHandle> {
        let guard = self.sessions.read().await;
        let entry = guard
            .get(id)
            .ok_or_else(|| Error::Other(format!("会话不存在：{id}")))?;
        Ok(Arc::clone(&entry.handle))
    }

    /// 列出远端目录（人类不限，AI 受访问范围约束）
    pub async fn sftp_list(
        &self,
        id: &str,
        source: AccessSource,
        path: &str,
    ) -> Result<crate::sftp::Listing> {
        let outcome = async {
            let roots = self.allowed_roots(id, source).await?;
            let handle = self.connection_handle(id).await?;
            crate::sftp::list_dir(&handle, path, &roots).await
        }
        .await;
        self.record_file_op(id, source, "list", path, 0, &outcome).await;
        outcome
    }

    /// 查看远端文件/目录属性
    pub async fn sftp_stat(
        &self,
        id: &str,
        source: AccessSource,
        path: &str,
    ) -> Result<crate::sftp::SftpEntry> {
        let outcome = async {
            let roots = self.allowed_roots(id, source).await?;
            let handle = self.connection_handle(id).await?;
            crate::sftp::stat(&handle, path, &roots).await
        }
        .await;
        self.record_file_op(id, source, "stat", path, 0, &outcome).await;
        outcome
    }

    /// 读取远端文件（带大小上限）
    pub async fn sftp_read(
        &self,
        id: &str,
        source: AccessSource,
        path: &str,
        max_bytes: usize,
    ) -> Result<Vec<u8>> {
        let outcome = async {
            let roots = self.allowed_roots(id, source).await?;
            let handle = self.connection_handle(id).await?;
            crate::sftp::read_file(&handle, path, max_bytes, &roots).await
        }
        .await;
        let bytes = outcome.as_ref().map(|data| data.len() as u64).unwrap_or(0);
        self.record_file_op(id, source, "read", path, bytes, &outcome)
            .await;
        outcome
    }

    /// 取会话对应的连接配置（用于写入策略校验）
    async fn connection_of(&self, id: &str) -> Result<Connection> {
        let connection_id = {
            let guard = self.sessions.read().await;
            let entry = guard
                .get(id)
                .ok_or_else(|| Error::Other(format!("会话不存在：{id}")))?;
            entry.info.connection_id.clone()
        };
        let store = ConnectionStore::load(&self.paths.connections_file())?;
        Ok(store.get(&connection_id)?.clone())
    }

    /// 人工在界面上授予以写权限的目录（传 None 撤销）
    pub async fn set_write_root(&self, id: &str, path: Option<String>) -> Result<Option<String>> {
        let guard = self.sessions.read().await;
        let entry = guard
            .get(id)
            .ok_or_else(|| Error::Other(format!("会话不存在：{id}")))?;
        let mut slot = entry
            .write_root
            .lock()
            .map_err(|_| Error::Other("写权限锁失败".to_string()))?;

        *slot = path
            .map(|value| value.trim().trim_end_matches('/').to_string())
            .filter(|value| !value.is_empty());
        Ok(slot.clone())
    }

    /// 当前会话的可写目录（未授权时为 None）
    pub async fn write_root(&self, id: &str) -> Result<Option<String>> {
        let guard = self.sessions.read().await;
        let entry = guard
            .get(id)
            .ok_or_else(|| Error::Other(format!("会话不存在：{id}")))?;
        let value = entry
            .write_root
            .lock()
            .map(|slot| slot.clone())
            .unwrap_or_default();
        Ok(value)
    }

    /// 本次调用允许访问的真实目录。
    /// 人类不限（用 "/" 表示放行全部）；AI 只拿界面授权目录 + 配置白名单。
    async fn allowed_roots(&self, id: &str, source: AccessSource) -> Result<Vec<String>> {
        if source == AccessSource::Human {
            return Ok(vec!["/".to_string()]);
        }
        let mut roots = Vec::new();
        if let Some(root) = self.write_root(id).await? {
            roots.push(root);
        }
        let conn = self.connection_of(id).await?;
        if let Some(policy) = conn.sftp {
            roots.extend(policy.write_paths);
        }
        Ok(roots)
    }

    /// 记一条文件操作审计（成功失败都记）
    async fn record_file_op<T>(
        &self,
        id: &str,
        source: AccessSource,
        op: &str,
        path: &str,
        bytes: u64,
        outcome: &Result<T>,
    ) {
        // 先取出 Arc 再释放 guard，避免 MutexGuard 临时值跑到 guard 之后才 drop
        let history = {
            let guard = self.sessions.read().await;
            guard.get(id).map(|entry| Arc::clone(&entry.file_history))
        };
        let Some(history) = history else {
            return;
        };

        let record = FileOpRecord {
            id: FILE_SEQ.fetch_add(1, Ordering::Relaxed),
            session_id: id.to_string(),
            source,
            op: op.to_string(),
            path: path.to_string(),
            bytes,
            ok: outcome.is_ok(),
            detail: outcome.as_ref().err().map(|err| err.to_string()),
            at: unix_millis(),
        };

        // 绑定到具名变量，避免 `if let Ok(..) = history.lock()` 的临时值活到函数末尾
        let Ok(mut list) = history.lock() else {
            return;
        };
        list.push(record.clone());
        if list.len() > FILE_HISTORY_LIMIT {
            let drop = list.len() - FILE_HISTORY_LIMIT;
            list.drain(0..drop);
        }
        drop(list);

        // 推给界面（AI 做过的文件操作，人能实时看到）
        let _ = self.events.send(SessionEvent::FileOp(record.clone()));
        // 进落库缓冲（定时批量写 SQLite，带 session_id）
        self.buffer_file_op(&record);
    }

    /// 文件操作历史（新→旧）
    pub async fn file_history(&self, id: &str) -> Result<Vec<FileOpRecord>> {
        let guard = self.sessions.read().await;
        let entry = guard
            .get(id)
            .ok_or_else(|| Error::Other(format!("会话不存在：{id}")))?;
        Ok(entry
            .file_history
            .lock()
            .map(|list| list.clone())
            .unwrap_or_default())
    }

    /// 写远端文件（覆盖）
    pub async fn sftp_write(
        &self,
        id: &str,
        source: AccessSource,
        path: &str,
        data: Vec<u8>,
    ) -> Result<()> {
        let bytes = data.len() as u64;
        let outcome = async {
            let roots = self.allowed_roots(id, source).await?;
            if source == AccessSource::Ai {
                let conn = self.connection_of(id).await?;
                let policy = conn.sftp.clone().unwrap_or_default();
                if bytes > policy.max_write_bytes {
                    return Err(Error::Other(format!(
                        "内容 {bytes} 字节，超过单次写入上限 {}",
                        policy.max_write_bytes
                    )));
                }
            }
            let handle = self.connection_handle(id).await?;
            crate::sftp::write_file(&handle, path, &data, &roots)
                .await
                .map(|_| ())
        }
        .await;
        self.record_file_op(id, source, "write", path, bytes, &outcome)
            .await;
        outcome
    }

    pub async fn sftp_mkdir(
        &self,
        id: &str,
        source: AccessSource,
        path: &str,
    ) -> Result<()> {
        let outcome = async {
            let roots = self.allowed_roots(id, source).await?;
            let handle = self.connection_handle(id).await?;
            crate::sftp::mkdir_p(&handle, path, &roots).await.map(|_| ())
        }
        .await;
        self.record_file_op(id, source, "mkdir", path, 0, &outcome)
            .await;
        outcome
    }

    pub async fn sftp_remove(
        &self,
        id: &str,
        source: AccessSource,
        path: &str,
    ) -> Result<()> {
        let outcome = async {
            let roots = self.allowed_roots(id, source).await?;
            let handle = self.connection_handle(id).await?;
            crate::sftp::remove(&handle, path, &roots).await.map(|_| ())
        }
        .await;
        self.record_file_op(id, source, "remove", path, 0, &outcome)
            .await;
        outcome
    }

    pub async fn sftp_rename(
        &self,
        id: &str,
        source: AccessSource,
        from: &str,
        to: &str,
    ) -> Result<()> {
        let outcome = async {
            let roots = self.allowed_roots(id, source).await?;
            let handle = self.connection_handle(id).await?;
            crate::sftp::rename(&handle, from, to, &roots).await
        }
        .await;
        self.record_file_op(id, source, "rename", from, 0, &outcome)
            .await;
        outcome
    }

    /// 远端路径是否已存在（供写入时判断是否需要 overwrite）
    pub async fn sftp_exists(&self, id: &str, path: &str) -> Result<bool> {
        let handle = self.connection_handle(id).await?;
        Ok(crate::sftp::exists(&handle, path).await)
    }

    async fn connect_params(
        &self,
        connection_id: &str,
        accept_new: bool,
    ) -> Result<(ConnectParams, Connection)> {
        let store = ConnectionStore::load(&self.paths.connections_file())?;
        let conn = store.get(connection_id)?.clone();
        let secret = credential::resolve_secret(&conn)?;
        let known = KnownHosts::load(&self.paths.known_hosts_file())?;
        let params = ConnectParams {
            target: Target::from_connection(&conn, secret),
            known_hosts: known,
            accept_new_host_key: accept_new,
        };
        Ok((params, conn))
    }

    /// 打开会话。未知主机指纹返回 `Error::HostKeyUnknown`，由界面确认后 `trust_host` 重试。
    pub async fn open(
        self: &Arc<Self>,
        connection_id: &str,
        cols: u32,
        rows: u32,
    ) -> Result<SessionInfo> {
        // 并发 open 会遛过重复检查，这里串行化
        let _open_guard = self.open_lock.lock().await;

        // reuse：同一连接已打开时直接返回既有会话，避免重复开 tab
        {
            let guard = self.sessions.read().await;
            if let Some(entry) = guard
                .values()
                .find(|e| e.info.connection_id == connection_id)
            {
                return Ok(entry.info.clone());
            }
        }

        let (params, conn) = self.connect_params(connection_id, false).await?;
        eprintln!("[open] 正在连接 {} ({}@{})", conn.id, conn.user, conn.host);
        // 启动命令优先级：连接配置 > 调试环境变量 > 默认登录 shell
        let shell_command = conn
            .shell_command
            .clone()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .or_else(|| std::env::var("SSHBOX_SHELL_CMD").ok());
        let mut session = match pty::open_pty_with(
            params,
            "xterm-256color",
            cols,
            rows,
            shell_command.as_deref(),
        )
        .await
        {
            Ok(session) => session,
            Err(err) => {
                eprintln!("[open] 失败：{err}");
                return Err(err);
            }
        };
        eprintln!("[open] 已建立会话 {} fp={}", session.id, session.fingerprint);

        // 注入 OSC 133 语义标记（bash/zsh 有效，其他 shell 静默降级）
        if conn.shell_integration {
            match session.install_shell_integration().await {
                Ok(()) => eprintln!("[open] 已注入 OSC 133 shell integration"),
                Err(err) => eprintln!("[open] 注入 shell integration 失败：{err}"),
            }
        }

        let info = SessionInfo {
            id: session.id.clone(),
            connection_id: conn.id.clone(),
            name: conn.name.clone(),
            host: conn.host.clone(),
            port: conn.port,
            user: conn.user.clone(),
            connected_at: unix_now(),
        };

        let history = Arc::new(Mutex::new(Vec::new()));
        let last_human_input: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
        let backlog: Arc<Mutex<Backlog>> = Arc::new(Mutex::new(Backlog::default()));
        let state = Arc::new(Mutex::new(TerminalState {
            integration: conn.shell_integration,
            ..Default::default()
        }));
        let screen = Arc::new(Mutex::new(Parser::new(rows as u16, cols as u16, 0)));
        let connection = session.handle();
        let write_root: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let file_history: Arc<Mutex<Vec<FileOpRecord>>> = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::unbounded_channel();

        self.sessions.write().await.insert(
            info.id.clone(),
            Entry {
                info: info.clone(),
                tx,
                history: Arc::clone(&history),
                last_human_input: Arc::clone(&last_human_input),
                backlog: Arc::clone(&backlog),
                state: Arc::clone(&state),
                screen: Arc::clone(&screen),
                handle: connection,
                write_root: Arc::clone(&write_root),
                file_history: Arc::clone(&file_history),
            },
        );

        tokio::spawn(run_session(
            Arc::clone(self),
            session,
            info.id.clone(),
            rx,
            history,
            last_human_input,
            backlog,
            state,
            screen,
        ));

        // 广播给界面（以及将来的 daemon）：有新会话了
        let _ = self.events.send(SessionEvent::Opened(info.clone()));

        Ok(info)
    }

    pub async fn input(&self, id: &str, source: InputSource, data: Vec<u8>) -> Result<()> {
        let tx = self.input_channel(id, source, data.len()).await?;
        tx.send(Cmd::Input {
            source,
            bytes: data,
            track: None,
        })
        .map_err(|_| Error::Other(format!("会话已结束：{id}")))
    }

    /// 取出发送通道，并按来源做“人类优先”仲裁。
    async fn input_channel(
        &self,
        id: &str,
        source: InputSource,
        byte_len: usize,
    ) -> Result<mpsc::UnboundedSender<Cmd>> {
        let guard = self.sessions.read().await;
        let entry = guard
            .get(id)
            .ok_or_else(|| Error::Other(format!("会话不存在：{id}")))?;

        match source {
            InputSource::Ai => {
                let age = entry
                    .last_human_input
                    .lock()
                    .ok()
                    .and_then(|g| *g)
                    .map(|t| t.elapsed());
                if let Some(age) = age {
                    if age < HUMAN_PRIORITY_WINDOW {
                        eprintln!(
                            "[input] 拒绝 AI 写入：人类输入在 {} ms 前",
                            age.as_millis()
                        );
                        return Err(Error::Other(
                            "人工正在输入该终端，AI 写入已被拒绝".to_string(),
                        ));
                    }
                }
            }
            InputSource::Human => {
                eprintln!("[input] 人类输入 {byte_len} 字节");
                if let Ok(mut slot) = entry.last_human_input.lock() {
                    *slot = Some(Instant::now());
                }
            }
            InputSource::Terminal => {}
        }

        Ok(entry.tx.clone())
    }

    /// 只发送按键（不进入命令流水）：用于 Ctrl-C、方向键、单字符按键等
    pub async fn send_keys(&self, id: &str, source: InputSource, data: Vec<u8>) -> Result<()> {
        let tx = self.input_channel(id, source, data.len()).await?;
        tx.send(Cmd::Input {
            source,
            bytes: data,
            track: Some(Vec::new()),
        })
        .map_err(|_| Error::Other(format!("会话已结束：{id}")))
    }

    /// 逐键发送：每个按键单独写一次，之间留 `gap`。
    ///
    /// 把多个按键一口气写进 PTY 时，TUI 会把连在一起的字节当成一个按键序列：
    /// `ESC` 紧跟下一个字符会被当成 Alt 组合键，`gg` 之类也可能被合成。
    /// 按键之间留出间隔，才能像人敲一样一次一个。
    pub async fn send_keys_paced(
        &self,
        id: &str,
        source: InputSource,
        keys: Vec<Vec<u8>>,
        gap: Duration,
    ) -> Result<()> {
        let byte_len: usize = keys.iter().map(|chunk| chunk.len()).sum();
        let tx = self.input_channel(id, source, byte_len).await?;
        let total = keys.len();

        for (index, chunk) in keys.into_iter().enumerate() {
            let lone_escape = chunk.as_slice() == b"\x1b";
            tx.send(Cmd::Input {
                source,
                bytes: chunk,
                track: Some(Vec::new()),
            })
            .map_err(|_| Error::Other(format!("会话已结束：{id}")))?;

            if index + 1 < total {
                let wait = if lone_escape { gap.max(ESCAPE_SETTLE) } else { gap };
                if !wait.is_zero() {
                    tokio::time::sleep(wait).await;
                }
            }
        }
        Ok(())
    }

    /// 文本 + 按键混合输入：只有文本部分（含回车）进入命令流水。
    /// 按键逐个发送，中间留 `gap`（理由见 `send_keys_paced`）。
    pub async fn input_mixed(
        &self,
        id: &str,
        source: InputSource,
        text: Vec<u8>,
        keys: Vec<Vec<u8>>,
        enter: bool,
        gap: Duration,
    ) -> Result<()> {
        let key_len: usize = keys.iter().map(|chunk| chunk.len()).sum();
        let tx = self
            .input_channel(id, source, text.len() + key_len + usize::from(enter))
            .await?;
        let ended = || Error::Other(format!("会话已结束：{id}"));

        // 文本自己先写：track 与 bytes 一致，行缓冲拿到的就是这行内容
        if !text.is_empty() {
            tx.send(Cmd::Input {
                source,
                bytes: text.clone(),
                track: Some(text),
            })
            .map_err(|_| ended())?;

            if !gap.is_zero() {
                tokio::time::sleep(gap).await;
            }
        }

        let total = keys.len();
        for (index, chunk) in keys.into_iter().enumerate() {
            let lone_escape = chunk.as_slice() == b"\x1b";
            tx.send(Cmd::Input {
                source,
                bytes: chunk,
                track: Some(Vec::new()),
            })
            .map_err(|_| ended())?;

            if index + 1 < total || enter {
                let wait = if lone_escape { gap.max(ESCAPE_SETTLE) } else { gap };
                if !wait.is_zero() {
                    tokio::time::sleep(wait).await;
                }
            }
        }

        // 回车单独发：行缓冲收到它才把这一行记进命令流水
        if enter {
            tx.send(Cmd::Input {
                source,
                bytes: b"\r".to_vec(),
                track: Some(b"\r".to_vec()),
            })
            .map_err(|_| ended())?;
        }
        Ok(())
    }

    pub async fn resize(&self, id: &str, cols: u32, rows: u32) -> Result<()> {
        self.send(id, Cmd::Resize { cols, rows }).await
    }

    pub async fn close(&self, id: &str) -> Result<()> {
        self.send(id, Cmd::Close).await
    }

    async fn send(&self, id: &str, cmd: Cmd) -> Result<()> {
        let guard = self.sessions.read().await;
        let entry = guard
            .get(id)
            .ok_or_else(|| Error::Other(format!("会话不存在：{id}")))?;
        entry
            .tx
            .send(cmd)
            .map_err(|_| Error::Other(format!("会话已结束：{id}")))
    }

    /// 探测连接：连上、取指纹、立刻断开。
    pub async fn probe(&self, connection_id: &str) -> Result<ProbeInfo> {
        let (params, conn) = self.connect_params(connection_id, true).await?;
        let mut session = pty::open_pty(params, "xterm-256color", 80, 24).await?;
        let fingerprint = session.fingerprint.clone();
        let _ = session.close().await;

        let known = KnownHosts::load(&self.paths.known_hosts_file())?;
        Ok(ProbeInfo {
            trusted: known.fingerprint(&conn.host, conn.port) == Some(fingerprint.as_str()),
            fingerprint,
        })
    }

    pub fn trust_host(&self, host: &str, port: u16, fingerprint: &str) -> Result<()> {
        let path = self.paths.known_hosts_file();
        let mut known = KnownHosts::load(&path)?;
        known.trust(host, port, fingerprint);
        known.save(&path)
    }

    pub fn forget_host(&self, host: &str, port: u16) -> Result<bool> {
        let path = self.paths.known_hosts_file();
        let mut known = KnownHosts::load(&path)?;
        let removed = known.forget(host, port);
        known.save(&path)?;
        Ok(removed)
    }
}

/// 删除缓冲区末尾的一个 UTF-8 码点（退格用）
fn pop_char(buf: &mut Vec<u8>) {
    if buf.is_empty() {
        return;
    }
    let mut idx = buf.len() - 1;
    while idx > 0 && (buf[idx] & 0xC0) == 0x80 {
        idx -= 1;
    }
    buf.truncate(idx);
}

/// 当前行输入缓冲。会丢弃 ANSI 转义序列（终端应答、光标移动、OSC 等），
/// 避免 `ESC[11;14R` 这类设备应答混进命令文本。
/// 去掉 ANSI 转义序列，便于做文本判断
fn strip_ansi_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('[') => {
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(next) = chars.next() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' {
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            Some(_) => {}
            None => {}
        }
    }
    out
}

/// 输出最后一行是否是密码/口令提示（用于脱敏下一条输入）
fn looks_like_secret_prompt(tail: &str) -> bool {
    let cleaned = strip_ansi_text(tail);
    let last_line = cleaned.rsplit('\n').next().unwrap_or("").trim();
    if last_line.is_empty() {
        return false;
    }
    let lower = last_line.to_lowercase();
    let asks_secret = lower.contains("password")
        || lower.contains("passphrase")
        || last_line.contains("密码")
        || last_line.contains("口令");
    let ends_with_colon = lower.ends_with(':') || last_line.ends_with('：');
    asks_secret && ends_with_colon
}

fn mark_prompt(state: &Arc<Mutex<TerminalState>>, at_prompt: bool) {    if let Ok(mut guard) = state.lock() {
        guard.at_prompt = at_prompt;
        // 收到任何 133 标记都说明注入生效了
        guard.integration = true;
    }
}

/// 检测 alternate screen 的进入/退出（tmux、vim、less 等全屏程序）
fn update_alt_screen(state: &Arc<Mutex<TerminalState>>, chunk: &[u8]) {
    let enter: [&[u8]; 3] = [b"\x1b[?1049h", b"\x1b[?1047h", b"\x1b[?47h"];
    let leave: [&[u8]; 3] = [b"\x1b[?1049l", b"\x1b[?1047l", b"\x1b[?47l"];

    let mut next = None;
    if enter.iter().any(|pat| contains_sub(chunk, pat)) {
        next = Some(true);
    }
    if leave.iter().any(|pat| contains_sub(chunk, pat)) {
        next = Some(false);
    }
    if let Some(value) = next {
        if let Ok(mut guard) = state.lock() {
            guard.alt_screen = value;
        }
    }
}

fn contains_sub(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

#[derive(Default)]
struct LineBuffer {
    bytes: Vec<u8>,
    state: AnsiState,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum AnsiState {
    #[default]
    Normal,
    Esc,
    Csi,
    Osc,
    OscEsc,
}

impl LineBuffer {
    /// 喂一个字节；遇到回车/换行时返回整行（已剔除转义序列）
    fn feed(&mut self, byte: u8) -> Option<Vec<u8>> {
        match self.state {
            AnsiState::Normal => match byte {
                b'\r' | b'\n' => return Some(std::mem::take(&mut self.bytes)),
                0x1b => self.state = AnsiState::Esc,
                0x7f | 0x08 => pop_char(&mut self.bytes),
                0x03 | 0x15 => self.bytes.clear(),
                b if b >= 0x20 => self.bytes.push(b),
                _ => {}
            },
            AnsiState::Esc => match byte {
                b'[' => self.state = AnsiState::Csi,
                b']' => self.state = AnsiState::Osc,
                0x1b => {}
                _ => self.state = AnsiState::Normal,
            },
            AnsiState::Csi => {
                if (0x40..=0x7e).contains(&byte) {
                    self.state = AnsiState::Normal;
                }
            }
            AnsiState::Osc => match byte {
                0x07 => self.state = AnsiState::Normal,
                0x1b => self.state = AnsiState::OscEsc,
                _ => {}
            },
            AnsiState::OscEsc => match byte {
                b'\\' => self.state = AnsiState::Normal,
                0x1b => {}
                _ => self.state = AnsiState::Osc,
            },
        }
        None
    }
}

async fn run_session(
    manager: Arc<SessionManager>,
    mut session: PtySession,
    id: String,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
    history: Arc<Mutex<Vec<CommandRecord>>>,
    _last_human_input: Arc<Mutex<Option<Instant>>>,
    backlog: Arc<Mutex<Backlog>>,
    state: Arc<Mutex<TerminalState>>,
    screen: Arc<Mutex<Parser>>,
) {
    let events = manager.events.clone();
    let mut line = LineBuffer::default();
    let mut scanner = OscScanner::default();
    // (命令记录 id, 发送时刻)：等 OSC 133 的 D 标记到来后回填退出码
    let mut pending: Option<(u64, u64)> = None;
    // 输出尾部：用于识别密码提示
    let mut out_tail = String::new();
    // 下一个回车要隐去的输入行（密码等）
    let mut mask_next_line = false;
    let mut reason = "unknown";

    eprintln!("[run] 会话任务开始 {id}");

    loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                Some(Cmd::Input { source, bytes, track }) => {
                    if let Err(err) = session.write(&bytes).await {
                        eprintln!("[run] 写入失败：{err}");
                        reason = "write-error";
                        break;
                    }

                    // 一旦有输入，就不再处于提示符
                    if let Ok(mut guard) = state.lock() {
                        guard.at_prompt = false;
                    }

                    // 只把“文本 + 回车”计入命令行（按键序列不参与，避免污染历史）
                    let tracked: &[u8] = track.as_deref().unwrap_or(&bytes);

                    // 按行切命令，回车即记一条（自动丢弃 ANSI 转义序列）
                    for byte in tracked {
                        if let Some(raw) = line.feed(*byte) {
                            let text = String::from_utf8_lossy(&raw).trim().to_string();
                            if !text.is_empty() {
                                // 上一轮输出是密码/口令提示时，这一行内容不进历史
                                let redacted = mask_next_line;
                                mask_next_line = false;

                                let record = CommandRecord {
                                    id: CMD_SEQ.fetch_add(1, Ordering::Relaxed),
                                    session_id: id.clone(),
                                    source,
                                    command: if redacted {
                                        "********".to_string()
                                    } else {
                                        text
                                    },
                                    sent_at: unix_millis(),
                                    exit_code: None,
                                    duration_ms: None,
                                    redacted,
                                };
                                pending = Some((record.id, record.sent_at));
                                if let Ok(mut list) = history.lock() {
                                    list.push(record.clone());
                                    if list.len() > HISTORY_LIMIT {
                                        let drop = list.len() - HISTORY_LIMIT;
                                        list.drain(0..drop);
                                    }
                                }
                                let _ = events.send(SessionEvent::Command(record.clone()));
                                // 进落库缓冲
                                manager.buffer_command(&record);
                            }
                        }
                    }
                }
                Some(Cmd::Resize { cols, rows }) => {
                    let _ = session.resize(cols, rows).await;
                    if let Ok(mut parser) = screen.lock() {
                        parser.set_size(rows as u16, cols as u16);
                    }
                }
                Some(Cmd::Close) => {
                    reason = "user-close";
                    break;
                }
                None => {
                    reason = "cmd-channel-closed";
                    break;
                }
            },
            event = session.next() => match event {
                Some(ChannelEvent::Data(chunk)) | Some(ChannelEvent::ExtendedData(chunk)) => {
                    if let Ok(mut parser) = screen.lock() {
                        parser.process(&chunk);
                    }
                    let cursor = backlog
                        .lock()
                        .map(|mut b| b.push(&chunk))
                        .unwrap_or(0);
                    let _ = events.send(SessionEvent::Output {
                        session_id: id.clone(),
                        cursor,
                        data: chunk.clone(),
                    });

                    // 全屏程序（tmux/vim/less）会切到 alternate screen
                    update_alt_screen(&state, &chunk);

                    // 跟踪输出尾部：识别密码/口令提示，下一行输入要脱敏
                    out_tail.push_str(&String::from_utf8_lossy(&chunk));
                    if out_tail.len() > 512 {
                        let cut = out_tail.len() - 512;
                        let cut = out_tail
                            .char_indices()
                            .map(|(index, _)| index)
                            .find(|&index| index >= cut)
                            .unwrap_or(out_tail.len());
                        out_tail.drain(..cut);
                    }
                    if looks_like_secret_prompt(&out_tail) {
                        mask_next_line = true;
                    }

                    // OSC 133：识别提示符状态、命令结束与退出码
                    let mut marks = Vec::new();
                    scanner.feed(&chunk, &mut marks);
                    for mark in marks {
                        match mark {
                            Osc133::PromptStart => mark_prompt(&state, true),
                            Osc133::CommandStart | Osc133::OutputStart => {
                                mark_prompt(&state, false)
                            }
                            Osc133::CommandEnd(code) => {
                                mark_prompt(&state, true);
                                if let Ok(mut guard) = state.lock() {
                                    guard.last_exit = Some(code);
                                }
                                let Some((record_id, started_at)) = pending.take() else {
                                    continue;
                                };
                                let duration = unix_millis().saturating_sub(started_at);
                                eprintln!(
                                    "[osc] 命令结束 record={record_id} code={code} duration={duration}ms"
                                );
                                let finished = history.lock().ok().and_then(|mut list| {
                                    list.iter_mut().find(|r| r.id == record_id).map(|r| {
                                        r.exit_code = Some(code);
                                        r.duration_ms = Some(duration);
                                        r.clone()
                                    })
                                });
                                if let Some(record) = finished {
                                    let _ = events.send(SessionEvent::CommandFinished(record.clone()));
                                    // 回填退出码/耗时后再落一次库
                                    manager.buffer_command(&record);
                                }
                            }
                        }
                    }
                }
                Some(ChannelEvent::ExitStatus(code)) => {
                    eprintln!("[run] 远端退出码 {code}");
                }
                Some(ChannelEvent::Eof) => {
                    reason = "eof";
                    break;
                }
                Some(ChannelEvent::Closed) => {
                    reason = "closed";
                    break;
                }
                None => {
                    reason = "stream-ended";
                    break;
                }
            },
        }
    }

    eprintln!("[run] 会话任务结束 {id} reason={reason}");
    let _ = session.close().await;
    manager.sessions.write().await.remove(&id);
    let _ = events.send(SessionEvent::Closed {
        session_id: id,
        reason: reason.to_string(),
    });
}

/// 把 vt100 渲染出的整屏文本压紧：去行尾空白、去末尾空行
fn trim_screen_text(raw: &str) -> String {
    let lines: Vec<&str> = raw.lines().map(|line| line.trim_end()).collect();
    let mut end = lines.len();
    while end > 0 && lines[end - 1].is_empty() {
        end -= 1;
    }
    lines[..end].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_trailing_spaces_and_blank_lines() {
        let raw = "hello   \nworld\n   \n\n";
        assert_eq!(trim_screen_text(raw), "hello\nworld");
    }

    #[test]
    fn vt100_renders_color_to_plain_text() {
        let mut parser = Parser::new(3, 20, 0);
        parser.process(b"\x1b[31mred\x1b[0m");
        parser.process(b" ok");
        assert_eq!(trim_screen_text(&parser.screen().contents()), "red ok");
    }

    #[test]
    fn vt100_applies_cursor_moves() {
        let mut parser = Parser::new(3, 20, 0);
        // 写 abc 后光标左移两格，再写 X 覆盖 b
        parser.process(b"abc\x1b[2D\x1b[1;31mX\x1b[0m");
        assert_eq!(trim_screen_text(&parser.screen().contents()), "aXc");
        assert_eq!(parser.screen().cursor_position(), (0, 2));
    }

    #[test]
    fn vt100_clears_line_before_redraw() {
        let mut parser = Parser::new(3, 20, 0);
        parser.process(b"loading...\r\x1b[2Kdone");
        assert_eq!(trim_screen_text(&parser.screen().contents()), "done");
    }

    #[test]
    fn alt_screen_flag_follows_sequences() {
        let state = Arc::new(Mutex::new(TerminalState::default()));
        update_alt_screen(&state, b"\x1b[?1049h");
        assert!(state.lock().map(|g| g.alt_screen).unwrap_or(false));
        update_alt_screen(&state, b"\x1b[?1049l");
        assert!(!state.lock().map(|g| g.alt_screen).unwrap_or(true));
    }

    // ---------------------------------------------------------- 访问范围校验

    fn roots(list: &[&str]) -> Vec<String> {
        list.iter().map(|path| path.to_string()).collect()
    }

    #[test]
    fn empty_scope_denies_everything() {
        // AI 未授权时：空范围 = 拒绝一切
        assert!(!crate::sftp::is_under(&[], "/tmp/a.txt"));
        assert!(!crate::sftp::is_under(&[], "/"));
    }

    #[test]
    fn scope_allows_only_its_subtree() {
        let granted = roots(&["/tmp/sbtest"]);
        assert!(crate::sftp::is_under(&granted, "/tmp/sbtest"));
        assert!(crate::sftp::is_under(&granted, "/tmp/sbtest/a.txt"));
        assert!(crate::sftp::is_under(&granted, "/tmp/sbtest/deep/nested/x"));
        // 前缀边界：/tmp/sbtestfoo 不算 /tmp/sbtest
        assert!(!crate::sftp::is_under(&granted, "/tmp/sbtestfoo/a.txt"));
        assert!(!crate::sftp::is_under(&granted, "/etc/passwd"));
    }

    #[test]
    fn slash_root_means_unrestricted() {
        // 人类调用传 ["/"]，表示不受限
        let anywhere = roots(&["/"]);
        assert!(crate::sftp::is_under(&anywhere, "/"));
        assert!(crate::sftp::is_under(&anywhere, "/etc/passwd"));
        assert!(crate::sftp::is_under(&anywhere, "/root/.ssh/id_rsa"));
    }

    #[test]
    fn symlink_escape_is_outside_scope_after_resolution() {
        // /tmp/sbtest/link -> /root，写入后服务器解析出的真实路径是 /root/escape.txt
        let granted = roots(&["/tmp/sbtest"]);
        assert!(!crate::sftp::is_under(&granted, "/root/escape.txt"));
    }
}
