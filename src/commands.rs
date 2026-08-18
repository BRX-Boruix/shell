//! 内建命令实现与命令分发。
//!
//! 各 `cmd_*` 对应一条 shell 内建命令；`exec_echo` 处理 `echo`；
//! `exec_line` 负责分词、参数重建并把命令名分派到对应实现。

use crate::env::{cmd_env, cmd_export};
use crate::tokenize::{tokenize_line, MAX_WORDS, WORD_CAP};
use crate::util::{out, outln, parse_u64, string_content, trim_bytes, u64_to_dec, unescape};
use libsys::{info, kill, now, ps, sleep, PsEntry};
use libsys::nr::{INFO_BOOT_MS, INFO_CPU_COUNT, INFO_VERSION};
use libsys::signal::LIST;

/// 列出全部内建命令。
fn cmd_help() {
    out(
        b"builtins: echo print println help now time uptime version uname cpu \
sleep clear env export ps kill signal\n",
    );
}

/// `now`/`time`：单调时钟（纳秒）。
fn cmd_now() {
    let ns = now();
    let mut b = [0u8; 24];
    out(b"now: ");
    out(u64_to_dec(ns, &mut b));
    out(b" ns (");
    out(u64_to_dec(ns / 1_000_000_000, &mut b));
    out(b".");
    out(u64_to_dec((ns % 1_000_000_000) / 1_000_000, &mut b));
    out(b" s)\n");
}

/// `uptime`：开机至今。
fn cmd_uptime() {
    let ms = info(INFO_BOOT_MS).unwrap_or(0);
    let mut b = [0u8; 24];
    out(b"uptime: ");
    out(u64_to_dec(ms / 1000, &mut b));
    out(b".");
    out(u64_to_dec(ms % 1000, &mut b));
    out(b" s\n");
}

/// `version`/`uname`：内核版本。
fn cmd_version() {
    let v = info(INFO_VERSION).unwrap_or(0);
    let mut b = [0u8; 24];
    out(b"BORUIX v");
    out(u64_to_dec((v >> 16) & 0xff, &mut b));
    out(b".");
    out(u64_to_dec((v >> 8) & 0xff, &mut b));
    out(b".");
    out(u64_to_dec(v & 0xff, &mut b));
    out(b"\n");
}

/// `cpu`：在线 CPU 数。
fn cmd_cpu() {
    let c = info(INFO_CPU_COUNT).unwrap_or(0);
    let mut b = [0u8; 24];
    out(b"cpus: ");
    out(u64_to_dec(c, &mut b));
    out(b"\n");
}

/// `sleep <秒>`：睡眠（内核当前为忙等实现）。
fn cmd_sleep(arg: &[u8]) {
    let a = trim_bytes(arg);
    match parse_u64(a) {
        Some(secs) => {
            let _ = sleep(secs * 1_000_000_000);
        }
        None => out(b"sleep: usage: sleep <seconds>\n"),
    }
}

/// `ps`：列出存活进程。
fn cmd_ps() {
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
        }
        Err(_) => out(b"ps: failed\n"),
    }
}

/// 列出已知信号（供 `kill -l` / `signal`）。
fn cmd_signal_list() {
    out(b"signals:\n");
    let mut b = [0u8; 24];
    for (num, name) in LIST {
        out(u64_to_dec(*num as u64, &mut b));
        out(b" ");
        out(name.as_bytes());
        out(b"\n");
    }
}

/// `kill [-s SIG|-SIG|-l] <pid>`：向进程发送信号。
fn cmd_kill(arg: &[u8]) {
    let a = trim_bytes(arg);
    if a == b"-l" {
        cmd_signal_list();
        return;
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
        if tok.starts_with(b"-") {
            let body = &tok[1..];
            if body == b"l" {
                cmd_signal_list();
                return;
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
        return;
    }
    match kill(pid, sig) {
        Ok(_) => {}
        Err(e) => {
            let mut b = [0u8; 24];
            out(b"kill: failed (errno ");
            out(u64_to_dec(e.to_errno() as u64, &mut b));
            out(b")\n");
        }
    }
}

/// 执行 `echo <文本>`：输出一行。支持双引号字符串与转义。
fn exec_echo(arg: &[u8]) {
    if let Some(content) = string_content(arg) {
        let mut buf = [0u8; 256];
        let n = unescape(content, &mut buf);
        outln(&buf[..n]);
    } else {
        // 裸文本（无引号）：去掉首尾空白后原样输出。
        outln(trim_bytes(arg));
    }
}

/// 执行一行输入。以 `;` 结尾可省略。空行/注释(`#`)跳过。
///
/// 两类语句在此分流：
/// 1. **函数调用语句**（`print(..)`/`println(..)`）：先由 `expr::try_exec_call`
///    按 `name(expr)` 形式解析，参数是类型表达式，与命令无关；
/// 2. **词式命令**（`echo`/`ps`/`kill`/`export`/…）：经 `tokenize_line` 分词并展开
///    `$VAR`，首词为命令名，其余词以单空格重连成 `arg` 传给对应实现。
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

    // 函数调用语句（print(..)/println(..)）：与“词式命令”明确区分，先在此
    // 尝试按 `name(expr)` 形式解析；命中则求值输出并结束，不进入命令分发。
    if crate::expr::try_exec_call(line) {
        return;
    }

    let mut wbuf = [[0u8; WORD_CAP]; MAX_WORDS];
    let mut wlen = [0usize; MAX_WORDS];
    let nw = tokenize_line(line, &mut wbuf, &mut wlen);
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

    match name {
        b"echo" => exec_echo(arg),
        b"help" => cmd_help(),
        b"now" | b"time" => cmd_now(),
        b"uptime" => cmd_uptime(),
        b"version" | b"uname" => cmd_version(),
        b"cpu" => cmd_cpu(),
        b"sleep" => cmd_sleep(arg),
        b"clear" => out(b"\x1b[2J\x1b[H"),
        b"env" => cmd_env(),
        b"export" => cmd_export(arg),
        b"ps" => cmd_ps(),
        b"kill" => cmd_kill(arg),
        b"signal" => cmd_signal_list(),
        other => {
            out(b"boruix: unknown command: ");
            out(other);
            out(b"\n");
        }
    }
}
