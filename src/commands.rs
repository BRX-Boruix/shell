//! 内建命令实现与命令分发。
//!
//! 各 `cmd_*` 对应一条 shell 内建命令；`exec_echo` 处理 `echo`；
//! `exec_line` 负责分词、参数重建并把命令名分派到对应实现。
//! 支持基础文件操作：`ls`, `cat`, `mkdir`, `touch`, `rm` 以及 `--json` 输出模式。

/// 所有内建命令名（供 Tab 补全使用）。
pub(crate) const COMMANDS: &[&[u8]] = &[
    b"echo", b"help", b"now", b"time", b"uptime", b"version", b"uname", b"cpu", b"sleep",
    b"clear", b"env", b"export", b"unset", b"ps", b"kill", b"signal", b"alias", b"unalias",
    b"which", b"jobs", b"ls", b"cat", b"mkdir", b"touch", b"rm", b"tree", b"jtree", b"cd",
    b"pwd",
];

/// 返回内建命令名列表（供补全遍历）。
pub(crate) fn command_names() -> &'static [&'static [u8]] {
    COMMANDS
}

extern crate alloc;

use alloc::string::ToString;
use alloc::vec::Vec;
use spin::Mutex;

struct AliasEntry {
    name: Vec<u8>,
    val: Vec<u8>,
}

static ALIAS_TABLE: Mutex<Vec<AliasEntry>> = Mutex::new(Vec::new());

/// 查别名，返回复制的值（缺失 `None`）。
pub(crate) fn alias_get(name: &[u8]) -> Option<Vec<u8>> {
    let tbl = ALIAS_TABLE.lock();
    for entry in tbl.iter() {
        if entry.name.as_slice() == name {
            return Some(entry.val.clone());
        }
    }
    None
}

/// 设置/覆盖别名。
pub(crate) fn alias_set(name: &[u8], val: &[u8]) {
    if name.is_empty() {
        return;
    }
    let mut tbl = ALIAS_TABLE.lock();
    for entry in tbl.iter_mut() {
        if entry.name.as_slice() == name {
            entry.val.clear();
            entry.val.extend_from_slice(val);
            return;
        }
    }
    tbl.push(AliasEntry {
        name: name.to_vec(),
        val: val.to_vec(),
    });
}

/// 删除别名。
pub(crate) fn alias_unset(name: &[u8]) {
    if name.is_empty() {
        return;
    }
    let mut tbl = ALIAS_TABLE.lock();
    if let Some(pos) = tbl.iter().position(|e| e.name.as_slice() == name) {
        tbl.remove(pos);
    }
}

/// 遍历所有已定义别名（供 `alias` 无参列出）。
pub(crate) fn for_each_alias<F: FnMut(&[u8], &[u8])>(mut f: F) {
    let tbl = ALIAS_TABLE.lock();
    for entry in tbl.iter() {
        f(entry.name.as_slice(), entry.val.as_slice());
    }
}

use crate::env::{cmd_env, cmd_export, env_unset, set_last_status};
use crate::tokenize::tokenize_line;
use crate::util::{out, outln, pad2, parse_u64, string_content, trim_bytes, u64_to_dec, unescape};

/// 后台作业条目。
struct JobEntry {
    pid: u32,
    cmd: Vec<u8>,
    active: bool,
}

static JOBS: Mutex<Vec<JobEntry>> = Mutex::new(Vec::new());

/// 登记一个后台作业，返回作业号（1 基，供 `%n` 引用）。
pub(crate) fn job_add(pid: u32, cmd: &[u8]) -> usize {
    let mut jobs = JOBS.lock();
    jobs.push(JobEntry {
        pid,
        cmd: cmd.to_vec(),
        active: true,
    });
    jobs.len()
}

/// 按作业号（1 基）取 pid；越界或空槽返回 `None`。
fn job_pid(idx: usize) -> Option<u32> {
    if idx == 0 {
        return None;
    }
    let jobs = JOBS.lock();
    if idx <= jobs.len() && jobs[idx - 1].active {
        Some(jobs[idx - 1].pid)
    } else {
        None
    }
}

/// 删除作业（如 `kill %n` 后）。
fn job_remove(idx: usize) {
    if idx == 0 {
        return;
    }
    let mut jobs = JOBS.lock();
    if idx <= jobs.len() {
        jobs[idx - 1].active = false;
    }
}

