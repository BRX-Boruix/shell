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

use libsys::{info, kill, now, ps, sleep, write, PsEntry, STDOUT};
use libsys::nr::{INFO_BOOT_MS, INFO_CPU_COUNT, INFO_VERSION};

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

/// 提取引号字符串的内容（去掉首尾引号）。支持双引号 `"` 与单引号 `'`。
/// 双引号内层支持 `\"` 转义；单引号内层为字面量（tok 已保证其内 `$` 不展开）。
/// 找不到闭合引号返回 `None`。
fn string_content(s: &[u8]) -> Option<&[u8]> {
    let s = trim_bytes(s);
    let q = *s.first()?;
    if q != b'"' && q != b'\'' {
        return None;
    }
    // 找闭合引号（双引号支持 \" 转义）。
    for i in 1..s.len() {
        let c = s[i];
        if c == q && s[i - 1] != b'\\' {
            return Some(&s[1..i]);
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

// ---------- 环境变量（固定大小静态表，无 alloc） ----------

const MAX_ENV: usize = 32;
const ENV_NAME: usize = 24;
const ENV_VAL: usize = 64;

/// 环境变量表：`(name, value, used)`。shell 单进程单线程裸机程序，用静态数组。
static mut ENV_TABLE: [([u8; ENV_NAME], [u8; ENV_VAL], bool); MAX_ENV] =
    [([0u8; ENV_NAME], [0u8; ENV_VAL], false); MAX_ENV];
static mut ENV_COUNT: usize = 0;

/// 查找环境变量值（返回切片，不含结尾 0）。
fn env_get(name: &[u8]) -> Option<&'static [u8]> {
    unsafe {
        for i in 0..ENV_COUNT {
            let (n, v, used) = &ENV_TABLE[i];
            if *used && n[..name.len()] == *name && n[name.len()..].iter().all(|&b| b == 0) {
                let mut len = 0;
                while len < ENV_VAL && v[len] != 0 {
                    len += 1;
                }
                return Some(&v[..len]);
            }
        }
    }
    None
}

/// 设置/覆盖环境变量。
fn env_set(name: &[u8], val: &[u8]) {
    if name.is_empty() || name.len() > ENV_NAME {
        return;
    }
    unsafe {
        for i in 0..ENV_COUNT {
            let (n, v, used) = &mut ENV_TABLE[i];
            if *used && n[..name.len()] == *name && n[name.len()..].iter().all(|&b| b == 0) {
                let l = val.len().min(ENV_VAL);
                v[..l].copy_from_slice(&val[..l]);
                for x in &mut v[l..] {
                    *x = 0;
                }
                return;
            }
        }
        if ENV_COUNT >= MAX_ENV {
            return;
        }
        let i = ENV_COUNT;
        let l = name.len();
        ENV_TABLE[i].0[..l].copy_from_slice(name);
        for x in &mut ENV_TABLE[i].0[l..] {
            *x = 0;
        }
        let l = val.len().min(ENV_VAL);
        ENV_TABLE[i].1[..l].copy_from_slice(&val[..l]);
        for x in &mut ENV_TABLE[i].1[l..] {
            *x = 0;
        }
        ENV_TABLE[i].2 = true;
        ENV_COUNT += 1;
    }
}

/// 单条命令最多参数词数（含命令名）。
const MAX_WORDS: usize = 16;
/// 单个词的最大字节数（与环境变量 VALUE 上限对齐）。
const WORD_CAP: usize = 64;

/// 把一行拆成词（尊重引号），并对每个词做 `$VAR` 展开。
///
/// 语义（贴近 POSIX shell）：
/// - 空白（` ` / `\t`）分词；
/// - 双引号 `"..."`：内部展开 `$VAR` 并去除引号，整体作为一个词；
/// - 单引号 `'...'`：字面量，**不**展开 `$VAR`，整体作为一个词；
/// - 引号可在词内拼接，如 `a"b"c` → `abc`；
/// - 未加引号的 `$VAR` 仍展开，其值若含空白会参与下一次分词。
///
/// 词内容写入 `wbuf`（每项 `WORD_CAP` 字节）、长度写入 `wlen`，返回词数。
/// 变量值经 `env_get` 取静态切片，缺失则该 `$VAR` 替换为空。
fn tokenize_line(
    line: &[u8],
    wbuf: &mut [[u8; WORD_CAP]; MAX_WORDS],
    wlen: &mut [usize; MAX_WORDS],
) -> usize {
    let mut nwords = 0usize;
    let mut word = [0u8; WORD_CAP];
    let mut wl = 0usize;
    let mut i = 0usize;
    let n = line.len();

    // 把当前正在累积的词提交到 wbuf（词非空且未满）。
    let mut commit = |word: &[u8; WORD_CAP], wl: &mut usize, wbuf: &mut [[u8; WORD_CAP]; MAX_WORDS], wlen: &mut [usize; MAX_WORDS], nwords: &mut usize| {
        if *wl > 0 && *nwords < MAX_WORDS {
            let l = (*wl).min(WORD_CAP);
            wbuf[*nwords][..l].copy_from_slice(&word[..l]);
            wlen[*nwords] = l;
            *nwords += 1;
        }
        *wl = 0;
    };

    // 把一个 `$VAR` 标识符（已定位 `[s, e)`）的展开值追加到当前词。
    let mut expand_at = |line: &[u8], s: usize, e: usize, word: &mut [u8; WORD_CAP], wl: &mut usize| {
        if let Some(val) = env_get(&line[s..e]) {
            for &b in val {
                if *wl < WORD_CAP {
                    word[*wl] = b;
                    *wl += 1;
                }
            }
        }
    };

    while i < n {
        let c = line[i];
        if c == b' ' || c == b'\t' {
            commit(&word, &mut wl, wbuf, wlen, &mut nwords);
            i += 1;
            continue;
        }
        if c == b'"' || c == b'\'' {
            let q = c;
            let do_expand = q == b'"'; // 单引号内不展开
            if wl < WORD_CAP {
                word[wl] = q; // 保留起始引号字符
                wl += 1;
            }
            i += 1;
            while i < n && line[i] != q {
                let ch = line[i];
                if do_expand && ch == b'$' && i + 1 < n {
                    let nxt = line[i + 1];
                    if nxt.is_ascii_alphanumeric() || nxt == b'_' {
                        let s = i + 1;
                        let mut e = s;
                        while e < n && (line[e].is_ascii_alphanumeric() || line[e] == b'_') {
                            e += 1;
                        }
                        expand_at(line, s, e, &mut word, &mut wl);
                        i = e;
                        continue;
                    }
                }
                if wl < WORD_CAP {
                    word[wl] = ch;
                    wl += 1;
                }
                i += 1;
            }
            if i < n {
                if wl < WORD_CAP {
                    word[wl] = q; // 保留闭合引号字符
                    wl += 1;
                }
                i += 1; // 跳过闭合引号
            }
            continue;
        }
        // 未加引号的普通字符（含裸 `$VAR`）。
        if c == b'$' && i + 1 < n {
            let nxt = line[i + 1];
            if nxt.is_ascii_alphanumeric() || nxt == b'_' {
                let s = i + 1;
                let mut e = s;
                while e < n && (line[e].is_ascii_alphanumeric() || line[e] == b'_') {
                    e += 1;
                }
                expand_at(line, s, e, &mut word, &mut wl);
                i = e;
                continue;
            }
        }
        if wl < WORD_CAP {
            word[wl] = c;
            wl += 1;
        }
        i += 1;
    }
    commit(&word, &mut wl, wbuf, wlen, &mut nwords);
    nwords
}

/// 把 `u64` 格式化为十进制字节，写入 `buf`，返回有效长度。
fn u64_to_dec(v: u64, buf: &mut [u8; 24]) -> &[u8] {
    if v == 0 {
        buf[0] = b'0';
        return &buf[..1];
    }
    let mut tmp = [0u8; 24];
    let mut i = 0;
    let mut n = v;
    while n > 0 {
        tmp[i] = (n % 10) as u8 + b'0';
        n /= 10;
        i += 1;
    }
    let mut j = 0;
    while i > 0 {
        i -= 1;
        buf[j] = tmp[i];
        j += 1;
    }
    &buf[..j]
}

/// 解析十进制无符号整数。
fn parse_u64(s: &[u8]) -> Option<u64> {
    if s.is_empty() {
        return None;
    }
    let mut v: u64 = 0;
    for &c in s {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (c - b'0') as u64;
    }
    Some(v)
}

/// 列出全部内建命令。
fn cmd_help() {
    out(
        b"builtins: echo print println help now time uptime version uname cpu \
sleep clear env export ps kill signal\n",
    );
}

/// `now`/`time`：单调时钟（纳秒）。
fn cmd_now() {
    let ns = now();
    let mut b = [0u8; 24];
    out(b"now: ");
    out(u64_to_dec(ns, &mut b));
    out(b" ns (");
    out(u64_to_dec(ns / 1_000_000_000, &mut b));
    out(b".");
    out(u64_to_dec((ns % 1_000_000_000) / 1_000_000, &mut b));
    out(b" s)\n");
}

/// `uptime`：开机至今。
fn cmd_uptime() {
    let ms = info(INFO_BOOT_MS).unwrap_or(0);
    let mut b = [0u8; 24];
    out(b"uptime: ");
    out(u64_to_dec(ms / 1000, &mut b));
    out(b".");
    out(u64_to_dec(ms % 1000, &mut b));
    out(b" s\n");
}

/// `version`/`uname`：内核版本。
fn cmd_version() {
    let v = info(INFO_VERSION).unwrap_or(0);
    let mut b = [0u8; 24];
    out(b"BORUIX v");
    out(u64_to_dec((v >> 16) & 0xff, &mut b));
    out(b".");
    out(u64_to_dec((v >> 8) & 0xff, &mut b));
    out(b".");
    out(u64_to_dec(v & 0xff, &mut b));
    out(b"\n");
}

/// `cpu`：在线 CPU 数。
fn cmd_cpu() {
    let c = info(INFO_CPU_COUNT).unwrap_or(0);
    let mut b = [0u8; 24];
    out(b"cpus: ");
    out(u64_to_dec(c, &mut b));
    out(b"\n");
}

/// `sleep <秒>`：睡眠（内核当前为忙等实现）。
fn cmd_sleep(arg: &[u8]) {
    let a = trim_bytes(arg);
    match parse_u64(a) {
        Some(secs) => {
            let _ = sleep(secs * 1_000_000_000);
        }
        None => out(b"sleep: usage: sleep <seconds>\n"),
    }
}

/// `env`：列出全部环境变量。
fn cmd_env() {
    unsafe {
        for i in 0..ENV_COUNT {
            let (n, v, used) = &ENV_TABLE[i];
            if !*used {
                continue;
            }
            let mut nl = 0;
            while nl < ENV_NAME && n[nl] != 0 {
                nl += 1;
            }
            out(&n[..nl]);
            out(b"=");
            let mut vl = 0;
            while vl < ENV_VAL && v[vl] != 0 {
                vl += 1;
            }
            out(&v[..vl]);
            out(b"\n");
        }
    }
}

/// `export NAME=VALUE`：设置环境变量。`VALUE` 两端若带引号（`"` 或 `'`）则剥除，
/// 使 `export A="a b"` 存的值为 `a b` 而非含引号原文。
fn cmd_export(arg: &[u8]) {
    let a = trim_bytes(arg);
    if let Some(pos) = a.iter().position(|&c| c == b'=') {
        let name = &a[..pos];
        let mut val = &a[pos + 1..];
        // 剥除值两端成对引号。
        if val.len() >= 2 {
            let f = val.first().copied().unwrap();
            let l = val.last().copied().unwrap();
            if (f == b'"' && l == b'"') || (f == b'\'' && l == b'\'') {
                val = &val[1..val.len() - 1];
            }
        }
        if name.is_empty() {
            out(b"export: empty name\n");
            return;
        }
        env_set(name, val);
    } else {
        out(b"export: usage: export NAME=VALUE\n");
    }
}

/// `ps`：列出存活进程。
fn cmd_ps() {
    let mut buf = [PsEntry { pid: 0, state: 0, _pad: [0; 3] }; 32];
    match ps(&mut buf) {
        Ok(n) => {
            out(b"PID  STATE\n");
            let mut b = [0u8; 24];
            for e in &buf[..n] {
                out(u64_to_dec(e.pid as u64, &mut b));
                out(b"   ");
                let st: &[u8] = match e.state {
                    1 => &b"Ready"[..],
                    2 => &b"Running"[..],
                    3 => &b"Blocked"[..],
                    _ => &b"?"[..],
                };
                out(st);
                out(b"\n");
            }
        }
        Err(_) => out(b"ps: failed\n"),
    }
}

/// 列出已知信号（供 `kill -l` / `signal`）。
fn cmd_signal_list() {
    out(b"signals:\n");
    let mut b = [0u8; 24];
    for (num, name) in libsys::signal::LIST {
        out(u64_to_dec(*num as u64, &mut b));
        out(b" ");
        out(name.as_bytes());
        out(b"\n");
    }
}

/// `kill [-s SIG|-SIG|-l] <pid>`：向进程发送信号。
fn cmd_kill(arg: &[u8]) {
    let a = trim_bytes(arg);
    if a == b"-l" {
        cmd_signal_list();
        return;
    }
    let mut sig: u64 = 15; // 默认 SIGTERM
    let mut pid: u64 = 0;
    let mut have_pid = false;
    let mut i = 0;
    while i < a.len() {
        while i < a.len() && a[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= a.len() {
            break;
        }
        let start = i;
        while i < a.len() && !a[i].is_ascii_whitespace() {
            i += 1;
        }
        let tok = &a[start..i];
        if tok.starts_with(b"-") {
            let body = &tok[1..];
            if body == b"l" {
                cmd_signal_list();
                return;
            }
            let num = if body.starts_with(b"s") { &body[1..] } else { body };
            if let Some(v) = parse_u64(num) {
                sig = v;
            }
        } else if let Some(v) = parse_u64(tok) {
            pid = v;
            have_pid = true;
        }
    }
    if !have_pid {
        out(b"kill: usage: kill [-s SIG|-SIG|-l] <pid>\n");
        return;
    }
    match kill(pid, sig) {
        Ok(_) => {}
        Err(e) => {
            let mut b = [0u8; 24];
            out(b"kill: failed (errno ");
            out(u64_to_dec(e.to_errno() as u64, &mut b));
            out(b")\n");
        }
    }
}

/// 执行一行命令。以 `;` 结尾可省略。空行/注释(`#`)跳过。
///
/// 先经 `tokenize_line` 按引号感知规则分词并展开 `$VAR`，首词为命令名，其余词
/// 以单空格重新连接成 `arg` 传给各命令（引号已在 `exec_echo`/`exec_print` 处解析，
/// 故此处保留引号字符）。
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

    let mut wbuf = [[0u8; WORD_CAP]; MAX_WORDS];
    let mut wlen = [0usize; MAX_WORDS];
    let nw = tokenize_line(line, &mut wbuf, &mut wlen);
    if nw == 0 {
        return;
    }
    let name = &wbuf[0][..wlen[0]];

    // 重建参数：剩余词以单空格连接（词内引号保留、VAR 已展开）。
    let mut argbuf = [0u8; 256];
    let mut al = 0usize;
    for k in 1..nw {
        if al > 0 && al < argbuf.len() {
            argbuf[al] = b' ';
            al += 1;
        }
        let w = &wbuf[k][..wlen[k]];
        for &b in w {
            if al < argbuf.len() {
                argbuf[al] = b;
                al += 1;
            }
        }
    }
    let arg = &argbuf[..al];

    match name {
        b"echo" => exec_echo(arg),
        b"print" => exec_print(arg, false),
        b"println" => exec_print(arg, true),
        b"help" => cmd_help(),
        b"now" | b"time" => cmd_now(),
        b"uptime" => cmd_uptime(),
        b"version" | b"uname" => cmd_version(),
        b"cpu" => cmd_cpu(),
        b"sleep" => cmd_sleep(arg),
        b"clear" => out(b"\x1b[2J\x1b[H"),
        b"env" => cmd_env(),
        b"export" => cmd_export(arg),
        b"ps" => cmd_ps(),
        b"kill" => cmd_kill(arg),
        b"signal" => cmd_signal_list(),
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
