//! 带类型表达式求值（print/println 专用迷你语言）。
//!
//! 语言：整数 / 浮点字面量、双引号字符串字面量、`$ident` 变量引用、括号，
//! 运算符 `+ - * /`（左结合）。`+` 若任一操作数为字符串则做拼接，否则数值加；
//! `- * /` 仅数值，字符串参与报类型错。

use crate::env::{env_get_kind, EnvTy};
use crate::num::i64_to_dec;
use crate::util::{out, trim_bytes};

/// 表达式求值错误类型（友好消息，而非笼统的 "bad expression"）。
#[derive(Clone, Copy)]
pub(crate) enum EvalErr {
    Empty,         // 表达式为空（如 `print()`）
    BadNumber,     // 整数 / 数字字面量非法或溢出
    BadFloat,      // 浮点字面量非法
    DivZero,       // 除以零
    BadParen,      // 括号不匹配 / 字符串未闭合
    Trailing,      // 表达式后有多余字符
    TypeMismatch,  // 对非数值类型使用了 - * / 等
    BadEscape,     // 字符串转义非法
}

impl EvalErr {
    /// 友好错误消息（统一 `boruix:` 前缀，与未知命令报错风格一致）。
    fn message(self) -> &'static [u8] {
        match self {
            EvalErr::Empty => b"boruix: empty expression",
            EvalErr::BadNumber => b"boruix: invalid number in expression",
            EvalErr::BadFloat => b"boruix: invalid number in expression",
            EvalErr::DivZero => b"boruix: division by zero",
            EvalErr::BadParen => b"boruix: mismatched parentheses",
            EvalErr::Trailing => b"boruix: unexpected trailing characters",
            EvalErr::TypeMismatch => b"boruix: type mismatch (string used in arithmetic)",
            EvalErr::BadEscape => b"boruix: invalid escape in string",
        }
    }
}

/// 表达式值（带类型）。字符串用定长缓冲（无 alloc）。
struct Val {
    ty: u8, // 0=str 1=i64 2=f64
    sbuf: [u8; crate::num::VAL_CAP],
    slen: usize,
    i: i64,
    f: f64,
}

impl Val {
    fn str_(bytes: &[u8]) -> Val {
        let mut sbuf = [0u8; crate::num::VAL_CAP];
        let l = bytes.len().min(crate::num::VAL_CAP);
        sbuf[..l].copy_from_slice(&bytes[..l]);
        Val { ty: 0, sbuf, slen: l, i: 0, f: 0.0 }
    }
    fn i64_(v: i64) -> Val {
        Val { ty: 1, sbuf: [0; crate::num::VAL_CAP], slen: 0, i: v, f: 0.0 }
    }
    fn f64_(v: f64) -> Val {
        Val { ty: 2, sbuf: [0; crate::num::VAL_CAP], slen: 0, i: 0, f: v }
    }
    fn is_str(&self) -> bool {
        self.ty == 0
    }
    /// 渲染为文本写入 `dst`，返回长度。
    fn render(&self, dst: &mut [u8]) -> usize {
        match self.ty {
            0 => {
                let mut o = 0;
                for k in 0..self.slen {
                    if o < dst.len() {
                        dst[o] = self.sbuf[k];
                        o += 1;
                    }
                }
                o
            }
            1 => {
                let mut buf = [0u8; 24];
                let s = i64_to_dec(self.i, &mut buf);
                let mut o = 0;
                for &c in s {
                    if o < dst.len() {
                        dst[o] = c;
                        o += 1;
                    }
                }
                o
            }
            _ => {
                let mut buf = [0u8; 32];
                // 复用 num 的 f64 文本格式化（与变量渲染保持一致）。
                let s = crate::num::f64_to_text(self.f, &mut buf);
                let mut o = 0;
                for &c in s {
                    if o < dst.len() {
                        dst[o] = c;
                        o += 1;
                    }
                }
                o
            }
        }
    }
}

