# SSH Box

轻量级 Windows SSH 工具。**人工和 AI 共用同一个终端会话**：你在界面里 `sudo -i`、进 `mysql`，然后把控制权交给 AI 继续；AI 敲的每个字符你都看得见，随时能接管。

- 桌面应用常驻托盘，对外提供本地 HTTP 服务（MCP Streamable HTTP + REST）
- AI 客户端（Claude Code / Codex / Cursor 等）按 URL 注册，**不是**把本应用当子进程拉起
- 密码与私钥口令存在 Windows 凭据管理器，不进配置文件、不进 AI 上下文、不进日志

设计文档：[`docs/design.md`](docs/design.md)

## 当前状态

| 阶段 | 内容 | 状态 |
|---|---|---|
| M0 | 开发环境（Rust MSVC / Node / VS Build Tools） | ✅ 完成 |
| M1 | `sshcore` 内核 + CLI：连接、凭据、指纹、PTY、exec | ✅ 完成，已用真实主机验证 密码认证 / TOFU / 退出码 / stdout+stderr |
| M2 | 界面：连接列表、标签页、xterm 终端、指纹弹窗、凭据管理 | ✅ 基本完成，双向收发已验证；待补：错误提示细化、重连 |
| M3 | 终端状态：命令流水 + OSC 133 退出码 + 输入来源仲裁 | ✅ 已完成（bash 真机验证；ash/dash 静默降级）。待补：输入锁界面横幅、vt100 屏幕快照 |
| M4 | daemon：HTTP + REST + SSE | ✅ 已完成，已用真实请求验证鉴权/会话/输入/输出/历史。待做：托盘常驻 |
| M5 | MCP `/mcp`（9 个工具）+ skill | ✅ 已完成，curl 验证 `initialize`/`tools/list`/`tools/call`。待做：stdio 中继（给只支持 stdio 的客户端） |
| M6 | 托盘常驻（关闭窗口不退出） | ✅ 已完成并验证：关闭窗口后进程与 MCP 服务照常运行；托盘菜单提供 显示/断开全部/退出 |
| M7 | 打包（含 WebView2 bootstrapper）、开机自启 | 未开始 |

## 运行桌面端

前置：按下面的「开发环境」装好 Rust MSVC / Node / VS Build Tools / pnpm（Windows 11 自带 WebView2）。

```powershell
# 0) 首次/换机器：安装前端依赖（仓库不带 node_modules）
cd apps\desktop
pnpm install

# 1) 构建前端资源（Tauri 会把 dist 内嵌进 exe）
pnpm build

# 2) 编译并运行桌面应用
cd ..\..
cargo build -p sshbox-desktop
.\target\debug\sshbox-desktop.exe
```

首次 `cargo build` 要拉依赖并编 russh/tauri，几分钟起步；之后增量很快。

窗口里：左侧「+ 新建」添加连接 → 双击连接开会话 → 顶部标签切换 → 右侧终端直接敲命令。
密码写进 Windows 凭据管理器；配置文件 `%APPDATA%\sshbox\connections.json` 里只有 `credentialRef`。

## 接入 AI 客户端（MCP）

应用运行时在本机提供 MCP 服务（Streamable HTTP，仅回环）：

- 地址：`http://127.0.0.1:17821/mcp`
- 认证：`Authorization: Bearer <令牌>`，令牌在界面左下角「本地服务」里，点开可复制

Claude Code：

```bash
claude mcp add --transport http sshbox http://127.0.0.1:17821/mcp \
  --header "Authorization: Bearer <令牌>"
```

只支持 stdio 的客户端，用 `mcp-remote` 中转：

```bash
npx -y mcp-remote http://127.0.0.1:17821/mcp --header "Authorization: Bearer <令牌>"
```

连上后 AI 能拿到 9 个工具；它执行的每条命令都会实时出现在界面的命令流水里（标记 `AI`）。
agent 的使用约定见 [`skills/sshbox/SKILL.md`](skills/sshbox/SKILL.md)。

## 目录

```
crates/sshcore/    内核：配置、凭据、known_hosts、会话池、PTY、OSC 133（无 UI 依赖）
crates/daemon/     本地 HTTP 服务：/v1 REST + SSE + /mcp（MCP 工具）
crates/cli/        sshbox-cli，内核验证用
apps/desktop/      Tauri v2 桌面应用（连接列表 / 标签页 / xterm / 命令流水）
skills/sshbox/     给 agent 的使用约定（SKILL.md）
docs/              设计文档
scripts/           开发环境与联调脚本
```

## 开发环境

```powershell
# 管理员 PowerShell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\setup-dev.ps1
# 新开终端，或：
. .\scripts\env.ps1
```

## M1 手动验证

```powershell
cargo build
$cli = ".\target\debug\sshbox-cli.exe"

# 1) 新增连接
& $cli add --id vm1 --name 测试机 --host 10.0.0.11 --user deploy --auth password

# 2) 写入密码（存进 Windows 凭据管理器，不落配置文件）
& $cli set-secret vm1

# 3) 一次性执行命令，检查退出码
& $cli exec vm1 --accept-new -- "uname -a; whoami"

# 4) 交互式连接（Ctrl+C 中断远端命令，exit 退出）
& $cli connect vm1 --accept-new
```

凭据可用「控制面板 → 凭据管理器 → Windows 凭据」查看，条目名 `sshbox`。

## 约定

- `%APPDATA%\sshbox\` 下只有配置与指纹，**没有任何明文密钥**
- 首次连接必须确认主机指纹（`--accept-new` / 界面弹窗），之后不匹配一律拒绝
- 内核 `sshcore` 不依赖 Tauri，便于单测和被 CLI/daemon/MCP 复用
