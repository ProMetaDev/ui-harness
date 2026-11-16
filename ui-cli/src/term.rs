//! 终端驱动：备用屏、raw mode、按键解析。
//!
//! 逻辑全在 `frame.rs`（纯函数），这里只负责搬字节：进入备用屏、把帧写出去、
//! 把按键读回来。退出时无论正常与否都必须还原终端——否则用户的 shell 会坏掉。

use crate::frame::{frame, ScreenState, Style};
use std::collections::VecDeque;
use std::io::{self, IsTerminal, Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use ui_core::cancel::Cancel;

/// 语义样式 → 256 色 SGR。色板见 docs/03-frontend.md。
pub fn sgr(style: Style) -> &'static str {
    match style {
        Style::Normal => "\x1b[38;5;253m",
        Style::Dim => "\x1b[38;5;245m",
        Style::Accent => "\x1b[38;5;179m",
        Style::Warn => "\x1b[38;5;214m",
        Style::Err => "\x1b[38;5;167m",
        Style::AccentStrong => "\x1b[1;38;5;179m",
    }
}

const BG: &str = "\x1b[48;5;234m";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Backspace,
    Esc,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Tab,
    CtrlC,
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

/// 纯函数：字节 → 按键。单独测，因为终端里没法断言。
pub fn parse_keys(buf: &[u8]) -> Vec<Key> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        let b = buf[i];
        if b == 0x1b {
            if i + 2 < buf.len() && buf[i + 1] == b'[' {
                let (key, len) = match buf[i + 2] {
                    b'A' => (Key::Up, 3),
                    b'B' => (Key::Down, 3),
                    b'C' => (Key::Right, 3),
                    b'D' => (Key::Left, 3),
                    b'H' => (Key::Home, 3),
                    b'F' => (Key::End, 3),
                    b'5' if i + 3 < buf.len() && buf[i + 3] == b'~' => (Key::PageUp, 4),
                    b'6' if i + 3 < buf.len() && buf[i + 3] == b'~' => (Key::PageDown, 4),
                    _ => (Key::Esc, 1),
                };
                out.push(key);
                i += len;
                continue;
            }
            out.push(Key::Esc);
            i += 1;
            continue;
        }
        match b {
            b'\r' | b'\n' => {
                out.push(Key::Enter);
                i += 1;
            }
            0x7f | 0x08 => {
                out.push(Key::Backspace);
                i += 1;
            }
            0x03 => {
                out.push(Key::CtrlC);
                i += 1;
            }
            b'\t' => {
                out.push(Key::Tab);
                i += 1;
            }
            _ => {
                let end = (i + utf8_len(b)).min(buf.len());
                if let Ok(s) = std::str::from_utf8(&buf[i..end]) {
                    if let Some(c) = s.chars().next() {
                        out.push(Key::Char(c));
                    }
                }
                i = end;
            }
        }
    }
    out
}

fn stty(args: &[&str]) -> io::Result<()> {
    let status = Command::new("stty")
        .args(args)
        .stderr(std::process::Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::Other, format!("stty {:?} 失败", args)))
    }
}

fn terminal_size() -> Option<(usize, usize)> {
    // `Command::output()` 默认把 stdin 接到 /dev/null；而 `stty` 必须从
    // **终端的 stdin** 取 ioctl。之前没继承 stdin，于是永远读不到尺寸，
    // TUI 一直退化成 80×24（宽屏右栏因此永远不出现）。
    let out = Command::new("stty")
        .arg("size")
        .stdin(Stdio::inherit())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut it = text.split_whitespace();
    let rows: usize = it.next()?.parse().ok()?;
    let cols: usize = it.next()?.parse().ok()?;
    if cols < 20 || rows < 6 {
        return None;
    }
    Some((cols, rows))
}

pub struct Tui {
    entered: bool,
    width: usize,
    height: usize,
    /// 后台读键线程写入，主线程取走
    keys: Arc<Mutex<VecDeque<Key>>>,
    /// 一轮任务运行中：此时 Esc / Ctrl-C 表示取消，而不是退出
    busy: Arc<AtomicBool>,
    cancel: Cancel,
}

