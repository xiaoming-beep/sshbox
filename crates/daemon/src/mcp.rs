//! 最小可用的 MCP 服务端（JSON-RPC 2.0 over HTTP POST `/mcp`）。
//!
//! 只实现工具调用所需的子集：`initialize` / `tools/list` / `tools/call` / `ping`。
//! 无状态：每个请求独立处理，直接返回 JSON（不升级为 SSE 流）。
//!
//! 工具操作的是同一个 `SessionManager`，所以 AI 的每次输入都会实时出现在
//! 界面的命令流水里，并受"人类优先"仲裁约束。

use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use sshcore::config::ConnectionStore;
use sshcore::manager::{AccessSource, InputSource, SessionManager};

use crate::AppState;

const PROTOCOL_VERSION: &str = "2025-06-18";
/// `wait_for` 默认的静默判定窗口
const DEFAULT_IDLE_MS: u64 = 1200;
/// 工具返回文本的默认上限
const DEFAULT_MAX_BYTES: usize = 65536;
/// `wait_for` 返回文本的默认上限（避免倾倒整个 scrollback）
const DEFAULT_WAIT_MAX_BYTES: usize = 8192;
/// 结构化按键之间的默认间隔（毫秒）
const DEFAULT_KEY_GAP_MS: u64 = 30;

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
                "properties": { "sessionId": { "type": "string", "description": "会话 ID；只有一个会话时可省略" } },
                "required": [],
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
                    "sessionId": { "type": "string", "description": "会话 ID；只有一个会话时可省略" },
                    "text": { "type": "string", "description": "普通文本；不要在 text 里塞控制字符，用 keys" },
                    "keys": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "结构化按键，按顺序逐键发送（默认间隔 30ms）。支持 C-a..C-z、Escape、Enter、Tab、Backspace、Up/Down/Left/Right、Home/End、PageUp/PageDown、Delete、Space；单个字符按键保留大小写（\"G\" 就是大写 G）"
                    },
                    "enter": { "type": "boolean", "description": "是否在末尾补回车，默认 true（只发 keys 时默认 false）" },
                    "intervalMs": { "type": "integer", "description": "按键之间的间隔（毫秒），默认 30，传 0 不等待；单独 Escape 后至少等 120ms" }
                },
                "required": [],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "send_key",
            "description": "只发送结构化按键，逐键发送（默认间隔 30ms）。例如 vim 里新开一行：[\"Escape\", \"G\", \"o\"]；退出：[\"Escape\", \":\", \"w\", \"q\", \"Enter\"]。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string", "description": "会话 ID；只有一个会话时可省略" },
                    "keys": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "按键序列，如 [\"C-c\"]、[\"Escape\"]、[\"Up\",\"Enter\"]；单个字符保留大小写"
                    },
                    "intervalMs": { "type": "integer", "description": "按键之间的间隔（毫秒），默认 30，传 0 不等待" }
                },
                "required": ["keys"],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "read",
            "description": "按 cursor 增量读取输出。默认会剥离 ANSI 转义序列。返回的 cursor 只前进已返回的字节数，未读完的部分下次可继续读（remainingBytes 表示还剩多少）。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string", "description": "会话 ID；只有一个会话时可省略" },
                    "cursor": { "type": "integer", "description": "上次返回的 cursor；不传则从“现在”开始" },
                    "maxBytes": { "type": "integer", "description": "本次最多返回字节数，默认 65536" },
                    "stripAnsi": { "type": "boolean", "description": "是否剥离 ANSI 转义序列，默认 true" }
                },
                "required": [],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "wait_for",
            "description": "等待终端状态。kind=prompt 等命令真正结束回到提示符（需要 shell integration，最可靠）；kind=idle 只是“输出静默”，静默不等于完成（sleep/long job 会误判）；kind=pattern 等指定文本出现。不传 cursor 时只从“现在”开始，不会倾倒历史输出。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string", "description": "会话 ID；只有一个会话时可省略" },
                    "kind": { "type": "string", "enum": ["prompt", "idle", "pattern"], "description": "默认 prompt，若 integration 未生效则自动退化为 idle 并在结果里说明" },
                    "pattern": { "type": "string", "description": "kind=pattern 时必须提供，按子串匹配" },
                    "cursor": { "type": "integer", "description": "可选：显式指定起始游标，默认从当前位置开始" },
                    "lookbackBytes": { "type": "integer", "description": "不传 cursor 时往回多扫多少字节（如 4096），用于命令输出早于本次调用到达的情况" },
                    "idleMs": { "type": "integer", "description": "kind=idle 的静默阈值，默认 1200ms" },
                    "timeoutMs": { "type": "integer", "description": "超时毫秒，默认 20000" },
                    "maxBytes": { "type": "integer", "description": "返回文本上限，默认 8192" }
                },
                "required": [],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "screen",
            "description": "返回终端渲染后的屏幕内容（纯文本，已去 ANSI）。适合 vim/tmux/top/less 这类全屏程序：它给你的是用户眼前看到的画面，而不是转义序列。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string", "description": "会话 ID；只有一个会话时可省略" },
                    "maxLines": { "type": "integer", "description": "最多返回多少行（从底部算），默认 200" }
                },
                "required": [],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "interrupt",
            "description": "向会话发送 Ctrl-C。等价 send_key(sessionId, [\"C-c\"])。",
            "inputSchema": {
                "type": "object",
                "properties": { "sessionId": { "type": "string", "description": "会话 ID；只有一个会话时可省略" } },
                "required": [],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "get_history",
            "description": "读取会话的命令流水（来源、退出码、耗时）。用于了解之前执行过什么，避免重复。",
            "inputSchema": {
                "type": "object",
                "properties": { "sessionId": { "type": "string", "description": "会话 ID；只有一个会话时可省略" } },
                "required": [],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "close_session",
            "description": "关闭会话。",
            "inputSchema": {
                "type": "object",
                "properties": { "sessionId": { "type": "string", "description": "会话 ID；只有一个会话时可省略" } },
                "required": [],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "sftp_list",
            "description": "列出远端目录（只读）。返回 { dir, entries }：dir 是规范化后的绝对路径，entries 含 name/path/isDir/size/mode/modified。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string", "description": "会话 ID；只有一个会话时可省略" },
                    "path": { "type": "string", "description": "目录路径；缺省为登录用户的 home（.）" }
                },
                "required": [],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "sftp_stat",
            "description": "查看远端文件或目录的属性（只读）。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string", "description": "会话 ID；只有一个会话时可省略" },
                    "path": { "type": "string" }
                },
                "required": ["path"],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "sftp_read",
            "description": "读取远端小文件（只读）。文本以 text 返回，二进制以 base64 返回；默认上限 64KB。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string", "description": "会话 ID；只有一个会话时可省略" },
                    "path": { "type": "string" },
                    "maxBytes": { "type": "integer", "description": "最大读取字节数，默认 65536" }
                },
                "required": ["path"],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "sftp_write",
            "description": "写远端文件（文本或 base64）。受连接级策略限制：只有配置了可写前缀的路径才允许；已存在且 overwrite 不为 true 时拒绝。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string", "description": "会话 ID；只有一个会话时可省略" },
                    "path": { "type": "string", "description": "绝对路径" },
                    "text": { "type": "string", "description": "文本内容（与 base64 二选一）" },
                    "base64": { "type": "string", "description": "二进制内容的 base64（与 text 二选一）" },
                    "overwrite": { "type": "boolean", "description": "为 true 时允许覆盖已存在的文件，默认 false" }
                },
                "required": ["path"],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "sftp_mkdir",
            "description": "在远端创建目录（受可写前缀限制）。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string", "description": "会话 ID；只有一个会话时可省略" },
                    "path": { "type": "string" }
                },
                "required": ["path"],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "sftp_remove",
            "description": "删除远端文件或空目录（受可写前缀限制）。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string", "description": "会话 ID；只有一个会话时可省略" },
                    "path": { "type": "string" }
                },
                "required": ["path"],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "sftp_rename",
            "description": "重命名/移动远端文件（源与目标都必须在可写前缀内）。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string", "description": "会话 ID；只有一个会话时可省略" },
                    "from": { "type": "string" },
                    "to": { "type": "string" }
                },
                "required": ["from", "to"],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "sftp_download",
            "description": "把远端文件下载到本机暂存区（Downloads/SSHBox），返回本地路径。适合“先下载到本地、再推送给它机器”的离线场景。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string", "description": "会话 ID；只有一个会话时可省略" },
                    "path": { "type": "string", "description": "远端文件路径" },
                    "name": { "type": "string", "description": "本地文件名（不得包含路径分隔符），缺省用远端文件名" }
                },
                "required": ["path"],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "sftp_upload",
            "description": "把暂存区（Downloads/SSHBox）里的文件推送到远端。只能读暂存区，读不到你其它本地文件；远端路径仍受可写范围限制。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string", "description": "会话 ID；只有一个会话时可省略" },
                    "name": { "type": "string", "description": "暂存区里的文件名" },
                    "remotePath": { "type": "string", "description": "目标绝对路径（需在可写范围内）" },
                    "overwrite": { "type": "boolean", "description": "已存在时是否覆盖，默认 false" }
                },
                "required": ["name", "remotePath"],
                "additionalProperties": false
            },
        }),
        json!({
            "name": "get_file_history",
            "description": "读取会话的文件操作审计（list/stat/read/write/mkdir/remove/rename/download/upload），含来源、路径、字节数、是否成功。",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionId": { "type": "string", "description": "会话 ID；只有一个会话时可省略" }
                },
                "required": [],
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
        "screen" => screen_tool(sessions, &args).await,
        "interrupt" => interrupt(sessions, &args).await,
        "get_history" => get_history(sessions, &args).await,
        "close_session" => close_session(sessions, &args).await,
        "sftp_list" => sftp_list_tool(sessions, &args).await,
        "sftp_stat" => sftp_stat_tool(sessions, &args).await,
        "sftp_read" => sftp_read_tool(sessions, &args).await,
        "sftp_write" => sftp_write_tool(sessions, &args).await,
        "sftp_mkdir" => sftp_mkdir_tool(sessions, &args).await,
        "sftp_remove" => sftp_remove_tool(sessions, &args).await,
        "sftp_rename" => sftp_rename_tool(sessions, &args).await,
        "sftp_download" => sftp_download_tool(sessions, &args).await,
        "sftp_upload" => sftp_upload_tool(sessions, &args).await,
        "get_file_history" => get_file_history(sessions, &args).await,
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

/// 解析会话：显式传了就验一下；没传时如果只有一个会话就自动选中。
async fn resolve_session(sessions: &Arc<SessionManager>, args: &Value) -> Result<String, String> {
    let explicit = args
        .get("sessionId")
        .and_then(|value| value.as_str())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    let list = sessions.list().await;

    if let Some(id) = explicit {
        if list.iter().any(|session| session.id == id) {
            return Ok(id);
        }
        return Err(format!("会话 {id} 不存在或已断开"));
    }

    match list.len() {
        0 => Err(
            "当前没有打开的会话：请先在 SSH Box 界面连接主机，或调用 open_session".to_string(),
        ),
        1 => Ok(list[0].id.clone()),
        count => {
            let options = list
                .iter()
                .map(|session| format!("{}={}", session.name, session.id))
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "当前有 {count} 个会话，请显式传 sessionId：{options}"
            ))
        }
    }
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
    // 单字符字面量（如 "d"、"G"、":"）直接原样发送，**保留大小写**：
    // vim 的 G / O / A / I 都区分大小写，之前统一转小写导致 "G" 变成 "g"、
    // "Escape G o" 被 vim 当成 "go"（跳到第 1 字节），后续文本就错位了。
    let mut chars = name.trim().chars();
    if let (Some(ch), None) = (chars.next(), chars.next()) {
        if !ch.is_control() {
            let mut buf = [0u8; 4];
            return Some(ch.encode_utf8(&mut buf).as_bytes().to_vec());
        }
    }
    None
}

