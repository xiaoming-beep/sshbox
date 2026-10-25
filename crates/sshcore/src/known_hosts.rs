//! 主机指纹记录（TOFU）。
//!
//! 首次连接时人工确认指纹，之后任何不匹配都直接拒绝连接，不提供"忽略"。

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KnownHostsEntry {
    #[serde(default)]
    pub fingerprint: String,
    #[serde(default)]
    pub first_seen: u64,
    #[serde(default)]
    pub last_seen: u64,
}

/// `host:22` 按 ssh 习惯简写为 `host`
pub fn host_key(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KnownHosts {
    #[serde(default)]
    pub entries: BTreeMap<String, KnownHostsEntry>,
}

impl KnownHosts {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path)?;
        let text = text.trim_start_matches('\u{feff}');
        serde_json::from_str(text)
            .map_err(|e| Error::Config(format!("解析 {} 失败：{e}", path.display())))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| Error::Config(format!("序列化主机指纹失败：{e}")))?;
        fs::write(path, text)?;
        Ok(())
    }

    pub fn fingerprint(&self, host: &str, port: u16) -> Option<&str> {
        self.entries
            .get(&host_key(host, port))
            .map(|e| e.fingerprint.as_str())
    }

    pub fn trust(&mut self, host: &str, port: u16, fingerprint: &str) {
        let now = now_unix();
        let entry = self
            .entries
            .entry(host_key(host, port))
            .or_insert_with(|| KnownHostsEntry {
                fingerprint: fingerprint.to_string(),
                first_seen: now,
                last_seen: now,
            });
        entry.fingerprint = fingerprint.to_string();
        entry.last_seen = now;
    }

    pub fn forget(&mut self, host: &str, port: u16) -> bool {
        self.entries.remove(&host_key(host, port)).is_some()
    }
}