/// `jobs`：列出全部后台作业及其存活状态。支持 `--json` 格式。返回 0。
fn cmd_jobs(arg: &[u8]) -> u8 {
    let is_json = trim_bytes(arg) == b"--json";
    let alive = libsys::ps_list().unwrap_or_default();
    let jobs = JOBS.lock();

    if is_json {
        use libsys::json::{JsonWriter, VecTarget};
        let mut target = VecTarget::new();
        let mut writer = JsonWriter::new(&mut target);
        if let Ok(mut arr) = writer.start_array() {
            for (i, job) in jobs.iter().enumerate() {
                if !job.active {
                    continue;
                }
                let live = alive.iter().any(|p| p.pid == job.pid);
                let state_str = if live { "Running" } else { "Done" };
                let cmd_str = core::str::from_utf8(&job.cmd).unwrap_or("");
                let _ = arr.push_object(|obj| {
                    let _ = obj.field_u64("job", (i + 1) as u64);
                    let _ = obj.field_u64("pid", job.pid as u64);
                    let _ = obj.field_str("state", state_str);
                    let _ = obj.field_str("command", cmd_str);
                    Ok(())
                });
            }
            let _ = arr.end();
        }
        let mut b = target.into_bytes();
        b.push(b'\n');
        out(&b);
        return 0;
    }

    let mut b = [0u8; 24];
    for (i, job) in jobs.iter().enumerate() {
        if !job.active {
            continue;
        }
        let live = alive.iter().any(|p| p.pid == job.pid);
        out(b"[");
        out(u64_to_dec((i + 1) as u64, &mut b));
        out(b"] ");
        out(u64_to_dec(job.pid as u64, &mut b));
        if live {
            out(b" Running    ");
        } else {
            out(b" Done       ");
        }
        out(&job.cmd);
        out(b"\n");
    }
    0
}

use libsys::nr::{INFO_BOOT_MS, INFO_CPU_COUNT, INFO_VERSION};
use libsys::{
    chdir, close, getcwd, info, kill, mkdir, now, open, ps, read_dir, read_to_end,
    read_wall_clock, sleep, unlink, OpenFlags, Permissions, PsEntry,
};
use libsys::signal::LIST;

/// `help`：列出全部内建命令（每条一行，英文说明）。
///
/// 对齐到固定列宽；多数状态命令支持 `--json` 结构化输出。
fn cmd_help() -> u8 {
    const COL: usize = 12; // 命令名左对齐列宽（含 2 空格缩进）
    const ITEMS: &[(&[u8], &[u8])] = &[
        (b"echo", b"print a line of text"),
        (b"help", b"list all builtin commands"),
        (b"now", b"monotonic clock in ns since boot"),
        (b"time", b"wall clock (date/time from CMOS RTC)"),
        (b"uptime", b"time since boot"),
        (b"version", b"show kernel version"),
        (b"uname", b"show kernel version"),
        (b"cpu", b"show number of online CPUs"),
        (b"sleep", b"sleep for N seconds"),
        (b"clear", b"clear the terminal screen"),
        (b"env", b"list environment variables"),
        (b"export", b"set an environment variable"),
        (b"unset", b"unset environment variable(s)"),
        (b"ps", b"list running processes"),
        (b"kill", b"send a signal to a process"),
        (b"signal", b"list available signals"),
        (b"alias", b"define or list aliases"),
        (b"unalias", b"remove alias(es)"),
        (b"which", b"locate a builtin/alias command"),
        (b"jobs", b"list background jobs"),
        (b"ls", b"list directory contents"),
        (b"cat", b"print file contents"),
        (b"mkdir", b"create a directory"),
        (b"touch", b"create or update a file"),
        (b"rm", b"remove a file"),
        (b"tree", b"visualize VFS directory tree"),
        (b"jtree", b"visualize a JSON value as a tree"),
        (b"cd", b"change the working directory"),
        (b"pwd", b"print the working directory"),
    ];
    out(b"boruix shell builtins:\n");
    for (c, d) in ITEMS {
        out(b"  ");
        out(c);
        for _ in c.len()..COL {
            out(b" ");
        }
        out(d);
        out(b"\n");
    }
    out(b"\nnote: many commands accept --json for structured output\n");
    0
}

