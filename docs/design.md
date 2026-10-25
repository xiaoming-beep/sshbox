# SSH Box 设计文档

> 轻量级 Windows SSH 工具。人工和 AI 共用同一个终端会话；AI 通过本应用提供的本地接口看到会话、输入命令。
> 凭据只存在于应用内（Windows 凭据管理器），永不出现在 AI 上下文或命令行里。

- 版本：v0.2 设计稿（引入共享终端模型）
- 目标平台：Windows 10 1809+ / Windows 11，x64
- 界面语言：简体中文
- 技术栈：Tauri v2（Rust 内核 + React/TS 前端）

---

## 1. 目标与非目标

### 1.1 一期目标（本设计范围）

1. 桌面应用常驻运行（托盘），管理 SSH 连接配置与会话。
2. **会话是共享终端**：人工在界面里敲的输入，与 AI 发来的输入，进入**同一条 SSH 会话的同一个 PTY**。人工可以先登录、`sudo -i`、进 `mysql`，然后把控制权交给 AI 继续。
3. AI 通过**本地 HTTP 服务**（MCP Streamable HTTP + 少量 REST）调用本应用；应用是服务端，不是被客户端拉起的子进程。
4. AI 能：列出活动会话 / 列出已保存连接 / 开会话 / 在共享终端里输入 / 读输出 / 读屏幕快照 / 发 Ctrl-C。
5. 人工能**实时看到 AI 输入了什么、得到什么结果**，并可随时接管或掐断。
6. 认证：密码、私钥（含带口令的私钥）。不含 ssh-agent、不含键盘交互式。

### 1.2 二期目标（设计预留，不实现）

- 文件上传/下载（SFTP），目录白名单 + 大小上限。
- 命令拦截与审批：策略引擎 + 界面审批弹窗（如 `rm -rf`）。
- 凭据注入的更多场景（多套 secret、按 host 匹配）。

### 1.3 非目标

- 不做多用户、不做远程访问（服务只绑 127.0.0.1）。
- 不做跨平台打包（内核保持可移植，但不承诺 mac/linux）。
- 不做跳板机、端口转发、SOCKS。
- **不做安全沙箱**。命令拦截只是"防手滑"的一层，不保证能拦住故意绕过（见 §6.4）。

---

## 2. 架构

### 2.1 进程模型

```
┌──────────────────────────── Desktop App (sshbox.exe, 常驻托盘) ──────────────────────────┐
│                                                                                         │
│  React UI (WebView2) ──Tauri IPC──► ┌─────────────────────────────────────────────────┐  │
│   ├ xterm.js 终端视图               │  sshcore（纯 Rust 内核）                          │  │
│   ├ 命令流水 / 审计                 │  连接库 · 凭据 · 会话池 · PTY 状态机 · 输入仲裁     │  │
│   └ 连接管理 / 设置                 │  执行器 · 审计 · 策略（二期）                      │  │
│                                     └──────┬────────────────────────┬──────────────────┘  │
│                                            │                        │                     │
│                                   adapter: IPC              adapter: HTTP                 │
│                                     (Tauri events)          127.0.0.1:17821               │
│                                                              ├ /mcp  (MCP Streamable HTTP)│
│                                                              └ /v1/* (REST, 调试/脚本)    │
└───────────────────────────────────────────────────────────────┼──────────────────────────┘
                                                                │ Bearer token
                                                     ┌──────────▼──────────┐
                                                     │ AI 客户端            │
                                                     │ Claude Code / Codex │
                                                     │ Cursor / 其他        │
                                                     └─────────────────────┘
```

要点：

- 应用**是服务端**。AI 客户端按 URL + token 注册，不需要拉起本程序。
- 内核 `sshcore` 无 UI 依赖，可单测、可被 CLI 驱动（M0 先打通）。
- 同一内核挂两个 adapter：IPC 给界面，HTTP 给 AI。行为、鉴权、审计完全一致。
- 界面通过 Tauri event 实时刷新（会话状态、AI 输入、输出流）。

### 2.2 会话 = SSH 连接 + 共享 PTY + 状态机 + 输入锁

一条 **Session** 由四部分组成：

