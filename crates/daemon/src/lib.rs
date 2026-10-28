//! SSH Box 本地 HTTP 服务。
//!
//! - 只绑 `127.0.0.1`
//! - 所有 `/v1/*` 需要 `Authorization: Bearer <token>`
//! - 校验 `Host` 头为回环地址，防 DNS rebinding
//!
//! 它与桌面界面共用同一个 `SessionManager`：AI 通过这里的每条输入，
//! 都会出现在界面的命令流水里。

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Path, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use base64::Engine as _;
use futures_util::Stream;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use sshcore::config::{AppPaths, ConnectionStore};
use sshcore::manager::{InputSource, SessionEvent, SessionManager};
use tokio_stream::wrappers::BroadcastStream;

mod mcp;

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) sessions: Arc<SessionManager>,
}

// ------------------------------------------------------------------ 错误

struct ApiError(StatusCode, Value);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(self.1)).into_response()
    }
}

impl From<sshcore::Error> for ApiError {
    fn from(err: sshcore::Error) -> Self {
        use sshcore::Error as E;
        let (status, body) = match err {
            E::HostKeyUnknown {
                host,
                port,
                fingerprint,
            } => (
                StatusCode::CONFLICT,
                json!({
                    "kind": "hostKeyUnknown",
                    "host": host,
                    "port": port,
                    "fingerprint": fingerprint,
                    "message": format!("首次连接 {host}:{port}，需要在界面确认主机指纹"),
                }),
            ),
            E::HostKeyMismatch { expected, actual } => (
                StatusCode::CONFLICT,
                json!({
                    "kind": "hostKeyMismatch",
                    "expected": expected,
                    "actual": actual,
                    "message": "主机指纹不匹配，连接已拒绝",
                }),
            ),
            E::ConnectionNotFound(id) => (
                StatusCode::NOT_FOUND,
                json!({ "kind": "connectionNotFound", "message": format!("找不到连接 {id}") }),
            ),
            other => (
                StatusCode::BAD_REQUEST,
                json!({ "kind": "error", "message": other.to_string() }),
            ),
        };
        Self(status, body)
    }
}

// ------------------------------------------------------------------ 鉴权

async fn auth(
    State(token): State<String>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    // 只接受回环 Host，防止 DNS rebinding 把浏览器请求打到本地服务
    if let Some(host) = req
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
    {
        let name = host.split(':').next().unwrap_or("");
        if !matches!(name, "127.0.0.1" | "localhost" | "[::1]" | "::1") {
            return Err(ApiError(
                StatusCode::FORBIDDEN,
                json!({ "kind": "forbidden", "message": "只允许回环地址访问" }),
            ));
        }
    }

    let provided = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if provided.strip_prefix("Bearer ") != Some(token.as_str()) {
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            json!({ "kind": "unauthorized", "message": "缺少或错误的访问令牌" }),
        ));
    }

    Ok(next.run(req).await)
}

// ------------------------------------------------------------------ 处理函数

async fn health(State(app): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "sessions": app.sessions.list().await.len(),
    }))
}

async fn list_connections(State(app): State<AppState>) -> Result<Json<Value>, ApiError> {
    let paths: &AppPaths = app.sessions.paths();
    let store = ConnectionStore::load(&paths.connections_file())?;
    // 只暴露可用信息，凭据引用不外泄
    let items: Vec<Value> = store
        .connections
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "name": c.name,
                "host": c.host,
                "port": c.port,
                "user": c.user,
                "auth": c.auth.label(),
                "aiAccess": c.ai_access,
                "group": c.group,
            })
        })
        .collect();
    Ok(Json(json!({ "connections": items })))
}

