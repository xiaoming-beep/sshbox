use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("配置错误：{0}")]
    Config(String),

    #[error("凭据错误：{0}")]
    Credential(String),

    #[error("找不到连接：{0}")]
    ConnectionNotFound(String),

    #[error("主机指纹不匹配：已记录 {expected}，实际 {actual}。已拒绝连接，请核对后在下方文件中移除该主机记录")]
    HostKeyMismatch { expected: String, actual: String },

    #[error("首次连接 {host}:{port}，主机指纹 {fingerprint}。请在 SSH Box 界面确认并信任该指纹后重试（命令行工具可用 --accept-new）")]
    HostKeyUnknown {
        host: String,
        port: u16,
        fingerprint: String,
    },

    #[error("认证失败：用户名或密码/私钥不正确")]
    AuthFailed,

    #[error("SSH 错误：{0}")]
    Ssh(#[from] russh::Error),

    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;