/// `now`：单调时钟（纳秒，自开机以来的近似计数）。
fn cmd_now() -> u8 {
    let ns = now();
    let mut b = [0u8; 24];
    out(b"now: ");
    out(u64_to_dec(ns, &mut b));
    out(b" ns (");
    out(u64_to_dec(ns / 1_000_000_000, &mut b));
    out(b".");
    out(u64_to_dec((ns % 1_000_000_000) / 1_000_000, &mut b));
    out(b" s)\n");
    0
}

/// `time`：墙钟时间（真实年月日时分秒，来自 CMOS/BIOS 硬件时钟）。
///
/// 文本输出格式 `YYYY-MM-DD HH:MM:SS`；支持 `--json` 输出结构化字段。
fn cmd_time(arg: &[u8]) -> u8 {
    match read_wall_clock() {
        Ok(wc) => {
            if trim_bytes(arg) == b"--json" {
                use libsys::json::{JsonWriter, VecTarget};
                let mut target = VecTarget::new();
                let mut writer = JsonWriter::new(&mut target);
                if let Ok(mut o) = writer.start_object() {
                    let _ = o.field_u64("year", wc.year);
                    let _ = o.field_u64("month", wc.month);
                    let _ = o.field_u64("day", wc.day);
                    let _ = o.field_u64("hour", wc.hour);
                    let _ = o.field_u64("minute", wc.minute);
                    let _ = o.field_u64("second", wc.second);
                    let _ = o.end();
                }
                let mut b = target.into_bytes();
                b.push(b'\n');
                out(&b);
            } else {
                // YYYY-MM-DD HH:MM:SS
                let mut b = [0u8; 24];
                out(u64_to_dec(wc.year, &mut b));
                out(b"-");
                out(pad2(wc.month, &mut b));
                out(b"-");
                out(pad2(wc.day, &mut b));
                out(b" ");
                out(pad2(wc.hour, &mut b));
                out(b":");
                out(pad2(wc.minute, &mut b));
                out(b":");
                out(pad2(wc.second, &mut b));
                out(b"\n");
            }
            0
        }
        Err(_) => {
            out(b"time: unable to read wall clock from /system/info/time\n");
            1
        }
    }
}

/// `uptime`：开机至今。支持 `--json` 输出。
fn cmd_uptime(arg: &[u8]) -> u8 {
    let ms = info(INFO_BOOT_MS).unwrap_or(0);
    if trim_bytes(arg) == b"--json" {
        use libsys::json::{JsonWriter, VecTarget};
        let mut target = VecTarget::new();
        let mut writer = JsonWriter::new(&mut target);
        if let Ok(mut obj) = writer.start_object() {
            let _ = obj.field_u64("uptime_ms", ms);
            let _ = obj.field_u64("uptime_seconds", ms / 1000);
            let _ = obj.end();
        }
        let mut b = target.into_bytes();
        b.push(b'\n');
        out(&b);
        return 0;
    }

    let mut b = [0u8; 24];
    out(b"uptime: ");
    out(u64_to_dec(ms / 1000, &mut b));
    out(b".");
    out(u64_to_dec(ms % 1000, &mut b));
    out(b" s\n");
    0
}

/// `version`/`uname`：内核版本。
fn cmd_version() -> u8 {
    let v = info(INFO_VERSION).unwrap_or(0);
    let mut b = [0u8; 24];
    out(b"BORUIX v");
    out(u64_to_dec((v >> 16) & 0xff, &mut b));
    out(b".");
    out(u64_to_dec((v >> 8) & 0xff, &mut b));
    out(b".");
    out(u64_to_dec(v & 0xff, &mut b));
    out(b"\n");
    0
}

/// `cpu`：在线 CPU 数。
fn cmd_cpu() -> u8 {
    let c = info(INFO_CPU_COUNT).unwrap_or(0);
    let mut b = [0u8; 24];
    out(b"cpus: ");
    out(u64_to_dec(c, &mut b));
    out(b"\n");
    0
}

/// `sleep <秒>`：睡眠。返回：0 成功，1 参数错误。
fn cmd_sleep(arg: &[u8]) -> u8 {
    let a = trim_bytes(arg);
    match parse_u64(a) {
        Some(secs) => {
            let _ = sleep(secs * 1_000_000_000);
            0
        }
        None => {
            out(b"sleep: usage: sleep <seconds>\n");
            1
        }
    }
}