/// 把 keys 数组拆成“一个按键一份字节”，交给发送层逐键发
fn keys_from(args: &Value) -> Result<Vec<Vec<u8>>, String> {
    let Some(list) = args.get("keys").and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(list.len());
    for item in list {
        let name = item.as_str().unwrap_or_default();
        match key_bytes(name) {
            Some(bytes) => out.push(bytes),
            None => return Err(format!("无法识别的按键：{name}")),
        }
    }
    Ok(out)
}

fn keys_len(keys: &[Vec<u8>]) -> usize {
    keys.iter().map(|chunk| chunk.len()).sum()
}

/// 按键之间的间隔（毫秒），默认 40ms；传 0 表示不等待
fn key_gap_from(args: &Value) -> Duration {
    let ms = args
        .get("intervalMs")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_KEY_GAP_MS);
    Duration::from_millis(ms.min(5000))
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
    let session_id = resolve_session(sessions, args).await?;
    let state = sessions
        .state(&session_id)
        .await
        .map_err(|e| e.to_string())?;
    let cursor = sessions
        .cursor(&session_id)
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
    let session_id = resolve_session(sessions, args).await?;
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

    let text_bytes = text.as_bytes().to_vec();
    let written = text_bytes.len() + keys_len(&key_bytes) + usize::from(enter);
    // 文本（含回车）计入命令流水，按键序列不算；按键逐个发，中间留间隔
    sessions
        .input_mixed(
            &session_id,
            InputSource::Ai,
            text_bytes,
            key_bytes,
            enter,
            key_gap_from(args),
        )
        .await
        .map_err(|e| e.to_string())?;

    let cursor = sessions
        .cursor(&session_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({ "written": true, "bytes": written, "cursor": cursor }).to_string())
}

