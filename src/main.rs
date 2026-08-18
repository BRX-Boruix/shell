//! BORUIX 用户态 shell（命令行解释器，独立可执行程序）。
//!
//! 由 init（PID 1）经 `exec` 系统调用加载运行为 PID 2。libsys 提供 `_start`
//! 入口，本文件导出 `user_main`（进程入口）。
//!
//! 命令集：
//! - `echo <文本>`：输出一行文本（支持双引号字符串与 `\n` 等转义）。
//! - `print("字符串")` / `print(<算术表达式>)`：输出内容（不换行）。
//!   字符串字面量支持 `\n`/`\t`/`\\`/`\"` 转义；表达式求值 `+ - * /` 四则运算
//!   （含括号、空格、一元负号），输出十进制结果。
//! - `println(...)`：同 `print(...)`，但在末尾追加换行。
//! - 任意不可识别的命令名报 `boruix: unknown command: <名字>`；
//!   表达式错误（空/非法数字/除零/括号不匹配/尾随字符）给出对应友好消息。

#![no_std]
#![no_main]

use libsys::{write, STDOUT};

// ---------- 输出辅助 ----------

/// 把字节切片输出到标准输出（丢弃错误，静默失败）。
fn out(s: &[u8]) {
    let _ = write(STDOUT, s);
}

/// 输出一行。
fn outln(s: &[u8]) {
    out(s);
    out(b"\n");
}

/// 打印 shell 提示符。
fn prompt() {
    out(b"boruix$ ");
}

/// `&[u8]` 的 `trim` 等价物：去掉首尾 ASCII 空白。
fn trim_bytes(s: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = s.len();
    while start < end && s[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && s[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &s[start..end]
}

// ---------- 字符串转义 ----------

/// 把带 `\n`/`\t`/`\\`/`\"` 转义的字面量内容解码为原始字节序列。
///
/// `src` 应已去掉外层双引号；返回解码后写入 `buf` 的字节数。
fn unescape(src: &[u8], buf: &mut [u8]) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < src.len() && n < buf.len() {
        let c = src[i];
        if c == b'\\' && i + 1 < src.len() {
            i += 1;
            let e = match src[i] {
                b'n' => b'\n',
                b't' => b'\t',
                b'r' => b'\r',
                b'0' => b'\0',
                b'\\' => b'\\',
                b'"' => b'"',
                other => other,
            };
            buf[n] = e;
        } else {
            buf[n] = c;
        }
        n += 1;
        i += 1;
    }
    n
}

/// 提取双引号字符串的内容（去掉首尾引号）。找不到闭合引号返回 `None`。
fn string_content(s: &[u8]) -> Option<&[u8]> {
    let s = trim_bytes(s);
    if s.first() == Some(&b'"') {
        // 找闭合引号
        for i in 1..s.len() {
            if s[i] == b'"' && s[i - 1] != b'\\' {
                return Some(&s[1..i]);
            }
        }
    }
    None
}

// ---------- 算术表达式求值 ----------

/// 简单的递归下降解析器：支持整数 + `+ - * /`（左结合）+ 括号 + 一元负号 + 空格。
struct Expr<'a> {
    s: &'a [u8],
    pos: usize,
}

/// 表达式求值错误类型（用于向用户输出友好错误，而非笼统的 "bad expression"）。
#[derive(Clone, Copy)]
enum EvalErr {
    Empty,     // 表达式为空（如 `print()`）
    BadNumber, // 操作数不是合法整数
    DivZero,   // 除以零
    BadParen,  // 括号不匹配 / 缺少右括号
    Trailing,  // 表达式后有多余字符
}

impl EvalErr {
    /// 友好错误消息（统一 `boruix:` 前缀，与未知命令报错风格一致）。
    fn message(self) -> &'static [u8] {
        match self {
            EvalErr::Empty => b"boruix: empty expression",
            EvalErr::BadNumber => b"boruix: invalid number in expression",
            EvalErr::DivZero => b"boruix: division by zero",
            EvalErr::BadParen => b"boruix: mismatched parentheses",
            EvalErr::Trailing => b"boruix: unexpected trailing characters",
        }
    }
}