/// `ps`：列出存活进程。支持 `--json` 输出。
fn cmd_ps(arg: &[u8]) -> u8 {
    let is_json = trim_bytes(arg) == b"--json";
    if is_json {
        // 直接从 ProcFS 读取 JSON 数组输出
        if let Ok(bytes) = read_to_end("/processes/list") {
            out(&bytes);
            return 0;
        }
    }

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
            0
        }
        Err(_) => {
            out(b"ps: failed\n");
            1
        }
    }
}

/// `signal`：打印信号清单。
fn cmd_signal_list() -> u8 {
    let mut b = [0u8; 24];
    for (num, name) in LIST {
        out(u64_to_dec(*num as u64, &mut b));
        out(b") SIG");
        out(name.as_bytes());
        out(b"\n");
    }
    0
}

/// `kill [-s SIG|-SIG|-l] <pid|%job>`：发送信号。
fn cmd_kill(arg: &[u8]) -> u8 {
    let a = trim_bytes(arg);
    let mut sig: u64 = 15;
    let mut pid: u64 = 0;
    let mut have_pid = false;
    for tok in a.split(|&c| c.is_ascii_whitespace()).filter(|s| !s.is_empty()) {
        if tok.starts_with(b"%") {
            let num = &tok[1..];
            match parse_u64(num) {
                Some(idx) => match job_pid(idx as usize) {
                    Some(p) => {
                        pid = p as u64;
                        have_pid = true;
                        job_remove(idx as usize);
                    }
                    None => {
                        out(b"kill: no such job: ");
                        out(tok);
                        out(b"\n");
                        return 1;
                    }
                },
                None => {
                    out(b"kill: bad job spec: ");
                    out(tok);
                    out(b"\n");
                    return 1;
                }
            }
            continue;
        }
        if tok.starts_with(b"-") {
            let body = &tok[1..];
            if body == b"l" {
                cmd_signal_list();
                return 0;
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
        return 127;
    }
    match kill(pid, sig) {
        Ok(_) => 0,
        Err(e) => {
            let mut b = [0u8; 24];
            out(b"kill: failed (errno ");
            out(u64_to_dec(e.to_errno() as u64, &mut b));
            out(b")\n");
            1
        }
    }
}

/// 执行 `echo <文本>`：输出一行。
fn exec_echo(arg: &[u8]) -> u8 {
    if let Some(content) = string_content(arg) {
        let mut buf = [0u8; 256];
        let n = unescape(content, &mut buf);
        outln(&buf[..n]);
    } else {
        outln(trim_bytes(arg));
    }
    0
}

/// `unset NAME...`：删除变量。
fn cmd_unset(arg: &[u8]) -> u8 {
    let a = trim_bytes(arg);
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
        env_unset(&a[start..i]);
    }
    0
}

/// `alias` / `alias NAME=VALUE`：别名支持。
fn cmd_alias(arg: &[u8]) -> u8 {
    let a = trim_bytes(arg);
    if a.is_empty() {
        for_each_alias(|n, v| {
            out(n);
            out(b"='");
            out(&v);
            out(b"'\n");
        });
        return 0;
    }
    if let Some(eq) = a.iter().position(|&c| c == b'=') {
        let name = &a[..eq];
        let mut val = &a[eq + 1..];
        if val.len() >= 2 {
            let f = val.first().copied().unwrap();
            let l = val.last().copied().unwrap();
            if (f == b'"' && l == b'"') || (f == b'\'' && l == b'\'') {
                val = &val[1..val.len() - 1];
            }
        }
        if name.is_empty() {
            out(b"alias: empty name\n");
            return 1;
        }
        alias_set(name, val);
        0
    } else {
        match alias_get(a) {
            Some(v) => {
                out(a);
                out(b"='");
                out(&v);
                out(b"'\n");
            }
            None => {
                out(b"alias: ");
                out(a);
                out(b": not found\n");
            }
        }
        0
    }
}

/// `unalias NAME...`：删除别名。
fn cmd_unalias(arg: &[u8]) -> u8 {
    let a = trim_bytes(arg);
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
        alias_unset(&a[start..i]);
    }
    0
}