async fn send_key(sessions: &Arc<SessionManager>, args: &Value) -> Result<String, String> {
    let session_id = resolve_session(sessions, args).await?;
    let bytes = keys_from(args)?;
    if bytes.is_empty() {
        return Err("keys 不能为空".to_string());
    }
    let written = keys_len(&bytes);
    // 纯按键不进入命令流水；逐键发送，避免 ESC 与后续字符粘成组合键
    sessions
        .send_keys_paced(&session_id, InputSource::Ai, bytes, key_gap_from(args))
        .await
        .map_err(|e| e.to_string())?;
    let cursor = sessions
        .cursor(&session_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({ "sent": true, "bytes": written, "cursor": cursor }).to_string())
}

async fn read(sessions: &Arc<SessionManager>, args: &Value) -> Result<String, String> {
    let session_id = resolve_session(sessions, args).await?;
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
            .cursor(&session_id)
            .await
            .map_err(|e| e.to_string())?,
    };

    let (data, end) = sessions
        .output_since(&session_id, cursor)
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
        .state(&session_id)
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
    let session_id = resolve_session(sessions, args).await?;
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

    // 不传 cursor 时默认从“现在”开始；可用 lookbackBytes 回扣一小段，
    // 避免“命令输出已经先到、再调用 wait_for”时 pattern 匹配扑空。
    let mut cursor = match args.get("cursor").and_then(|v| v.as_u64()) {
        Some(value) => value,
        None => {
            let end = sessions
                .cursor(&session_id)
                .await
                .map_err(|e| e.to_string())?;
            let lookback = args
                .get("lookbackBytes")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            end.saturating_sub(lookback)
        }
    };
    let scanned_from = cursor;

    // integration 不可用时，prompt 无法判断，退化为 idle 并明确告知
    let state0 = sessions.state(&session_id).await.unwrap_or_default();
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
            .output_since(&session_id, cursor)
            .await
            .map_err(|e| e.to_string())?;
        if !data.is_empty() {
            acc.push_str(&String::from_utf8_lossy(&data));
            cursor = next;
            last_output_at = Instant::now();
            saw_output = true;
        }

        let state = sessions.state(&session_id).await.unwrap_or_default();

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
                &sessions, &session_id, true, &kind, cursor, &acc, max_bytes, hint, &state, scanned_from,
            )
            .await);
        }

        // idle：只对“确实产生过输出”之后才生效，避免刚开始就立刻返回
        if kind == "idle" && saw_output && last_output_at.elapsed() >= Duration::from_millis(idle_ms)
        {
            return Ok(result_json(
                &sessions, &session_id, true, "idle", cursor, &acc, max_bytes, hint, &state, scanned_from,
            )
            .await);
        }

        if started.elapsed() > Duration::from_millis(timeout_ms) {
            return Ok(result_json(
                &sessions, &session_id, false, "timeout", cursor, &acc, max_bytes, hint, &state, scanned_from,
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
    scanned_from: u64,
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
        "scannedFrom": scanned_from,
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

async fn screen_tool(sessions: &Arc<SessionManager>, args: &Value) -> Result<String, String> {
    let session_id = resolve_session(sessions, args).await?;
    let max_lines = args
        .get("maxLines")
        .and_then(|v| v.as_u64())
        .unwrap_or(200) as usize;

    let snap = sessions
        .screen(&session_id)
        .await
        .map_err(|e| e.to_string())?;

    let lines: Vec<&str> = snap.text.lines().collect();
    let truncated = lines.len() > max_lines;
    let start = lines.len().saturating_sub(max_lines);

    Ok(json!({
        "text": lines[start..].join("\n"),
        "rows": snap.rows,
        "cols": snap.cols,
        "cursorRow": snap.cursor_row,
        "cursorCol": snap.cursor_col,
        "atPrompt": snap.at_prompt,
        "altScreen": snap.alt_screen,
        "truncated": truncated,
    })
    .to_string())
}

async fn get_file_history(
    sessions: &Arc<SessionManager>,
    args: &Value,
) -> Result<String, String> {
    let session_id = resolve_session(sessions, args).await?;
    let records = sessions
        .file_history(&session_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::to_string_pretty(&records).unwrap_or_else(|_| "[]".into()))
}
async fn interrupt(sessions: &Arc<SessionManager>, args: &Value) -> Result<String, String> {    let session_id = resolve_session(sessions, args).await?;
    sessions
        .send_keys(&session_id, InputSource::Terminal, b"\x03".to_vec())
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({ "sent": true }).to_string())
}

async fn get_history(sessions: &Arc<SessionManager>, args: &Value) -> Result<String, String> {
    let session_id = resolve_session(sessions, args).await?;
    let records = sessions
        .history(&session_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::to_string_pretty(&records).unwrap_or_else(|_| "[]".into()))
}

async fn sftp_list_tool(sessions: &Arc<SessionManager>, args: &Value) -> Result<String, String> {
    let session_id = resolve_session(sessions, args).await?;
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let listing = sessions
        .sftp_list(&session_id, AccessSource::Ai, path)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({ "dir": listing.dir, "entries": listing.entries }).to_string())
}

async fn sftp_stat_tool(sessions: &Arc<SessionManager>, args: &Value) -> Result<String, String> {
    let session_id = resolve_session(sessions, args).await?;
    let path = arg_str(args, "path")?;
    let entry = sessions
        .sftp_stat(&session_id, AccessSource::Ai, path)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::to_string_pretty(&entry).unwrap_or_default())
}

