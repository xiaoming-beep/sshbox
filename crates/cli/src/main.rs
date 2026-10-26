//! SSH Box CLI —— M1 阶段用来验证内核，不对外发布。
//!
//! 注意：`connect` 走 Windows 控制台行模式（非 raw mode），交互程序体验有限；
//! 真正的终端体验在 Tauri 界面的 xterm.js 里。
//!
//! 输出一律用 std 的阻塞句柄：tokio 的 stdout 在管道重定向下会丢数据。

use std::io::Write as _;

use clap::{Parser, Subcommand};
use sshcore::config::{AppPaths, AuthKind, Connection, ConnectionStore};
use sshcore::pty::{self, ChannelEvent, ConnectParams, Target};
use sshcore::{credential, known_hosts, Error, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

#[derive(Parser)]
#[command(name = "sshbox-cli", version, about = "SSH Box 内核验证工具")]
struct Cli {
    /// 数据目录（默认 %APPDATA%\sshbox）
    #[arg(long, global = true)]
    data_dir: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 列出已保存的连接
    List,
    /// 新增/覆盖一条连接
    Add {
        #[arg(long)]
        id: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        host: String,
        #[arg(long, default_value_t = 22)]
        port: u16,
        #[arg(long)]
        user: String,
        /// 认证方式：password | key
        #[arg(long, default_value = "password")]
        auth: String,
        /// 私钥路径（--auth key 时必填）
        #[arg(long)]
        key: Option<String>,
        /// 私钥是否带口令
        #[arg(long)]
        passphrase: bool,
    },
    /// 从标准输入读取并写入凭据管理器
    SetSecret { id: String },
    /// 交互式连接（本地终端 ↔ 远端 PTY）
    Connect {
        id: String,
        /// 首次连接直接信任主机指纹
        #[arg(long)]
        accept_new: bool,
        #[arg(long, default_value_t = 120)]
        cols: u32,
        #[arg(long, default_value_t = 40)]
        rows: u32,
    },
    /// 一次性执行命令（独立通道，返回退出码）
    Exec {
        id: String,
        #[arg(long)]
        accept_new: bool,
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
}

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("错误：{err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let paths = match &cli.data_dir {
        Some(dir) => AppPaths::at(dir.clone()),
        None => AppPaths::discover()?,
    };
    paths.ensure()?;

    match cli.command {
        Command::List => cmd_list(&paths),
        Command::Add {
            id,
            name,
            host,
            port,
            user,
            auth,
            key,
            passphrase,
        } => cmd_add(&paths, id, name, host, port, user, auth, key, passphrase),
        Command::SetSecret { id } => cmd_set_secret(&paths, &id).await,
        Command::Connect {
            id,
            accept_new,
            cols,
            rows,
        } => cmd_connect(&paths, &id, accept_new, cols, rows).await,
        Command::Exec {
            id,
            accept_new,
            command,
        } => cmd_exec(&paths, &id, accept_new, command).await,
    }
}

fn cmd_list(paths: &AppPaths) -> Result<()> {
    let path = paths.connections_file();
    let store = ConnectionStore::load(&path)?;
    if store.connections.is_empty() {
        println!("还没有连接。用 `sshbox-cli add ...` 新增一条。");
        println!("配置文件：{}", path.display());
        return Ok(());
    }
    println!("数据目录：{}", paths.root.display());
    for conn in &store.connections {
        println!(
            "- {:<16} {:<20} {}@{}:{}  [{}]  ai={}",
            conn.id,
            conn.name,
            conn.user,
            conn.host,
            conn.port,
            conn.auth.label(),
            if conn.ai_access { "on" } else { "off" }
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_add(
    paths: &AppPaths,
    id: String,
    name: String,
    host: String,
    port: u16,
    user: String,
    auth: String,
    key: Option<String>,
    passphrase: bool,
) -> Result<()> {
    let auth_kind = match auth.as_str() {
        "password" => AuthKind::Password,
        "key" => {
            let key_path = key.ok_or_else(|| {
                Error::Config("--auth key 需要同时指定 --key <私钥路径>".to_string())
            })?;
            AuthKind::PrivateKey {
                key_path,
                has_passphrase: passphrase,
            }
        }
        other => {
            return Err(Error::Config(format!(
                "未知认证方式 {other}，可选：password | key"
            )))
        }
    };

    let path = paths.connections_file();
    let mut store = ConnectionStore::load(&path)?;
    store.upsert(Connection {
        id: id.clone(),
        name,
        host,
        port,
        user,
        auth: auth_kind,
        credential_ref: credential::credential_ref_for(&id),
        shell_integration: true,
        ai_access: true,
        shell_command: None,
        group: String::new(),
        tags: vec![],
        extra: Default::default(),
    });
    store.save(&path)?;

    println!("已保存连接 {id} → {}", path.display());
    println!("下一步：sshbox-cli set-secret {id}");
    Ok(())
}

async fn cmd_set_secret(paths: &AppPaths, id: &str) -> Result<()> {
    let store = ConnectionStore::load(&paths.connections_file())?;
    let conn = store.get(id)?.clone();

    eprintln!("请输入 {id} 的凭据（回车结束；也支持通过管道传入）：");
    let mut line = String::new();
    {
        // tokio::io::Stdin 只实现 AsyncRead，read_line 需要 AsyncBufRead
        let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
        reader.read_line(&mut line).await?;
    }
    let secret = line.trim_end_matches(['\r', '\n']);
    if secret.is_empty() {
        return Err(Error::Config("凭据不能为空".to_string()));
    }

    credential::set_secret(&conn.credential_ref, secret)?;
    println!("凭据已写入 Windows 凭据管理器：{}", conn.credential_ref);
    Ok(())
}

fn resolve_secret(conn: &Connection) -> Result<String> {
    credential::resolve_secret(conn)
}

async fn cmd_connect(
    paths: &AppPaths,
    id: &str,
    accept_new: bool,
    cols: u32,
    rows: u32,
) -> Result<()> {
    let store = ConnectionStore::load(&paths.connections_file())?;
    let conn = store.get(id)?.clone();
    let secret = resolve_secret(&conn)?;

    let kh_path = paths.known_hosts_file();
    let mut known = known_hosts::KnownHosts::load(&kh_path)?;

    let params = ConnectParams {
        target: Target::from_connection(&conn, secret),
        known_hosts: known.clone(),
        accept_new_host_key: accept_new,
    };

    let mut session = pty::open_pty(params, "xterm-256color", cols, rows).await?;

    if known.fingerprint(&conn.host, conn.port) != Some(session.fingerprint.as_str()) {
        known.trust(&conn.host, conn.port, &session.fingerprint);
        known.save(&kh_path)?;
        eprintln!(
            "[sshbox] 已信任 {}:{} 指纹 {}",
            conn.host, conn.port, session.fingerprint
        );
    }

    eprintln!(
        "[sshbox] 已连接 {}（{}@{}:{}）",
        conn.name, conn.user, conn.host, conn.port
    );

    let result = pump(&mut session).await;
    let _ = session.close().await;
    result
}

async fn pump(session: &mut pty::PtySession) -> Result<()> {
    let mut stdin = tokio::io::stdin();
    let mut buf = vec![0u8; 4096];

    loop {
        tokio::select! {
            event = session.next() => {
                match event {
                    Some(ChannelEvent::Data(chunk)) | Some(ChannelEvent::ExtendedData(chunk)) => {
                        let mut out = std::io::stdout();
                        out.write_all(&chunk)?;
                        out.flush()?;
                    }
                    Some(ChannelEvent::ExitStatus(code)) => {
                        let mut out = std::io::stdout();
                        out.write_all(format!("\r\n[sshbox] 远端退出码 {code}\r\n").as_bytes())?;
                        out.flush()?;
                    }
                    Some(ChannelEvent::Eof) | Some(ChannelEvent::Closed) | None => break,
                }
            }
            read = stdin.read(&mut buf) => {
                let n = read?;
                if n == 0 {
                    break;
                }
                session.write(&buf[..n]).await?;
            }
        }
    }
    Ok(())
}

async fn cmd_exec(
    paths: &AppPaths,
    id: &str,
    accept_new: bool,
    command: Vec<String>,
) -> Result<()> {
    let store = ConnectionStore::load(&paths.connections_file())?;
    let conn = store.get(id)?.clone();
    let secret = resolve_secret(&conn)?;

    let kh_path = paths.known_hosts_file();
    let mut known = known_hosts::KnownHosts::load(&kh_path)?;
    let params = ConnectParams {
        target: Target::from_connection(&conn, secret),
        known_hosts: known.clone(),
        accept_new_host_key: accept_new,
    };

    let command_line = command.join(" ");
    let out = pty::exec(params, &command_line).await?;

    // 首次信任后落盘，后续连接不再需要 --accept-new
    if known.fingerprint(&conn.host, conn.port) != Some(out.fingerprint.as_str()) {
        known.trust(&conn.host, conn.port, &out.fingerprint);
        known.save(&kh_path)?;
        eprintln!(
            "[sshbox] 已信任 {}:{} 指纹 {}",
            conn.host, conn.port, out.fingerprint
        );
    }

    {
        let mut stdout = std::io::stdout();
        stdout.write_all(&out.stdout)?;
        stdout.flush()?;
        let mut stderr = std::io::stderr();
        stderr.write_all(&out.stderr)?;
        stderr.flush()?;
    }

    let code = out.exit_status.unwrap_or(0);
    eprintln!(
        "\n[sshbox] exit={code}  stdout={}B stderr={}B",
        out.stdout.len(),
        out.stderr.len()
    );
    std::process::exit(code as i32);
}
