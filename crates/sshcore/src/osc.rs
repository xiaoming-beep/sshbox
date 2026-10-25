//! OSC 133 语义标记解析。
//!
//! 这是 iTerm2 / WezTerm / VS Code 共用的 shell integration 协议，序列形如：
//!
//! ```text
//! ESC ] 133 ; <类型> [; <参数>] BEL   （或 ST = ESC \）
//!   A        提示符开始（可以输入了）
//!   B        用户开始输入命令
//!   C        命令开始输出（已在执行）
//!   D;<code> 命令结束，code 为退出码
//! ```
//!
//! 扫描器按字节增量喂入，序列被 chunk 边界切开也能正确识别；
//! 非 133 的 OSC（窗口标题等）会被忽略。

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Osc133 {
    /// 提示符出现，可以输入
    PromptStart,
    /// 用户开始输入命令
    CommandStart,
    /// 命令开始输出
    OutputStart,
    /// 命令结束，携带退出码
    CommandEnd(i32),
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Esc,
    Osc,
    OscEsc,
}

#[derive(Debug, Default)]
pub struct OscScanner {
    state: State,
    buf: Vec<u8>,
}

impl OscScanner {
    /// 喂入一段输出，把解析到的标记追加到 `out`
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<Osc133>) {
        for &byte in bytes {
            match self.state {
                State::Ground => {
                    if byte == 0x1b {
                        self.state = State::Esc;
                    }
                }
                State::Esc => match byte {
                    b']' => {
                        self.state = State::Osc;
                        self.buf.clear();
                    }
                    0x1b => {}
                    _ => self.state = State::Ground,
                },
                State::Osc => match byte {
                    0x07 => self.finish(out),
                    0x1b => self.state = State::OscEsc,
                    _ => {
                        // 防御异常长的序列
                        if self.buf.len() < 256 {
                            self.buf.push(byte);
                        }
                    }
                },
                State::OscEsc => match byte {
                    b'\\' => self.finish(out),
                    0x1b => {}
                    _ => {
                        if self.buf.len() < 256 {
                            self.buf.push(byte);
                        }
                        self.state = State::Osc;
                    }
                },
            }
        }
    }

    fn finish(&mut self, out: &mut Vec<Osc133>) {
        let payload = std::mem::take(&mut self.buf);
        self.state = State::Ground;
        parse_payload(&payload, out);
    }
}

fn parse_payload(payload: &[u8], out: &mut Vec<Osc133>) {
    let text = String::from_utf8_lossy(payload);
    let Some(rest) = text.strip_prefix("133;") else {
        return;
    };
    let mut parts = rest.split(';');
    match parts.next().unwrap_or("") {
        "A" => out.push(Osc133::PromptStart),
        "B" => out.push(Osc133::CommandStart),
        "C" => out.push(Osc133::OutputStart),
        "D" => {
            let code = parts
                .next()
                .and_then(|s| s.trim().parse::<i32>().ok())
                .unwrap_or(0);
            out.push(Osc133::CommandEnd(code));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(chunks: &[&[u8]]) -> Vec<Osc133> {
        let mut scanner = OscScanner::default();
        let mut out = Vec::new();
        for chunk in chunks {
            scanner.feed(chunk, &mut out);
        }
        out
    }

    #[test]
    fn parses_bel_terminated_sequence() {
        let events = scan(&[b"\x1b]133;D;0\x07"]);
        assert_eq!(events, vec![Osc133::CommandEnd(0)]);
    }

    #[test]
    fn parses_st_terminated_sequence() {
        let events = scan(&[b"\x1b]133;D;127\x1b\\"]);
        assert_eq!(events, vec![Osc133::CommandEnd(127)]);
    }

    #[test]
    fn parses_all_markers_in_order() {
        let events = scan(&[b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07\x1b]133;D;2\x07"]);
        assert_eq!(
            events,
            vec![
                Osc133::PromptStart,
                Osc133::CommandStart,
                Osc133::OutputStart,
                Osc133::CommandEnd(2),
            ]
        );
    }

    #[test]
    fn handles_sequence_split_across_chunks() {
        let events = scan(&[b"\x1b]133", b";D;", b"42\x07"]);
        assert_eq!(events, vec![Osc133::CommandEnd(42)]);
    }

    #[test]
    fn ignores_other_osc_and_plain_text() {
        let events = scan(&[b"hello\x1b]0;title\x07world\x1b]133;D;1\x07"]);
        assert_eq!(events, vec![Osc133::CommandEnd(1)]);
    }

    #[test]
    fn missing_exit_code_defaults_to_zero() {
        let events = scan(&[b"\x1b]133;D\x07"]);
        assert_eq!(events, vec![Osc133::CommandEnd(0)]);
    }
}