/// 变量名 → 带类型值（缺失 → 空字符串）。
fn var_val(name: &[u8]) -> Val {
    match env_get_kind(name) {
        Some((EnvTy::Str, raw)) => Val::str_(raw),
        Some((EnvTy::I64, raw)) if raw.len() >= 8 => {
            let mut b = [0u8; 8];
            b.copy_from_slice(&raw[..8]);
            Val::i64_(i64::from_le_bytes(b))
        }
        Some((EnvTy::F64, raw)) if raw.len() >= 8 => {
            let mut b = [0u8; 8];
            b.copy_from_slice(&raw[..8]);
            Val::f64_(f64::from_le_bytes(b))
        }
        _ => Val::str_(b""),
    }
}

/// 一元负号（仅数值；字符串 → 类型错）。
fn neg_val(v: Val) -> Result<Val, EvalErr> {
    match v.ty {
        1 => Ok(Val::i64_(-v.i)),
        2 => Ok(Val::f64_(-v.f)),
        _ => Err(EvalErr::TypeMismatch),
    }
}

/// `+`：任一为字符串 → 拼接；否则数值加（混合 i64/f64 提升为 f64）。
fn apply_add(a: Val, b: Val) -> Result<Val, EvalErr> {
    if a.is_str() || b.is_str() {
        let mut buf = [0u8; crate::num::VAL_CAP * 2];
        let mut o = a.render(&mut buf);
        o += b.render(&mut buf[o..]);
        Ok(Val::str_(&buf[..o.min(crate::num::VAL_CAP)]))
    } else {
        apply_arith(a, b, b'+')
    }
}

/// 数值二元运算（`- * /`；`+` 也走此路径做数值加）。字符串参与 → 类型错。
fn apply_arith(a: Val, b: Val, op: u8) -> Result<Val, EvalErr> {
    if a.is_str() || b.is_str() {
        return Err(EvalErr::TypeMismatch);
    }
    if a.ty == 2 || b.ty == 2 {
        let x = if a.ty == 2 { a.f } else { a.i as f64 };
        let y = if b.ty == 2 { b.f } else { b.i as f64 };
        let r = match op {
            b'-' => x - y,
            b'*' => x * y,
            b'/' => {
                if y == 0.0 {
                    return Err(EvalErr::DivZero);
                }
                x / y
            }
            _ => x + y,
        };
        Ok(Val::f64_(r))
    } else {
        let x = a.i;
        let y = b.i;
        let r = match op {
            b'-' => x.checked_sub(y).ok_or(EvalErr::BadNumber)?,
            b'*' => x.checked_mul(y).ok_or(EvalErr::BadNumber)?,
            b'/' => {
                if y == 0 {
                    return Err(EvalErr::DivZero);
                }
                x / y
            }
            _ => x.checked_add(y).ok_or(EvalErr::BadNumber)?,
        };
        Ok(Val::i64_(r))
    }
}

/// 递归下降求值器。
struct Ev<'a> {
    s: &'a [u8],
    pos: usize,
}

impl<'a> Ev<'a> {
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
    fn parse_expr(&mut self) -> Result<Val, EvalErr> {
        let mut left = self.parse_term()?;
        loop {
            if self.eat(b'+') {
                left = apply_add(left, self.parse_term()?)?;
            } else if self.eat(b'-') {
                left = apply_arith(left, self.parse_term()?, b'-')?;
            } else {
                break;
            }
        }
        Ok(left)
    }

    /// `term := factor (('*'|'/') factor)*`
    fn parse_term(&mut self) -> Result<Val, EvalErr> {
        let mut left = self.parse_factor()?;
        loop {
            if self.eat(b'*') {
                left = apply_arith(left, self.parse_factor()?, b'*')?;
            } else if self.eat(b'/') {
                left = apply_arith(left, self.parse_factor()?, b'/')?;
            } else {
                break;
            }
        }
        Ok(left)
    }

