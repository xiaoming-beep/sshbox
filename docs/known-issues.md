# 已知问题（v0.1.1）

按严重度排列，修好后从这里删掉。

## 1. tmux 内的 OSC 133 失效

在会话里跑 tmux 后，外层 shell 被 tmux 接管，不再产生 `133;A/D` 标记，
因此 `atPrompt` 会一直为 `false`，`wait_for(kind="prompt")` 只能超时。

**当前兜底**：`altScreen: true` + 超时；AI 应改用 `send_key` 交互。
**彻底解决**：给 tmux 配 shell integration（`tmux set -g ...`），或用 tmux 的
`pane_current_command` 之类信号。

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

## 6. 没有开机自启，也没打包

当前只能 `cargo build -p sshbox-desktop` + 手动运行。
托盘常驻已实现（关窗口不退出），但重启系统后需要手动启动。

## 7. 会话还原不完整

应用重启后不会恢复上次打开的会话（凭据在，连接配置在，但会话是空的）。
