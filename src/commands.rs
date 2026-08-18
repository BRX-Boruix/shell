//! 内建命令实现与命令分发。
//!
//! 各 `cmd_*` 对应一条 shell 内建命令；`exec_echo` 处理 `echo`；
//! `exec_line` 负责分词、参数重建并把命令名分派到对应实现。

/// 所有内建命令名（供 Tab 补全使用）。
pub(crate) const COMMANDS: &[&[u8]] = &[
    b"echo", b"help", b"now", b"time", b"uptime", b"version", b"uname", b"cpu", b"sleep",
    b"clear", b"env", b"export", b"unset", b"ps", b"kill", b"signal", b"alias", b"unalias",
    b"which", b"jobs",
];

/// 返回内建命令名列表（供补全遍历）。
pub(crate) fn command_names() -> &'static [&'static [u8]] {
    COMMANDS
}

/// 别名表容量。
const MAX_ALIAS: usize = 16;
const ALIAS_NAME: usize = 16;
const ALIAS_VAL: usize = 64;

/// 命令别名表：`(name, value, used)`。仅展开命令行首词（见 `exec_line`）。
static mut ALIAS_TABLE: [([u8; ALIAS_NAME], [u8; ALIAS_VAL], bool); MAX_ALIAS] =
    [([0u8; ALIAS_NAME], [0u8; ALIAS_VAL], false); MAX_ALIAS];
static mut ALIAS_COUNT: usize = 0;

/// 查别名，返回其值切片（缺失 `None`）。
pub(crate) fn alias_get(name: &[u8]) -> Option<&'static [u8]> {
    unsafe {
        for i in 0..ALIAS_COUNT {
            let (n, v, used) = &ALIAS_TABLE[i];
            if *used && n[..name.len()] == *name && n[name.len()..].iter().all(|&b| b == 0) {
                let mut len = 0;
                while len < ALIAS_VAL && v[len] != 0 {
                    len += 1;
                }
                return Some(&v[..len]);
            }
        }
    }
    None
}

/// 设置/覆盖别名。
pub(crate) fn alias_set(name: &[u8], val: &[u8]) {
    if name.is_empty() || name.len() > ALIAS_NAME {
        return;
    }
    let vlen = val.len().min(ALIAS_VAL);
    unsafe {
        for i in 0..ALIAS_COUNT {
            let (n, v, used) = &mut ALIAS_TABLE[i];
            if *used && n[..name.len()] == *name && n[name.len()..].iter().all(|&b| b == 0) {
                v[..vlen].copy_from_slice(&val[..vlen]);
                for x in &mut v[vlen..] {
                    *x = 0;
                }
                return;
            }
        }
        if ALIAS_COUNT >= MAX_ALIAS {
            return;
        }
        let i = ALIAS_COUNT;
        let l = name.len();
        ALIAS_TABLE[i].0[..l].copy_from_slice(name);
        for x in &mut ALIAS_TABLE[i].0[l..] {
            *x = 0;
        }
        ALIAS_TABLE[i].1[..vlen].copy_from_slice(&val[..vlen]);
        for x in &mut ALIAS_TABLE[i].1[vlen..] {
            *x = 0;
        }
        ALIAS_TABLE[i].2 = true;
        ALIAS_COUNT += 1;
    }
}

/// 删除别名（软删除）。
pub(crate) fn alias_unset(name: &[u8]) {
    if name.is_empty() || name.len() > ALIAS_NAME {
        return;
    }
    unsafe {
        for i in 0..ALIAS_COUNT {
            let (n, _v, used) = &mut ALIAS_TABLE[i];
            if *used && n[..name.len()] == *name && n[name.len()..].iter().all(|&b| b == 0) {
                *used = false;
                return;
            }
        }
    }
}

/// 遍历所有已定义别名（供 `alias` 无参列出）。
pub(crate) fn for_each_alias<F: FnMut(&'static [u8], &'static [u8])>(mut f: F) {
    unsafe {
        for i in 0..ALIAS_COUNT {
            let (n, v, used) = &ALIAS_TABLE[i];
            if !*used {
                continue;
            }
            let mut nl = 0;
            while nl < ALIAS_NAME && n[nl] != 0 {
                nl += 1;
            }
            let mut vl = 0;
            while vl < ALIAS_VAL && v[vl] != 0 {
                vl += 1;
            }
            f(&n[..nl], &v[..vl]);
        }
    }
}

