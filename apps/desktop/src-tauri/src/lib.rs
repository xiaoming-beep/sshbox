//! Tauri 外壳：把 `SessionManager` 暴露成 IPC 命令，并把会话事件转发成窗口事件。
//!
//! 这里不含 SSH 逻辑——所有能力都在 `sshcore` 里，将来 daemon/MCP 复用同一套。

use std::sync::Arc;

use base64::Engine as _;
use serde::Serialize;
use sshcore::config::{AppPaths, Connection, ConnectionStore};
use sshcore::manager::{CommandRecord, InputSource, ProbeInfo, SessionEvent, SessionInfo, SessionManager};
use sshcore::{credential, Error as CoreError};
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, State, WindowEvent,
};

struct AppState {
    paths: AppPaths,
    sessions: Arc<SessionManager>,
}

/// 前端可判别的错误类型
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum CmdError {
    HostKeyUnknown {
        host: String,
        port: u16,
        fingerprint: String,
    },
    HostKeyMismatch {
        expected: String,
        actual: String,
    },
    Message {
        message: String,
    },
}

impl From<CoreError> for CmdError {
    fn from(err: CoreError) -> Self {
        match err {
            CoreError::HostKeyUnknown {
                host,
                port,
                fingerprint,
            } => Self::HostKeyUnknown {
                host,
                port,
                fingerprint,
            },
            CoreError::HostKeyMismatch { expected, actual } => {
                Self::HostKeyMismatch { expected, actual }
            }
            other => Self::Message {
                message: other.to_string(),
            },
        }
    }
}

type CmdResult<T> = Result<T, CmdError>;

// ---------------------------------------------------------------- 连接管理

#[tauri::command]
fn list_connections(state: State<'_, AppState>) -> CmdResult<Vec<Connection>> {
    let store = ConnectionStore::load(&state.paths.connections_file())?;
    Ok(store.connections)
}

#[tauri::command]
fn save_connection(state: State<'_, AppState>, mut conn: Connection) -> CmdResult<Connection> {
    if conn.id.trim().is_empty() {
        conn.id = slug(&conn.name, &conn.host);
    }
    if conn.credential_ref.trim().is_empty() {
        conn.credential_ref = credential::credential_ref_for(&conn.id);
    }
    // 启动命令：空白视为未设置
    conn.shell_command = conn
        .shell_command
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    let path = state.paths.connections_file();
    let mut store = ConnectionStore::load(&path)?;
    store.upsert(conn.clone());
    store.save(&path)?;
    Ok(conn)
}

#[tauri::command]
fn delete_connection(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    let path = state.paths.connections_file();
    let mut store = ConnectionStore::load(&path)?;
    if let Ok(conn) = store.get(&id).cloned() {
        let _ = credential::delete_secret(&conn.credential_ref);
    }
    store.remove(&id);
    store.save(&path)?;
    Ok(())
}

#[tauri::command]
fn set_secret(state: State<'_, AppState>, id: String, secret: String) -> CmdResult<()> {
    let store = ConnectionStore::load(&state.paths.connections_file())?;
    let conn = store.get(&id)?.clone();
    if secret.is_empty() {
        credential::delete_secret(&conn.credential_ref)?;
    } else {
        credential::set_secret(&conn.credential_ref, &secret)?;
    }
    Ok(())
}

#[tauri::command]
fn has_secret(state: State<'_, AppState>, id: String) -> CmdResult<bool> {
    let store = ConnectionStore::load(&state.paths.connections_file())?;
    let conn = store.get(&id)?.clone();
    if !conn.auth.needs_secret() {
        return Ok(true);
    }
    Ok(credential::get_secret(&conn.credential_ref).is_ok())
}

#[tauri::command]
async fn probe_connection(state: State<'_, AppState>, connection_id: String) -> CmdResult<ProbeInfo> {
    Ok(state.sessions.probe(&connection_id).await?)
}

#[tauri::command]
fn trust_host(
    state: State<'_, AppState>,
    host: String,
    port: u16,
    fingerprint: String,
) -> CmdResult<()> {
    state.sessions.trust_host(&host, port, &fingerprint)?;
    Ok(())
}

#[tauri::command]
fn forget_host(state: State<'_, AppState>, host: String, port: u16) -> CmdResult<bool> {
    Ok(state.sessions.forget_host(&host, port)?)
}

