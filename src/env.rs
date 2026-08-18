//! 环境变量表（带类型的名字绑定，UNIX 超集）与 `export`/`env` 命令。
//!
//! 设计（讨论决定）：默认 `export X=文本` 永远是字符串（向后兼容 UNIX 语义）；
//! 可选 `:i64` / `:f64` / `:str` 注解声明类型。`print`/`println` 内的 `$ident`
//! 按"带类型变量引用"解析（求值器直接查本表），故 `+` 能按操作数类型在
//! "数值加 / 字符串拼接"间重载；命令行（echo/命令参数）的 `$VAR` 仍是文本形态替换。

use crate::num::{f64_to_le, f64_to_text, i64_to_dec, i64_to_le, parse_f64, parse_i64};
use crate::util::out;

pub(crate) const MAX_ENV: usize = 32;
pub(crate) const ENV_NAME: usize = 24;
pub(crate) const ENV_VAL: usize = 64;

/// 变量类型标签（与存储布局一致）。
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnvTy {
    Str = 0,
    I64 = 1,
    F64 = 2,
}

/// 环境变量表：`(name, value-bytes, ty, used)`。
/// `Str` 的 value 为 UTF-8 原文；`I64`/`F64` 的 value 为 8 字节小端二进制。
/// shell 单进程单线程裸机程序，用静态数组（无 alloc）。
static mut ENV_TABLE: [([u8; ENV_NAME], [u8; ENV_VAL], u8, bool); MAX_ENV] =
    [([0u8; ENV_NAME], [0u8; ENV_VAL], 0, false); MAX_ENV];
static mut ENV_COUNT: usize = 0;

/// 按名字查找变量，返回 `(ty, 存储字节切片)`。
pub(crate) fn env_get_kind(name: &[u8]) -> Option<(EnvTy, &'static [u8])> {
    unsafe {
        for i in 0..ENV_COUNT {
            let (n, v, ty, used) = &ENV_TABLE[i];
            if *used && n[..name.len()] == *name && n[name.len()..].iter().all(|&b| b == 0) {
                let mut len = 0;
                while len < ENV_VAL && v[len] != 0 {
                    len += 1;
                }
                let t = match *ty {
                    1 => EnvTy::I64,
                    2 => EnvTy::F64,
                    _ => EnvTy::Str,
                };
                return Some((t, &v[..len]));
            }
        }
    }
    None
}