use crate::env::{cmd_env, cmd_export, env_unset, set_last_status};
use crate::tokenize::{tokenize_line, MAX_WORDS, WORD_CAP};
use crate::util::{out, outln, parse_u64, string_content, trim_bytes, u64_to_dec, unescape};

/// 作业表容量（同时存在的后台作业上限）。
const JOB_MAX: usize = 16;
/// 作业命令文本最大字节数。
const JOB_CMD: usize = 64;

/// 后台作业表：`(pid, cmd, used)`。索引 `i` 对应作业号 `i+1`（供 `%n` 引用）。
/// 作业由主 shell 经 `exec(shell, cmd)` 拉起，独立地址空间并行运行，跑完自退。
static mut JOBS: [(u32, [u8; JOB_CMD], bool); JOB_MAX] =
    [(0u32, [0u8; JOB_CMD], false); JOB_MAX];

/// 登记一个后台作业，返回作业号（1 基，供 `%n` 引用）。表满时覆盖最旧槽（0）。
pub(crate) fn job_add(pid: u32, cmd: &[u8]) -> usize {
    unsafe {
        let mut slot = 0usize;
        for i in 0..JOB_MAX {
            if !JOBS[i].2 {
                slot = i;
                break;
            }
            if i == JOB_MAX - 1 {
                slot = 0; // 全满：覆盖最旧
            }
        }
        JOBS[slot].0 = pid;
        let l = cmd.len().min(JOB_CMD);
        JOBS[slot].1[..l].copy_from_slice(&cmd[..l]);
        for x in &mut JOBS[slot].1[l..] {
            *x = 0;
        }
        JOBS[slot].2 = true;
        slot + 1
    }
}

/// 按作业号（1 基）取 pid；越界或空槽返回 `None`。
fn job_pid(idx: usize) -> Option<u32> {
    if idx == 0 || idx > JOB_MAX {
        return None;
    }
    unsafe {
        if JOBS[idx - 1].2 {
            Some(JOBS[idx - 1].0)
        } else {
            None
        }
    }
}

/// 删除作业（如 `kill %n` 后）。
fn job_remove(idx: usize) {
    if idx == 0 || idx > JOB_MAX {
        return;
    }
    unsafe {
        JOBS[idx - 1].2 = false;
    }
}

/// 当前存活进程 pid 集合（供 `jobs` 判定状态）。
fn alive_pids() -> [u32; 32] {
    let mut snap = [PsEntry { pid: 0, state: 0, _pad: [0; 3] }; 32];
    let mut set = [0u32; 32];
    if let Ok(n) = ps(&mut snap) {
        for k in 0..n.min(set.len()) {
            set[k] = snap[k].pid;
        }
    }
    set
}

/// `jobs`：列出全部后台作业及其存活状态（`ps` 判定 Running/Done）。返回 0。
fn cmd_jobs() -> u8 {
    let alive = alive_pids();
    let mut b = [0u8; 24];
    unsafe {
        for i in 0..JOB_MAX {
            if !JOBS[i].2 {
                continue;
            }
            let (pid, cmd, _) = &JOBS[i];
            let mut live = false;
            for k in 0..alive.len() {
                if alive[k] == *pid {
                    live = true;
                    break;
                }
            }
            out(b"[");
            out(u64_to_dec((i + 1) as u64, &mut b));
            out(b"] ");
            out(u64_to_dec(*pid as u64, &mut b));
            out(b" ");
            out(if live { b"Running " } else { b"Done    " });
            let mut cl = 0;
            while cl < JOB_CMD && cmd[cl] != 0 {
                cl += 1;
            }
            out(&cmd[..cl]);
            out(b"\n");
        }
    }
    0
}
use libsys::{info, kill, now, ps, sleep, PsEntry};
use libsys::nr::{INFO_BOOT_MS, INFO_CPU_COUNT, INFO_VERSION};
use libsys::signal::LIST;