#[tauri::command]
fn daemon_info(state: State<'_, AppState>) -> CmdResult<serde_json::Value> {
    let config = sshcore::config::AppConfig::load_or_create(&state.paths.config_file())?;
    Ok(serde_json::json!({
        "port": config.port,
        "token": config.token,
        "baseUrl": format!("http://127.0.0.1:{}/v1", config.port),
    }))
}

// ---------------------------------------------------------------- 会话

#[tauri::command]
async fn list_sessions(state: State<'_, AppState>) -> CmdResult<Vec<SessionInfo>> {
    Ok(state.sessions.list().await)
}

#[tauri::command]
async fn open_session(
    state: State<'_, AppState>,
    connection_id: String,
    cols: u32,
    rows: u32,
) -> CmdResult<SessionInfo> {
    Ok(state.sessions.open(&connection_id, cols, rows).await?)
}

#[tauri::command]
async fn session_input(
    state: State<'_, AppState>,
    id: String,
    data: String,
    source: Option<String>,
) -> CmdResult<()> {
    let source = InputSource::parse(source.as_deref().unwrap_or("human"));
    state.sessions.input(&id, source, data.into_bytes()).await?;
    Ok(())
}

#[tauri::command]
async fn session_history(
    state: State<'_, AppState>,
    id: String,
) -> CmdResult<Vec<CommandRecord>> {
    Ok(state.sessions.history(&id).await?)
}

/// 读取自 `cursor` 起的输出，用于终端晚挂载时回放
#[tauri::command]
async fn session_read(
    state: State<'_, AppState>,
    id: String,
    cursor: u64,
) -> CmdResult<serde_json::Value> {
    let (data, next) = state.sessions.output_since(&id, cursor).await?;
    Ok(serde_json::json!({
        "data": base64::engine::general_purpose::STANDARD.encode(data),
        "cursor": next,
    }))
}

#[tauri::command]
async fn session_resize(
    state: State<'_, AppState>,
    id: String,
    cols: u32,
    rows: u32,
) -> CmdResult<()> {
    state.sessions.resize(&id, cols, rows).await?;
    eprintln!("[resize] {id} -> {cols}x{rows}");
    Ok(())
}

/// 前端调试日志（开发期用）
#[tauri::command]
fn debug_log(message: String) {
    eprintln!("[ui] {message}");
}

#[tauri::command]
async fn close_session(state: State<'_, AppState>, id: String) -> CmdResult<()> {
    state.sessions.close(&id).await?;
    Ok(())
}

// ---------------------------------------------------------------- 工具

fn slug(name: &str, host: &str) -> String {
    let base = if !name.trim().is_empty() { name } else { host };
    let mut out = String::new();
    for ch in base.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        format!("conn-{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0))
    } else {
        trimmed
    }
}

