//! BORUIX 用户态 shell（命令行解释器，独立可执行程序）。
//!
//! 由 init（PID 1）经 `exec` 系统调用加载运行为 PID 2。libsys 提供 `_start`
//! 入口，本文件导出 `user_main`（进程入口）。
//!
//! 命令集：
//! - `echo <文本>`：输出一行文本（支持双引号字符串与 `\n` 等转义）。
//! - `print("字符串")`：输出字符串字面量（支持 `\n`/`\t`/`\\`/`\"` 转义）。
//! - `print(<算术表达式>)`：解析并求值 `+ - * /` 四则运算（含括号、空格、
//!   一元负号），输出十进制结果。

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
    fn parse_expr(&mut self) -> Option<i64> {
        let mut v = self.parse_term()?;
        loop {
            if self.eat(b'+') {
                v = v.checked_add(self.parse_term()?)?;
            } else if self.eat(b'-') {
                v = v.checked_sub(self.parse_term()?)?;
            } else {
                break;
            }
        }
        Some(v)
    }

    /// `term := factor (('*'|'/') factor)*`
    fn parse_term(&mut self) -> Option<i64> {
        let mut v = self.parse_factor()?;
        loop {
            if self.eat(b'*') {
                v = v.checked_mul(self.parse_factor()?)?;
            } else if self.eat(b'/') {
                let d = self.parse_factor()?;
                if d == 0 {
                    return None; // 除零
                }
                v = v / d;
            } else {
                break;
            }
        }
        Some(v)
    }

    /// `factor := NUMBER | '(' expr ')' | '-' factor | '+' factor`
    fn parse_factor(&mut self) -> Option<i64> {
        self.skip_ws();
        if self.eat(b'-') {
            return self.parse_factor().map(|v| -v);
        }
        if self.eat(b'+') {
            return self.parse_factor();
        }
        if self.eat(b'(') {
            let v = self.parse_expr()?;
            if !self.eat(b')') {
                return None; // 缺右括号
            }
            return Some(v);
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
            return None;
        }
        let txt = core::str::from_utf8(&self.s[start..self.pos]).ok()?;
        txt.parse::<i64>().ok()
    }

    /// 解析整个表达式，要求全部消费（无尾随垃圾）。
    fn evaluate(src: &[u8]) -> Option<i64> {
        let mut p = Expr::new(src);
        let v = p.parse_expr()?;
        p.skip_ws();
        if p.pos == p.s.len() {
            Some(v)
        } else {
            None
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

/// 执行 `print(参数)`：参数为字符串字面量则输出（含转义），否则作表达式求值。
fn exec_print(arg: &[u8]) {
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
    } else {
        match Expr::evaluate(inner) {
            Some(v) => {
                let mut buf = [0u8; 24];
                let s = i64_to_dec(v, &mut buf);
                out(s);
            }
            None => out(b"<error: bad expression>"),
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
        b"print" => exec_print(arg),
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
/// 读取经 `read` 系统调用从内核键盘输入缓冲取一行；当前无键盘输入时
/// 返回空/阻塞。空行与 `#` 注释跳过。
fn repl_loop() {
    let mut line = [0u8; 256];
    loop {
        prompt();
        let n = read_line(&mut line);
        if n == 0 {
            // 无输入源/读失败：短暂让出避免忙等；无调度时直接返回结束。
            // （键盘输入接入后，read 会阻塞直到有数据。）
            return;
        }
        exec_line(&line[..n]);
    }
}

/// 从标准输入读一行（直到 `\n` 或缓冲满），返回有效长度（不含 `\n`）。
/// 读到的字节数。当前经 `read` syscall 从内核键盘缓冲取；无可用输入返回 0。
fn read_line(buf: &mut [u8]) -> usize {
    let mut n = 0;
    while n < buf.len() {
        let mut one = [0u8; 1];
        let got = libsys::read(STDIN, &mut one).unwrap_or(0);
        if got == 0 {
            break; // 无可读数据
        }
        let c = one[0];
        if c == b'\n' {
            break;
        }
        // 回显 + 暂存
        let _ = write(STDOUT, &one);
        if c == b'\r' {
            continue;
        }
        buf[n] = c;
        n += 1;
    }
    out(b"\n");
    n
}

/// 标准输入文件描述符。
const STDIN: u64 = 0;