/// 设置/覆盖变量（带类型）。`val` 语义随 `ty`：Str 原样存 UTF-8；I64/F64 此处
/// `val` 已是 8 字节小端二进制（由调用方解析好）。
pub(crate) fn env_set_typed(name: &[u8], ty: EnvTy, val: &[u8]) {
    if name.is_empty() || name.len() > ENV_NAME {
        return;
    }
    let vlen = val.len().min(ENV_VAL);
    unsafe {
        for i in 0..ENV_COUNT {
            let (n, v, vt, used) = &mut ENV_TABLE[i];
            if *used && n[..name.len()] == *name && n[name.len()..].iter().all(|&b| b == 0) {
                v[..vlen].copy_from_slice(&val[..vlen]);
                for x in &mut v[vlen..] {
                    *x = 0;
                }
                *vt = ty as u8;
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
        ENV_TABLE[i].1[..vlen].copy_from_slice(&val[..vlen]);
        for x in &mut ENV_TABLE[i].1[vlen..] {
            *x = 0;
        }
        ENV_TABLE[i].2 = ty as u8;
        ENV_TABLE[i].3 = true;
        ENV_COUNT += 1;
    }
}

/// 把变量渲染成文本形态写入 `dst`，返回有效长度。用于命令行 `$VAR` 文本替换；
/// 缺失则写入空（替换为空串）。Str→原文；I64→十进制；F64→十进制（≤6 位小数）。
pub(crate) fn env_render(name: &[u8], dst: &mut [u8]) -> usize {
    let mut o = 0usize;
    if let Some((ty, raw)) = env_get_kind(name) {
        match ty {
            EnvTy::Str => {
                for &b in raw {
                    if o < dst.len() {
                        dst[o] = b;
                        o += 1;
                    }
                }
            }
            EnvTy::I64 => {
                if raw.len() >= 8 {
                    let mut b = [0u8; 8];
                    b.copy_from_slice(&raw[..8]);
                    let v = i64::from_le_bytes(b);
                    let mut buf = [0u8; 24];
                    let s = i64_to_dec(v, &mut buf);
                    for &c in s {
                        if o < dst.len() {
                            dst[o] = c;
                            o += 1;
                        }
                    }
                }
            }
            EnvTy::F64 => {
                if raw.len() >= 8 {
                    let mut b = [0u8; 8];
                    b.copy_from_slice(&raw[..8]);
                    let v = f64::from_le_bytes(b);
                    let mut buf = [0u8; 32];
                    let s = f64_to_text(v, &mut buf);
                    for &c in s {
                        if o < dst.len() {
                            dst[o] = c;
                            o += 1;
                        }
                    }
                }
            }
        }
    }
    o
}

/// `env`：列出全部环境变量（`name[:TYPE]=value`；非字符串类型显示类型标注）。
pub(crate) fn cmd_env() {
    unsafe {
        for i in 0..ENV_COUNT {
            let (n, _v, ty, used) = &ENV_TABLE[i];
            if !*used {
                continue;
            }
            let mut nl = 0;
            while nl < ENV_NAME && n[nl] != 0 {
                nl += 1;
            }
            out(&n[..nl]);
            match *ty {
                1 => out(b":i64"),
                2 => out(b":f64"),
                _ => {}
            }
            out(b"=");
            let mut buf = [0u8; 80];
            let o = env_render(&n[..nl], &mut buf);
            out(&buf[..o]);
            out(b"\n");
        }
    }
}

/// `export [NAME[:TYPE]=VALUE]`：设置变量。`TYPE` 可为 `i64` / `f64` / `str`
/// （缺省 `str`，永远字符串，向后兼容）。`VALUE` 两端若带引号则剥除。
/// `i64`/`f64` 要求值为合法数字，否则报错；未知类型名回退为字符串。
pub(crate) fn cmd_export(arg: &[u8]) {
    let a = crate::util::trim_bytes(arg);
    if let Some(eq) = a.iter().position(|&c| c == b'=') {
        // 在 `=` 之前解析可选 `:TYPE`。
        let head = &a[..eq];
        let mut name = head;
        let mut ty = EnvTy::Str;
        if let Some(colon) = head.iter().position(|&c| c == b':') {
            let ts = &head[colon + 1..];
            if ts == b"i64" {
                ty = EnvTy::I64;
            } else if ts == b"f64" {
                ty = EnvTy::F64;
            } else if ts == b"str" {
                ty = EnvTy::Str;
            } else {
                name = head; // 未知类型：整体当名字（无注解）
            }
        }
        let mut val = &a[eq + 1..];
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
        // 按类型解析并存储。
        match ty {
            EnvTy::Str => {
                let mut vbuf = [0u8; ENV_VAL];
                let l = val.len().min(ENV_VAL);
                vbuf[..l].copy_from_slice(&val[..l]);
                env_set_typed(name, EnvTy::Str, &vbuf[..l]);
            }
            EnvTy::I64 => match parse_i64(val) {
                Some(v) => {
                    let mut vbuf = [0u8; ENV_VAL];
                    let l = i64_to_le(v, &mut vbuf);
                    env_set_typed(name, EnvTy::I64, &vbuf[..l]);
                }
                None => {
                    out(b"export: value not i64\n");
                    return;
                }
            },
            EnvTy::F64 => match parse_f64(val) {
                Some(v) => {
                    let mut vbuf = [0u8; ENV_VAL];
                    let l = f64_to_le(v, &mut vbuf);
                    env_set_typed(name, EnvTy::F64, &vbuf[..l]);
                }
                None => {
                    out(b"export: value not f64\n");
                    return;
                }
            },
        }
    } else {
        out(b"export: usage: export [NAME[:TYPE]=VALUE]\n");
    }
}