impl Tui {
    pub fn enter() -> io::Result<Tui> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "需要交互式终端：stdin/stdout 不是 tty（管道里请用 ui chat）",
            ));
        }
        stty(&["raw", "-echo"])?;
        let mut out = io::stdout();
        write!(out, "\x1b[?1049h\x1b[?25l{}{}\x1b[2J", BG, sgr(Style::Normal))?;
        out.flush()?;
        let (width, height) = terminal_size().unwrap_or((80, 24));

        let keys: Arc<Mutex<VecDeque<Key>>> = Arc::new(Mutex::new(VecDeque::new()));
        let busy = Arc::new(AtomicBool::new(false));
        let cancel = Cancel::new();

        // 后台读键：模型调用期间主线程是阻塞的，只有它能把「取消」送进去。
        {
            let keys = Arc::clone(&keys);
            let busy = Arc::clone(&busy);
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                let mut stdin = io::stdin();
                let mut buf = [0u8; 64];
                loop {
                    match stdin.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            for k in parse_keys(&buf[..n]) {
                                if busy.load(Ordering::SeqCst)
                                    && matches!(k, Key::CtrlC | Key::Esc)
                                {
                                    cancel.cancel();
                                    continue;
                                }
                                keys.lock().unwrap().push_back(k);
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        Ok(Tui {
            entered: true,
            width,
            height,
            keys,
            busy,
            cancel,
        })
    }

    /// 交给 Agent：模型适配器轮询它来决定是否杀子进程
    pub fn cancel_token(&self) -> Cancel {
        self.cancel.clone()
    }

    /// 标记「一轮是否在跑」：决定 Esc/Ctrl-C 是取消还是退出
    pub fn set_busy(&self, value: bool) {
        self.busy.store(value, Ordering::SeqCst);
    }

    pub fn size(&mut self) -> (usize, usize) {
        if let Some((w, h)) = terminal_size() {
            self.width = w;
            self.height = h;
        }
        (self.width, self.height)
    }

    pub fn draw(&mut self, state: &ScreenState) -> io::Result<()> {
        let (w, h) = self.size();
        let rows = frame(state, w, h);
        let mut buf = String::new();
        // 逐行绝对定位，不用 \r\n：最后一行换行会让整个终端滚动一行。
        for (i, row) in rows.iter().enumerate() {
            buf.push_str(&format!("\x1b[{};1H", i + 1));
            for span in &row.spans {
                buf.push_str(sgr(span.style));
                buf.push_str(&span.text);
            }
            // 先复位再补背景，最后擦到行尾——否则底色会被擦成终端默认色
            buf.push_str("\x1b[0m");
            buf.push_str(BG);
            buf.push_str("\x1b[K");
        }
        buf.push_str(&format!("{}", BG));
        // 光标停在输入行末尾（倒数第二行：最后一行是提示）
        let cx = 3 + crate::frame::disp_width(&state.input);
        buf.push_str(&format!(
            "\x1b[{};{}H\x1b[?25h",
            h.saturating_sub(1),
            cx.min(w)
        ));
        let mut out = io::stdout();
        out.write_all(buf.as_bytes())?;
        out.flush()
    }

    pub fn wait_key(&mut self) -> io::Result<Key> {
        loop {
            if let Some(key) = self.keys.lock().unwrap().pop_front() {
                return Ok(key);
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        if !self.entered {
            return;
        }
        let mut out = io::stdout();
        let _ = write!(out, "\x1b[?25h\x1b[0m\x1b[?1049l");
        let _ = out.flush();
        let _ = stty(&["sane"]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_arrows_and_paging() {
        assert_eq!(parse_keys(b"\x1b[A"), vec![Key::Up]);
        assert_eq!(parse_keys(b"\x1b[B"), vec![Key::Down]);
        assert_eq!(parse_keys(b"\x1b[5~"), vec![Key::PageUp]);
        assert_eq!(parse_keys(b"\x1b[6~"), vec![Key::PageDown]);
        assert_eq!(parse_keys(b"\x1b[5~\x1b[H"), vec![Key::PageUp, Key::Home]);
    }

    #[test]
    fn parses_control_keys() {
        assert_eq!(parse_keys(b"\r"), vec![Key::Enter]);
        assert_eq!(parse_keys(b"\x7f"), vec![Key::Backspace]);
        assert_eq!(parse_keys(b"\x03"), vec![Key::CtrlC]);
    }

    #[test]
    fn parses_multibyte_input() {
        // 「好」是 3 字节；不能被拆成三个垃圾字符
        assert_eq!(parse_keys("好".as_bytes()), vec![Key::Char('好')]);
        assert_eq!(
            parse_keys("ya好".as_bytes()),
            vec![Key::Char('y'), Key::Char('a'), Key::Char('好')]
        );
    }
}