async fn sftp_read_tool(sessions: &Arc<SessionManager>, args: &Value) -> Result<String, String> {
    let session_id = resolve_session(sessions, args).await?;
    let path = arg_str(args, "path")?;
    let max_bytes = args
        .get("maxBytes")
        .and_then(|v| v.as_u64())
        .unwrap_or(65_536) as usize;

    let data = sessions
        .sftp_read(&session_id, AccessSource::Ai, path, max_bytes)
        .await
        .map_err(|e| e.to_string())?;
    let bytes = data.len();

    match String::from_utf8(data) {
        Ok(text) => Ok(json!({ "text": text, "bytes": bytes, "encoding": "utf8" }).to_string()),
        Err(err) => Ok(json!({
            "base64": base64::engine::general_purpose::STANDARD.encode(err.into_bytes()),
            "bytes": bytes,
            "encoding": "base64",
        })
        .to_string()),
    }
}

async fn sftp_write_tool(sessions: &Arc<SessionManager>, args: &Value) -> Result<String, String> {
    let session_id = resolve_session(sessions, args).await?;
    let path = arg_str(args, "path")?;
    let overwrite = args
        .get("overwrite")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let data = match (args.get("base64").and_then(|v| v.as_str()), args.get("text").and_then(|v| v.as_str())) {
        (Some(encoded), _) => base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| format!("base64 解码失败：{e}"))?,
        (None, Some(text)) => text.as_bytes().to_vec(),
        (None, None) => return Err("text 与 base64 至少给一个".to_string()),
    };

    if !overwrite && sessions.sftp_exists(&session_id, path).await.map_err(|e| e.to_string())? {
        return Err(format!(
            "{path} 已存在；如确认覆盖请传 overwrite=true"
        ));
    }

    let bytes = data.len();
    sessions
        .sftp_write(&session_id, AccessSource::Ai, path, data)
        .await
        .map_err(|e| e.to_string())?;

    Ok(json!({ "written": true, "bytes": bytes, "path": path }).to_string())
}