| 组件 | 作用 |
|---|---|
| SSH 连接 | 一条 `russh` 连接，可承载多个 channel |
| **主 PTY channel** | 交互终端。人工输入与 AI 输入都写进它的 master；输出流向界面（xterm.js）与内核状态机 |
| **终端状态机** | `vt100` 解析器维护屏幕缓冲、光标、滚动区；对外提供"纯文本屏幕快照" |
| **语义标记** | OSC 133 解析结果：是否在提示符、命令边界、上一条命令的退出码 |
| 输入锁 | 每个时刻只有一个来源（human/ai）持有输入权，人类优先抢占 |

附加能力：会话可另开 **exec channel**（无 PTY）执行无状态命令，用于需要可信退出码/分离 stdout 的场合。

### 2.3 两种输入路径

| 路径 | 进入哪里 | 输出形态 | 适用 |
|---|---|---|---|
| `send_input` | 主 PTY（共享终端，人工可见可接管） | 字节流 + 屏幕快照，AI 用游标读增量 | 交互态、需要延续上下文（`sudo -i` 后、`mysql` 里、手工多步操作） |
| `exec` | 独立 exec channel | 结构化 stdout/stderr/exitCode | 无状态单条命令（`df -h`、`systemctl status`），结果可信 |

两条路径的输出都汇总到界面的同一个会话视图：终端区显示 PTY 流，下方"命令流水"面板同时记录 `exec` 与 `send_input` 的起止、来源、退出码与完整输出。

> 为什么两者都要：只有 PTY 会拿不到可信退出码、也容易被分页器/交互程序卡住；只有 exec 则无法继承人工已经建立的交互态。两者都不贵，因为同一条 SSH 连接可以并存多个 channel。

### 2.4 共享终端的三个技术难点与对策

**难点一：PTY 没有退出码**

向远端 shell 注入 shell integration（OSC 133）：

```
133;A  提示符开始     133;B  命令输入开始
133;C  输出开始       133;D;<code>  命令结束，带退出码
```

- 注入方式：连接建立后按 `$SHELL` 注入 `PROMPT_COMMAND` / `PS1` 片段（bash）或 `precmd`（zsh / fish），**保留用户原来的提示符视觉**，只叠加不可见控制序列。
- 兜底：不支持时退回标记协议 —— 每条命令后追加
  `printf '\x1e__SSHBOX_<id>__%d\n' "$?"`（`0x1E` 是控制字符，终端不显示）。解析到标记即取退出码并切分输出。
- 该机制同时提供"是否在提示符前"和"命令是否跑完"，是 AI 判断能否输入的唯一可靠依据。
- 每个连接有开关"终端语义集成"，默认开；注入失败则自动降级。

**难点二：输出是混流且无法切片**

终端天然混合 stdout/stderr 与回显。有了 OSC 133 的 `C`/`D` 边界，可按命令切出"这条命令产生的输出"。AI 侧不承诺 stdout/stderr 分离，只承诺：命令文本、输出片段、退出码（或 `unknown`）。

**难点三：交互态对 AI 不可见**

- 内核跑 `vt100` 解析器，对外提供 `read(mode="screen")`：当前可见区域的纯文本 + 光标位置。
- `read(mode="stream")` 提供带游标的增量字节流（可选剥离 ANSI 得到纯文本）。
- 前台程序探测是**尽力而为**：`atPrompt`（来自 OSC 133）最可靠；无语义集成时退回"空闲 N ms + 无前台输出"的启发式，并在结果里标注 `atPrompt: null`（未知），让 AI 知道自己在猜。

### 2.5 输入仲裁（人类优先）

- 每个会话一把输入锁，`owner: none | human | ai`，带 `acquiredAt` 与 `ttl`。
- 人工在界面敲键 → IPC 消息 → **立刻抢锁**；AI 的锁被撤销，正在进行的 AI 输入中止，事件 `lock.revoked{by:"human"}`。
- AI 写入前必须拿锁；拿不到返回明确错误 `HUMAN_TOOK_OVER`（附最后活动时间），而不是把字符混进人工的光标位置。
- AI 写入是**整行原子写**（文本 + `\r` 一次写入），不逐字符；文本中若包含换行按多行处理并逐行进策略检查（二期）。
- 界面在 AI 持有锁时显示横幅"AI 正在操作此终端"，输入框进入"点此接管"状态，避免两个人同时打字把终端打乱。

