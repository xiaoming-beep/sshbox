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
