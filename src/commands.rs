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
    b"pwd", b"pipe", b"libccheck",
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
    chdir, close, dup2, exec_path, getcwd, info, kill, mkdir, now, open, pipe_create, ps,
    read, read_dir, read_to_end, read_wall_clock, sleep, sync_create, sync_delete, sync_wake,
    unlink, waitpid_any, write, yield_now, OpenFlags, Permissions, PsEntry, STDIN, STDOUT,
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
        (b"ls", b"list directory contents (-a show hidden, -l long, --json)"),
        (b"cat", b"print file contents"),
        (b"mkdir", b"create a directory"),
        (b"touch", b"create or update a file"),
        (b"rm", b"remove a file"),
        (b"tree", b"visualize VFS directory tree"),
        (b"jtree", b"visualize a JSON value as a tree"),
        (b"cd", b"change the working directory"),
        (b"pwd", b"print the working directory"),
        (b"pipe", b"self-test: create a pipe, write+read roundtrip"),
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
    let mut show_all = false;
    let mut path = getcwd().unwrap_or_else(|_| alloc::string::String::from("/"));

    for tok in a.split(|&c| c.is_ascii_whitespace()).filter(|s| !s.is_empty()) {
        if tok == b"-l" {
            long_mode = true;
        } else if tok == b"-a" || tok == b"--all" {
            show_all = true;
        } else if tok == b"--json" {
            json_mode = true;
        } else if !tok.starts_with(b"-") {
            if let Ok(p) = core::str::from_utf8(tok) {
                path = alloc::string::String::from(p);
            }
        }
    }

    // POSIX 惯例：`ls` 默认隐藏 `.` 开头条目（如 `.`/`..`，以及真实文件系统
    // 落盘的隐藏项）；`ls -a` 才如实全显。内核层如实回显盘上 dirent，过滤
    // 是**显示层**职责——不隐藏会破坏与 RamFS（不返回 `.`/`..`）的一致性。
    let show = |name: &str| show_all || !name.starts_with('.');

    match read_dir(&path) {
        Ok(entries) => {
            if json_mode {
                use libsys::json::{JsonWriter, VecTarget};
                let mut target = VecTarget::new();
                let mut writer = JsonWriter::new(&mut target);
                if let Ok(mut arr) = writer.start_array() {
                    for entry in &entries {
                        if !show(&entry.name) {
                            continue;
                        }
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
                    if !show(&entry.name) {
                        continue;
                    }
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
                    if !show(&entry.name) {
                        continue;
                    }
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

/// `cat [path]`：打印文件内容；无路径时读 stdin（支持管道右段 `A | cat`）。
fn cmd_cat(arg: &[u8]) -> u8 {
    let a = trim_bytes(arg);
    if a.is_empty() {
        // 无路径：从 stdin（fd 0）读到 EOF，直接转发到 stdout。
        // 管道右段 `echo hi | cat` 走此路径（pipe-features A3）。
        return match read_stdin_all(&mut |chunk: &[u8]| out(chunk)) {
            Ok(()) => 0,
            Err(()) => {
                out(b"cat: stdin read failed\n");
                1
            }
        };
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

/// `pipe`：管道自检（pipe-features.md 第 1 层验收）。创建一对匿名管道端，
/// 写端写数据 → 读端读回 → 校验字节一致，打印结果。验证 libsys `pipe_create`
/// 封装的解包 + 内核 FLAG_PIPE 路由（`SYS_STREAM_CREATE` → `ipc::pipe_*`）在
/// 真实用户进程里可用。成功返回 0，任何一步失败返回 1。
fn cmd_pipe(_arg: &[u8]) -> u8 {
    let (rfd, wfd) = match pipe_create() {
        Ok(v) => v,
        Err(_) => {
            out(b"pipe: pipe_create failed\n");
            return 1;
        }
    };
    const MSG: &[u8] = b"hello-pipe";
    if write(wfd, MSG) != Ok(MSG.len()) {
        out(b"pipe: write failed\n");
        let _ = close(wfd);
        let _ = close(rfd);
        return 1;
    }
    let mut buf = [0u8; 64];
    let n = match read(rfd, &mut buf) {
        Ok(n) => n,
        Err(_) => {
            out(b"pipe: read failed\n");
            let _ = close(wfd);
            let _ = close(rfd);
            return 1;
        }
    };
    let _ = close(wfd);
    let _ = close(rfd);
    if n != MSG.len() || buf[..n] != *MSG {
        out(b"pipe: mismatch read=");
        out(u64_to_dec(n as u64, &mut [0u8; 24]));
        out(b"\n");
        return 1;
    }
    out(b"pipe: ok \"");
    out(&buf[..n]);
    out(b"\"\n");
    0
}
/// `synce2e`：SYNC 域（ADR-032）端到端**阻塞往返**测试（协调端）。
///
/// 流程：create 同步字(id,0) → `exec_path("/programs/synce2e.elf", "waiter:<id>")`
/// 派生子进程（等待端）→ sleep 让子进程在 `sync_wait` 上真实阻塞 →
/// `sync_wake(id,42,1)` 唤醒并预置值 42 → `waitpid_any()` 收子进程退出码断言为 0。
/// 若 `sync_wake` 返回 1，则证明子进程确实被登记为等待者并阻塞过（真实切换往返）。
///
/// **liveCD 专属测试命令（ADR-029）**：`/programs/synce2e.elf` 依赖内核内嵌测试 payload
/// （liveCD 模式经 ADR-028 单源填充 `/programs`）。安装模式下 `/programs` 是磁盘 root 的普通
/// 目录、**不做 payload 兜底**，此 ELF 不保证存在——本命令定位为开发/验收期诊断，非生产特性。
fn cmd_synce2e() -> u8 {
    let mut b = [0u8; 24];
    let mut id_buf = [0u8; 24];
    // 1. create 同步字，初值 0。
    let id = match sync_create(0) {
        Ok(id) => id,
        Err(_) => { out(b"synce2e: create failed\n"); return 1; }
    };
    out(b"synce2e: create id=");
    out(u64_to_dec(id, &mut b));
    out(b" init=0\n");
    // 2. 派生等待端子进程，命令行 = "waiter:<id>"。
    let id_s = u64_to_dec(id, &mut id_buf);
    let mut cmd: alloc::vec::Vec<u8> = alloc::vec![b'w', b'a', b'i', b't', b'e', b'r', b':'];
    cmd.extend_from_slice(id_s);
    let child = match exec_path("/programs/synce2e.elf", &cmd) {
        Ok(pid) => pid,
        Err(e) => {
            // ADR-029：安装模式 `/programs` 无 payload 兜底，synce2e.elf 可能不存在。
            out(b"synce2e: spawn waiter failed errno=");
            out(u64_to_dec(e.to_errno() as u64, &mut b));
            out(b" (liveCD-only test payload /programs/synce2e.elf required, ADR-029)\n");
            let _ = sync_delete(id);
            return 1;
        }
    };
    out(b"synce2e: spawned waiter pid=");
    out(u64_to_dec(child, &mut b));
    out(b"\n");
    // 3. yield 循环：保持父进程就绪（而非阻塞），让子进程被调度去
    //    sync_wait 上真正阻塞。若父进程 sleep 阻塞，子进程阻塞时无就绪同伴，
    //    block_current_with 会 NotSwitched → WouldBlock（见内核注释）。
    for _ in 0..2000 {
        let _ = yield_now();
    }
    // 4. wake：设值 42 并唤醒（至多 1 个）等待者。返回 1 证明子进程已阻塞。
    let woke = match sync_wake(id, 42, 1) {
        Ok(n) => n,
        Err(_) => { out(b"synce2e: wake failed\n"); let _ = sync_delete(id); return 1; }
    };
    if woke != 1 {
        out(b"synce2e: wake returned ");
        out(u64_to_dec(woke, &mut b));
        out(b" (expected 1 = child was blocked)\n");
        let _ = sync_delete(id);
        return 1;
    }
    out(b"synce2e: woke 1 blocked waiter, value=42\n");
    // 5. 收子进程退出码，断言 0 且 pid == 派生子进程（waitpid 真实返回被收尸 pid）。
    match waitpid_any() {
        Ok(wr) => {
            if wr.pid != child {
                out(b"synce2e: waitpid pid=");
                out(u64_to_dec(wr.pid, &mut b));
                out(b" (expected ");
                out(u64_to_dec(child, &mut b));
                out(b")\n");
                let _ = sync_delete(id);
                return 1;
            }
            if wr.code != 0 {
                out(b"synce2e: child exit=");
                out(u64_to_dec(wr.code, &mut b));
                out(b" (expected 0)\n");
                let _ = sync_delete(id);
                return 1;
            }
            out(b"synce2e: waitpid pid=");
            out(u64_to_dec(wr.pid, &mut b));
            out(b" exit=0, round-trip OK\n");
        }
        Err(_) => { out(b"synce2e: waitpid failed\n"); let _ = sync_delete(id); return 1; }
    }
    // 6. delete 同步字。
    match sync_delete(id) {
        Ok(()) => out(b"synce2e: delete ok\n"),
        Err(_) => { out(b"synce2e: delete failed\n"); return 1; }
    }
    out(b"synce2e: ALL OK (blocking round-trip)\n");
    0
}

/// `mkdir <dir>`：创建目录。
fn cmd_mkdir(arg: &[u8]) -> u8 {    let a = trim_bytes(arg);
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

/// stdio 备份高位预留槽（shell 正常只开 0/1/2，这些槽空闲）。集中定义避免
/// 魔法数字散落（规则 16：业务语义常量集中定义并附注释说明来源与含义）。
/// 管道外层 stdout 备份槽（exec_pipeline 用）：把 shell 原始 fd 1 暂存，
/// 左段经管道写端改指后据此还原。
const SAVE_FD_OUT: u64 = 200;
/// 管道外层 stdin 备份槽（exec_pipeline 用）：把 shell 原始 fd 0 暂存，
/// 右段经管道读端改指后据此还原。
const SAVE_FD_IN: u64 = 201;
/// 段内重定向 stdout 备份槽（with_redirects 用）：把段执行时当前 fd 1
/// （可能是管道写端）暂存，文件输出重定向改指后据此还原。与管道外层槽
/// 200/201 分离，避免管道 + 段内重定向嵌套时互相覆盖备份。
const REDIR_SAVE_FD_OUT: u64 = 202;
/// 段内重定向 stdin 备份槽（with_redirects 用）：把段执行时当前 fd 0
/// （可能是管道读端）暂存，文件输入重定向改指后据此还原。
const REDIR_SAVE_FD_IN: u64 = 203;

/// 一条重定向指令：命令执行期间把目标标准 fd 改指某个文件。
///
/// - `target`：0 = 标准输入，1 = 标准输出（用 libsys 常量 `STDIN`/`STDOUT`，规则 16）。
///   内建命令错误统一写 stdout（`out()` 写 `STDOUT`），全 shell 已核验无任何命令写
///   fd 2（src 无 `write(2`/`STDERR` 调用），故不实现 `2>`——否则是无人消费的死机制
///   （规则 27：零死代码）。该取舍为显式设计决定（规则 29/规则 25：默认值带理由），
///   若未来引入写 stderr 的命令须一并补 `2>`。
/// - `input`：true = 输入重定向（读文件，`<`）；false = 输出重定向。
/// - `append`：输出重定向追加（`>>`）而非截断（`>`）。输入重定向恒 false。
struct Redirect {
    target: u8,
    input: bool,
    append: bool,
    path: Vec<u8>,
}

/// 从分词后的词表中剥离重定向词（`>`/`>>`/`<` + 紧随路径词），返回净化后的
/// 命令词 + 重定向指令表。
///
/// 与管道同约定：重定向操作符是**空白分隔的独立词**（tokenizer 产出），
/// `echo hi>f` 不会被当作重定向（需 `echo hi > f`）。
///
/// 操作符后缺路径 → `Err(())`，调用方报错短路、不执行命令（宁缺毋假）。
fn split_redirects(words: &[Vec<u8>]) -> Result<(Vec<Vec<u8>>, Vec<Redirect>), ()> {
    let mut clean: Vec<Vec<u8>> = Vec::new();
    let mut redirs: Vec<Redirect> = Vec::new();
    let mut i = 0usize;
    while i < words.len() {
        let w = words[i].as_slice();
        // 识别重定向操作符（独立词）。元组 = (输入?, 追加?)。
        let op: Option<(bool, bool)> = if w == b"<" {
            Some((true, false)) // 输入重定向
        } else if w == b">" {
            Some((false, false)) // 输出截断
        } else if w == b">>" {
            Some((false, true)) // 输出追加
        } else {
            None
        };
        match op {
            None => {
                // 普通词：保留进净化命令词。
                clean.push(words[i].clone());
                i += 1;
            }
            Some((input, append)) => {
                // 操作符后必须紧跟一个路径词。
                let Some(path) = words.get(i + 1) else {
                    return Err(());
                };
                if path.is_empty() {
                    return Err(());
                }
                redirs.push(Redirect {
                    target: if input { STDIN as u8 } else { STDOUT as u8 },
                    input,
                    append,
                    path: path.clone(),
                });
                i += 2; // 跳过 操作符 + 路径
            }
        }
    }
    Ok((clean, redirs))
}

/// 在命令执行期间临时套用重定向，跑完还原。
///
/// - 对每条重定向：`open` 目标文件拿 fd，`dup2` 到目标标准 fd（`STDIN`/`STDOUT`）；
/// - 执行 `run`（命令的 `out()` 写 `STDOUT` → 落文件 / `read(STDIN)` 读文件）；
/// - 跑完还原：把目标 fd 恢复为备份的原 stdio，关闭备份槽与打开的文件 fd。
/// - 任何 `open` 失败：**不执行命令**并如实报错（POSIX 语义，如
///   `echo hi > /no/such/dir/f` 不执行 echo）。
///
/// 返回 `Ok(命令退出状态)`；打开失败返回 `Err(())`。
fn with_redirects(redirs: &[Redirect], run: impl FnOnce() -> u8) -> Result<u8, ()> {
    // 备份：先保存可能被改写的目标 fd（0 与/或 1）到高位预留槽。
    let wants_in = redirs.iter().any(|r| r.target == STDIN as u8);
    let wants_out = redirs.iter().any(|r| r.target == STDOUT as u8);
    if (wants_in && dup2(STDIN, REDIR_SAVE_FD_IN).is_err())
        || (wants_out && dup2(STDOUT, REDIR_SAVE_FD_OUT).is_err())
    {
        out(b"boruix: redirect: stdio backup failed\n");
        return Err(());
    }
    // 打开并安装每条重定向。任一条失败：还原已安装的、关闭已打开的、短路。
    let mut installed: Vec<(u8, u64)> = Vec::new(); // (目标fd, 打开的文件fd)
    let mut fail = false;
    for r in redirs {
        let flags = if r.input {
            OpenFlags::READ_ONLY
        } else if r.append {
            // 追加写：write + create（不存在则建），不截断。
            OpenFlags {
                read: false,
                write: true,
                create: true,
                truncate: false,
                append: true,
                directory: false,
                pipe: false,
            }
        } else {
            // 截断写：write + create + truncate（不存在则建，存在则清空）。
            OpenFlags {
                read: false,
                write: true,
                create: true,
                truncate: true,
                append: false,
                directory: false,
                pipe: false,
            }
        };
        let path = match core::str::from_utf8(&r.path) {
            Ok(p) => p,
            Err(_) => {
                out(b"boruix: redirect: invalid path\n");
                fail = true;
                break;
            }
        };
        // 权限仅在创建文件时生效：输入重定向只读、不创建，用 readonly；
        // 输出重定向写/建/截断，用 read_write（规则 34：语义一致）。
        let perm = if r.input {
            Permissions::readonly()
        } else {
            Permissions::read_write()
        };
        let file_fd = match open(path, flags, perm) {
            Ok(fd) => fd,
            Err(_) => {
                out(b"boruix: cannot open '");
                out(&r.path);
                out(b"' for redirect\n");
                fail = true;
                break;
            }
        };
        // dup2 安装：把目标标准 fd 改指打开的文件。dup2 后原 file_fd 仍有效，
        // 记录待关闭。
        if dup2(file_fd, r.target as u64).is_err() {
            let _ = close(file_fd);
            out(b"boruix: redirect: dup2 failed\n");
            fail = true;
            break;
        }
        installed.push((r.target, file_fd));
    }
    if fail {
        // 还原已安装的重定向（先 restore 再关备份槽/文件 fd，顺序重要）。
        for (target, file_fd) in installed.iter().rev() {
            let _ = if *target == STDIN as u8 {
                dup2(REDIR_SAVE_FD_IN, STDIN)
            } else {
                dup2(REDIR_SAVE_FD_OUT, STDOUT)
            };
            let _ = close(*file_fd);
        }
        if wants_in {
            let _ = close(REDIR_SAVE_FD_IN);
        }
        if wants_out {
            let _ = close(REDIR_SAVE_FD_OUT);
        }
        return Err(());
    }
    // 执行命令。
    let status = run();
    // 还原重定向。
    for (target, file_fd) in installed.iter().rev() {
        let _ = if *target == STDIN as u8 {
            dup2(REDIR_SAVE_FD_IN, STDIN)
        } else {
            dup2(REDIR_SAVE_FD_OUT, STDOUT)
        };
        let _ = close(*file_fd);
    }
    if wants_in {
        let _ = close(REDIR_SAVE_FD_IN);
    }
    if wants_out {
        let _ = close(REDIR_SAVE_FD_OUT);
    }
    Ok(status)
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
    // 管道 `|`：若存在独立的 `|` 词，按管道执行（pipe-features A3）。
    // 先于单命令分发——`|` 是保留分隔符，不作为普通命令/参数。
    if final_words.iter().any(|w| w.as_slice() == b"|") {
        let status = exec_pipeline(&final_words);
        set_last_status(status);
        return;
    }
    // 单命令路径：先剥离重定向（> / >> / <），剩下的才是指令词。
    let (clean_words, redirs) = match split_redirects(&final_words) {
        Ok(v) => v,
        Err(()) => {
            out(b"boruix: malformed redirect\n");
            set_last_status(1);
            return;
        }
    };
    if clean_words.is_empty() {
        // 只有重定向没有命令（如 > f）——无可执行指令，如实报错。
        out(b"boruix: missing command before redirect\n");
        set_last_status(1);
        return;
    }
    let name = clean_words[0].as_slice();

    let mut argbuf = Vec::new();
    for (k, w) in clean_words.iter().enumerate().skip(1) {
        if k > 1 {
            argbuf.push(b' ');
        }
        argbuf.extend_from_slice(w);
    }
    let arg = argbuf.as_slice();

    let status = if redirs.is_empty() {
        run_builtin(name, arg)
    } else {
        match with_redirects(&redirs, || run_builtin(name, arg)) {
            Ok(s) => s,
            Err(()) => 1, // open/dup2 失败已报错
        }
    };
    set_last_status(status);
}

/// 分发单条内建命令（`name` = 命令名，`arg` = 空格连接的剩余参数）。供
/// `exec_line` 与管道 `|` 的各段（`run_pipeline_stage`）共用。返回退出状态。
fn run_builtin(name: &[u8], arg: &[u8]) -> u8 {
    match name {
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
        b"pipe" => cmd_pipe(arg),
        b"synce2e" => cmd_synce2e(),
        b"libccheck" => crate::libc_check::cmd_libccheck(arg),
        other => {
            out(b"boruix: unknown command: ");
            out(other);
            out(b"\n");
            127
        }
    }
}

/// 执行管道 `A | B | C ...`（pipe-features A3）：把前一段的 stdout 经真实匿名
/// 管道接到后一段的 stdin，支持**多段**（评审 🟠 加入）。用内核 `dup2` +
/// `pipe_create`（方案 A 的 fd 重定向原语）。
///
/// 实现（Unix 管道本质，顺序模型——与既有单管道一致的非并发语义）：
/// 1. 按 `|` 把词表拆成 N 段，建 N-1 条匿名管道 `(rfd[i], wfd[i])`；
/// 2. 把 shell 自身 fd 0/1 备份到高位预留槽（`dup2`），**整条管道期间保持
///    打开**（每段结束时还原到原始 stdio，而不是关备份槽）；
/// 3. 对每段 i：`dup2(rfd[i-1], STDIN)`（i>0）使 stdin ← 前段读端；
///    `dup2(wfd[i], STDOUT)`（i<n-1）使 stdout → 后段写端；执行该段；
///    再 `dup2(备份, STDOUT/STDIN)` 还原原始 stdio；
/// 4. 段结束后关掉本段的管道端（产出写端 + 消费读端），使后段读到 EOF。
///
/// 段间流通量**无上限**：内核管道缓冲已改为无界（ipc1 同期修复），顺序模型下
/// 写者一次性产出全部数据、后段再读，绝无"缓冲满"阻塞/死锁。仍为顺序非并发
/// 语义（每段跑完再跑下一段），但任意大中间数据都能正确流通。
///
/// 返回首个非零段退出状态；全零则返回 0（保留既有"首失败优先"约定）。
fn exec_pipeline(words: &[Vec<u8>]) -> u8 {
    // 按 `|` 词把 words 拆成多段。`|` 是保留分隔符，不进入任何段。
    let mut segments: Vec<&[Vec<u8>]> = Vec::new();
    let mut start = 0usize;
    for (i, w) in words.iter().enumerate() {
        if w.as_slice() == b"|" {
            segments.push(&words[start..i]);
            start = i + 1;
        }
    }
    segments.push(&words[start..]);
    let n = segments.len();
    // 至少两段；任一段为空（首/尾 `|` 或连续 `||`）即畸形。
    if n < 2 || segments.iter().any(|s| s.is_empty()) {
        out(b"boruix: malformed pipeline\n");
        return 1;
    }
    // 建 N-1 条管道。中途失败须回滚已建的（避免 fd 泄漏）。
    let mut pipes: Vec<(u64, u64)> = Vec::new();
    for _ in 0..n - 1 {
        match pipe_create() {
            Ok(v) => pipes.push(v),
            Err(_) => {
                out(b"boruix: pipe_create failed\n");
                for &(r, w) in pipes.iter() {
                    let _ = close(r);
                    let _ = close(w);
                }
                return 1;
            }
        }
    }
    // 备份 shell 自身 stdio 到高位预留槽（模块级 SAVE_FD_* 常量，集中定义）。
    // 失败（槽被占/超上限）如实报错并回滚全部管道端。
    if dup2(STDOUT, SAVE_FD_OUT).is_err() || dup2(STDIN, SAVE_FD_IN).is_err() {
        out(b"boruix: dup2 backup failed\n");
        for &(r, w) in pipes.iter() {
            let _ = close(r);
            let _ = close(w);
        }
        return 1;
    }
    // 顺序执行每段；备份槽保持打开到整条管道结束，便于每段还原原始 stdio。
    let mut first_err: u8 = 0;
    for i in 0..n {
        // 段 i 的 stdin ← 前段读端（i>0）；段 i 的 stdout → 后段写端（i<n-1）。
        if i > 0 {
            let _ = dup2(pipes[i - 1].0, STDIN);
        }
        if i < n - 1 {
            let _ = dup2(pipes[i].1, STDOUT);
        }
        let st = run_pipeline_stage(segments[i]);
        // 还原原始 stdio（顺序重要：先还原 fd0/1 才能让后续 out 输出正常）。
        let _ = dup2(SAVE_FD_OUT, STDOUT);
        let _ = dup2(SAVE_FD_IN, STDIN);
        if first_err == 0 && st != 0 {
            first_err = st;
        }
        // 关掉本段消费的读端与产出的写端，使后段能读到 EOF。
        if i > 0 {
            let _ = close(pipes[i - 1].0);
        }
        if i < n - 1 {
            let _ = close(pipes[i].1);
        }
    }
    // 关备份槽（整条管道结束）。
    let _ = close(SAVE_FD_OUT);
    let _ = close(SAVE_FD_IN);
    first_err
}

/// 运行管道的一段的命令分发（参数重建 + `run_builtin`）。
///
/// 段级作用域：先剥离本段的重定向（`>`/`>>`/`<`）并经 `with_redirects` 套用，
/// 使 `echo hi > f | cat` 中左段的 stdout 落到文件 f（而非管道），右段仍从
/// 管道读。
fn run_pipeline_stage(stage: &[Vec<u8>]) -> u8 {
    let (clean, redirs) = match split_redirects(stage) {
        Ok(v) => v,
        Err(()) => {
            out(b"boruix: malformed redirect in pipeline\n");
            return 1;
        }
    };
    if clean.is_empty() {
        out(b"boruix: missing command before redirect in pipeline\n");
        return 1;
    }
    let name = clean[0].as_slice();
    let mut argbuf = Vec::new();
    for (k, w) in clean.iter().enumerate().skip(1) {
        if k > 1 {
            argbuf.push(b' ');
        }
        argbuf.extend_from_slice(w);
    }
    let arg = argbuf.as_slice();
    if redirs.is_empty() {
        run_builtin(name, arg)
    } else {
        match with_redirects(&redirs, || run_builtin(name, arg)) {
            Ok(s) => s,
            Err(()) => 1, // open/dup2 失败已报错
        }
    }
}

/// 从 fd 0（stdin）读到 EOF，把内容喂给 `feed`。供 `cat`（无路径读 stdin）
/// 等管道右段消费。
///
/// EOF 语义：管道写端全部关闭且缓冲排空后，内核 `pipe_read` 返回 `WouldBlock`
/// （当前内核不追踪"写端是否全部关闭"来给 EOF(0)）。对顺序管道右段（写端
/// 已在本段运行前关闭），`WouldBlock` 即等价 EOF——此处如实把 `WouldBlock`
/// 当 EOF 结束读取，而不是误报失败。其它错误才上抛。
fn read_stdin_all(feed: &mut dyn FnMut(&[u8])) -> Result<(), ()> {
    let mut buf = [0u8; 128];
    loop {
        let n = match read(STDIN, &mut buf) {
            Ok(n) => n,
            Err(libsys::error::Error::WouldBlock) => break, // 写端已关 = EOF
            Err(_) => return Err(()),
        };
        if n == 0 {
            break; // EOF（写端已关）
        }
        feed(&buf[..n]);
    }
    Ok(())
}
