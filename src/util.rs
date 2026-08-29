//! 输出辅助与通用字节处理工具。
//!
//! 本模块不依赖 shell 其它模块，是其它模块的公共基础。

use libsys::{write, STDOUT};

/// 把字节切片输出到标准输出（丢弃错误，静默失败）。
pub(crate) fn out(s: &[u8]) {
    let _ = write(STDOUT, s);
}

/// 输出一行。
pub(crate) fn outln(s: &[u8]) {
    out(s);
    out(b"\n");
}

/// 打印 shell 提示符。
pub(crate) fn prompt() {
    out(b"boruix$ ");
}

/// `&[u8]` 的 `trim` 等价物：去掉首尾 ASCII 空白。
pub(crate) fn trim_bytes(s: &[u8]) -> &[u8] {
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

/// 把带 `\n`/`\t`/`\\`/`\"` 转义的字面量内容解码为原始字节序列。
///
/// `src` 应已去掉外层双引号；返回解码后写入 `buf` 的字节数。
pub(crate) fn unescape(src: &[u8], buf: &mut [u8]) -> usize {
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
pub(crate) fn string_content(s: &[u8]) -> Option<&[u8]> {
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

/// 把 `u64` 格式化为十进制字节，写入 `buf`，返回有效长度。
pub(crate) fn u64_to_dec(v: u64, buf: &mut [u8; 24]) -> &[u8] {
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
pub(crate) fn parse_u64(s: &[u8]) -> Option<u64> {
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

/// 把 0~99 格式化为两位十进制（高位补零），写入 `buf`，返回有效切片。
/// 用于墙钟时间的月/日/时/分/秒字段（如 `05`、`07`）。
pub(crate) fn pad2(v: u64, buf: &mut [u8; 24]) -> &[u8] {
    buf[0] = b'0' + ((v / 10) % 10) as u8;
    buf[1] = b'0' + (v % 10) as u8;
    &buf[..2]
}
