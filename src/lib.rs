//! BORUIX 用户态 shell（命令行解释器，库形式，由 init 调用）。
//!
//! 当前形态：由于内核尚无键盘输入（输入后置），shell 以**内置脚本**方式运行，
//! 逐行解释执行以下命令，验证「词法 + 表达式求值 + 转义」能力：
//!
//! - `echo <文本>`：输出一行文本（支持双引号字符串与 `\n` 等转义）。
//! - `print("字符串")`：输出字符串字面量（支持 `\n`/`\t`/`\\`/`\"` 转义）。
//! - `print(<算术表达式>)`：解析并求值 `+ - * /` 四则运算（含括号、空格），
//!   输出十进制结果。
//!
//! 交互式输入（read syscall + 键盘）落地后，把内置脚本替换为「读一行 → 执行」即可。

#![no_std]

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

    fn peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.s.get(self.pos).copied()
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

// ---------- 解释执行 ----------

/// 内置演示脚本（验证 echo / print 字符串 / print 算术）。
/// 中文经 UTF-8 字节转义；`\n`/`\t` 等转义以字面反斜杠序列书写，
/// 由 `unescape` 解码为实际控制字符。
const SCRIPT: &[&[u8]] = &[
    b"echo helloworld",
    b"print(\"\xe8\xae\xa1\xe7\xae\x97\xe7\xbb\x93\xe6\x9e\x9c\xe6\x98\xaf\xef\xbc\x9a\\n\")",
    b"print(1+1)",
    b"print((10 + 2) * 3 / 4 - 5)",
    b"print(-7 + 3 * 2)",
    b"print(\"tab:\\t done, slash:\\\\, quote:\\\"\\n\")",
];

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

/// shell 主循环：执行内置脚本。返回退出码。
pub fn run() -> i32 {
    outln(b"BORUIX shell (interactive input pending; running built-in script)");
    for line in SCRIPT {
        prompt();
        outln(line);
        exec_line(line);
        out(b"\n");
    }
    outln(b"script done");
    0
}
