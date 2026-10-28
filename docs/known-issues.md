# 已知问题（v0.1.1）

按严重度排列，修好后从这里删掉。

## 1. tmux 内的 OSC 133 失效

在会话里跑 tmux 后，外层 shell 被 tmux 接管，不再产生 `133;A/D` 标记，
因此 `atPrompt` 会一直为 `false`，`wait_for(kind="prompt")` 只能超时。

**现在有绕法**：用 `screen` 工具读**渲染后的屏幕**（v0.1.2 起）。
tmux 里拿到的是带状态栏的那一屏，AI 能直接看清当前状态，不必依赖 prompt 判定。

**彻底解决**（未做）：给 tmux 配 shell integration，或引入 `pane_current_command` 类信号。
考虑到需要在远端 shell rc 里持久化注入，侵入性较强，暫不做。

## 2. 交互式程序的退出码可能为 null

`vim`、`less` 这类全屏程序，退出码依赖外层 shell 的 PROMPT_COMMAND。
如果命令是在 shell 的“非提示符态”下发出的，`pending` 可能已被前一条命令消费掉，
该条记录的 `exitCode` 就是 `null`。

**建议**：把 `exitCode: null` 当成“未知”，用 `wait_for(prompt)` + 输出内容判断。

## 3. 密码脱敏是启发式的

只对“输出最后一行包含 password / passphrase / 密码 / 口令 且以冒号结尾”的场景生效。
自定义提示（例如 `Enter your key:`）、或有程序自己绘制的输入框，不会被识别。

**影响**：这类输入仍会进入命令流水。
**缓解**：敏感操作让用户在界面里手动输入（人类输入同样会被记录，但至少不在 AI 上下文里）。

## 4. send_key 的按键面有限

支持：`C-a`..`C-z`、`Escape`、`Enter`、`Tab`、`Backspace`、`Up/Down/Left/Right`、
`Home/End`、`PageUp/PageDown`、`Delete`、`Insert`、`Space`，以及任意单字符字面量。

不支持：`F1`..`F12`、`C-S-x` 这类多修饰组合、鼠标事件。

## 5. 首连主机指纹只能在界面信任

MCP/REST 侧没有“信任该主机”的工具（刻意不做，避免 AI 自行 TOFU）。
`open_session` 遇到未知主机时返回 `hostKeyUnknown`，需要人点一次界面。

## 6. 没有打包

当前只能 `cargo build -p sshbox-desktop` + 手动运行。
托盘常驻已实现（关窗口不退出）。

开机自启 **明确不做**（用户确认不需要）。

## 8. tmux 场景未在真机验证

`screen` 工具已在 USB 测试机（Debian 11 / aarch64）上用 **vim** 验证通过：
进入 alt-screen 后 `altScreen=true`、返回的是渲染后的画面，
`Escape` + `:q!` 退出后 `atPrompt` 恢复为 `true`。

**tmux 仍未验证**：测试棒里没有 tmux，也没有外网装不了包。
有一台带 tmux 的机器时补一次：`tmux new -s demo` 后调 `screen`，
确认能拿到带状态栏的那一屏。

## 7. 会话还原不完整

应用重启后不会恢复上次打开的会话（凭据在，连接配置在，但会话是空的）。

## 9. SFTP 权限模型：读写同范围，靠目录授权兜底

写入能力（v0.3.0/v0.3.1）：`sftp_write` / `sftp_mkdir` / `sftp_remove` /
`sftp_rename`，以及 `sftp_download` / `sftp_upload`（本机暂存区 `Downloads/SSHBox`）。

**v0.4.0 起读与写同范围**（之前读是全开的，属于权限不对称）：

- AI（MCP）只能访问授权目录：界面授权的 `write_root` + 配置 `sftp.writePaths`。
  两边都空 = 什么都看不到（不是“能读不能写”）。
- 人（界面文件面板）不受限，相当于根是 `/`。

授权来源，AI 满足其一即可：
- 界面：文件面板里对某目录点「允许写入此目录」（会话级，随时可撤）
- 配置：`sftp.writePaths` 绝对路径 + `sftp.maxWriteBytes`（默认 4MB）

**路径安全（v0.4.0）**：
- 所有操作先 `canonicalize` 拿真实路径再校验，不再只比字符串前缀。
- 写入前解析父目录真实路径，目标本身是符号链接则直接拒。
- 前缀比较按路径段，`/tmp/sbtestfoo` 不算在 `/tmp/sbtest` 内。
- 实测：`/tmp/sbtest/link -> /root` 后写 `/tmp/sbtest/link/escape.txt` 被拒。

**审计（v0.4.0）**：每个 SFTP 操作（含被拒的）都记一条，来源 `ai`/`human`，
带路径、字节数、成败，用 `get_file_history` 读（每会话最多 500 条）。

路径带 `..` 直接拒；上传前父目录自动创建（mkdir -p）。

**逐次人工审批：明确不做**（用户判断不现实：AI 连续干活时每条写入都弹窗，体验不可接受）。
当前靠“只在授权目录内可读写”兜底。

其它限制：单文件 32MB 上限（未做分块续传）；
`Downloads/SSHBox` 是 AI 唯一的本地读写区，不会碰你其它文件。