impl<'a> Expr<'a> {
    fn new(s: &'a [u8]) -> Self {
        Self { s, pos: 0 }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.s.len() && self.s[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.skip_ws();
        if self.s.get(self.pos) == Some(&c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// `expr := term (('+'|'-') term)*`
    fn parse_expr(&mut self) -> Result<i64, EvalErr> {
        let mut v = self.parse_term()?;
        loop {
            if self.eat(b'+') {
                v = v
                    .checked_add(self.parse_term()?)
                    .ok_or(EvalErr::BadNumber)?;
            } else if self.eat(b'-') {
                v = v
                    .checked_sub(self.parse_term()?)
                    .ok_or(EvalErr::BadNumber)?;
            } else {
                break;
            }
        }
        Ok(v)
    }

    /// `term := factor (('*'|'/') factor)*`
    fn parse_term(&mut self) -> Result<i64, EvalErr> {
        let mut v = self.parse_factor()?;
        loop {
            if self.eat(b'*') {
                v = v
                    .checked_mul(self.parse_factor()?)
                    .ok_or(EvalErr::BadNumber)?;
            } else if self.eat(b'/') {
                let d = self.parse_factor()?;
                if d == 0 {
                    return Err(EvalErr::DivZero);
                }
                v = v / d;
            } else {
                break;
            }
        }
        Ok(v)
    }

    /// `factor := NUMBER | '(' expr ')' | '-' factor | '+' factor`
    fn parse_factor(&mut self) -> Result<i64, EvalErr> {
        self.skip_ws();
        if self.eat(b'-') {
            return Ok(-self.parse_factor()?);
        }
        if self.eat(b'+') {
            return self.parse_factor();
        }
        if self.eat(b'(') {
            let v = self.parse_expr()?;
            if !self.eat(b')') {
                return Err(EvalErr::BadParen);
            }
            return Ok(v);
        }
        // 数字
        self.skip_ws();
        let start = self.pos;
        let mut is_digit = false;
        while self.pos < self.s.len() && self.s[self.pos].is_ascii_digit() {
            is_digit = true;
            self.pos += 1;
        }
        if !is_digit {
            return Err(EvalErr::BadNumber);
        }
        let txt = core::str::from_utf8(&self.s[start..self.pos]).map_err(|_| EvalErr::BadNumber)?;
        txt.parse::<i64>().map_err(|_| EvalErr::BadNumber)
    }

    /// 解析整个表达式，要求全部消费（无尾随垃圾）。
    fn evaluate(src: &[u8]) -> Result<i64, EvalErr> {
        if trim_bytes(src).is_empty() {
            return Err(EvalErr::Empty);
        }
        let mut p = Expr::new(src);
        let v = p.parse_expr()?;
        p.skip_ws();
        if p.pos == p.s.len() {
            Ok(v)
        } else {
            Err(EvalErr::Trailing)
        }
    }
}

/// 把 `i64` 格式化为十进制字节（含负号），写入 `buf`，返回有效长度。
fn i64_to_dec(v: i64, buf: &mut [u8; 24]) -> &[u8] {
    if v == 0 {
        buf[0] = b'0';
        return &buf[..1];
    }
    let neg = v < 0;
    let mut n = v.unsigned_abs();
    let mut tmp = [0u8; 24];
    let mut i = 0;
    while n > 0 {
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    let mut j = 0;
    if neg {
        buf[j] = b'-';
        j += 1;
    }
    while i > 0 {
        i -= 1;
        buf[j] = tmp[i];
        j += 1;
    }
    &buf[..j]
}

// ---------- 命令执行 ----------

/// 执行 `echo <文本>`：输出一行。支持双引号字符串与转义。
fn exec_echo(arg: &[u8]) {
    if let Some(content) = string_content(arg) {
        let mut buf = [0u8; 256];
        let n = unescape(content, &mut buf);
        outln(&buf[..n]);
    } else {
        // 裸文本（无引号）：去掉首尾空白后原样输出。
        outln(trim_bytes(arg));
    }
}

/// 执行 `print(...)` / `println(...)`：参数为字符串字面量则原样输出（含转义），
/// 否则作表达式求值。`newline=true`（`println`）时在末尾追加换行。
fn exec_print(arg: &[u8], newline: bool) {
    let s = trim_bytes(arg);
    // 去掉外层括号
    let inner = if s.starts_with(b"(") && s.ends_with(b")") {
        &s[1..s.len() - 1]
    } else {
        s
    };

    if let Some(content) = string_content(inner) {
        let mut buf = [0u8; 256];
        let n = unescape(content, &mut buf);
        out(&buf[..n]);
        if newline {
            out(b"\n");
        }
    } else {
        match Expr::evaluate(inner) {
            Ok(v) => {
                let mut buf = [0u8; 24];
                let s = i64_to_dec(v, &mut buf);
                out(s);
                if newline {
                    out(b"\n");
                }
            }
            Err(e) => {
                out(e.message());
                out(b"\n");
            }
        }
    }
}

/// 执行一行命令。以 `;` 结尾可省略。空行/注释(`#`)跳过。
fn exec_line(line: &[u8]) {
    let line = trim_bytes(line);
    if line.is_empty() || line.first() == Some(&b'#') {
        return;
    }
    // 去掉结尾分号
    let line = if line.last() == Some(&b';') {
        &line[..line.len() - 1]
    } else {
        line
    };

    // 命令名：第一个空白或 '(' 前
    let name_len = line
        .iter()
        .position(|&c| c == b' ' || c == b'\t' || c == b'(')
        .unwrap_or(line.len());
    let name = &line[..name_len];
    let arg = &line[name_len..];

    match name {
        b"echo" => exec_echo(arg),
        b"print" => exec_print(arg, false),
        b"println" => exec_print(arg, true),
        other => {
            out(b"boruix: unknown command: ");
            out(other);
            out(b"\n");
        }
    }
}

/// shell 入口（libsys `_start` 调用）：输出横幅并进入 REPL 循环。
/// 返回退出码。
#[unsafe(no_mangle)]
pub extern "C" fn user_main(_argc: isize, _argv: *const *const u8) -> i32 {
    outln(b"BORUIX shell (PID 2)");
    repl_loop();
    0
}

/// REPL（Read-Eval-Print-Loop）主循环：读一行 → 执行。
///
/// 经 `read` 系统调用从内核键盘输入缓冲逐字符读行；缓冲空（`WouldBlock`）时
/// 让出 CPU 并重试（键盘是异步中断驱动，无输入时不忙等）。空行与 `#` 注释跳过。
fn repl_loop() {
    let mut line = [0u8; 256];
    loop {
        prompt();
        let n = read_line(&mut line);
        if n == 0 {
            // 读到空行（直接回车）：继续下一轮。
            continue;
        }
        exec_line(&line[..n]);
    }
}

/// 从标准输入读一行（直到 `\n` 或缓冲满），返回有效长度（不含 `\n`）。
///
/// 逐字符 `read(0, &1)`：内核键盘缓冲有字符则回显并暂存；空则返回
/// `WouldBlock`（`Err`），此处先让出 CPU 再继续，避免忙等。
fn read_line(buf: &mut [u8]) -> usize {
    let mut n = 0;
    // 转义序列丢弃状态机：键盘驱动对方向键/编辑键/F 键输出 ANSI 序列（如 `↑`→
    // `\x1b[A`，Insert→`\x1b[2~`，F1→`\x1bOP`）。此处吞掉整个序列，避免污染命令行
    // （行编辑留作后续；现阶段这些键仅被忽略而不报错）。
    // 0=普通 1=已遇 ESC 2=CSI 参数/中间字节 3=SS3 单字节终结。
    let mut esc_state: u8 = 0;
    while n < buf.len() {
        let mut one = [0u8; 1];
        match libsys::read(STDIN, &mut one) {
            Ok(got) if got == 1 => {
                let c = one[0];
                // 正处于转义序列中：按状态机吞掉剩余字节。
                if esc_state != 0 {
                    match esc_state {
                        1 => {
                            // ESC 后：期待引导字节 '['(CSI) 或 'O'(SS3)。
                            if c == b'[' {
                                esc_state = 2;
                            } else if c == b'O' {
                                esc_state = 3;
                            } else {
                                esc_state = 0; // 未知引导，停止丢弃（本字节忽略）
                            }
                        }
                        2 => {
                            // CSI：参数(0x30..=0x3F)/中间(0x20..=0x2F)继续，
                            // 终结字节(0x40..=0x7E)结束；其余视为畸形停止。
                            if (0x40..=0x7E).contains(&c) {
                                esc_state = 0;
                            } else if !(0x20..=0x3F).contains(&c) {
                                esc_state = 0;
                            }
                        }
                        3 => {
                            esc_state = 0; // SS3：下一字节即终结
                        }
                        _ => {}
                    }
                    continue;
                }
                if c == 0x1B {
                    // ESC：开始丢弃整个转义序列。
                    esc_state = 1;
                    continue;
                }
                if c == b'\n' || c == b'\r' {
                    break; // 行结束
                } else if c == 0x7F || c == 0x08 {
                    // 退格（DEL 0x7F 或 BS 0x08）：删除上一个已输入字符，
                    // 并向终端发送擦除序列（BS + 空格 + BS）刷新光标。
                    if n > 0 {
                        n -= 1;
                        out(b"\x08 \x08");
                    }
                } else {
                    // 回显 + 暂存
                    let _ = write(STDOUT, &one);
                    buf[n] = c;
                    n += 1;
                }
            }
            Ok(_) => {
                // 读到 0 字节：无更多数据，继续尝试。
            }
            Err(_) => {
                // WouldBlock / 其它：键盘尚未就绪或缓冲空，让出 CPU 重试。
                let _ = libsys::yield_now();
            }
        }
    }
    out(b"\n");
    n
}

/// 标准输入文件描述符。
const STDIN: u64 = 0;