/// `which NAME`：查找命令类型。
fn cmd_which(arg: &[u8]) -> u8 {
    let name = trim_bytes(arg);
    if name.is_empty() {
        out(b"which: missing argument\n");
        return 1;
    }
    if alias_get(name).is_some() {
        out(name);
        out(b": aliased to `");
        if let Some(v) = alias_get(name) {
            out(&v);
        }
        out(b"`\n");
        return 0;
    }
    for &b in COMMANDS {
        if b == name {
            out(name);
            out(b": shell built-in command\n");
            return 0;
        }
    }
    out(name);
    out(b": not found\n");
    1
}

// ==================== M6.5 文件系统内建命令 ====================

/// `ls [-l] [--json] [path]`：列出目录项（带颜色高亮与类型区分标识）。
///
/// 特殊显示规则：
/// - **目录（Directory）**：蓝粗体 `\x1b[1;34m` + 尾部 `/`（如 `binaries/`）
/// - **可执行文件（.elf）**：亮绿体 `\x1b[1;32m` + 尾部 `*`（如 `shell.elf*`）
/// - **符号链接（Symlink）**：青色 `\x1b[1;36m` + 尾部 `@`
/// - **设备/特殊节点（Device）**：黄色 `\x1b[1;33m` + 尾部 `%`
/// - **普通文本/数据文件**：默认白色
/// - 支持 `ls -l` 详细长列表模式（显示类型、字节大小、名称）
/// - 支持 `ls --json` 结构化输出
fn cmd_ls(arg: &[u8]) -> u8 {
    let a = trim_bytes(arg);
    let mut long_mode = false;
    let mut json_mode = false;
    let mut path = getcwd().unwrap_or_else(|_| alloc::string::String::from("/"));

    for tok in a.split(|&c| c.is_ascii_whitespace()).filter(|s| !s.is_empty()) {
        if tok == b"-l" {
            long_mode = true;
        } else if tok == b"--json" {
            json_mode = true;
        } else if !tok.starts_with(b"-") {
            if let Ok(p) = core::str::from_utf8(tok) {
                path = alloc::string::String::from(p);
            }
        }
    }

    match read_dir(&path) {
        Ok(entries) => {
            if json_mode {
                use libsys::json::{JsonWriter, VecTarget};
                let mut target = VecTarget::new();
                let mut writer = JsonWriter::new(&mut target);
                if let Ok(mut arr) = writer.start_array() {
                    for entry in &entries {
                        let _ = arr.push_object(|obj| {
                            let _ = obj.field_str("name", &entry.name);
                            let _ = obj.field_str("type", &entry.node_type);
                            let _ = obj.field_u64("size", entry.size);
                            Ok(())
                        });
                    }
                    let _ = arr.end();
                }
                let mut b = target.into_bytes();
                b.push(b'\n');
                out(&b);
                return 0;
            }

            if long_mode {
                out(b"TYPE        SIZE   NAME\n");
                let mut b = [0u8; 24];
                for entry in &entries {
                    let (type_badge, color, indicator): (&str, &[u8], &str) = match entry.node_type.as_str() {
                        "dir" | "Directory" => ("<DIR>   ", b"\x1b[1;34m", "/"),
                        "link" | "Symlink" => ("<LNK>   ", b"\x1b[1;36m", "@"),
                        "chardev" | "blkdev" | "Device" => ("<DEV>   ", b"\x1b[1;33m", "%"),
                        _ => {
                            if entry.name.ends_with(".elf") {
                                ("<BIN>   ", b"\x1b[1;32m", "*")
                            } else {
                                ("<FILE>  ", b"\x1b[0m", "")
                            }
                        }
                    };
                    out(type_badge.as_bytes());
                    let size_bytes = u64_to_dec(entry.size, &mut b);
                    let pad = 8usize.saturating_sub(size_bytes.len());
                    for _ in 0..pad {
                        out(b" ");
                    }
                    out(size_bytes);
                    out(b"   ");
                    out(color);
                    out(entry.name.as_bytes());
                    out(indicator.as_bytes());
                    out(b"\x1b[0m\n");
                }
            } else {
                // 简洁彩色网格模式
                for entry in &entries {
                    let (color, indicator): (&[u8], &str) = match entry.node_type.as_str() {
                        "dir" | "Directory" => (b"\x1b[1;34m", "/"),
                        "link" | "Symlink" => (b"\x1b[1;36m", "@"),
                        "chardev" | "blkdev" | "Device" => (b"\x1b[1;33m", "%"),
                        _ => {
                            if entry.name.ends_with(".elf") {
                                (b"\x1b[1;32m", "*")
                            } else {
                                (b"\x1b[0m", "")
                            }
                        }
                    };
                    out(color);
                    out(entry.name.as_bytes());
                    out(indicator.as_bytes());
                    out(b"\x1b[0m  ");
                }
                out(b"\n");
            }
            0
        }
        Err(_) => {
            out(b"ls: cannot access '");
            out(path.as_bytes());
            out(b"': No such file or directory\n");
            1
        }
    }
}

