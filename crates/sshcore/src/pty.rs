//! SSH 连接与 PTY 会话。
//!
//! API 按 `russh 0.45.0` 对齐（以 registry 源码为准）：
//! - `authenticate_password/publickey` 返回 `bool`
//! - `Channel::data` 接收 `AsyncRead`，`Channel::exec` 接收 `Into<Vec<u8>>`
//! - `client::Handler` 是 `#[async_trait]`

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use russh::client::{self, Handle};
use russh::keys::key;
use russh::{Channel, ChannelMsg};

use crate::config::{AuthKind, Connection};
use crate::error::{Error, Result};
use crate::known_hosts::KnownHosts;

/// 连接目标（由 `Connection` + 解析出来的密钥组装）
#[derive(Debug, Clone)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: AuthKind,
    /// 密码，或私钥口令（私钥无口令时为空串）
    pub secret: String,
}

impl Target {
    pub fn from_connection(conn: &Connection, secret: String) -> Self {
        Self {
            host: conn.host.clone(),
            port: conn.port,
            user: conn.user.clone(),
            auth: conn.auth.clone(),
            secret,
        }
    }
}

/// 服务器出示的主机密钥信息（用于 TOFU 落盘）
#[derive(Debug, Clone, Default)]
pub struct ObservedHostKey {
    pub fingerprint: String,
    pub expected: Option<String>,
    pub known: bool,
}

pub(crate) struct ClientHandler {
    host: String,
    port: u16,
    known: Arc<KnownHosts>,
    accept_new: bool,
    observed: Arc<Mutex<ObservedHostKey>>,
}

#[async_trait]
impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &key::PublicKey,
    ) -> std::result::Result<bool, Self::Error> {
        // 0.45 返回裸 base64（无 "SHA256:" 前缀），这里补上前缀便于人眼识别
        let fingerprint = format!("SHA256:{}", server_public_key.fingerprint());
        let expected = self
            .known
            .fingerprint(&self.host, self.port)
            .map(|s| s.to_string());
        let accept = match expected.as_deref() {
            Some(known_fp) => known_fp == fingerprint,
            None => self.accept_new,
        };
        if let Ok(mut slot) = self.observed.lock() {
            *slot = ObservedHostKey {
                known: expected.is_some(),
                expected,
                fingerprint,
            };
        }
        Ok(accept)
    }
}

#[derive(Debug, Clone)]
pub struct ConnectParams {
    pub target: Target,
    pub known_hosts: KnownHosts,
    /// 允许首次连接直接信任（CLI 用 `--accept-new`；GUI 里是确认弹窗）
    pub accept_new_host_key: bool,
}

#[derive(Debug)]
pub enum ChannelEvent {
    Data(Vec<u8>),
    ExtendedData(Vec<u8>),
    ExitStatus(u32),
    Eof,
    Closed,
}

type SessionHandle = Arc<Handle<ClientHandler>>;

/// 供 SFTP 等子系统复用的连接句柄
pub(crate) type ConnectionHandle = SessionHandle;

/// 注入远端的 shell integration 片段。
///
/// 关键约束：**对任何 shell 都必须静默无害**。
/// 所以不用 `if/elif/else`——busybox ash 这种实现会整行解析失败并把
/// `syntax error` 打在用户屏幕上。改成 `[ … ] && { … }` 的守卫形式，
/// 条件不成立时直接短路（ash/dash 下就是空操作）。
const SHELL_INTEGRATION: &str = r#"[ -n "$BASH_VERSION" ] && { PROMPT_COMMAND='printf "\033]133;D;%s\007\033]133;A\007" "$?";'"$PROMPT_COMMAND"; PS0='$(printf "\033]133;C\007")'; }; [ -n "$ZSH_VERSION" ] && { precmd() { printf "\033]133;D;%s\007\033]133;A\007" "$?"; }; preexec() { printf "\033]133;C\007"; }; }; true"#;

async fn establish(params: &ConnectParams) -> Result<(SessionHandle, ObservedHostKey)> {
    let observed = Arc::new(Mutex::new(ObservedHostKey::default()));
    let handler = ClientHandler {
        host: params.target.host.clone(),
        port: params.target.port,
        known: Arc::new(params.known_hosts.clone()),
        accept_new: params.accept_new_host_key,
        observed: observed.clone(),
    };
    let config = Arc::new(client::Config::default());

    let mut handle = match client::connect(
        config,
        (params.target.host.as_str(), params.target.port),
        handler,
    )
    .await
    {
        Ok(handle) => handle,
        Err(err) => {
            let obs = observed.lock().map(|g| g.clone()).unwrap_or_default();
            if !obs.fingerprint.is_empty() {
                if let Some(expected) = obs.expected {
                    return Err(Error::HostKeyMismatch {
                        expected,
                        actual: obs.fingerprint,
                    });
                }
                return Err(Error::HostKeyUnknown {
                    host: params.target.host.clone(),
                    port: params.target.port,
                    fingerprint: obs.fingerprint,
                });
            }
            return Err(Error::Ssh(err));
        }
    };

    let ok = match &params.target.auth {
        AuthKind::Password => {
            handle
                .authenticate_password(
                    params.target.user.as_str(),
                    params.target.secret.as_str(),
                )
                .await?
        }
        AuthKind::PrivateKey {
            key_path,
            has_passphrase,
        } => {
            let passphrase = if *has_passphrase {
                Some(params.target.secret.as_str())
            } else {
                None
            };
            let key_pair = russh::keys::load_secret_key(Path::new(key_path), passphrase)
                .map_err(|e| Error::Other(format!("读取私钥 {key_path} 失败：{e}")))?;
            handle
                .authenticate_publickey(params.target.user.as_str(), Arc::new(key_pair))
                .await?
        }
    };

    if !ok {
        return Err(Error::AuthFailed);
    }

    let obs = observed.lock().map(|g| g.clone()).unwrap_or_default();
    Ok((Arc::new(handle), obs))
}