### 2.6 客户端兼容性：为什么"通用"要打一点折扣

MCP 协议通用，但**传输方式**不是每种客户端都齐全：

| 传输 | 支持情况 | 对策 |
|---|---|---|
| Streamable HTTP (URL) | 较新客户端（Claude Code、Cursor、Codex 等） | **主路径** |
| stdio（客户端拉子进程） | 几乎所有（含 Claude Desktop） | 兜底：`sshbox-bridge.exe`，约 100 行，把 stdio 转发到本地 HTTP 端口 |

bridge 是**纯中继**，不是桌面应用本体，只在"客户端只吃 stdio"时才需要（或直接用社区通用的 `mcp-remote`）。

---

## 3. 数据与凭据

### 3.1 文件布局

```
%APPDATA%\sshbox\
├── config.json              应用配置（端口、自启、AI 访问开关、token）
├── connections.json         连接配置（无任何明文密钥）
├── known_hosts.json         主机指纹（TOFU）
├── runtime.json             运行时发现（port / pid / token / 启动时间）
└── logs\
    ├── audit-20250101.jsonl 审计日志，按天轮转，保留 30 天
    └── app.log
```

文件 ACL 收窄到当前用户。

### 3.2 连接配置

```jsonc
{
  "id": "dev-web-01",
  "name": "测试机 web-01",
  "host": "10.0.0.11", "port": 22, "user": "deploy",
  "auth": { "kind": "password" },          // 或 "privateKey" + keyPath + hasPassphrase
  "credentialRef": "wincred:sshbox/conn/dev-web-01",
  "shellIntegration": true,                 // 终端语义集成（OSC 133）
  "policy": { "default": "auto", "rules": [] },   // 二期
  "secrets": [                              // 可选：登录后可注入的 secret（如 sudo 密码）
    { "id": "sudo", "label": "sudo 密码", "credentialRef": "wincred:sshbox/secret/dev-web-01/sudo" }
  ],
  "aiAccess": true,
  "tags": ["dev"],
  "createdAt": "...", "lastUsedAt": "..."
}
```

**凭据规则（硬性）**

- 密码、私钥口令、secret：一律写入 Windows 凭据管理器（`CredWrite`，`CRED_TYPE_GENERIC`，`CURRENT_USER`），仓库与内存之外无明文落盘。
- 私钥文件不复制、不缓存，只记路径；读取失败提示人工重选。
- 任何 HTTP 接口都无法读取凭据内容；响应里不出现 password/secret 字段。
- 日志写入前脱敏：`password`、`token`、`secret`、`BEGIN * PRIVATE KEY`、`Authorization:`、以及本轮注入的 secret 明文。

### 3.3 主机指纹

- 首次连接：计算 `SHA256:...`，弹窗人工确认后写入 `known_hosts.json`。
- 后续不匹配 → **直接拒绝连接**，红色告警，需人工核对后手动删除记录。不提供"忽略"按钮。

---

## 4. 运行时接口

### 4.1 监听与鉴权

| 项 | 值 |
|---|---|
| 绑定 | `127.0.0.1`（绝不绑 `0.0.0.0`） |
| 端口 | **固定 `17821`**（设置页可改）。被占用时**不静默顺延**，界面明确报错，避免客户端配置悄悄失效 |
| 鉴权 | `Authorization: Bearer <token>`，32 字节随机，首次启动生成并持久化 |
| 防护 | 校验 `Host` 必须是 `127.0.0.1:<port>`；拒绝外部 `Origin`（防 DNS rebinding） |
| 未授权 | `401` + 中文说明，不泄露内部信息 |

界面"连接信息"面板显示 MCP 地址与 token，一键复制，并生成 Claude Code / Cursor / Codex 的现成配置片段。

### 4.2 MCP 工具（一期）

**没有任何工具接受 host/port/user 参数**，只能引用已保存的 `connectionId` 或已存在的 `sessionId`。

