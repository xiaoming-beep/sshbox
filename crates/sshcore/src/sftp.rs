//! 只读 SFTP：列目录、stat、读文件。
//!
//! 每次操作单独开一个 sftp subsystem channel（快），暂时不做连接缓存；
//! 写入类操作（上传/删除/重命名）留到下一版，配合路径白名单与界面审批。

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::{Error, Result};
use crate::pty::ConnectionHandle;

/// 目录项 / stat 结果
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SftpEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    /// 权限位（如 0o644）
    pub mode: Option<u32>,
    /// 修改时间（Unix 秒）
    pub modified: Option<u32>,
}

fn normalize(path: &str) -> Result<&str> {
    let trimmed = path.trim();
    if trimmed.contains('\0') {
        return Err(Error::Other("路径包含非法字符".to_string()));
    }
    Ok(if trimmed.is_empty() { "." } else { trimmed })
}

fn join_path(parent: &str, name: &str) -> String {
    if parent.ends_with('/') {
        format!("{parent}{name}")
    } else {
        format!("{parent}/{name}")
    }
}

fn to_entry(path: &str, name: &str, attrs: &russh_sftp::protocol::FileAttributes) -> SftpEntry {
    // 权限位高 4 位标识文件类型：0o040000 = 目录
    let is_dir = attrs
        .permissions
        .map(|mode| mode & 0o170000 == 0o040000)
        .unwrap_or(false);

    SftpEntry {
        name: name.to_string(),
        path: path.to_string(),
        is_dir,
        size: attrs.size.unwrap_or(0),
        mode: attrs.permissions.map(|mode| mode & 0o7777),
        modified: attrs.mtime,
    }
}

/// 在既有 SSH 连接上开一个 SFTP 会话
async fn connect(handle: &ConnectionHandle) -> Result<russh_sftp::client::SftpSession> {
    let channel = handle.channel_open_session().await?;
    channel.request_subsystem(false, "sftp").await?;
    russh_sftp::client::SftpSession::new(channel.into_stream())
        .await
        .map_err(|err| Error::Other(format!("SFTP 初始化失败：{err}")))
}

pub async fn list_dir(handle: &ConnectionHandle, path: &str) -> Result<Vec<SftpEntry>> {
    let path = normalize(path)?;
    let sftp = connect(handle).await?;

    let mut reader = sftp
        .read_dir(path)
        .await
        .map_err(|err| Error::Other(format!("列目录失败（{path}）：{err}")))?;

    let mut entries: Vec<SftpEntry> = Vec::new();
    for entry in reader.by_ref() {
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        let full = join_path(path, &name);
        entries.push(to_entry(&full, &name, &entry.metadata()));
    }

    // 目录在前，其余按名字排序
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    Ok(entries)
}

pub async fn stat(handle: &ConnectionHandle, path: &str) -> Result<SftpEntry> {
    let path = normalize(path)?;
    let sftp = connect(handle).await?;
    let attrs = sftp
        .metadata(path)
        .await
        .map_err(|err| Error::Other(format!("读取属性失败（{path}）：{err}")))?;
    let name = path.rsplit('/').next().unwrap_or(path);
    Ok(to_entry(path, name, &attrs))
}

pub async fn read_file(handle: &ConnectionHandle, path: &str, max_bytes: usize) -> Result<Vec<u8>> {
    let path = normalize(path)?;
    let sftp = connect(handle).await?;

    let attrs = sftp
        .metadata(path)
        .await
        .map_err(|err| Error::Other(format!("读取属性失败（{path}）：{err}")))?;

    if attrs.permissions.map(|m| m & 0o170000 == 0o040000).unwrap_or(false) {
        return Err(Error::Other(format!("{path} 是目录，不能按文件读取")));
    }

    let size = attrs.size.unwrap_or(0) as usize;
    if size > max_bytes {
        return Err(Error::Other(format!(
            "文件 {size} 字节，超过上限 {max_bytes}；请调大 maxBytes 或改用其他方式"
        )));
    }

    let file = sftp
        .open(path)
        .await
        .map_err(|err| Error::Other(format!("打开文件失败（{path}）：{err}")))?;

    let mut buf = Vec::with_capacity(size);
    // 多读 1 字节用于判断是否被截断（文件可能在 stat 之后变大）
    file.take(max_bytes as u64 + 1)
        .read_to_end(&mut buf)
        .await
        .map_err(|err| Error::Other(format!("读取文件失败（{path}）：{err}")))?;

    if buf.len() > max_bytes {
        buf.truncate(max_bytes);
    }
    Ok(buf)
}

/// 判断远端路径是否存在
pub async fn exists(handle: &ConnectionHandle, path: &str) -> bool {
    let Ok(path) = normalize(path) else {
        return false;
    };
    match connect(handle).await {
        Ok(sftp) => sftp.metadata(path).await.is_ok(),
        Err(_) => false,
    }
}

/// 写入远端文件（覆盖）。调用方负责路径策略与大小校验。
pub async fn write_file(handle: &ConnectionHandle, path: &str, data: &[u8]) -> Result<()> {
    let path = normalize(path)?;
    let sftp = connect(handle).await?;

    // 父目录不存在时自动创建：推送文件不该要求调用方先 mkdir
    if let Some((parent, _)) = path.rsplit_once('/') {
        let mut current = String::new();
        for segment in parent.split('/').filter(|segment| !segment.is_empty()) {
            current.push('/');
            current.push_str(segment);
            let _ = sftp.create_dir(&current).await;
        }
    }

    let mut file = sftp
        .create(path)
        .await
        .map_err(|err| Error::Other(format!("创建远端文件失败（{path}）：{err}")))?;
    file.write_all(data)
        .await
        .map_err(|err| Error::Other(format!("写入失败（{path}）：{err}")))?;
    let _ = file.flush().await;
    Ok(())
}

pub async fn mkdir(handle: &ConnectionHandle, path: &str) -> Result<()> {
    let path = normalize(path)?;
    let sftp = connect(handle).await?;
    sftp.create_dir(path)
        .await
        .map_err(|err| Error::Other(format!("创建目录失败（{path}）：{err}")))
}

/// 递归创建目录：已存在的层级直接跳过（mkdir -p）
pub async fn mkdir_p(handle: &ConnectionHandle, path: &str) -> Result<()> {
    let path = normalize(path)?;
    if !path.starts_with('/') {
        return Err(Error::Other("目录路径必须是绝对路径".to_string()));
    }
    let sftp = connect(handle).await?;
    let mut current = String::new();
    for segment in path.split('/').filter(|segment| !segment.is_empty()) {
        current.push('/');
        current.push_str(segment);
        // 已存在会报错，忽略；最终写入时会暴露真正的权限问题
        let _ = sftp.create_dir(&current).await;
    }
    Ok(())
}

/// 删除：先按文件删，失败再按空目录删
pub async fn remove(handle: &ConnectionHandle, path: &str) -> Result<()> {
    let path = normalize(path)?;
    let sftp = connect(handle).await?;
    if sftp.remove_file(path).await.is_ok() {
        return Ok(());
    }
    sftp.remove_dir(path)
        .await
        .map_err(|err| Error::Other(format!("删除失败（{path}）：{err}")))
}

pub async fn rename(handle: &ConnectionHandle, from: &str, to: &str) -> Result<()> {
    let from = normalize(from)?;
    let to = normalize(to)?;
    let sftp = connect(handle).await?;
    sftp.rename(from, to)
        .await
        .map_err(|err| Error::Other(format!("重命名失败（{from} → {to}）：{err}")))
}
