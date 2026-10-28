//! SFTP：列目录、stat、读写、删除、重命名。
//!
//! 安全要点：**所有路径都先按真实路径（realpath / canonicalize）解析，再与允许范围比对**。
//! 只做字符串前缀比对是能被符号链接绕过的（`ln -s /root /tmp/safe/link`），
//! 所以写入前会解析父目录、读前会解析目标本身，并且拒绝“目标是符号链接”的写入。
//!
//! 调用方（SessionManager）负责决定允许范围；这里的 `roots` 为空表示不限制
//! （只给界面这类人类操作使用，AI 侧永远会带上范围）。

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

/// 一次目录列举的结果（带规范化后的绝对路径，供面包屑使用）
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Listing {
    pub dir: String,
    pub entries: Vec<SftpEntry>,
}

fn normalize(path: &str) -> Result<&str> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(Error::Other("路径不能为空".to_string()));
    }
    if trimmed.contains('\0') {
        return Err(Error::Other("路径包含非法字符".to_string()));
    }
    Ok(trimmed)
}

fn join_path(parent: &str, name: &str) -> String {
    if parent.ends_with('/') {
        format!("{parent}{name}")
    } else {
        format!("{parent}/{name}")
    }
}

/// 真实路径是否落在允许范围内（按路径段匹配，`/tmp/a` 不算 `/tmp/ab`）
pub fn is_under(roots: &[String], real_path: &str) -> bool {
    if roots.is_empty() {
        return false;
    }
    roots.iter().any(|root| {
        let root = root.trim_end_matches('/');
        if root.is_empty() {
            // 根目录授权
            return real_path.starts_with('/');
        }
        real_path == root || real_path.starts_with(&format!("{root}/"))
    })
}

/// 校验真实路径在范围内。**空范围 = 拒绝一切**（人类传入 ["/"] 表示不限）。
fn ensure_under(roots: &[String], real_path: &str) -> Result<()> {
    if is_under(roots, real_path) {
        return Ok(());
    }
    let scope = if roots.is_empty() {
        "未授权任何目录（在文件面板里点「允许写入」，或在连接配置里设 sftp.writePaths）".to_string()
    } else {
        roots.join(", ")
    };
    Err(Error::Other(format!(
        "路径 {real_path} 不在允许范围内（{scope}）"
    )))
}

/// 把路径解析成真实绝对路径；解析失败则原样返回（新文件可能还不存在）
async fn real_path(
    sftp: &russh_sftp::client::SftpSession,
    path: &str,
) -> String {
    sftp.canonicalize(path.to_string())
        .await
        .unwrap_or_else(|_| path.to_string())
}

/// 解析写入目标：父目录按真实路径解析，文件名保持原样
async fn resolve_write_target(
    sftp: &russh_sftp::client::SftpSession,
    path: &str,
    roots: &[String],
) -> Result<String> {
    let (parent, name) = match path.rsplit_once('/') {
        Some((parent, name)) => (if parent.is_empty() { "/" } else { parent }, name),
        None => ("/", path),
    };

    if name.is_empty() {
        return Err(Error::Other("写入路径缺少文件名".to_string()));
    }

    let real_parent = real_path(sftp, parent).await;
    if !real_parent.starts_with('/') {
        return Err(Error::Other(format!("无法解析目录：{parent}")));
    }

    let target = join_path(&real_parent, name);

    // 用**目标**眼里而不是父目录校验：
    // 否则“创建授权根目录本身”（/tmp/sbtest）会因父目录 /tmp 不在范围内而被误拦。
    // 由于 target 是基于 real_parent 拼的，符号链接逃逸仍会被拦住。
    ensure_under(roots, &target)?;

    // 目标已存在时看它是不是符号链接：是则拒绝，防止写到范围外
    if let Ok(meta) = sftp.symlink_metadata(target.clone()).await {
        if meta.file_type().is_symlink() {
            return Err(Error::Other(format!(
                "{target} 是符号链接，出于安全考虑拒绝写入"
            )));
        }
    } else if let Ok(meta) = sftp.metadata(target.clone()).await {
        if meta.file_type().is_dir() {
            return Err(Error::Other(format!("{target} 是目录，不能按文件写入")));
        }
    }

    Ok(target)
}