| 工具 | 入参 | 返回 | 说明 |
|---|---|---|---|
| `list_sessions` | — | `Session[]` | 活动会话：来源、连接时间、锁持有者、`atPrompt`、最后活动 |
| `list_connections` | — | `Connection[]` | 已保存配置，无凭据，含 `aiAccess` |
| `open_session` | `connectionId`, `reuse=true` | `Session` | 打开共享终端会话 |
| `close_session` | `sessionId` | `{closed:true}` | |
| `send_input` | `sessionId`, `text`, `enter=true`, `waitFor="command_end"\|"prompt"\|"idle"\|"none"`, `timeoutMs=30000` | `{written, cursor, output?, exitCode?, matched}` | 写入共享 PTY（需持锁）。默认顺带等待并返回该命令的输出片段与退出码；交互态逐步操作时用 `waitFor="none"` |
| `read` | `sessionId`, `mode="stream"\|"screen"`, `cursor?`, `maxBytes=65536`, `stripAnsi=false` | `{chunk\|screen, cursor, truncated, atPrompt}` | 增量读流 或 读屏幕快照 |
| `wait_for` | `sessionId`, `kind="prompt"\|"pattern"\|`<br>`"idle"\|"command_end"`, `pattern?`, `timeoutMs=30000` | `{matched, reason, atPrompt}` | 交互态的关键工具：等提示符/等正则/等静默/等命令结束 |
| `interrupt` | `sessionId` | `{sent:true}` | 向 PTY 发 `Ctrl-C` |
| `inject_secret` | `sessionId`, `secretId`, `enter=true` | `{injected:true}` | 从凭据库直接写入 PTY，**AI 看不到明文** |
| `exec` | `sessionId`, `command`, `timeoutMs=60000`, `cwd?` | `ExecResult` | 无状态快路径（独立 channel） |
| `exec_batch` | `sessionId`, `commands[]`, `stopOnError=false` | `ExecResult[]` | 减少往返 |
| `cancel` | `execId` | `{cancelled}` | 掐断 exec |
| `get_transcript` | `sessionId`, `limit=50` | `Record[]` | 之前跑过什么，避免重复 |

`Session`：

```ts
{
  sessionId, connectionId, connectionName, host, user,
  origin: "manual" | "ai",
  connectedAt, lastActivityAt,
  lockOwner: "none" | "human" | "ai",
  atPrompt: boolean | null,
  shellIntegration: true | false,   // false 表示已降级为标记协议
  executions: number
}
```

`read` 的游标语义：每次返回 `cursor`，下次带上它取增量；游标失效（缓冲被裁剪）时返回 `truncated:true` 并从最早可用位置开始。

`inject_secret` 的回显遮蔽：TTY 会把输入回显进流里，所以注入期间开一个遮蔽窗口，把该字节序列在**AI 可见流**和**界面历史**中都替换为 `***`，避免密码经 `read` 泄露。审计里记 `secret:<id>`，不记内容。

### 4.3 REST（同端口，便于 curl 调试与后续脚本）

```
GET    /v1/health                       {version, uptimeSec, sessionCount}
GET    /v1/connections                  Connection[]
POST   /v1/connections                  新建/修改（仅 IPC；HTTP 只读）
GET    /v1/sessions                     Session[]
POST   /v1/sessions                     {connectionId} → Session
DELETE /v1/sessions/{id}
POST   /v1/sessions/{id}/input          {text, enter}
GET    /v1/sessions/{id}/output         ?cursor&mode=stream|screen
POST   /v1/sessions/{id}/interrupt
POST   /v1/sessions/{id}/exec           {command, timeoutMs, cwd}
POST   /v1/executions/{execId}/cancel
GET    /v1/sessions/{id}/transcript     ?limit
GET    /v1/events                       SSE，实时事件
```

配置写接口（增改删连接、读凭据）**只对 IPC 开放**，HTTP 层不暴露，防止 AI 或被窃取的 token 篡改连接库。

### 4.4 事件

```jsonc
{ "type":"session.opened",  "session":{...} }
{ "type":"session.closed",  "sessionId":"...", "reason":"user|remote|error" }
{ "type":"input",           "sessionId":"...", "source":"human|ai", "text":"df -h", "secret":false }
{ "type":"output",          "sessionId":"...", "chunk":"...", "cursor":12345 }
{ "type":"command.start",   "sessionId":"...", "command":"df -h", "source":"ai" }
{ "type":"command.end",     "sessionId":"...", "exitCode":0, "durationMs":182 }
{ "type":"lock.changed",    "sessionId":"...", "owner":"ai", "by":"ai" }
{ "type":"lock.revoked",    "sessionId":"...", "by":"human" }
{ "type":"exec.finished",   "execId":"...", "exitCode":0, "durationMs":182 }
```