/// `cat <file>`：打印文件内容。
fn cmd_cat(arg: &[u8]) -> u8 {
    let a = trim_bytes(arg);
    if a.is_empty() {
        out(b"cat: missing file operand\n");
        return 1;
    }
    let path = match core::str::from_utf8(a) {
        Ok(p) => p,
        Err(_) => return 1,
    };
    match read_to_end(path) {
        Ok(bytes) => {
            out(&bytes);
            if !bytes.ends_with(b"\n") {
                out(b"\n");
            }
            0
        }
        Err(_) => {
            out(b"cat: ");
            out(a);
            out(b": No such file or directory\n");
            1
        }
    }
}

/// `mkdir <dir>`：创建目录。
fn cmd_mkdir(arg: &[u8]) -> u8 {
    let a = trim_bytes(arg);
    if a.is_empty() {
        out(b"mkdir: missing operand\n");
        return 1;
    }
    let path = match core::str::from_utf8(a) {
        Ok(p) => p,
        Err(_) => return 1,
    };
    match mkdir(path, Permissions::all()) {
        Ok(_) => 0,
        Err(_) => {
            out(b"mkdir: cannot create directory '");
            out(a);
            out(b"'\n");
            1
        }
    }
}

/// `touch <file>`：创建空文件。
fn cmd_touch(arg: &[u8]) -> u8 {
    let a = trim_bytes(arg);
    if a.is_empty() {
        out(b"touch: missing file operand\n");
        return 1;
    }
    let path = match core::str::from_utf8(a) {
        Ok(p) => p,
        Err(_) => return 1,
    };
    match open(path, OpenFlags::CREATE_OR_TRUNCATE, Permissions::all()) {
        Ok(fd) => {
            let _ = close(fd);
            0
        }
        Err(_) => {
            out(b"touch: cannot touch '");
            out(a);
            out(b"'\n");
            1
        }
    }
}

/// `rm <file_or_dir>`：删除文件或空目录。
fn cmd_rm(arg: &[u8]) -> u8 {
    let a = trim_bytes(arg);
    if a.is_empty() {
        out(b"rm: missing operand\n");
        return 1;
    }
    let path = match core::str::from_utf8(a) {
        Ok(p) => p,
        Err(_) => return 1,
    };
    match unlink(path) {
        Ok(_) => 0,
        Err(_) => {
            out(b"rm: cannot remove '");
            out(a);
            out(b"'\n");
            1
        }
    }
}

