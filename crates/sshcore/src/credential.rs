//! 凭据存储：Windows 凭据管理器（经 keyring 的 windows-native 后端）。
//!
//! 约定：`credential_ref` 形如 `wincred:conn/<id>`，去掉 `wincred:` 前缀后作为
//! keyring 的 account，service 固定为 `sshbox`。明文只在内存中短暂存在。

use keyring::Entry;

use crate::error::{Error, Result};

const SERVICE: &str = "sshbox";

pub fn credential_ref_for(conn_id: &str) -> String {
    format!("wincred:conn/{conn_id}")
}

pub fn secret_ref_for(conn_id: &str, secret_id: &str) -> String {
    format!("wincred:secret/{conn_id}/{secret_id}")
}

fn account(credential_ref: &str) -> &str {
    credential_ref.strip_prefix("wincred:").unwrap_or(credential_ref)
}

fn entry(credential_ref: &str) -> Result<Entry> {
    Entry::new(SERVICE, account(credential_ref))
        .map_err(|e| Error::Credential(format!("打开凭据条目 {credential_ref} 失败：{e}")))
}

pub fn set_secret(credential_ref: &str, secret: &str) -> Result<()> {
    entry(credential_ref)?
        .set_password(secret)
        .map_err(|e| Error::Credential(format!("写入凭据失败：{e}")))
}

pub fn get_secret(credential_ref: &str) -> Result<String> {
    entry(credential_ref)?.get_password().map_err(|e| match e {
        keyring::Error::NoEntry => Error::Credential(format!(
            "尚未保存凭据（{credential_ref}），请先运行：sshbox-cli set-secret <连接ID>"
        )),
        other => Error::Credential(format!("读取凭据失败：{other}")),
    })
}

pub fn delete_secret(credential_ref: &str) -> Result<()> {
    match entry(credential_ref)?.delete_password() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(Error::Credential(format!("删除凭据失败：{e}"))),
    }
}

/// 按连接配置取出本次连接所需的密钥（无口令私钥返回空串）。
pub fn resolve_secret(conn: &crate::config::Connection) -> Result<String> {
    if conn.auth.needs_secret() {
        get_secret(&conn.credential_ref)
    } else {
        Ok(String::new())
    }
}