### 4.5 验证连通（不依赖 MCP 客户端）

```powershell
$rt = Get-Content "$env:APPDATA\sshbox\runtime.json" | ConvertFrom-Json
$h  = @{ Authorization = "Bearer $($rt.token)" }
Invoke-RestMethod "http://127.0.0.1:$($rt.port)/v1/sessions" -Headers $h | Format-List
```

---

## 5. 界面

### 5.1 布局

```
┌─────────────┬─────────────────────────────────────┬──────────────────┐
│ 连接          │ [web-01 ×][db-02 ×]     ● 已连接     │ 活动流（全局）     │
│ ───────────  │ ⚠ AI 正在操作此终端      [接管]      │ ─────────────    │
│ ● 生产         │ ┌─────────────────────────────────┐ │ 14:02 AI 输入     │
│   web-01  ●   │ │ deploy@web-01:~$ sudo -i        │ │ 14:02 命令完成 0  │
│   db-02       │ │ root@web-01:~# df -h            │ │ 14:03 会话已连接   │
│ ● 测试         │ │ Filesystem  Size  Used ...      │ │                  │
│   web-03      │ │ ⟨AI⟩ root@web-01:~# ▊            │ │                  │
│               │ └─────────────────────────────────┘ │                  │
│ [+ 新建连接]   │ 命令流水： [AI] send_input  df -h  0 182ms              │
└─────────────┴─────────────────────────────────────┴──────────────────┘
```

### 5.2 会话视图

- **上方**：真正的 xterm.js 终端，绑定主 PTY，与 SSH 客户端体验一致（颜色、光标、全屏程序都正常）。人工在此直接打字。
- **AI 输入标注**：AI 写入的字符在终端里以不同颜色的短竖线/标记标注来源（xterm.js decoration），并在命令流水里留一条 `[AI]` 记录；人工输入标 `[手动]`。
- **锁状态横幅**：AI 持锁时显示"AI 正在操作此终端 \[接管]"，点击或直接打字即抢锁。
- **下方命令流水**：每条记录含来源徽章、命令原文、退出码（非 0 标红）、耗时、输出折叠、`复制`、`重跑`、`在终端中执行`。`exec` 与 `send_input` 都在这里。
- 正在执行时有进度指示与 `Ctrl-C` / `掐断` 按钮。

### 5.3 其他界面

- **连接编辑弹窗**：名称、host、port、user、认证方式、私钥与口令、标签、是否允许 AI 访问、终端语义集成开关、secrets 列表；底部"测试连接"。
- **主机指纹确认**：首次连接展示指纹，需点"信任并保存"。
- **设置页**：MCP 端口、token 复制/重置、开机自启、AI 访问总开关、日志保留、审计导出。
- **托盘**：显示窗口 / 会话数 / 全部断开 / 退出。
- **AI 访问总开关**：关闭即停止监听并断开 AI 侧会话。

### 5.4 错误文案

- 无会话时 `exec`：`没有打开的会话。请先在 SSH Box 界面连接目标主机，或调用 open_session。`
- 锁冲突：`人工已接管终端，AI 输入被拒绝。请等待人工停止操作或显式释放。`
- 连接失败分类：DNS、超时、认证失败、指纹不匹配、私钥口令错误 —— 各一句中文说明 + 原始报错折叠。

---

## 6. 安全

### 6.1 主要防线

| 风险 | 对策 |
|---|---|
| 凭据泄露 | 只进 Windows 凭据管理器；HTTP 无读凭据路径；日志脱敏；`inject_secret` 遮蔽回显 |
| token 泄露 | 只在本机 `runtime.json`；仅回环监听；界面可一键重置 |
| AI 连到未授权主机 | 工具不接受 host/port，只能引用用户创建的连接；`aiAccess=false` 直接拒绝 |
| 提示注入（远端返回恶意文本） | 输出标注 `untrusted:true`；**没有任何 MCP 工具能改配置/策略/凭据** |
| 抢终端导致操作错乱 | 输入锁 + 人类优先抢占 + 整行原子写 |
| 人工手敲的密码被 AI 读到 | 已知 secret 走 `inject_secret` 注入并遮蔽回显；`read` 对 `password:` 之后的内容做启发式打码（尽力而为，见 §6.4） |
| DNS rebinding | 校验 Host + 拒绝外部 Origin |
| 审计被篡改 | JSONL 追加写，按天分文件 |

