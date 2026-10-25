//! 最小可用的 MCP 服务端（JSON-RPC 2.0 over HTTP POST `/mcp`）。
//!
//! 只实现工具调用所需的子集：`initialize` / `tools/list` / `tools/call` / `ping`。
//! 无状态：每个请求独立处理，直接返回 JSON（不升级为 SSE 流）。
//!
//! 工具操作的是同一个 `SessionManager`，所以 AI 的每次输入都会实时出现在
//! 界面的命令流水里，并受"人类优先"仲裁约束。

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use sshcore::config::ConnectionStore;
use sshcore::manager::{InputSource, SessionManager};

use crate::AppState;

const PROTOCOL_VERSION: &str = "2025-06-18";
/// `wait_for` 默认的静默判定窗口
const DEFAULT_IDLE_MS: u64 = 1200;
/// 工具返回文本的默认上限
const DEFAULT_MAX_BYTES: usize = 65536;
/// `wait_for` 返回文本的默认上限（避免倾倒整个 scrollback）
const DEFAULT_WAIT_MAX_BYTES: usize = 8192;

pub async fn handle(State(app): State<AppState>, Json(request): Json<Value>) -> Response {
    let id = request.get("id").cloned().filter(|v| !v.is_null());
    let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = request.get("params").cloned().unwrap_or(Value::Null);

    // 通知（notifications/initialized 等）不需要响应体
    if id.is_none() {
        return StatusCode::ACCEPTED.into_response();
    }
    let id = id.unwrap_or(Value::Null);

    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "sshbox",
                "version": env!("CARGO_PKG_VERSION"),
            },
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => call_tool(&app.sessions, &params).await,
        other => Err((-32601, format!("未知方法：{other}"))),
    };

    match result {
        Ok(value) => Json(json!({ "jsonrpc": "2.0", "id": id, "result": value })).into_response(),
        Err((code, message)) => Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        }))
        .into_response(),
    }
}

// ------------------------------------------------------------------ 工具定义

fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "list_connections",
            "description": "列出用户已保存的 SSH 连接（不含任何凭据）。先用它找到可用的 connectionId。",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        }),
        json!({
            "name": "list_sessions",
            "description": "列出当前打开的 SSH 会话。AI 只能在这些会话上执行操作。",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        }),
        json!({
            "name": "session_state",
            "description": "查看会话终端状态：是否在提示符（atPrompt）、是否处于全屏程序（altScreen）、shell integration 是否生效（integration）、最近退出码（lastExit）。",
            "inputSchema": {
                "type": "object",
                "properties": { "sessionId": { "type": "string" } },
                "required": ["sessionId"],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "open_session",
            "description": "根据 connectionId 打开一个共享终端会话；已打开则复用。会话对用户同样可见、可接管。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "connectionId": { "type": "string", "description": "来自 list_connections 的 id" },
                    "cols": { "type": "integer", "description": "终端列数，默认 100" },
                    "rows": { "type": "integer", "description": "终端行数，默认 30" }
                },
                "required": ["connectionId"],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "send_input",
            "description": "把命令/按键写入共享终端（等同用户手敲）。text 是普通文本，keys 是结构化按键（如 C-c、Escape、Up）。若用户正在输入会被拒绝。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string" },
                    "text": { "type": "string", "description": "普通文本；不要在 text 里塞控制字符，用 keys" },
                    "keys": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "结构化按键，按顺序发送。支持 C-a..C-z、Escape、Enter、Tab、Backspace、Up/Down/Left/Right、Home/End、PageUp/PageDown、Delete、Space"
                    },
                    "enter": { "type": "boolean", "description": "是否在末尾补回车，默认 true（只发 keys 时默认 false）" }
                },
                "required": ["sessionId"],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "send_key",
            "description": "只发送结构化按键，例如 vim 里退出：[\"Escape\", \"C-c\"] 或 [\":\", \"q\", \"Enter\"]（配合 send_input）。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string" },
                    "keys": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "按键序列，如 [\"C-c\"]、[\"Escape\"]、[\"Up\",\"Enter\"]"
                    }
                },
                "required": ["sessionId", "keys"],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "read",
            "description": "按 cursor 增量读取输出。默认会剥离 ANSI 转义序列。返回的 cursor 只前进已返回的字节数，未读完的部分下次可继续读（remainingBytes 表示还剩多少）。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string" },
                    "cursor": { "type": "integer", "description": "上次返回的 cursor；不传则从“现在”开始" },
                    "maxBytes": { "type": "integer", "description": "本次最多返回字节数，默认 65536" },
                    "stripAnsi": { "type": "boolean", "description": "是否剥离 ANSI 转义序列，默认 true" }
                },
                "required": ["sessionId"],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "wait_for",
            "description": "等待终端状态。kind=prompt 等命令真正结束回到提示符（需要 shell integration，最可靠）；kind=idle 只是“输出静默”，静默不等于完成（sleep/long job 会误判）；kind=pattern 等指定文本出现。不传 cursor 时只从“现在”开始，不会倾倒历史输出。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string" },
                    "kind": { "type": "string", "enum": ["prompt", "idle", "pattern"], "description": "默认 prompt，若 integration 未生效则自动退化为 idle 并在结果里说明" },
                    "pattern": { "type": "string", "description": "kind=pattern 时必须提供，按子串匹配" },
                    "cursor": { "type": "integer", "description": "可选：显式指定起始游标，默认从当前位置开始" },
                    "idleMs": { "type": "integer", "description": "kind=idle 的静默阈值，默认 1200ms" },
                    "timeoutMs": { "type": "integer", "description": "超时毫秒，默认 20000" },
                    "maxBytes": { "type": "integer", "description": "返回文本上限，默认 8192" }
                },
                "required": ["sessionId"],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "interrupt",
            "description": "向会话发送 Ctrl-C。等价 send_key(sessionId, [\"C-c\"])。",
            "inputSchema": {
                "type": "object",
                "properties": { "sessionId": { "type": "string" } },
                "required": ["sessionId"],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "get_history",
            "description": "读取会话的命令流水（来源、退出码、耗时）。用于了解之前执行过什么，避免重复。",
            "inputSchema": {
                "type": "object",
                "properties": { "sessionId": { "type": "string" } },
                "required": ["sessionId"],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "close_session",
            "description": "关闭会话。",
            "inputSchema": {
                "type": "object",
                "properties": { "sessionId": { "type": "string" } },
                "required": ["sessionId"],
                "additionalProperties": false
            },
        }),
    ]
}

// ------------------------------------------------------------------ 工具实现

async fn call_tool(sessions: &Arc<SessionManager>, params: &Value) -> Result<Value, (i32, String)> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

    let outcome = match name.as_str() {
        "list_connections" => list_connections(sessions).await,
        "list_sessions" => list_sessions(sessions).await,
        "session_state" => session_state(sessions, &args).await,
        "open_session" => open_session(sessions, &args).await,
        "send_input" => send_input(sessions, &args).await,
        "send_key" => send_key(sessions, &args).await,
        "read" => read(sessions, &args).await,
        "wait_for" => wait_for(sessions, &args).await,
        "interrupt" => interrupt(sessions, &args).await,
        "get_history" => get_history(sessions, &args).await,
        "close_session" => close_session(sessions, &args).await,
        other => Err(format!("未知工具：{other}")),
    };

    match outcome {
        Ok(text) => Ok(json!({
            "content": [{ "type": "text", "text": text }],
            "isError": false,
        })),
        Err(message) => Ok(json!({
            "content": [{ "type": "text", "text": message }],
            "isError": true,
        })),
    }
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("缺少参数 {key}"))
}