async fn sftp_mkdir_tool(sessions: &Arc<SessionManager>, args: &Value) -> Result<String, String> {
    let session_id = resolve_session(sessions, args).await?;
    let path = arg_str(args, "path")?;
    sessions
        .sftp_mkdir(&session_id, AccessSource::Ai, path)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({ "created": true, "path": path }).to_string())
}

async fn sftp_remove_tool(sessions: &Arc<SessionManager>, args: &Value) -> Result<String, String> {
    let session_id = resolve_session(sessions, args).await?;
    let path = arg_str(args, "path")?;
    sessions
        .sftp_remove(&session_id, AccessSource::Ai, path)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({ "removed": true, "path": path }).to_string())
}

async fn sftp_rename_tool(sessions: &Arc<SessionManager>, args: &Value) -> Result<String, String> {
    let session_id = resolve_session(sessions, args).await?;
    let from = arg_str(args, "from")?;
    let to = arg_str(args, "to")?;
    sessions
        .sftp_rename(&session_id, AccessSource::Ai, from, to)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({ "renamed": true, "from": from, "to": to }).to_string())
}

/// 本机暂存区：Downloads/SSHBox（AI 只能读写这里）
fn staging_dir() -> std::path::PathBuf {
    let base = std::env::var("USERPROFILE")
        .map(std::path::PathBuf::from)
        .ok()
        .map(|home| home.join("Downloads"))
        .filter(|dir| dir.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    base.join("SSHBox")
}

/// 只允许纯文件名，挡住 ../ 与路径分隔符
fn staging_file(name: &str) -> Result<std::path::PathBuf, String> {
    let name = name.trim();
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err("文件名不能包含路径分隔符或 ..".to_string());
    }
    let dir = staging_dir();
    std::fs::create_dir_all(&dir).map_err(|err| format!("创建暂存目录失败：{err}"))?;
    Ok(dir.join(name))
}

