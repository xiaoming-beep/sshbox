use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::error::{Error, Result};

/// 应用数据目录（Windows: %APPDATA%\sshbox）
#[derive(Debug, Clone)]
pub struct AppPaths {
    pub root: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        let base = dirs::config_dir()
            .ok_or_else(|| Error::Config("无法定位用户配置目录（%APPDATA%）".to_string()))?;
        Ok(Self {
            root: base.join("sshbox"),
        })
    }

    /// 指定根目录（测试与 CLI `--data-dir` 用）
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn ensure(&self) -> Result<()> {
        fs::create_dir_all(&self.root)?;
        fs::create_dir_all(self.logs())?;
        Ok(())
    }

    pub fn connections_file(&self) -> PathBuf {
        self.root.join("connections.json")
    }

    /// 应用级配置（daemon 端口 / 访问令牌）
    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.json")
    }

    pub fn known_hosts_file(&self) -> PathBuf {
        self.root.join("known_hosts.json")
    }

    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// SQLite 数据库文件
    pub fn db_file(&self) -> PathBuf {
        self.root.join("sshbox.db")
    }
}

fn default_port() -> u16 {
    22
}

fn default_daemon_port() -> u16 {
    17821
}

fn default_true() -> bool {
    true
}

/// 保存的连接配置。**不含任何明文凭据**，只保留壁纸引用。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Connection {
    pub id: String,
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub user: String,
    pub auth: AuthKind,
    /// 形如 `wincred:conn/<id>`，指向 Windows 凭据管理器条目
    pub credential_ref: String,
    /// 是否注入 OSC 133 终端语义标记（降级时自动关闭）
    #[serde(default)]
    pub shell_integration: bool,
    /// 是否允许 AI 在该连接上开会话/输入
    #[serde(default = "default_true")]
    pub ai_access: bool,
    /// 启动命令（代替默认登录 shell），如 `bash`、`tmux new -A -s main`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_command: Option<String>,
    /// 所属文件夹 id（folders 表主键）。空字符串表示未分组。
    ///
    /// 历史遗留：老版本存的是"分组路径字符串"（如 `自己的服务器/生产`），
    /// 现在改成文件夹 id 后，`ConnectionStore` 会把它映射到 `folders` 表。
    /// 由于不再兼容老用户，`group` 字段直接承载 folder_id。
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// 未知字段原样保留：避免旧版本客户端回写时丢数据（曾因此丢掉
    /// group / shellCommand），也让新旧版本可以混跑
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
    /// SFTP 写入策略（缺省 = 禁止一切写入）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sftp: Option<SftpPolicy>,
}

fn default_max_write_bytes() -> u64 {
    4 * 1024 * 1024
}

/// SFTP 写入策略。空 `write_paths` 意味着任何写入都被拒绝。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SftpPolicy {
    /// 允许写入的远端路径前缀（绝对路径），例如 ["/tmp", "/root/uploads"]
    #[serde(default)]
    pub write_paths: Vec<String>,
    /// 单次写入上限（字节），默认 4MB
    #[serde(default = "default_max_write_bytes")]
    pub max_write_bytes: u64,
}

impl Default for SftpPolicy {
    fn default() -> Self {
        Self {
            write_paths: Vec::new(),
            max_write_bytes: default_max_write_bytes(),
        }
    }
}

impl SftpPolicy {
    /// 路径是否在允许写入的前缀之下（按路径段匹配，`/tmpfoo` 不算 `/tmp`）
    pub fn allows(&self, path: &str) -> bool {
        self.write_paths.iter().any(|prefix| {
            let prefix = prefix.trim_end_matches('/');
            path == prefix || path.starts_with(&format!("{prefix}/"))
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum AuthKind {
    Password,
    PrivateKey {
        #[serde(default)]
        key_path: String,
        #[serde(default)]
        has_passphrase: bool,
    },
}

impl AuthKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Password => "密码",
            Self::PrivateKey { .. } => "私钥",
        }
    }

    /// 该认证方式是否需要从凭据管理器取值
    pub fn needs_secret(&self) -> bool {
        match self {
            Self::Password => true,
            Self::PrivateKey { has_passphrase, .. } => *has_passphrase,
        }
    }
}

/// 文件夹（分组）记录。独立成表，支持空文件夹、折叠状态持久化。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Folder {
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub collapsed: bool,
    #[serde(default)]
    pub sort: i64,
    #[serde(default)]
    pub created_at: i64,
}

/// 连接存储：现在是 SQLite（`Db`）的兼容封装。
///
/// 保留 `load(path)` / `save(path)` 的旧签名是为了让 desktop/daemon/cli
/// 三端的调用代码无需改动；实际数据落在 `path` 同目录下的 `sshbox.db`。
#[derive(Debug, Clone)]
pub struct ConnectionStore {
    pub connections: Vec<Connection>,
}

impl ConnectionStore {
    /// 从 SQLite 读取全部连接。`path` 通常是 `connections.json` 的旧路径，
    /// 我们只取它的父目录来定位数据库。
    pub fn load(path: &Path) -> Result<Self> {
        let root = path.parent().unwrap_or(path);
        let db = Db::open(root)?;
        let connections = db.list_connections()?;
        Ok(Self { connections })
    }

    /// 打开底层数据库句柄（新增的读写入口，逐步替代整文件读写）
    pub fn open_db(path: &Path) -> Result<Db> {
        let root = path.parent().unwrap_or(path);
        Db::open(root)
    }

    /// 把内存里的连接全部写回（兼容旧调用；建议改用 `open_db` + `upsert_connection`）
    pub fn save(&self, path: &Path) -> Result<()> {
        let db = Self::open_db(path)?;
        for conn in &self.connections {
            db.upsert_connection(conn)?;
        }
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<&Connection> {
        self.connections
            .iter()
            .find(|c| c.id == id)
            .ok_or_else(|| Error::ConnectionNotFound(id.to_string()))
    }

    pub fn upsert(&mut self, conn: Connection) {
        match self.connections.iter_mut().find(|c| c.id == conn.id) {
            Some(slot) => *slot = conn,
            None => self.connections.push(conn),
        }
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.connections.len();
        self.connections.retain(|c| c.id != id);
        self.connections.len() != before
    }
}

/// 应用级配置：本地服务的端口与访问令牌。
///
/// 令牌在首次启动时生成并落盘，客户端（AI）需要用它在 `Authorization: Bearer` 头里带上，
/// 未授权的请求一律 401。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    #[serde(default = "default_daemon_port")]
    pub port: u16,
    #[serde(default)]
    pub token: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            port: default_daemon_port(),
            token: String::new(),
        }
    }
}

impl AppConfig {
    /// 读取配置；首次运行（或令牌为空）时生成新令牌并写入。
    pub fn load_or_create(path: &Path) -> Result<Self> {
        let mut config = if path.exists() {
            let text = fs::read_to_string(path)?;
            let text = text.trim_start_matches('\u{feff}');
            serde_json::from_str(text)
                .map_err(|e| Error::Config(format!("解析 {} 失败：{e}", path.display())))?
        } else {
            Self::default()
        };

        if config.token.trim().is_empty() {
            config.token = generate_token();
            config.save(path)?;
        }
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| Error::Config(format!("序列化配置失败：{e}")))?;
        fs::write(path, text)?;
        Ok(())
    }
}

fn generate_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