/// 把按键名映射成终端字节序列
fn key_bytes(name: &str) -> Option<Vec<u8>> {
    let lower = name.trim().to_ascii_lowercase();
    let direct = match lower.as_str() {
        "enter" | "return" => Some(b"\r".to_vec()),
        "tab" => Some(b"\t".to_vec()),
        "backspace" => Some(vec![0x7f]),
        "escape" | "esc" => Some(vec![0x1b]),
        "space" => Some(b" ".to_vec()),
        "up" => Some(b"\x1b[A".to_vec()),
        "down" => Some(b"\x1b[B".to_vec()),
        "right" => Some(b"\x1b[C".to_vec()),
        "left" => Some(b"\x1b[D".to_vec()),
        "home" => Some(b"\x1b[H".to_vec()),
        "end" => Some(b"\x1b[F".to_vec()),
        "pageup" | "pgup" => Some(b"\x1b[5~".to_vec()),
        "pagedown" | "pgdn" => Some(b"\x1b[6~".to_vec()),
        "delete" | "del" => Some(b"\x1b[3~".to_vec()),
        "insert" => Some(b"\x1b[2~".to_vec()),
        _ => None,
    };
    if direct.is_some() {
        return direct;
    }

    // C-a .. C-z
    if let Some(rest) = lower.strip_prefix("c-") {
        let mut chars = rest.chars();
        if let (Some(ch), None) = (chars.next(), chars.next()) {
            let byte = ch as u32;
            if (b'a' as u32..=b'z' as u32).contains(&byte) {
                return Some(vec![(byte - b'a' as u32 + 1) as u8]);
            }
            match ch {
                '@' => return Some(vec![0x00]),
                '[' => return Some(vec![0x1b]),
                '\\' => return Some(vec![0x1c]),
                ']' => return Some(vec![0x1d]),
                '^' => return Some(vec![0x1e]),
                '_' => return Some(vec![0x1f]),
                _ => {}
            }
        }
    }
    None
}

fn keys_from(args: &Value) -> Result<Vec<u8>, String> {
    let Some(list) = args.get("keys").and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for item in list {
        let name = item.as_str().unwrap_or_default();
        match key_bytes(name) {
            Some(bytes) => out.extend_from_slice(&bytes),
            None => return Err(format!("无法识别的按键：{name}")),
        }
    }
    Ok(out)
}

/// 剥离 ANSI 转义序列与 \r，便于 AI 阅读
fn strip_ansi(bytes: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte == 0x1b {
            i += 1;
            match bytes.get(i) {
                Some(b'[') => {
                    i += 1;
                    while i < bytes.len() {
                        let c = bytes[i];
                        i += 1;
                        if (0x40..=0x7e).contains(&c) {
                            break;
                        }
                    }
                }
                Some(b']') => {
                    i += 1;
                    while i < bytes.len() {
                        let c = bytes[i];
                        i += 1;
                        if c == 0x07 {
                            break;
                        }
                        if c == 0x1b && bytes.get(i) == Some(&b'\\') {
                            i += 1;
                            break;
                        }
                    }
                }
                Some(_) => i += 1,
                None => {}
            }
        } else if byte == b'\r' {
            i += 1;
        } else {
            out.push(byte);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

async fn list_connections(sessions: &Arc<SessionManager>) -> Result<String, String> {
    let paths = sessions.paths();
    let store = ConnectionStore::load(&paths.connections_file()).map_err(|e| e.to_string())?;
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
                "group": c.group,
                "aiAccess": c.ai_access,
            })
        })
        .collect();
    Ok(serde_json::to_string_pretty(&items).unwrap_or_else(|_| "[]".into()))
}

async fn list_sessions(sessions: &Arc<SessionManager>) -> Result<String, String> {
    let list = sessions.list().await;
    let mut items = Vec::new();
    for info in list {
        let state = sessions.state(&info.id).await.unwrap_or_default();
        let mut value = serde_json::to_value(&info).unwrap_or(json!({}));
        if let Some(obj) = value.as_object_mut() {
            obj.insert("state".to_string(), json!(state));
        }
        items.push(value);
    }
    Ok(serde_json::to_string_pretty(&items).unwrap_or_else(|_| "[]".into()))
}

async fn session_state(sessions: &Arc<SessionManager>, args: &Value) -> Result<String, String> {
    let session_id = arg_str(args, "sessionId")?;
    let state = sessions
        .state(session_id)
        .await
        .map_err(|e| e.to_string())?;
    let cursor = sessions
        .cursor(session_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({ "state": state, "cursor": cursor }).to_string())
}