/// 列出全部内建命令。
fn cmd_help() -> u8 {
    out(
        b"builtins: echo help now time uptime version uname cpu \
sleep clear env export unset ps kill signal alias unalias which\n",
    );
    0
}

/// `now`/`time`：单调时钟（纳秒）。
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

/// `uptime`：开机至今。
fn cmd_uptime() -> u8 {
    let ms = info(INFO_BOOT_MS).unwrap_or(0);
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

/// `sleep <秒>`：睡眠（内核当前为忙等实现）。返回：0 成功，1 参数错误。
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

/// `ps`：列出存活进程。返回：0 成功，1 查询失败。
fn cmd_ps() -> u8 {
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

/// 列出已知信号（供 `kill -l` / `signal`）。返回 0。
fn cmd_signal_list() -> u8 {
    out(b"signals:\n");
    let mut b = [0u8; 24];
    for (num, name) in LIST {
        out(u64_to_dec(*num as u64, &mut b));
        out(b" ");
        out(name.as_bytes());
        out(b"\n");
    }
    0
}

/// `kill [-s SIG|-SIG|-l] <pid>`：向进程发送信号。
/// 返回：0 成功，1 发送失败，127 用法错误。
fn cmd_kill(arg: &[u8]) -> u8 {
    let a = trim_bytes(arg);
    if a == b"-l" {
        cmd_signal_list();
        return 0;
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
        // `kill %n`：按作业号（1 基）引用后台作业。
        if tok.first() == Some(&b'%') {
            match parse_u64(&tok[1..]) {
                Some(v) => match job_pid(v as usize) {
                    Some(p) => {
                        pid = p as u64;
                        have_pid = true;
                        job_remove(v as usize);
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

/// 执行 `echo <文本>`：输出一行。支持双引号字符串与转义。返回 0。
fn exec_echo(arg: &[u8]) -> u8 {
    if let Some(content) = string_content(arg) {
        let mut buf = [0u8; 256];
        let n = unescape(content, &mut buf);
        outln(&buf[..n]);
    } else {
        // 裸文本（无引号）：去掉首尾空白后原样输出。
        outln(trim_bytes(arg));
    }
    0
}

/// `unset NAME...`：删除一个或多个变量（空格分隔）。缺失的名字静默忽略。
/// 返回 0（与 POSIX 一致）。
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

/// `alias` / `alias NAME=VALUE` / `alias NAME`：列出全部、设置或查询别名。
/// 返回：0 正常，1 空名字。
fn cmd_alias(arg: &[u8]) -> u8 {
    let a = trim_bytes(arg);
    if a.is_empty() {
        for_each_alias(|n, v| {
            out(n);
            out(b"='");
            out(v);
            out(b"'\n");
        });
        return 0;
    }
    if let Some(eq) = a.iter().position(|&c| c == b'=') {
        let name = &a[..eq];
        let mut val = &a[eq + 1..];
        // 剥除值两端成对引号（如 `alias g='echo hi'` 的 `'...'`），
        // 否则引号会被原样存下、展开时变成字面命令名的一部分。
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
        // 仅查询单个别名
        match alias_get(a) {
            Some(v) => {
                out(a);
                out(b"='");
                out(v);
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

/// `unalias NAME...`：删除一个或多个别名（空格分隔）。缺失的名字静默忽略。
/// 返回 0。
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

/// `which NAME...`：报告每个名字是别名还是内建命令，否则 not found。
/// 返回：0 正常，1 用法错误（无参数）。
fn cmd_which(arg: &[u8]) -> u8 {
    let a = trim_bytes(arg);
    if a.is_empty() {
        out(b"which: usage: which <name...>\n");
        return 1;
    }
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
        let name = &a[start..i];
        if let Some(av) = alias_get(name) {
            out(name);
            out(b" is aliased to '");
            out(av);
            out(b"'\n");
        } else if command_names().iter().any(|c| c == &name) {
            out(name);
            out(b" is a shell builtin\n");
        } else {
            out(name);
            out(b": not found\n");
        }
    }
    0
}

/// 去掉行内注释：从第一个**未加引号**的 `#` 起截到行尾。
/// 双引号/单引号内的 `#` 不当作注释（如 `echo "a#b"`）。
fn strip_comment(line: &[u8]) -> &[u8] {
    let n = line.len();
    let mut i = 0;
    let mut in_q = 0u8; // 0=无 1=双引号 2=单引号
    while i < n {
        let c = line[i];
        if in_q != 0 {
            if c == b'\\' && in_q == 1 && i + 1 < n {
                i += 2; // 双引号内转义，跳过下一字符
                continue;
            }
            if (in_q == 1 && c == b'"') || (in_q == 2 && c == b'\'') {
                in_q = 0;
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            in_q = 1;
            i += 1;
            continue;
        }
        if c == b'\'' {
            in_q = 2;
            i += 1;
            continue;
        }
        if c == b'#' {
            return &line[..i];
        }
        i += 1;
    }
    line
}

/// 执行一行输入。以 `;` 结尾可省略。空行/整行注释(`#`)跳过；行内 `#`（引号外）
/// 视为注释截掉。执行后把命令退出码写入 `$?`（见 `env::set_last_status`）。
///
/// 经 `tokenize_line` 按引号感知规则分词并展开 `$VAR`，首词为命令名，其余词
/// 以单空格重连成 `arg` 传给对应实现（引号已在 `exec_echo` 处解析，故此处保留
/// 引号字符）。
pub(crate) fn exec_line(line: &[u8]) {
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
    // 行内注释（引号外的 # 起截到行尾）
    let line = strip_comment(line);

    let mut wbuf = [[0u8; WORD_CAP]; MAX_WORDS];
    let mut wlen = [0usize; MAX_WORDS];
    let nw = tokenize_line(line, &mut wbuf, &mut wlen);
    if nw == 0 {
        return;
    }
    // 暂存首词到栈上，避免持有 wbuf 的不可变借用跨过下面的重新分词。
    let mut oname = [0u8; WORD_CAP];
    let onlen = wlen[0].min(WORD_CAP);
    oname[..onlen].copy_from_slice(&wbuf[0][..onlen]);

    // 别名展开：仅替换首词一次（若展开后的首词仍是该别名则停止，防自环）。
    let mut expanded = [0u8; 512];
    let mut active: &[u8] = line;
    if let Some(av) = alias_get(&oname[..onlen]) {
        let rest = &line[onlen..]; // 首词之后的剩余部分（含前导空白）
        let mut el = av.len().min(ALIAS_VAL);
        expanded[..el].copy_from_slice(&av[..el]);
        for &b in rest {
            if el < expanded.len() {
                expanded[el] = b;
                el += 1;
            }
        }
        let nw2 = tokenize_line(&expanded[..el], &mut wbuf, &mut wlen);
        // 展开后首词与原首词比较（均来自栈上/刚写入，不持有旧借用）。
        let new_len = wlen[0].min(WORD_CAP);
        let mut same = new_len == onlen;
        if same {
            for k in 0..new_len {
                if wbuf[0][k] != oname[k] {
                    same = false;
                    break;
                }
            }
        }
        if nw2 > 0 && !same {
            active = &expanded[..el];
        }
    }

    // 按最终（可能已展开）的命令行重新分词。
    let nw = tokenize_line(active, &mut wbuf, &mut wlen);
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

    let status = match name {
        b"echo" => exec_echo(arg),
        b"help" => cmd_help(),
        b"now" | b"time" => cmd_now(),
        b"uptime" => cmd_uptime(),
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
        b"ps" => cmd_ps(),
        b"kill" => cmd_kill(arg),
        b"signal" => cmd_signal_list(),
        b"alias" => cmd_alias(arg),
        b"unalias" => cmd_unalias(arg),
        b"which" => cmd_which(arg),
        b"jobs" => cmd_jobs(),
        other => {
            out(b"boruix: unknown command: ");
            out(other);
            out(b"\n");
            127
        }
    };
    set_last_status(status);
}