async fn list_sessions(State(app): State<AppState>) -> impl IntoResponse {
    Json(json!({ "sessions": app.sessions.list().await }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenSessionBody {
    connection_id: String,
    #[serde(default = "default_cols")]
    cols: u32,
    #[serde(default = "default_rows")]
    rows: u32,
}

fn default_cols() -> u32 {
    100
}
fn default_rows() -> u32 {
    30
}

async fn open_session(
    State(app): State<AppState>,
    Json(body): Json<OpenSessionBody>,
) -> Result<Json<Value>, ApiError> {
    let info = app
        .sessions
        .open(&body.connection_id, body.cols, body.rows)
        .await?;
    Ok(Json(json!(info)))
}

async fn close_session(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    app.sessions.close(&id).await?;
    Ok(Json(json!({ "closed": true })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InputBody {
    text: String,
    #[serde(default)]
    source: Option<String>,
    /// 是否在末尾自动补回车（默认 true）
    #[serde(default = "default_true")]
    enter: bool,
}

fn default_true() -> bool {
    true
}

async fn input(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<InputBody>,
) -> Result<Json<Value>, ApiError> {
    let source = InputSource::parse(body.source.as_deref().unwrap_or("ai"));
    let mut bytes = body.text.into_bytes();
    if body.enter {
        bytes.push(b'\r');
    }
    app.sessions.input(&id, source, bytes).await?;
    Ok(Json(json!({ "written": true })))
}

async fn interrupt(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    app.sessions
        .input(&id, InputSource::Terminal, b"\x03".to_vec())
        .await?;
    Ok(Json(json!({ "sent": true })))
}

async fn resize(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let cols = body.get("cols").and_then(|v| v.as_u64()).unwrap_or(100) as u32;
    let rows = body.get("rows").and_then(|v| v.as_u64()).unwrap_or(30) as u32;
    app.sessions.resize(&id, cols, rows).await?;
    Ok(Json(json!({ "resized": true })))
}

#[derive(Debug, Deserialize)]
struct OutputQuery {
    #[serde(default)]
    cursor: u64,
    #[serde(default)]
    format: Option<String>,
}

async fn output(
    State(app): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<OutputQuery>,
) -> Result<Json<Value>, ApiError> {
    let (data, cursor) = app.sessions.output_since(&id, query.cursor).await?;
    if query.format.as_deref() == Some("text") {
        Ok(Json(json!({
            "text": String::from_utf8_lossy(&data),
            "cursor": cursor,
        })))
    } else {
        Ok(Json(json!({
            "data": base64::engine::general_purpose::STANDARD.encode(data),
            "cursor": cursor,
        })))
    }
}

async fn history(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let records = app.sessions.history(&id).await?;
    Ok(Json(json!({ "commands": records })))
}

fn event_payload(event: &SessionEvent) -> Option<(&'static str, Value)> {
    match event {
        SessionEvent::Opened(info) => Some(("opened", json!(info))),
        SessionEvent::Output {
            session_id,
            cursor,
            data,
        } => Some((
            "output",
            json!({
                "sessionId": session_id,
                "cursor": cursor,
                "data": base64::engine::general_purpose::STANDARD.encode(data),
            }),
        )),
        SessionEvent::Command(record) => Some(("command", json!(record))),
        SessionEvent::CommandFinished(record) => Some(("commandFinished", json!(record))),
        SessionEvent::FileOp(record) => Some(("fileOp", json!(record))),
        SessionEvent::Closed { session_id, reason } => Some((
            "closed",
            json!({ "sessionId": session_id, "reason": reason }),
        )),
    }
}

async fn events(
    State(app): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let receiver = app.sessions.subscribe();
    let stream = BroadcastStream::new(receiver).filter_map(|item| async move {
        let event = item.ok()?;
        let (name, payload) = event_payload(&event)?;
        Some(Ok(Event::default().event(name).data(payload.to_string())))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ------------------------------------------------------------------ 组装

pub fn router(sessions: Arc<SessionManager>, token: String) -> Router {
    let state = AppState { sessions };
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/connections", get(list_connections))
        .route("/v1/sessions", get(list_sessions).post(open_session))
        .route("/v1/sessions/:id", delete(close_session))
        .route("/v1/sessions/:id/input", post(input))
        .route("/v1/sessions/:id/interrupt", post(interrupt))
        .route("/v1/sessions/:id/resize", post(resize))
        .route("/v1/sessions/:id/output", get(output))
        .route("/v1/sessions/:id/history", get(history))
        .route("/v1/events", get(events))
        .route("/mcp", post(mcp::handle))
        .layer(middleware::from_fn_with_state(token, auth))
        .with_state(state)
}

pub async fn serve(addr: SocketAddr, router: Router) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router).await
}