async fn open_session(sessions: &Arc<SessionManager>, args: &Value) -> Result<String, String> {
    let connection_id = arg_str(args, "connectionId")?;
    let cols = args.get("cols").and_then(|v| v.as_u64()).unwrap_or(100) as u32;
    let rows = args.get("rows").and_then(|v| v.as_u64()).unwrap_or(30) as u32;
    let info = sessions
        .open(connection_id, cols, rows)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::to_string_pretty(&info).unwrap_or_default())
}

async fn send_input(sessions: &Arc<SessionManager>, args: &Value) -> Result<String, String> {
    let session_id = arg_str(args, "sessionId")?;
    let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
    let key_bytes = keys_from(args)?;

    // 只发按键时不自动补回车；给了 text 才默认补
    let default_enter = !text.is_empty();
    let enter = args
        .get("enter")
        .and_then(|v| v.as_bool())
        .unwrap_or(default_enter);

    if text.is_empty() && key_bytes.is_empty() {
        return Err("text 与 keys 至少给一个".to_string());
    }

    let mut bytes = text.as_bytes().to_vec();
    bytes.extend_from_slice(&key_bytes);
    if enter {
        bytes.push(b'\r');
    }

    let written = bytes.len();
    sessions
        .input(session_id, InputSource::Ai, bytes)
        .await
        .map_err(|e| e.to_string())?;

    let cursor = sessions
        .cursor(session_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({ "written": true, "bytes": written, "cursor": cursor }).to_string())
}

async fn send_key(sessions: &Arc<SessionManager>, args: &Value) -> Result<String, String> {
    let session_id = arg_str(args, "sessionId")?;
    let bytes = keys_from(args)?;
    if bytes.is_empty() {
        return Err("keys 不能为空".to_string());
    }
    let written = bytes.len();
    sessions
        .input(session_id, InputSource::Ai, bytes)
        .await
        .map_err(|e| e.to_string())?;
    let cursor = sessions
        .cursor(session_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({ "sent": true, "bytes": written, "cursor": cursor }).to_string())
}

async fn read(sessions: &Arc<SessionManager>, args: &Value) -> Result<String, String> {
    let session_id = arg_str(args, "sessionId")?;
    let max_bytes = args
        .get("maxBytes")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_MAX_BYTES as u64) as usize;
    let strip = args
        .get("stripAnsi")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    // 不传 cursor 时从“现在”开始，避免倾倒历史
    let cursor = match args.get("cursor").and_then(|v| v.as_u64()) {
        Some(value) => value,
        None => sessions
            .cursor(session_id)
            .await
            .map_err(|e| e.to_string())?,
    };

    let (data, end) = sessions
        .output_since(session_id, cursor)
        .await
        .map_err(|e| e.to_string())?;

    // 只取前面 maxBytes，cursor 也只前进已返回的字节数，未读部分下次还能读
    let take = data.len().min(max_bytes);
    let slice = &data[..take];
    let next_cursor = cursor + take as u64;
    let remaining = (end - next_cursor) as usize;

    let text = if strip {
        strip_ansi(slice)
    } else {
        String::from_utf8_lossy(slice).to_string()
    };

    let state = sessions
        .state(session_id)
        .await
        .unwrap_or_default();

    Ok(json!({
        "text": text,
        "cursor": next_cursor,
        "returnedBytes": take,
        "remainingBytes": remaining,
        "truncated": remaining > 0,
        "atPrompt": state.at_prompt,
        "altScreen": state.alt_screen,
        "integration": state.integration,
    })
    .to_string())
}