/// `jtree [--utf8] <path_or_json_string>`：自动树状可视化展示 JSON 结构（默认 ASCII，加 `--utf8` 开启 UTF-8 盒子绘图）。
fn cmd_jtree(arg: &[u8]) -> u8 {
    let a = trim_bytes(arg);
    if a.is_empty() {
        out(b"jtree: usage: jtree [--utf8] <file_path|json_string>\n");
        return 1;
    }

    let mut use_utf8 = false;
    let mut payload = "";

    for tok in a.split(|&c| c.is_ascii_whitespace()).filter(|s| !s.is_empty()) {
        if tok == b"--utf8" {
            use_utf8 = true;
        } else if payload.is_empty() {
            if let Ok(s) = core::str::from_utf8(tok) {
                payload = s;
            }
        }
    }

    // 如果命令行包含空格且是 JSON 字符串，直接提取完整参数文本（去掉 `--utf8`）
    let full_str = core::str::from_utf8(a).unwrap_or("");
    let json_text = if full_str.contains('{') || full_str.contains('[') {
        full_str.replace("--utf8", "").trim().to_string()
    } else {
        payload.to_string()
    };

    if json_text.is_empty() {
        out(b"jtree: usage: jtree [--utf8] <file_path|json_string>\n");
        return 1;
    }

    // 1. 如果是以 '{' 或 '[' 开始，直接作为 JSON 字符串解析
    if json_text.starts_with('{') || json_text.starts_with('[') {
        let mut parser = libsys::json::JsonParser::new(&json_text);
        match parser.parse() {
            Ok(val) => {
                crate::json_tree::print_tree(&val, Some("json"), use_utf8);
                0
            }
            Err(e) => {
                out(b"jtree: json parse error: ");
                out(e.as_bytes());
                out(b"\n");
                1
            }
        }
    } else {
        // 2. 作为 VFS 文件路径读取后解析
        match read_to_end(&json_text) {
            Ok(bytes) => {
                let file_str = match core::str::from_utf8(&bytes) {
                    Ok(s) => s,
                    Err(_) => {
                        out(b"jtree: file content is not valid utf-8\n");
                        return 1;
                    }
                };
                let mut parser = libsys::json::JsonParser::new(file_str);
                match parser.parse() {
                    Ok(val) => {
                        crate::json_tree::print_tree(&val, Some(&json_text), use_utf8);
                        0
                    }
                    Err(e) => {
                        out(b"jtree: json parse error: ");
                        out(e.as_bytes());
                        out(b"\n");
                        1
                    }
                }
            }
            Err(_) => {
                out(b"jtree: cannot read '");
                out(json_text.as_bytes());
                out(b"': No such file or directory\n");
                1
            }
        }
    }
}

/// `cd [path]`：切换当前工作目录（无参回根目录 `/`）。相对路径相对当前
/// cwd 解析（内核 syscall 层拼接）。
fn cmd_cd(arg: &[u8]) -> u8 {
    let a = trim_bytes(arg);
    let mut target = alloc::string::String::from("/");
    for tok in a.split(|&c| c.is_ascii_whitespace()).filter(|s| !s.is_empty()) {
        if let Ok(p) = core::str::from_utf8(tok) {
            target = alloc::string::String::from(p);
        }
    }
    match chdir(&target) {
        Ok(()) => 0,
        Err(_) => {
            out(b"cd: no such directory: ");
            out(target.as_bytes());
            out(b"\n");
            1
        }
    }
}

/// `pwd`：打印当前工作目录（绝对路径）。
fn cmd_pwd() -> u8 {
    match getcwd() {
        Ok(cwd) => {
            out(cwd.as_bytes());
            out(b"\n");
            0
        }
        Err(_) => {
            out(b"pwd: unable to read working directory\n");
            1
        }
    }
}

/// `tree [--utf8] [path]`：递归遍历并树状可视化打印 VFS 目录树骨架（默认 ASCII，加 `--utf8` 开启 UTF-8 盒子绘图）。
fn cmd_tree(arg: &[u8]) -> u8 {
    let a = trim_bytes(arg);
    let mut use_utf8 = false;
    let mut root_path = getcwd().unwrap_or_else(|_| alloc::string::String::from("/"));

    for tok in a.split(|&c| c.is_ascii_whitespace()).filter(|s| !s.is_empty()) {
        if tok == b"--utf8" {
            use_utf8 = true;
        } else if !tok.starts_with(b"-") {
            if let Ok(p) = core::str::from_utf8(tok) {
                root_path = alloc::string::String::from(p);
            }
        }
    }

    out(root_path.as_bytes());
    out(b"\n");
    print_vfs_tree(&root_path, "", use_utf8);
    0
}

