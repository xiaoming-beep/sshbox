# SSH Box · MCP 接入说明

本机已安装 **SSH Box**（桌面程序）。它在本机回环地址上提供 MCP 服务，
AI 工具通过下面这组「地址 + 令牌」接入，**不需要**把 SSH Box 当子进程拉起。

| 项 | 值 |
|---|---|
| MCP 端点（Streamable HTTP） | `{{URL}}` |
| REST 端点（调试用） | `http://127.0.0.1:{{PORT}}/v1` |
| 访问令牌 | `{{TOKEN}}` |

> **前提**：SSH Box 必须在运行。关闭窗口只是缩到托盘，服务照常在线；
> 如果托盘里没有它的图标，双击桌面/开始菜单里的 **SSH Box** 启动即可。

---

## 1. Claude Code（推荐）

```bash
claude mcp add --transport http sshbox {{URL}} \
  --header "Authorization: Bearer {{TOKEN}}"

claude mcp list
```

## 2. Cursor

编辑 `%USERPROFILE%\.cursor\mcp.json`（没有就新建），然后重启 Cursor：

```json
{
  "mcpServers": {
    "sshbox": {
      "url": "{{URL}}",
      "headers": { "Authorization": "Bearer {{TOKEN}}" }
    }
  }
}
```

## 3. Claude Desktop / 只支持 stdio 的客户端

这类客户端不能直接填 URL，用 `mcp-remote` 做一次转换（需要 Node）：

配置路径：`%APPDATA%\Claude\claude_desktop_config.json`

```json
{
  "mcpServers": {
    "sshbox": {
      "command": "npx",
      "args": [
        "-y",
        "mcp-remote",
        "{{URL}}",
        "--header",
        "Authorization: Bearer {{TOKEN}}"
      ]
    }
  }
}
```

## 4. 其它支持「远程 MCP / HTTP MCP」的工具

- URL：`{{URL}}`
- 自定义请求头：`Authorization: Bearer {{TOKEN}}`

---

## 5. 不依赖 AI 工具的自测（PowerShell）

```powershell
$h  = @{ Authorization = "Bearer {{TOKEN}}"; Accept = "application/json, text/event-stream" }
$ep = "{{URL}}"

# 1) 初始化
Invoke-RestMethod $ep -Method Post -Headers $h -ContentType 'application/json' `
  -Body '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"ps","version":"0"}}}' |
  ConvertTo-Json -Depth 6

# 2) 列出工具（应看到 9 个）
Invoke-RestMethod $ep -Method Post -Headers $h -ContentType 'application/json' `
  -Body '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' |
  Select-Object -ExpandProperty result |
  Select-Object -ExpandProperty tools |
  Select-Object -ExpandProperty name
```

---

## 能做什么

| 工具 | 作用 |
|---|---|
| `list_connections` | 已保存的连接（不含凭据） |
| `list_sessions` | 当前打开的会话（带 atPrompt / altScreen / integration） |
| `session_state` | 查会话状态与当前 cursor |
| `open_session` | 打开或复用一个会话 |
| `send_input` | 往共享终端写文本，可附带结构化按键 |
| `send_key` | 只发按键（`C-c`、`Escape`、方向键等） |
| `read` | 按 cursor 增量读输出（默认已去 ANSI） |
| `screen` | 读渲染后的屏幕（vim / tmux / top 用这个） |
| `wait_for` | 等提示符 / 等静默 / 等文本 |
| `interrupt` | 发送 Ctrl-C |
| `get_history` | 命令流水（来源 / 退出码 / 耗时） |
| `get_file_history` | 文件操作审计（来源 / 路径 / 字节数 / 成败） |
| `sftp_list` | 列远端目录（限授权范围内） |
| `sftp_stat` | 查看远端文件/目录属性（限授权范围内） |
| `sftp_read` | 读远端小文件（限授权范围内，默认 64KB 上限） |
| `sftp_write` | 写远端文件（限授权范围内） |
| `sftp_mkdir` | 建远端目录（限授权范围内） |
| `sftp_remove` | 删除远端文件或空目录（限授权范围内） |
| `sftp_rename` | 重命名/移动（源与目标都需在授权范围内） |
| `sftp_download` | 下载到本机暂存区 `Downloads/SSHBox`（限授权范围内） |
| `sftp_upload` | 暂存区文件推送到远端 |
| `close_session` | 关闭会话 |

所有工具都接受 `sessionId`，但只有一个会话时可以省略（自动选中）；多会话时必须显式指定。

### 等待命令结束的正确姿势

- 首选 `wait_for { kind: "prompt" }`：等 OSC 133 的提示符标记，**命令真正结束**才返回。
- `kind: "idle"` 只是“输出静默”，`sleep 20` 这种会被误判；只有 `integration: true` 时 prompt 才可靠。
- 先 `session_state` 看 `integration`：`false` 说明远端 shell 不是 bash/zsh，
  去连接配置里把「启动命令」设成 `bash` 即可。

### 控制键

```text
send_key { sessionId, keys: ["C-c"] }              # 中断
send_key { sessionId, keys: ["Escape", ":q!", "Enter"] }  # 退出 vim
```

可以这样问它：

- 「列出 sshbox 里的连接」
- 「连上 vm，看看磁盘占用」（它会 open_session → send_input → wait_for → read）
- 「刚才 AI 执行过哪些命令，有没有失败的」

## 边界与安全

- 令牌**等同密码**：拿到它就能在你已打开的会话里执行命令。试完建议删掉这个文件。
- AI 只能操作**已保存的连接**和**已打开的会话**，不能连任意主机，也不能改配置、凭据或策略。
- AI 的每次输入都会实时出现在 SSH Box 界面的「命令流水」里并标记为 `AI`；
  你随时可以接管——**人类输入优先**，AI 会被拒绝并等待。
- 想让它更守规矩，可以把 `skills/sshbox/SKILL.md` 放进客户端的 skills / rules 目录
  （内容是：先看会话再动手、破坏性命令先说明、不要试图输入密码等）。