async fn wait_for(sessions: &Arc<SessionManager>, args: &Value) -> Result<String, String> {
    let session_id = arg_str(args, "sessionId")?;
    let requested = args.get("kind").and_then(|v| v.as_str()).unwrap_or("prompt");
    let pattern = args.get("pattern").and_then(|v| v.as_str()).map(|s| s.to_string());
    let idle_ms = args
        .get("idleMs")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_IDLE_MS);
    let timeout_ms = args
        .get("timeoutMs")
        .and_then(|v| v.as_u64())
        .unwrap_or(20_000);
    let max_bytes = args
        .get("maxBytes")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_WAIT_MAX_BYTES as u64) as usize;

    // 不传 cursor 时从“现在”开始（修复：不再倾倒整个 scrollback）
    let mut cursor = match args.get("cursor").and_then(|v| v.as_u64()) {
        Some(value) => value,
        None => sessions
            .cursor(session_id)
            .await
            .map_err(|e| e.to_string())?,
    };

    // integration 不可用时，prompt 无法判断，退化为 idle 并明确告知
    let state0 = sessions.state(session_id).await.unwrap_or_default();
    let mut kind = requested.to_string();
    let mut hint: Option<String> = None;
    if kind == "prompt" && !state0.integration {
        kind = "idle".to_string();
        hint = Some(
            "该会话没有 shell integration（远端 shell 不是 bash/zsh，或注入未生效），无法判断“命令是否结束”。已退化为 idle：静默不等于完成，请用 get_history 或 wait_for(kind=pattern) 交叉确认。"
                .to_string(),
        );
    }

    let started = Instant::now();
    let mut acc = String::new();
    let mut last_output_at = Instant::now();
    let mut saw_output = false;

    loop {
        let (data, next) = sessions
            .output_since(session_id, cursor)
            .await
            .map_err(|e| e.to_string())?;
        if !data.is_empty() {
            acc.push_str(&String::from_utf8_lossy(&data));
            cursor = next;
            last_output_at = Instant::now();
            saw_output = true;
        }

        let state = sessions.state(session_id).await.unwrap_or_default();

        let matched = match kind.as_str() {
            "prompt" => state.integration && state.at_prompt,
            "pattern" => pattern
                .as_deref()
                .map(|needle| acc.contains(needle))
                .unwrap_or(false),
            _ => false,
        };

        if matched {
            return Ok(result_json(
                &sessions, session_id, true, &kind, cursor, &acc, max_bytes, hint, &state,
            )
            .await);
        }

        // idle：只对“确实产生过输出”之后才生效，避免刚开始就立刻返回
        if kind == "idle" && saw_output && last_output_at.elapsed() >= Duration::from_millis(idle_ms)
        {
            return Ok(result_json(
                &sessions, session_id, true, "idle", cursor, &acc, max_bytes, hint, &state,
            )
            .await);
        }

        if started.elapsed() > Duration::from_millis(timeout_ms) {
            return Ok(result_json(
                &sessions, session_id, false, "timeout", cursor, &acc, max_bytes, hint, &state,
            )
            .await);
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn result_json(
    sessions: &Arc<SessionManager>,
    session_id: &str,
    matched: bool,
    kind: &str,
    cursor: u64,
    acc: &str,
    max_bytes: usize,
    hint: Option<String>,
    state: &sshcore::manager::TerminalState,
) -> String {
    let bytes = acc.as_bytes();
    let truncated = bytes.len() > max_bytes;
    let slice = if truncated {
        &bytes[bytes.len() - max_bytes..]
    } else {
        bytes
    };
    let text = strip_ansi(slice);
    let _ = sessions;
    json!({
        "matched": matched,
        "reason": kind,
        "cursor": cursor,
        "text": text,
        "truncated": truncated,
        "atPrompt": state.at_prompt,
        "altScreen": state.alt_screen,
        "integration": state.integration,
        "lastExit": state.last_exit,
        "hint": hint,
        "sessionId": session_id,
    })
    .to_string()
}

async fn interrupt(sessions: &Arc<SessionManager>, args: &Value) -> Result<String, String> {
    let session_id = arg_str(args, "sessionId")?;
    sessions
        .input(session_id, InputSource::Terminal, b"\x03".to_vec())
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({ "sent": true }).to_string())
}

async fn get_history(sessions: &Arc<SessionManager>, args: &Value) -> Result<String, String> {
    let session_id = arg_str(args, "sessionId")?;
    let records = sessions
        .history(session_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::to_string_pretty(&records).unwrap_or_else(|_| "[]".into()))
}

async fn close_session(sessions: &Arc<SessionManager>, args: &Value) -> Result<String, String> {
    let session_id = arg_str(args, "sessionId")?;
    sessions
        .close(session_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({ "closed": true }).to_string())
}