fn print_vfs_tree(dir_path: &str, prefix: &str, use_utf8: bool) {
    let entries = match read_dir(dir_path) {
        Ok(e) => e,
        Err(_) => return,
    };
    let total = entries.len();
    for (idx, entry) in entries.iter().enumerate() {
        let is_last = idx + 1 == total;
        let branch = if use_utf8 {
            if is_last { "└── " } else { "├── " }
        } else {
            if is_last { "`-- " } else { "|-- " }
        };
        let next_prefix = if use_utf8 {
            if is_last {
                alloc::format!("{}    ", prefix)
            } else {
                alloc::format!("{}│   ", prefix)
            }
        } else {
            if is_last {
                alloc::format!("{}    ", prefix)
            } else {
                alloc::format!("{}|   ", prefix)
            }
        };

        let is_dir = entry.node_type == "dir" || entry.node_type == "Directory";
        if is_dir {
            out(alloc::format!("{}{}{}/\n", prefix, branch, entry.name).as_bytes());
            let sub_path = if dir_path == "/" {
                alloc::format!("/{}", entry.name)
            } else {
                alloc::format!("{}/{}", dir_path, entry.name)
            };
            print_vfs_tree(&sub_path, &next_prefix, use_utf8);
        } else {
            out(alloc::format!("{}{}{}\n", prefix, branch, entry.name).as_bytes());
        }
    }
}

/// 剥除注释。
fn strip_comment(line: &[u8]) -> &[u8] {
    let mut in_single = false;
    let mut in_double = false;
    let mut escape = false;
    let mut i = 0;
    while i < line.len() {
        let c = line[i];
        if escape {
            escape = false;
            i += 1;
            continue;
        }
        if c == b'\\' && !in_single {
            escape = true;
            i += 1;
            continue;
        }
        if c == b'\'' && !in_double {
            in_single = !in_single;
        } else if c == b'"' && !in_single {
            in_double = !in_double;
        } else if c == b'#' && !in_single && !in_double {
            return &line[..i];
        }
        i += 1;
    }
    line
}

/// 执行一行输入。
pub(crate) fn exec_line(line: &[u8]) {
    let line = trim_bytes(line);
    if line.is_empty() || line.first() == Some(&b'#') {
        return;
    }
    let line = if line.last() == Some(&b';') {
        &line[..line.len() - 1]
    } else {
        line
    };
    let line = strip_comment(line);

    let words = tokenize_line(line);
    if words.is_empty() {
        return;
    }
    let oname = words[0].clone();

    let mut expanded = Vec::new();
    let mut active: &[u8] = line;
    if let Some(av) = alias_get(&oname) {
        let rest = &line[oname.len()..];
        expanded.extend_from_slice(&av);
        expanded.extend_from_slice(rest);
        let words2 = tokenize_line(&expanded);
        if !words2.is_empty() && words2[0] != oname {
            active = &expanded;
        }
    }

    let final_words = tokenize_line(active);
    if final_words.is_empty() {
        return;
    }
    let name = final_words[0].as_slice();

    let mut argbuf = Vec::new();
    for (k, w) in final_words.iter().enumerate().skip(1) {
        if k > 1 {
            argbuf.push(b' ');
        }
        argbuf.extend_from_slice(w);
    }
    let arg = argbuf.as_slice();

    let status = match name {
        b"echo" => exec_echo(arg),
        b"help" => cmd_help(),
        b"now" => cmd_now(),
        b"time" => cmd_time(arg),
        b"uptime" => cmd_uptime(arg),
        b"version" | b"uname" => cmd_version(),
        b"cpu" => cmd_cpu(),
        b"sleep" => cmd_sleep(arg),
        b"clear" => {
            out(b"\x1b[2J\x1b[H");
            0
        }
        b"env" => cmd_env(),
        b"export" => cmd_export(arg),
        b"unset" => cmd_unset(arg),
        b"ps" => cmd_ps(arg),
        b"kill" => cmd_kill(arg),
        b"signal" => cmd_signal_list(),
        b"alias" => cmd_alias(arg),
        b"unalias" => cmd_unalias(arg),
        b"which" => cmd_which(arg),
        b"jobs" => cmd_jobs(arg),
        b"ls" => cmd_ls(arg),
        b"cat" => cmd_cat(arg),
        b"mkdir" => cmd_mkdir(arg),
        b"touch" => cmd_touch(arg),
        b"rm" => cmd_rm(arg),
        b"tree" => cmd_tree(arg),
        b"jtree" => cmd_jtree(arg),
        b"cd" => cmd_cd(arg),
        b"pwd" => cmd_pwd(),
        other => {
            out(b"boruix: unknown command: ");
            out(other);
            out(b"\n");
            127
        }
    };
    set_last_status(status);
}