### 6.2 审计记录

```jsonc
{"ts":"2025-01-01T14:02:11.482Z","source":"ai","kind":"send_input",
 "sessionId":"s-3f2a","connectionId":"dev-web-01","tool":"send_input",
 "command":"df -h","matchedRule":"rule-allow-readonly","exitCode":0,
 "durationMs":182,"outputBytes":1043,"redacted":false}

{"ts":"...","source":"ai","kind":"inject_secret","sessionId":"s-3f2a",
 "secretId":"sudo","redacted":true}
```

### 6.3 二期：命令拦截的落点

- 因为 daemon 站在键盘与 SSH 之间，**可以**做行级拦截：缓冲输入 → Enter 时过策略 → 放行 / 拦截 / 请求批准。
- 只允许在**提示符前**做行缓冲（靠 OSC 133 判断），否则原样透传，避免破坏 `vim`/`mysql` 等交互程序。
- `exec` 路径更可靠：命令字符串显式，可解析参数。
- 策略结构：

```yaml
default: ask
rules:
  - { match: "^(ls|cat|tail|df|ps|uptime|git status)\\b", action: allow }
  - { match: "^(sudo|rm -rf|systemctl (restart|stop))", action: ask }
  - { match: "curl[^|]*\\|\\s*(ba)?sh", action: deny }
appliesTo: [ai]        # 或 [ai, human]
```

- 审批：阻塞等待界面决策（上限 120s），超时返回 `PENDING_APPROVAL` + `execId`，另给 `approval_status` 轮询；“仅此一次 / 10 分钟 / 本会话 / 永久”。

### 6.4 拦截能力的诚实边界

必须写清楚，避免误导使用者：

- 行级匹配是**防手滑**，不是安全边界。`bash x.sh`、heredoc、Base64 解码执行、`find -delete`、别名/变量展开、`tmux` 内再执行、嵌套 `ssh`，都能绕过。
- 交互程序内部（`vim` 的 `:!rm`、`mysql` 的 `\!`）默认不拦截。
- 真正的权限控制应该在远端（sudoers、SELinux/AppArmor、容器、最小权限账号）。
- 因此文档与界面都要避免用"拦截/禁止"这类绝对措辞，改用"提示/需确认"。

---

## 7. 代码结构

```
ssh-box/
├── crates/
│   ├── sshcore/            # 内核：配置、凭据、known_hosts、会话池、PTY 状态机、
│   │                       #       输入仲裁、执行器、审计、策略（无 UI 依赖）
│   ├── daemon/             # HTTP adapter：axum + 鉴权 + /mcp + /v1 + SSE
│   ├── mcp/                # MCP 工具定义（rmcp）
│   ├── bridge/             # stdio → 本地 HTTP 中继（给只支持 stdio 的客户端）
│   └── cli/                # sshbox-cli，M0 验证用
├── apps/desktop/
│   ├── src/                # React + TS + Vite + xterm.js
│   └── src-tauri/          # Tauri 入口、IPC 命令、托盘、单实例、自启
├── docs/{design.md,protocol.md}
└── skills/sshbox/SKILL.md
```

依赖要点：`russh`（SSH）、`vt100`（终端状态机）、`tokio`、`axum`、`rmcp`、`keyring` 或 `windows` crate、`serde`、`thiserror`、`tauri` v2 + `tauri-plugin-autostart`；前端 `@xterm/xterm` + `@xterm/addon-fit`。

---

## 8. 里程碑