async fn sftp_download_tool(
    sessions: &Arc<SessionManager>,
    args: &Value,
) -> Result<String, String> {
    let session_id = resolve_session(sessions, args).await?;
    let path = arg_str(args, "path")?;
    let name = args
        .get("name")
        .and_then(|value| value.as_str())
        .map(|value| value.to_string())
        .unwrap_or_else(|| path.rsplit('/').next().unwrap_or("download").to_string());

    let local = staging_file(&name)?;
    const MAX_BYTES: usize = 64 * 1024 * 1024;

    let data = sessions
        .sftp_read(&session_id, AccessSource::Ai, path, MAX_BYTES)
        .await
        .map_err(|e| e.to_string())?;
    let bytes = data.len();

    std::fs::write(&local, &data).map_err(|err| format!("写入本地暂存失败：{err}"))?;

    Ok(json!({
        "localPath": local.display().to_string(),
        "bytes": bytes,
        "remotePath": path,
    })
    .to_string())
}

async fn sftp_upload_tool(
    sessions: &Arc<SessionManager>,
    args: &Value,
) -> Result<String, String> {
    let session_id = resolve_session(sessions, args).await?;
    let name = arg_str(args, "name")?;
    let remote_path = arg_str(args, "remotePath")?;
    let overwrite = args
        .get("overwrite")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);

    let local = staging_file(name)?;
    let data = std::fs::read(&local)
        .map_err(|err| format!("读取暂存文件失败（{name}）：{err}"))?;
    let bytes = data.len();

    if !overwrite
        && sessions
            .sftp_exists(&session_id, remote_path)
            .await
            .map_err(|e| e.to_string())?
    {
        return Err(format!(
            "{remote_path} 已存在；确认覆盖请传 overwrite=true"
        ));
    }

    sessions
        .sftp_write(&session_id, AccessSource::Ai, remote_path, data)
        .await
        .map_err(|e| e.to_string())?;

    Ok(json!({
        "uploaded": true,
        "bytes": bytes,
        "remotePath": remote_path,
    })
    .to_string())
}