    /// `factor := '-' factor | '+' factor | '(' expr ')' | STRING | $ident | NUMBER`
    fn parse_factor(&mut self) -> Result<Val, EvalErr> {
        self.skip_ws();
        if self.eat(b'-') {
            return neg_val(self.parse_factor()?);
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
        // 字符串字面量 `"..."`
        if self.s.get(self.pos) == Some(&b'"') {
            return self.parse_string();
        }
        // 变量引用 `$ident`
        if self.s.get(self.pos) == Some(&b'$')
            && self.pos + 1 < self.s.len()
            && (self.s[self.pos + 1].is_ascii_alphanumeric() || self.s[self.pos + 1] == b'_')
        {
            let s = self.pos + 1;
            let mut e = s;
            while e < self.s.len() && (self.s[e].is_ascii_alphanumeric() || self.s[e] == b'_') {
                e += 1;
            }
            let name = &self.s[s..e];
            self.pos = e;
            return Ok(var_val(name));
        }
        // 数字字面量（整数或浮点）
        self.parse_number()
    }

    /// 解析双引号字符串（支持 `\\ \n \t \r \0 \"` 转义）。
    fn parse_string(&mut self) -> Result<Val, EvalErr> {
        self.pos += 1; // 跳过开引号
        let mut sbuf = [0u8; crate::num::VAL_CAP];
        let mut slen = 0usize;
        while self.pos < self.s.len() {
            let c = self.s[self.pos];
            if c == b'"' {
                self.pos += 1;
                return Ok(Val::str_(&sbuf[..slen]));
            }
            if c == b'\\' && self.pos + 1 < self.s.len() {
                let nxt = self.s[self.pos + 1];
                let e = match nxt {
                    b'n' => b'\n',
                    b't' => b'\t',
                    b'r' => b'\r',
                    b'0' => b'\0',
                    b'\\' => b'\\',
                    b'"' => b'"',
                    _ => return Err(EvalErr::BadEscape),
                };
                if slen < crate::num::VAL_CAP {
                    sbuf[slen] = e;
                    slen += 1;
                }
                self.pos += 2;
            } else {
                if slen < crate::num::VAL_CAP {
                    sbuf[slen] = c;
                    slen += 1;
                }
                self.pos += 1;
            }
        }
        Err(EvalErr::BadParen) // 未闭合引号
    }

    /// 解析数字字面量：整数（溢出降级 f64）/ 浮点。
    fn parse_number(&mut self) -> Result<Val, EvalErr> {
        self.skip_ws();
        let start = self.pos;
        let mut is_digit = false;
        let mut has_dot = false;
        while self.pos < self.s.len() {
            let c = self.s[self.pos];
            if c.is_ascii_digit() {
                is_digit = true;
                self.pos += 1;
            } else if c == b'.' && !has_dot {
                has_dot = true;
                self.pos += 1;
            } else {
                break;
            }
        }
        if !is_digit {
            return Err(EvalErr::BadNumber);
        }
        let txt = &self.s[start..self.pos];
        if has_dot {
            match crate::num::parse_f64(txt) {
                Some(v) => Ok(Val::f64_(v)),
                None => Err(EvalErr::BadFloat),
            }
        } else {
            let st = core::str::from_utf8(txt).map_err(|_| EvalErr::BadNumber)?;
            match st.parse::<i64>() {
                Ok(v) => Ok(Val::i64_(v)),
                Err(_) => match crate::num::parse_f64(txt) {
                    Some(v) => Ok(Val::f64_(v)),
                    None => Err(EvalErr::BadNumber),
                },
            }
        }
    }

    /// 解析整个表达式，要求全部消费（无尾随垃圾）。
    fn evaluate(src: &[u8]) -> Result<Val, EvalErr> {
        if trim_bytes(src).is_empty() {
            return Err(EvalErr::Empty);
        }
        let mut p = Ev::new(src);
        let v = p.parse_expr()?;
        p.skip_ws();
        if p.pos == p.s.len() {
            Ok(v)
        } else {
            Err(EvalErr::Trailing)
        }
    }
}

/// 执行 `print(...)` / `println(...)`：把参数当作**带类型表达式**求值（字符串 /
/// i64 / f64，`$ident` 按带类型变量引用解析），渲染后输出。`newline=true`
/// （`println`）时在末尾追加换行。表达式非法时输出对应友好错误。
pub(crate) fn exec_print(arg: &[u8], newline: bool) {
    let s = trim_bytes(arg);
    // 去掉外层括号
    let inner = if s.starts_with(b"(") && s.ends_with(b")") {
        &s[1..s.len() - 1]
    } else {
        s
    };

    match Ev::evaluate(inner) {
        Ok(v) => {
            let mut buf = [0u8; 256];
            let n = v.render(&mut buf);
            out(&buf[..n]);
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