| 阶段 | 交付 | 验收标准 |
|---|---|---|
| **M0 环境** | Rust(MSVC) + VS Build Tools + Node LTS + Tauri CLI | `cargo`、`pnpm`、`cargo tauri` 均可用 |
| **M1 内核连通** | `sshcore` + `sshbox-cli` | CLI 能连上、拿到 PTY、双向收发；密码与私钥两种认证；凭据在凭据管理器；指纹 TOFU 生效 |
| **M2 终端状态** | vt100 状态机 + OSC 133 注入/降级 + 输入锁 | CLI 下能判定 `atPrompt`、取到退出码、取到屏幕快照；两个来源写入不交错 |
| **M3 服务** | daemon + REST + SSE + 单实例 + 托盘 | PowerShell 能开会话、写输入、读增量、发 Ctrl-C；关窗后服务仍在 |
| **M4 MCP** | `/mcp` + token + bridge | Claude Code 里 `list_sessions` / `send_input` / `read` / `wait_for` 可用 |
| **M5 界面** | 连接管理、xterm 终端、命令流水、锁横幅、活动流、设置 | AI 输入实时可见可接管；连接信息一键复制配置片段 |
| **M6 发布** | 打包（含 WebView2 bootstrapper）、自启、签名、SKILL.md | 干净 Win10 可装可用 |
| M7+ 二期 | 策略引擎、审批弹窗、SFTP | 见 §1.2 / §6.3 |

> **实施顺序调整（v0.3）**：先做界面，后做 daemon/MCP。
> 理由：界面里的「共享终端 + 会话池」是最容易返工的部分，先用它把 `SessionManager` 的接口压出来；
> daemon 只是同一套事件的另一个订阅者，MCP 只是同一套命令的另一个 adapter。
> 实际执行顺序：**M0 环境 → M1 内核+CLI 验证 → M2 界面（连接管理 / 标签页 / xterm 终端）→ M3 终端状态（vt100 + OSC 133 + 输入锁）→ M4 daemon → M5 MCP → M6 发布**。
> M2 验收：左侧连接列表、顶部标签页、右侧 xterm 终端可双向收发、指纹确认弹窗、凭据写入/读取正常。

---

## 9. 决策记录

| # | 决策 | 结论 | 影响 |
|---|---|---|---|
| 1 | AI 能否自己 `open_session` | **可以**，限 `aiAccess=true` 的已保存连接；AI 打开的会话人工同样可输入、可接管 | 会话 `origin` 标记 `ai`；事件流广播给界面 |
| 2 | AI 默认输入路径 | **`send_input`**（写共享 PTY） | `exec` 降级为显式可选工具；`send_input` 增加 `waitFor`，默认等待命令结束并返回输出与退出码，避免三次往返 |
| 3 | 端口 | **固定 `17821`**，可配置；占用时报错不顺延 | 客户端 URL 长期有效；端口变更需同步改客户端配置（界面提示） |
| 4 | `/v1` REST | **保留** | 便于 curl 验证、脚本直连、未来 skill 使用 |
| 5 | 一期人工审批 | **不做** | 靠 `aiAccess` + 实时可见 + 接管/`Ctrl-C`；审批留给二期策略引擎 |
| 6 | 策略作用范围 | **AI 与人工一视同仁**（`appliesTo: [ai, human]`） | 实现简单一致；默认动作设为 `allow`，避免人工自己每条命令都要确认 |
| 7 | 人工手敲密码的可见性 | 采用 **手段 1（`inject_secret` 遮蔽回显）+ 手段 3（`read` 启发式打码）** | 不做"暂停 AI 读取"按钮；文档需说明启发式打码不保证覆盖所有情况 |

### 9.1 `send_input` 的等待语义

单条命令的最简用法（默认，一次调用拿到结果）：

```jsonc
send_input({ sessionId, text: "df -h" })
// → { written: true, match: "command_end", exitCode: 0,
//     output: "Filesystem Size Used ...", cursor: 12345 }
```

交互态分步（如 `sudo -i` 要等口令提示）：

```jsonc
send_input({ sessionId, text: "sudo -i", waitFor: "none" })
wait_for  ({ sessionId, kind: "pattern", pattern: "[Pp]assword", timeoutMs: 5000 })
inject_secret({ sessionId, secretId: "sudo" })
wait_for  ({ sessionId, kind: "prompt" })
send_input({ sessionId, text: "df -h" })
```

`command_end` 依赖 OSC 133；降级为标记协议时同样返回退出码；两者都不可用时退化为 `idle` 并置 `exitCode: null`、`match: "idle"`。