pub struct PtySession {
    pub id: String,
    pub fingerprint: String,
    handle: SessionHandle,
    channel: Channel<client::Msg>,
}

pub async fn open_pty(
    params: ConnectParams,
    term: &str,
    cols: u32,
    rows: u32,
) -> Result<PtySession> {
    open_pty_with(params, term, cols, rows, None).await
}

/// 同 `open_pty`，但可以用 `shell_command` 代替默认 shell（如 `bash -l`、`tmux attach`）。
pub async fn open_pty_with(
    params: ConnectParams,
    term: &str,
    cols: u32,
    rows: u32,
    shell_command: Option<&str>,
) -> Result<PtySession> {
    let (handle, observed) = establish(&params).await?;
    let channel = handle.channel_open_session().await?;
    channel
        .request_pty(false, term, cols, rows, 0, 0, &[])
        .await?;
    // `shell_command` 若是纯 shell 名（bash/zsh/sh/…），直接 `request_shell`
    // 让服务器启动该用户的登录 shell（交互式、出提示符）；只有真正的
    // “程序命令”（含空格/参数，如 `tmux new -A -s main`）才走 `exec`。
    // 否则 `exec("bash")` 会以非交互方式执行 bash，读完 stdin 立即退出，
    // 终端看起来就是“卡住”且无任何输出。
    let is_plain_shell = shell_command
        .map(|cmd| {
            let t = cmd.trim();
            !t.is_empty()
                && !t.contains(char::is_whitespace)
                && !t.contains('/')
                && matches!(
                    t,
                    "sh" | "bash" | "zsh" | "dash" | "ash" | "fish" | "ksh" | "csh" | "tcsh"
                )
        })
        .unwrap_or(false);
    match shell_command {
        Some(command) if !is_plain_shell => channel.exec(true, command.to_string()).await?,
        _ => channel.request_shell(false).await?,
    }
    Ok(PtySession {
        id: crate::new_session_id(),
        fingerprint: observed.fingerprint,
        handle: handle,
        channel,
    })
}

impl PtySession {
    /// 共享的连接句柄（用于开 SFTP 等子系统 channel）
    pub fn handle(&self) -> ConnectionHandle {
        Arc::clone(&self.handle)
    }
    /// 注入 shell integration（OSC 133）。
    ///
    /// 只对 bash / zsh 生效；busybox ash、dash 等无钩子的 shell 会静默降级
    /// （没有退出码，但会话仍正常工作）。末尾的 `printf '\r\033[K'` 用来
    /// 抹掉命令行回显，避免挡住 MOTD。
    pub async fn install_shell_integration(&mut self) -> Result<()> {
        // 先关回显并清行，避免注入内容刷在用户屏幕上；注入完再恢复
        self.write(b"stty -echo; printf '\\r\\033[K'\r").await?;
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        self.write(SHELL_INTEGRATION.as_bytes()).await?;
        self.write(b"\r").await?;
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        self.write(b"stty echo; printf '\\r\\033[K'\r").await?;
        Ok(())
    }

    pub async fn write(&mut self, data: &[u8]) -> Result<()> {
        self.channel.data(data).await?;
        Ok(())
    }

    pub async fn send_ctrl_c(&mut self) -> Result<()> {
        self.write(b"\x03").await
    }

    pub async fn resize(&mut self, cols: u32, rows: u32) -> Result<()> {
        self.channel.window_change(cols, rows, 0, 0).await?;
        Ok(())
    }

    /// 取下一条事件；连接结束时返回 None
    pub async fn next(&mut self) -> Option<ChannelEvent> {
        loop {
            let msg = self.channel.wait().await?;
            match msg {
                ChannelMsg::Data { data } => return Some(ChannelEvent::Data(data.to_vec())),
                ChannelMsg::ExtendedData { data, .. } => {
                    return Some(ChannelEvent::ExtendedData(data.to_vec()))
                }
                ChannelMsg::ExitStatus { exit_status } => {
                    return Some(ChannelEvent::ExitStatus(exit_status))
                }
                ChannelMsg::Eof => return Some(ChannelEvent::Eof),
                ChannelMsg::Close => return Some(ChannelEvent::Closed),
                _ => continue,
            }
        }
    }

    pub async fn close(&mut self) -> Result<()> {
        let _ = self.channel.eof().await;
        let _ = self
            .handle
            .disconnect(russh::Disconnect::ByApplication, "bye", "zh-CN")
            .await;
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct ExecOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_status: Option<u32>,
    /// 服务器出示的主机指纹，调用方据此落盘 TOFU 记录
    pub fingerprint: String,
}

/// 在独立通道上执行一次性命令（不带 PTY，退出码可信）
pub async fn exec(params: ConnectParams, command: &str) -> Result<ExecOutput> {
    let (handle, observed) = establish(&params).await?;
    let channel = handle.channel_open_session().await?;
    channel.exec(true, command.to_string()).await?;

    let mut out = ExecOutput {
        fingerprint: observed.fingerprint,
        ..Default::default()
    };
    let mut channel = channel;
    while let Some(msg) = channel.wait().await {
        match msg {
            ChannelMsg::Data { data } => out.stdout.extend_from_slice(&data),
            ChannelMsg::ExtendedData { data, .. } => out.stderr.extend_from_slice(&data),
            ChannelMsg::ExitStatus { exit_status } => out.exit_status = Some(exit_status),
            ChannelMsg::Eof | ChannelMsg::Close => break,
            _ => {}
        }
    }
    Ok(out)
}
