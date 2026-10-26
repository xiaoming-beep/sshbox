//! SSH Box 内核。
//!
//! 不依赖任何 UI 框架，可被桌面应用（Tauri IPC）、HTTP 服务（MCP/REST）和 CLI 共同驱动。
//!
//! 当前阶段（M1）：连接配置、Windows 凭据、主机指纹 TOFU、PTY 会话、一次性 exec。

pub mod config;
pub mod credential;
pub mod error;
pub mod known_hosts;
pub mod manager;
pub mod osc;
pub mod pty;
pub mod sftp;

pub use config::{AppPaths, AuthKind, Connection, ConnectionStore};
pub use error::{Error, Result};
pub use known_hosts::KnownHosts;
pub use manager::{CommandRecord, InputSource, ProbeInfo, ScreenSnapshot, SessionEvent, SessionInfo, SessionManager, TerminalState};
pub use pty::{ChannelEvent, ConnectParams, ExecOutput, PtySession, Target};

/// 生成会话 ID（M1 用时间戳，接入 daemon 后改为由会话池分配）。
pub fn new_session_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("s-{nanos:x}")
}
