//! 纯数值格式化与解析（无 alloc，无 shell 内部依赖）。
//!
//! 被 `env`（渲染/存储变量）与 `expr`（表达式求值）共同复用；把数值相关的
//! 手工实现集中于此，避免裸机 `no_std` 下缺失 `f64`/`i64` 格式化支持。

use crate::util::trim_bytes;

/// 表达式值中字符串的最大字节数。
pub(crate) const VAL_CAP: usize = 64;

/// 把 `i64` 格式化为十进制字节（含负号），写入 `buf`，返回有效长度。
pub(crate) fn i64_to_dec(v: i64, buf: &mut [u8; 24]) -> &[u8] {
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

/// 把 `i64` 的小端 8 字节写入 `dst`，返回写入长度（用于 `:i64` 变量存储）。
pub(crate) fn i64_to_le(v: i64, dst: &mut [u8]) -> usize {
    let b = v.to_le_bytes();
    let n = b.len().min(dst.len());
    dst[..n].copy_from_slice(&b[..n]);
    n
}

/// 把 `f64` 的位模式小端 8 字节写入 `dst`，返回写入长度（用于 `:f64` 变量存储）。
pub(crate) fn f64_to_le(v: f64, dst: &mut [u8]) -> usize {
    let b = v.to_bits().to_le_bytes();
    let n = b.len().min(dst.len());
    dst[..n].copy_from_slice(&b[..n]);
    n
}

/// 解析十进制有符号整数（ASCII，忽略首尾空白）。失败返回 `None`。
pub(crate) fn parse_i64(s: &[u8]) -> Option<i64> {
    let s = trim_bytes(s);
    if s.is_empty() {
        return None;
    }
    core::str::from_utf8(s).ok()?.parse::<i64>().ok()
}

/// 解析十进制浮点字面量（可选符号 + 整数 + 可选小数；不含指数）。失败返回 `None`。
pub(crate) fn parse_f64(s: &[u8]) -> Option<f64> {
    let s = trim_bytes(s);
    if s.is_empty() {
        return None;
    }
    let mut i = 0;
    let mut negative = false;
    if s[i] == b'+' {
        i += 1;
    } else if s[i] == b'-' {
        negative = true;
        i += 1;
    }
    let mut int_part: u64 = 0;
    let mut has_digit = false;
    while i < s.len() && s[i].is_ascii_digit() {
        int_part = int_part.wrapping_mul(10).wrapping_add((s[i] - b'0') as u64);
        has_digit = true;
        i += 1;
    }
    let mut frac_part: u64 = 0;
    let mut frac_scale: u64 = 1;
    if i < s.len() && s[i] == b'.' {
        i += 1;
        while i < s.len() && s[i].is_ascii_digit() {
            if frac_scale < 1_000_000_000 {
                frac_part = frac_part * 10 + (s[i] - b'0') as u64;
                frac_scale *= 10;
            }
            has_digit = true;
            i += 1;
        }
    }
    if !has_digit || i != s.len() {
        return None;
    }
    let mut v = int_part as f64;
    if frac_scale > 1 {
        v += (frac_part as f64) / (frac_scale as f64);
    }
    if negative {
        v = -v;
    }
    Some(v)
}

/// 向下取整（no_std 下 `f64::floor` 未必可用，手写实现）。
pub(crate) fn ffloor(v: f64) -> f64 {
    let t = v as i64;
    let tf = t as f64;
    if tf <= v {
        tf
    } else {
        tf - 1.0
    }
}

/// 把 `f64` 格式化为十进制文本（最多 6 位小数、去尾零），写入 `buf`，返回有效长度。
pub(crate) fn f64_to_text(v: f64, buf: &mut [u8; 32]) -> &[u8] {
    if v.is_nan() {
        return b"nan";
    }
    if v.is_infinite() {
        return if v < 0.0 { b"-inf" } else { b"inf" };
    }
    let negative = v < 0.0;
    let av = v.abs();
    let int_part = ffloor(av) as u64;
    let frac = av - ffloor(av);
    let mut tmp = [0u8; 32];
    let mut i = 0;
    if int_part == 0 {
        tmp[i] = b'0';
        i += 1;
    } else {
        let mut n = int_part;
        let mut t = [0u8; 24];
        let mut ti = 0;
        while n > 0 {
            t[ti] = b'0' + (n % 10) as u8;
            n /= 10;
            ti += 1;
        }
        while ti > 0 {
            ti -= 1;
            tmp[i] = t[ti];
            i += 1;
        }
    }
    // 小数部分（最多 6 位，去尾零）
    let mut dig = [0u8; 8];
    let mut nd = 0;
    let mut f = frac;
    for _ in 0..6 {
        if f == 0.0 {
            break;
        }
        f *= 10.0;
        dig[nd] = b'0' + ffloor(f) as u8;
        nd += 1;
        f -= ffloor(f);
    }
    while nd > 0 && dig[nd - 1] == b'0' {
        nd -= 1;
    }
    let mut j = 0;
    if negative {
        buf[j] = b'-';
        j += 1;
    }
    let mut k = 0;
    while k < i {
        buf[j] = tmp[k];
        j += 1;
        k += 1;
    }
    if nd > 0 {
        buf[j] = b'.';
        j += 1;
        let mut m = 0;
        while m < nd {
            buf[j] = dig[m];
            j += 1;
            m += 1;
        }
    }
    &buf[..j]
}
