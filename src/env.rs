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

/// 读取变量值（不存在返回 `None`），返回**独立副本**。
///
/// 为什么要副本：调用方常需要越过锁使用它（PATH 查找要按 `:` 切分并逐个拼
/// 路径），而 `ENV_TABLE` 是 spin 锁、**不可重入**——返回借用会把锁的生命周期
/// 绑到调用方的整个查找过程上。
pub(crate) fn env_get(name: &[u8]) -> Option<Vec<u8>> {
    let tbl = ENV_TABLE.lock();
    tbl.iter()
        .find(|e| e.name.as_slice() == name)
        .map(|e| e.val.clone())
}

/// 确保 `PATH` 有值：**仅在尚未定义时**写入内置默认。
///
/// 默认值由 `libsys::search_path()` 在**运行期**算出：
/// `PROGRAM_DIR` + 每个**已挂载卷**上真实存在的 `3p` 子目录。
///
/// **为什么不是编译期常量**：卷标、有几块盘、`3p` 在哪块盘上都是**部署事实**。
/// 烧死成 `/volumes/<某个卷标>/3p` 等于假定用户用的就是作者那块盘——换卷标、
/// 没插第二块盘、`3p` 在系统盘上时，该路径**不存在且不会报错**。
///
/// **为什么必须有内置默认**：3P6-1 的验收要在 shell 里直接敲 `tcc`。若没有默认值，
/// 首次登录的 shell 里根本没有 `PATH`，用户必须先 `export PATH=...` 才能用——
/// 那是把「系统该怎么找到程序」这件系统的事推给每个用户。
///
/// **不覆盖**已存在的值（3P4-2 的 envp 可能已带来 PATH，用户也可能自己 export）。
/// **只在 shell 启动时调用一次**，故 `unset PATH` 之后不会被悄悄恢复——
/// POSIX 下未设 PATH 就是「没有可搜索目录，只有内建可用」，如实报错，
/// 而不是假装用户没 unset 过（S09：不把失败伪装成成功）。
pub(crate) fn init_path_default() {
    if env_get(b"PATH").is_none() {
        env_set(b"PATH", libsys::search_path().as_bytes());
    }
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