fn to_entry(path: &str, name: &str, attrs: &russh_sftp::protocol::FileAttributes) -> SftpEntry {
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

pub async fn list_dir(
    handle: &ConnectionHandle,
    path: &str,
    roots: &[String],
) -> Result<Listing> {
    let path = normalize(path)?;
    let sftp = connect(handle).await?;

    let base = real_path(&sftp, path).await;
    ensure_under(roots, &base)?;

    let mut reader = sftp
        .read_dir(base.as_str())
        .await
        .map_err(|err| Error::Other(format!("列目录失败（{path}）：{err}")))?;

    let mut entries: Vec<SftpEntry> = Vec::new();
    for entry in reader.by_ref() {
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        let full = join_path(&base, &name);
        entries.push(to_entry(&full, &name, &entry.metadata()));
    }

    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    Ok(Listing { dir: base, entries })
}

pub async fn stat(
    handle: &ConnectionHandle,
    path: &str,
    roots: &[String],
) -> Result<SftpEntry> {
    let path = normalize(path)?;
    let sftp = connect(handle).await?;
    let real = real_path(&sftp, path).await;
    ensure_under(roots, &real)?;

    let attrs = sftp
        .metadata(real.as_str())
        .await
        .map_err(|err| Error::Other(format!("读取属性失败（{path}）：{err}")))?;
    let name = real.rsplit('/').next().unwrap_or(real.as_str());
    Ok(to_entry(&real, name, &attrs))
}

pub async fn read_file(
    handle: &ConnectionHandle,
    path: &str,
    max_bytes: usize,
    roots: &[String],
) -> Result<Vec<u8>> {
    let path = normalize(path)?;
    let sftp = connect(handle).await?;

    // 符号链接会先被解析成真实路径，再与允许范围比对
    let real = real_path(&sftp, path).await;
    ensure_under(roots, &real)?;

    let attrs = sftp
        .metadata(real.as_str())
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
        .open(real.as_str())
        .await
        .map_err(|err| Error::Other(format!("打开文件失败（{path}）：{err}")))?;

    let mut buf = Vec::with_capacity(size);
    file.take(max_bytes as u64 + 1)
        .read_to_end(&mut buf)
        .await
        .map_err(|err| Error::Other(format!("读取文件失败（{path}）：{err}")))?;

    if buf.len() > max_bytes {
        buf.truncate(max_bytes);
    }
    Ok(buf)
}

/// 判断远端路径是否存在（用于 overwrite 判断，不做范围校验）
pub async fn exists(handle: &ConnectionHandle, path: &str) -> bool {
    let Ok(path) = normalize(path) else {
        return false;
    };
    match connect(handle).await {
        Ok(sftp) => sftp.metadata(path).await.is_ok(),
        Err(_) => false,
    }
}

/// 写入远端文件（覆盖）。父目录不存在时自动创建。
pub async fn write_file(
    handle: &ConnectionHandle,
    path: &str,
    data: &[u8],
    roots: &[String],
) -> Result<String> {
    let path = normalize(path)?;
    let sftp = connect(handle).await?;

    let target = resolve_write_target(&sftp, path, roots).await?;

    // 父目录不存在时自动创建（写入目标已解析过真实父目录，这里只补缺失层级）
    if let Some((parent, _)) = target.rsplit_once('/') {
        let mut current = String::new();
        for segment in parent.split('/').filter(|segment| !segment.is_empty()) {
            current.push('/');
            current.push_str(segment);
            let _ = sftp.create_dir(&current).await;
        }
    }

    let mut file = sftp
        .create(target.clone())
        .await
        .map_err(|err| Error::Other(format!("创建远端文件失败（{target}）：{err}")))?;
    file.write_all(data)
        .await
        .map_err(|err| Error::Other(format!("写入失败（{target}）：{err}")))?;
    let _ = file.flush().await;
    Ok(target)
}

/// 递归创建目录：已存在的层级直接跳过（mkdir -p）
pub async fn mkdir_p(
    handle: &ConnectionHandle,
    path: &str,
    roots: &[String],
) -> Result<String> {
    let path = normalize(path)?;
    if !path.starts_with('/') {
        return Err(Error::Other("目录路径必须是绝对路径".to_string()));
    }
    let sftp = connect(handle).await?;

    let target = resolve_write_target(&sftp, path, roots).await?;

    let mut current = String::new();
    for segment in target.split('/').filter(|segment| !segment.is_empty()) {
        current.push('/');
        current.push_str(segment);
        let _ = sftp.create_dir(&current).await;
    }
    Ok(target)
}

/// 删除：先按文件删，失败再按空目录删
pub async fn remove(
    handle: &ConnectionHandle,
    path: &str,
    roots: &[String],
) -> Result<String> {
    let path = normalize(path)?;
    let sftp = connect(handle).await?;

    let real = real_path(&sftp, path).await;
    ensure_under(roots, &real)?;

    if sftp.remove_file(real.as_str()).await.is_ok() {
        return Ok(real);
    }
    sftp.remove_dir(real.as_str())
        .await
        .map_err(|err| Error::Other(format!("删除失败（{path}）：{err}")))?;
    Ok(real)
}

pub async fn rename(
    handle: &ConnectionHandle,
    from: &str,
    to: &str,
    roots: &[String],
) -> Result<()> {
    let from = normalize(from)?;
    let to = normalize(to)?;
    let sftp = connect(handle).await?;

    let real_from = real_path(&sftp, from).await;
    ensure_under(roots, &real_from)?;

    let real_to = resolve_write_target(&sftp, to, roots).await?;

    sftp.rename(real_from.as_str(), real_to.as_str())
        .await
        .map_err(|err| Error::Other(format!("重命名失败（{from} → {to}）：{err}")))
}
