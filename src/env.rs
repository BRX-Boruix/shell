//! 环境变量表（纯字符串，UNIX 风格）与 `export`/`env` 命令。

extern crate alloc;

use alloc::vec::Vec;
use spin::Mutex;
use crate::util::{out, trim_bytes, u64_to_dec};

/// 单个环境变量条目。
struct EnvEntry {
    name: Vec<u8>,
    val: Vec<u8>,
}

static ENV_TABLE: Mutex<Vec<EnvEntry>> = Mutex::new(Vec::new());

/// 上一条命令的退出码（`$?` 的来源）。0=成功；非 0=失败。
static LAST_STATUS: spin::Mutex<u8> = spin::Mutex::new(0);

/// 读取上一条命令的退出码。
pub(crate) fn last_status() -> u8 {
    *LAST_STATUS.lock()
}

/// 设置上一条命令的退出码（由 `exec_line` 在命令分发后写入）。
pub(crate) fn set_last_status(s: u8) {
    *LAST_STATUS.lock() = s;
}

/// 设置/覆盖变量（纯字符串，动态扩容无长度/数量限制）。
pub(crate) fn env_set(name: &[u8], val: &[u8]) {
    if name.is_empty() {
        return;
    }
    let mut tbl = ENV_TABLE.lock();
    for entry in tbl.iter_mut() {
        if entry.name.as_slice() == name {
            entry.val.clear();
            entry.val.extend_from_slice(val);
            return;
        }
    }
    tbl.push(EnvEntry {
        name: name.to_vec(),
        val: val.to_vec(),
    });
}

/// 删除变量。
pub(crate) fn env_unset(name: &[u8]) {
    if name.is_empty() {
        return;
    }
    let mut tbl = ENV_TABLE.lock();
    if let Some(pos) = tbl.iter().position(|e| e.name.as_slice() == name) {
        tbl.remove(pos);
    }
}

/// 把变量渲染成文本形态写入 `dst`，返回有效长度。用于命令行 `$VAR` 文本替换；
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
    let tbl = ENV_TABLE.lock();
    if let Some(entry) = tbl.iter().find(|e| e.name.as_slice() == name) {
        for &b in &entry.val {
            if o < dst.len() {
                dst[o] = b;
                o += 1;
            }
        }
    }
    o
}

/// 遍历所有已定义变量名（供 Tab 补全）。对每个名字调用 `f`。
pub(crate) fn for_each_env_name<F: FnMut(&[u8])>(mut f: F) {
    let tbl = ENV_TABLE.lock();
    for entry in tbl.iter() {
        f(entry.name.as_slice());
    }
}

/// `env`：列出全部环境变量（`name=value`）。返回 0。
pub(crate) fn cmd_env() -> u8 {
    let tbl = ENV_TABLE.lock();
    for entry in tbl.iter() {
        out(&entry.name);
        out(b"=");
        out(&entry.val);
        out(b"\n");
    }
    0
}

/// `export [NAME[=VALUE]]`：查看或设置环境变量。
pub(crate) fn cmd_export(arg: &[u8]) -> u8 {
    let s = trim_bytes(arg);
    if s.is_empty() {
        return cmd_env();
    }
    let eq = s.iter().position(|&b| b == b'=');
    let (name, val) = match eq {
        Some(pos) => (&s[..pos], &s[pos + 1..]),
        None => (s, &b""[..]),
    };
    let name = trim_bytes(name);
    let val = trim_bytes(val);
    if name.is_empty() {
        out(b"export: invalid syntax\n");
        return 1;
    }
    env_set(name, val);
    0
}

/// `unset NAME`：删除环境变量。
pub(crate) fn cmd_unset(arg: &[u8]) -> u8 {
    let s = trim_bytes(arg);
    if s.is_empty() {
        out(b"unset: missing variable name\n");
        return 1;
    }
    env_unset(s);
    0
}