fn forward_events(handle: AppHandle, event: SessionEvent) {    match event {
        SessionEvent::Opened(info) => {
            let _ = handle.emit("ssh://opened", info);
        }
        SessionEvent::Output {
            session_id,
            cursor,
            data,
        } => {
            let payload = serde_json::json!({
                "sessionId": session_id,
                "cursor": cursor,
                "data": base64::engine::general_purpose::STANDARD.encode(data),
            });
            let _ = handle.emit("ssh://output", payload);
        }
        SessionEvent::Command(record) => {
            let _ = handle.emit("ssh://command", record);
        }
        SessionEvent::CommandFinished(record) => {
            let _ = handle.emit("ssh://command-finished", record);
        }
        SessionEvent::Closed { session_id, reason } => {
            let payload = serde_json::json!({ "sessionId": session_id, "reason": reason });
            let _ = handle.emit("ssh://closed", payload);
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let paths = AppPaths::discover().map_err(|e| e.to_string())?;
            paths.ensure().map_err(|e| e.to_string())?;
            let sessions = SessionManager::new(paths.clone());

            // 本地 HTTP 服务（供给 AI 客户端；与界面共用同一个 SessionManager）
            let app_config = sshcore::config::AppConfig::load_or_create(&paths.config_file())
                .map_err(|e| e.to_string())?;
            let daemon_port = app_config.port;
            let daemon_token = app_config.token.clone();
            {
                let daemon_sessions = Arc::clone(&sessions);
                tauri::async_runtime::spawn(async move {
                    let router = sshbox_daemon::router(daemon_sessions, daemon_token);
                    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], daemon_port));
                    eprintln!("[daemon] 监听 http://127.0.0.1:{daemon_port}/v1");
                    if let Err(err) = sshbox_daemon::serve(addr, router).await {
                        eprintln!("[daemon] 启动失败：{err}");
                    }
                });
            }

            // 调试/自动化用：设置 SSHBOX_OPEN_ON_START=<连接ID> 时启动即开会话
            // 可选 SSHBOX_EXEC_ON_START=<命令>，以 AI 身份写入该命令（验证 AI 路径）
            if let Ok(conn_id) = std::env::var("SSHBOX_OPEN_ON_START") {
                let auto = Arc::clone(&sessions);
                let auto_command = std::env::var("SSHBOX_EXEC_ON_START").ok();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
                    match auto.open(&conn_id, 100, 30).await {
                        Ok(info) => {
                            eprintln!("[auto] 会话已打开 {}", info.id);
                            if let Some(command) = auto_command {
                                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                                let mut bytes = command.into_bytes();
                                bytes.push(b'\r');
                                let mut last_error = None;
                                for attempt in 1..=5 {
                                    match auto.input(&info.id, InputSource::Ai, bytes.clone()).await {
                                        Ok(()) => {
                                            eprintln!("[auto] 第 {attempt} 次以 AI 身份写入成功");
                                            last_error = None;
                                            break;
                                        }
                                        Err(err) => {
                                            eprintln!("[auto] 第 {attempt} 次写入失败：{err}");
                                            last_error = Some(err);
                                            tokio::time::sleep(std::time::Duration::from_millis(900)).await;
                                        }
                                    }
                                }
                                if let Some(err) = last_error {
                                    eprintln!("[auto] 最终写入失败：{err}");
                                }
                            }
                        }
                        Err(err) => eprintln!("[auto] 打开失败：{err}"),
                    }
                });
            }

            let handle = app.handle().clone();
            let subscriber = Arc::clone(&sessions);
            tauri::async_runtime::spawn(async move {
                let mut rx = subscriber.subscribe();
                while let Ok(event) = rx.recv().await {
                    forward_events(handle.clone(), event);
                }
            });

            app.manage(AppState { paths, sessions });

            // 150% DPI 下创建时的 size/center/maximized 配置不可靠，运行时直接最大化
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.center();
                let _ = window.maximize();
            }

            // ---------------------------------------------------------- 托盘常驻
            let show_item = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
            let disconnect_item =
                MenuItem::with_id(app, "disconnect", "断开全部会话", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "退出 SSH Box", true, None::<&str>)?;
            let separator = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(
                app,
                &[&show_item, &disconnect_item, &separator, &quit_item],
            )?;

            let icon = tauri::include_image!("icons/32x32.png");
            let tray = TrayIconBuilder::with_id("main")
                .icon(icon)
                .tooltip("SSH Box — 本地服务运行中")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.unminimize();
                            let _ = window.set_focus();
                        }
                    }
                    "disconnect" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let state = app.state::<AppState>();
                            let sessions = state.sessions.list().await;
                            eprintln!("[tray] 断开全部会话：{} 个", sessions.len());
                            for session in sessions {
                                let _ = state.sessions.close(&session.id).await;
                            }
                        });
                    }
                    "quit" => {
                        eprintln!("[tray] 退出应用");
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.unminimize();
                            let _ = window.set_focus();
                        }
                    }
                })
                .build(app)?;
            // 托盘句柄一旦 drop 会被移除，这里故意泄漏到进程生命周期
            std::mem::forget(tray);

            Ok(())
        })
        .on_window_event(|window, event| {
            // 关闭窗口＝隐藏到托盘：本地服务与 MCP 继续运行
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
                eprintln!("[tray] 窗口已隐藏，本地服务继续运行");
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_connections,
            daemon_info,
            save_connection,
            delete_connection,
            set_secret,
            has_secret,
            probe_connection,
            trust_host,
            forget_host,
            debug_log,
            list_sessions,
            open_session,
            session_input,
            session_history,
            session_read,
            session_resize,
            close_session,
        ])
        .run(tauri::generate_context!())
        .expect("Tauri 启动失败");
}
