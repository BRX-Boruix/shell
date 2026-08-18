//! 环境变量表（纯字符串，UNIX 风格）与 `export`/`env` 命令。
//!
//! `export NAME=VALUE` 永远是字符串（与 POSIX shell 一致）。命令行中的 `$VAR`
//! 按文本形态替换（`tokenize` 经 `env_render` 取值）。不接受任何类型注解。

use crate::util::{out, trim_bytes, u64_to_dec};

pub(crate) const MAX_ENV: usize = 32;
pub(crate) const ENV_NAME: usize = 24;
pub(crate) const ENV_VAL: usize = 64;

/// 环境变量表：`(name, value, used)`。value 为 UTF-8 原文。
/// shell 单进程单线程裸机程序，用静态数组（无 alloc）。
static mut ENV_TABLE: [([u8; ENV_NAME], [u8; ENV_VAL], bool); MAX_ENV] =
    [([0u8; ENV_NAME], [0u8; ENV_VAL], false); MAX_ENV];
static mut ENV_COUNT: usize = 0;

/// 上一条命令的退出码（`$?` 的来源）。0=成功；非 0=失败。
static mut LAST_STATUS: u8 = 0;

/// 读取上一条命令的退出码。
pub(crate) fn last_status() -> u8 {
    unsafe { LAST_STATUS }
}

/// 设置上一条命令的退出码（由 `exec_line` 在命令分发后写入）。
pub(crate) fn set_last_status(s: u8) {
    unsafe {
        LAST_STATUS = s;
    }
}

/// 按名字查找变量，返回存储的字节切片（缺失返回 `None`）。
pub(crate) fn env_get(name: &[u8]) -> Option<&'static [u8]> {
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

/// 设置/覆盖变量（纯字符串）。
pub(crate) fn env_set(name: &[u8], val: &[u8]) {
    if name.is_empty() || name.len() > ENV_NAME {
        return;
    }
    let vlen = val.len().min(ENV_VAL);
    unsafe {
        for i in 0..ENV_COUNT {
            let (n, v, used) = &mut ENV_TABLE[i];
            if *used && n[..name.len()] == *name && n[name.len()..].iter().all(|&b| b == 0) {
                v[..vlen].copy_from_slice(&val[..vlen]);
                for x in &mut v[vlen..] {
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
        ENV_TABLE[i].1[..vlen].copy_from_slice(&val[..vlen]);
        for x in &mut ENV_TABLE[i].1[vlen..] {
            *x = 0;
        }
        ENV_TABLE[i].2 = true;
        ENV_COUNT += 1;
    }
}

/// 删除变量（软删除：标记 `used=false`）。缺失的名字静默忽略。
/// 删除后该名字从 `env`/`$VAR` 中消失；再次 `export` 同名会新建一条记录
/// （旧槽位保留为未使用，受 `MAX_ENV` 上限约束，对本 shell 足够）。
pub(crate) fn env_unset(name: &[u8]) {
    if name.is_empty() || name.len() > ENV_NAME {
        return;
    }
    unsafe {
        for i in 0..ENV_COUNT {
            let (n, _v, used) = &mut ENV_TABLE[i];
            if *used && n[..name.len()] == *name && n[name.len()..].iter().all(|&b| b == 0) {
                *used = false;
                return;
            }
        }
    }
}

/// 把变量渲染成文本形态写入 `dst`，返回有效长度。用于命令行 `$VAR` 文本替换；
/// 缺失则写入空（替换为空串）。`$?` 为特殊伪变量，渲染为上条命令退出码。
pub(crate) fn env_render(name: &[u8], dst: &mut [u8]) -> usize {
    let mut o = 0usize;
    if name == b"?" {
        let mut b = [0u8; 24];
        let s = u64_to_dec(last_status() as u64, &mut b);
        for &c in s {
            if o < dst.len() {
                dst[o] = c;
                o += 1;
            }
        }
        return o;
    }
    if let Some(raw) = env_get(name) {
        for &b in raw {
            if o < dst.len() {
                dst[o] = b;
                o += 1;
            }
        }
    }
    o
}

/// 遍历所有已定义变量名（供 Tab 补全）。对每个名字调用 `f`。
/// 名字切片来自 `static` 表，生命周期为 `'static`，便于调用方暂存引用。
pub(crate) fn for_each_env_name<F: FnMut(&'static [u8])>(mut f: F) {
    unsafe {
        for i in 0..ENV_COUNT {
            let (n, _v, used) = &ENV_TABLE[i];
            if !*used {
                continue;
            }
            let mut len = 0;
            while len < ENV_NAME && n[len] != 0 {
                len += 1;
            }
            f(&n[..len]);
        }
    }
}

/// `env`：列出全部环境变量（`name=value`）。返回 0。
pub(crate) fn cmd_env() -> u8 {
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
    0
}

/// `export [NAME=VALUE]`：设置字符串变量。`VALUE` 两端若带引号则剥除。
/// 不识别任何类型注解（纯字符串，UNIX 风格）。返回：0 成功，1 用法错误。
pub(crate) fn cmd_export(arg: &[u8]) -> u8 {
    let a = trim_bytes(arg);
    if let Some(eq) = a.iter().position(|&c| c == b'=') {
        let name = &a[..eq];
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
            return 1;
        }
        env_set(name, val);
        0
    } else {
        out(b"export: usage: export [NAME=VALUE]\n");
        1
    }
}