async fn close_session(sessions: &Arc<SessionManager>, args: &Value) -> Result<String, String> {
    let session_id = resolve_session(sessions, args).await?;
    sessions
        .close(&session_id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(json!({ "closed": true }).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(list: &[&str]) -> Vec<Vec<u8>> {
        let args = json!({ "keys": list });
        keys_from(&args).expect("keys 应该能解析")
    }

    #[test]
    fn single_char_keys_keep_case() {
        // vim 的 G/O/A/I 区分大小写；曾经全转小写，导致 "G" 变 "g"
        assert_eq!(keys(&["G"]), vec![b"G".to_vec()]);
        assert_eq!(keys(&["g"]), vec![b"g".to_vec()]);
        assert_eq!(keys(&["O"]), vec![b"O".to_vec()]);
        assert_eq!(keys(&[":"]), vec![b":".to_vec()]);
        assert_eq!(keys(&["~"]), vec![b"~".to_vec()]);
    }

    #[test]
    fn escape_g_o_stays_three_keys() {
        let parsed = keys(&["Escape", "G", "o"]);
        assert_eq!(parsed.len(), 3, "每个按键应该单独一份字节");
        assert_eq!(parsed[0], vec![0x1b]);
        assert_eq!(parsed[1], b"G".to_vec());
        assert_eq!(parsed[2], b"o".to_vec());
    }

    #[test]
    fn named_keys_are_case_insensitive() {
        assert_eq!(keys(&["escape"]), keys(&["Escape"]));
        assert_eq!(keys(&["ENTER"]), vec![b"\r".to_vec()]);
        assert_eq!(keys(&["esc"]), vec![vec![0x1b]]);
    }

    #[test]
    fn control_keys_map_to_bytes() {
        assert_eq!(keys(&["C-c"]), vec![vec![0x03]]);
        assert_eq!(keys(&["C-a"]), vec![vec![0x01]]);
        // C-[ 等价 ESC；这里确保没有被单字符分支提前吃掉
        assert_eq!(keys(&["C-["]), vec![vec![0x1b]]);
    }

    #[test]
    fn unknown_key_is_rejected() {
        let args = json!({ "keys": ["NotAKey"] });
        assert!(keys_from(&args).is_err());
    }

    #[test]
    fn gap_defaults_and_clamps() {
        assert_eq!(
            key_gap_from(&json!({})),
            Duration::from_millis(DEFAULT_KEY_GAP_MS)
        );
        assert_eq!(key_gap_from(&json!({ "intervalMs": 0 })), Duration::ZERO);
        // 上限 5s，避免误传大值把请求拖死
        assert_eq!(
            key_gap_from(&json!({ "intervalMs": 99999 })),
            Duration::from_millis(5000)
        );
    }
}
