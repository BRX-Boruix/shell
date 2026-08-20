//! BORUIX 用户态 shell（命令行解释器，独立可执行程序）。
//!
//! 由 init（PID 1）经 `exec` 系统调用加载运行为 PID 2。libsys 提供 `_start`
//! 入口，本文件导出 `user_main`（进程入口）。
//!
//! 模块划分（按职责解耦，依赖单向无环）：
//! - `util`    : 输出辅助与通用字节/文本工具
//! - `env`     : 纯字符串变量表 + `export`/`env` 命令
//! - `tokenize`: 引号感知分词与 `$VAR` 展开
//! - `commands`: 内建命令与命令分发
//! - `main`    : 入口、REPL 循环、行读取（含历史与 Tab 补全）

#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]

#[cfg(test)]
extern crate std;

extern crate alloc;

mod commands;
mod env;
mod tokenize;
mod util;

use alloc::vec::Vec;
use crate::commands::{command_names, exec_line, job_add};
use crate::env::{for_each_env_name, last_status};
use crate::util::{out, outln, prompt, u64_to_dec};
use libsys::{exec, read, yield_now, nr::PROG_SHELL};

use spin::Mutex;

/// 标准输入文件描述符。
const STDIN: u64 = 0;

/// 命令历史动态列表（无静态条数和长度上限）。
static HIST: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

/// shell 入口（libsys `_start` 调用）：输出横幅并进入 REPL 循环。
#[unsafe(no_mangle)]
pub extern "C" fn user_main(argc: isize, argv: *const *const u8) -> i32 {
    if argc >= 1 && !argv.is_null() {
        let cmd = unsafe { *argv };
        if !cmd.is_null() {
            let mut buf = Vec::new();
            unsafe {
                let mut p = cmd;
                while *p != 0 {
                    buf.push(*p);
                    p = p.add(1);
                }
            }
            if !buf.is_empty() {
                exec_line(&buf);
            }
            return last_status() as i32;
        }
    }
    outln(b"BORUIX shell (PID 2)");
    repl_loop();
    0
}

/// REPL 主循环。
fn repl_loop() {
    loop {
        prompt();
        let line = read_line();
        if line.is_empty() {
            continue;
        }
        match background_split(&line) {
            Some(cmd) => spawn_background(cmd),
            None => exec_line(&line),
        }
    }
}

fn background_split(line: &[u8]) -> Option<&[u8]> {
    let n = line.len();
    let mut i = n;
    while i > 0 && line[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    if i == 0 || line[i - 1] != b'&' {
        return None;
    }
    let amp = i - 1;
    let mut j = amp;
    while j > 0 && line[j - 1].is_ascii_whitespace() {
        j -= 1;
    }
    if j > 0 && line[j - 1] == b'&' {
        return None;
    }
    let mut k = amp;
    while k > 0 && line[k - 1].is_ascii_whitespace() {
        k -= 1;
    }
    if k == 0 {
        return None;
    }
    Some(&line[..k])
}

fn spawn_background(cmd: &[u8]) {
    match exec(PROG_SHELL, cmd) {
        Ok(pid) => {
            let idx = job_add(pid as u32, cmd);
            let mut b = [0u8; 24];
            out(b"[");
            out(u64_to_dec(idx as u64, &mut b));
            out(b"] ");
            out(u64_to_dec(pid, &mut b));
            out(b" ");
            out(cmd);
            out(b"\n");
        }
        Err(_) => {
            out(b"boruix: background exec failed\n");
        }
    }
}

fn redraw_at(buf: &[u8], cursor: usize) {
    out(b"\r");
    out(b"\x1b[K");
    prompt();
    out(buf);
    let back = buf.len().saturating_sub(cursor);
    if back > 0 {
        let mut b = [0u8; 24];
        out(b"\x1b[");
        out(u64_to_dec(back as u64, &mut b));
        out(b"D");
    }
}

fn history_push(line: &[u8]) {
    if line.is_empty() {
        return;
    }
    let mut hist = HIST.lock();
    if let Some(last) = hist.last() {
        if last.as_slice() == line {
            return;
        }
    }
    hist.push(line.to_vec());
}

fn history_prev(
    buf: &mut Vec<u8>,
    off: &mut Option<usize>,
    draft: &mut Vec<u8>,
) {
    let hist = HIST.lock();
    if hist.is_empty() {
        return;
    }
    match off {
        None => {
            *draft = buf.clone();
            *off = Some(0);
        }
        Some(o) => {
            if *o + 1 < hist.len() {
                *o += 1;
            }
        }
    }
    let o = off.unwrap();
    let idx = hist.len() - 1 - o;
    *buf = hist[idx].clone();
    let cur = buf.len();
    redraw_at(buf, cur);
}

fn history_next(
    buf: &mut Vec<u8>,
    off: &mut Option<usize>,
    draft: &mut Vec<u8>,
) {
    let hist = HIST.lock();
    match off {
        None => return,
        Some(o) => {
            if *o > 0 {
                *o -= 1;
                let idx = hist.len() - 1 - *o;
                *buf = hist[idx].clone();
            } else {
                *off = None;
                *buf = draft.clone();
            }
        }
    }
    let cur = buf.len();
    redraw_at(buf, cur);
}

/// Smart-Case 智能匹配：如果输入前缀全为小写，则忽略大小写模糊匹配；若输入含大写字母，则严格匹配。
fn smart_case_match(prefix: &[u8], candidate: &[u8]) -> bool {
    if candidate.len() < prefix.len() {
        return false;
    }
    let has_uppercase = prefix.iter().any(|b| b.is_ascii_uppercase());
    if has_uppercase {
        &candidate[..prefix.len()] == prefix
    } else {
        let cand_prefix = &candidate[..prefix.len()];
        cand_prefix
            .iter()
            .zip(prefix.iter())
            .all(|(c, p)| c.to_ascii_lowercase() == *p)
    }
}

fn tab_complete(buf: &mut Vec<u8>) {
    let mut ws = 0usize;
    for k in 0..buf.len() {
        if buf[k] == b' ' || buf[k] == b'\t' {
            ws = k + 1;
        }
    }
    let word = &buf[ws..];
    let (prefix, with_dollar, is_cmd) = if word.first() == Some(&b'$') {
        (&word[1..], true, false)
    } else if ws == 0 {
        (word, false, true)
    } else {
        (word, false, false)
    };

    let mut matches: Vec<Vec<u8>> = Vec::new();
    if is_cmd {
        for name in command_names() {
            if smart_case_match(prefix, name) {
                matches.push(name.to_vec());
            }
        }
    } else {
        for_each_env_name(|name| {
            if smart_case_match(prefix, name) {
                matches.push(name.to_vec());
            }
        });
    }
    if matches.is_empty() {
        return;
    }

    let mut lcp = prefix.len();
    'outer: while lcp < matches[0].len() {
        let b = matches[0][lcp];
        for m in &matches[1..] {
            if lcp >= m.len() || m[lcp] != b {
                break 'outer;
            }
        }
        lcp += 1;
    }

    let mut rep: Vec<u8> = Vec::new();
    if with_dollar {
        rep.push(b'$');
    }
    if matches.len() == 1 {
        rep.extend_from_slice(&matches[0]);
        if is_cmd {
            rep.push(b' ');
        }
    } else {
        rep.extend_from_slice(&matches[0][..lcp]);
    }

    buf.truncate(ws);
    buf.extend_from_slice(&rep);
    let cur = buf.len();
    redraw_at(buf, cur);

    if matches.len() > 1 {
        out(b"\n");
        for m in &matches {
            out(m.as_slice());
            out(b" ");
        }
        out(b"\n");
        redraw_at(buf, cur);
    }
}

/// 从标准输入读一行（基于动态 Vec<u8>，彻底消除静态长度上限）。
fn read_line() -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::new();
    let mut cur = 0usize;
    let mut off: Option<usize> = None;
    let mut draft: Vec<u8> = Vec::new();
    let mut csi_param: u8 = 0;
    let mut esc_state: u8 = 0;

    loop {
        let mut one = [0u8; 1];
        match read(STDIN, &mut one) {
            Ok(got) if got == 1 => {
                let c = one[0];
                if esc_state != 0 {
                    match esc_state {
                        1 => {
                            if c == b'[' {
                                esc_state = 2;
                                csi_param = 0;
                            } else if c == b'O' {
                                esc_state = 3;
                            } else {
                                esc_state = 0;
                            }
                        }
                        2 => {
                            if c.is_ascii_digit() {
                                csi_param = c;
                            } else if (0x40..=0x7E).contains(&c) {
                                match c {
                                    b'A' => {
                                        history_prev(&mut buf, &mut off, &mut draft);
                                        cur = buf.len();
                                    }
                                    b'B' => {
                                        history_next(&mut buf, &mut off, &mut draft);
                                        cur = buf.len();
                                    }
                                    b'C' => {
                                        if cur < buf.len() {
                                            cur += 1;
                                        }
                                        redraw_at(&buf, cur);
                                    }
                                    b'D' => {
                                        if cur > 0 {
                                            cur -= 1;
                                        }
                                        redraw_at(&buf, cur);
                                    }
                                    b'H' => {
                                        cur = 0;
                                        redraw_at(&buf, cur);
                                    }
                                    b'F' => {
                                        cur = buf.len();
                                        redraw_at(&buf, cur);
                                    }
                                    b'~' => {
                                        match csi_param {
                                            b'1' => cur = 0,
                                            b'4' => cur = buf.len(),
                                            b'3' => {
                                                if cur < buf.len() {
                                                    buf.remove(cur);
                                                }
                                            }
                                            _ => {}
                                        }
                                        redraw_at(&buf, cur);
                                    }
                                    _ => {}
                                }
                                esc_state = 0;
                            } else if !(0x20..=0x3F).contains(&c) {
                                esc_state = 0;
                            }
                        }
                        3 => {
                            esc_state = 0;
                        }
                        _ => {}
                    }
                    continue;
                }
                if c == 0x1B {
                    esc_state = 1;
                    continue;
                }
                if c == 0x09 {
                    tab_complete(&mut buf);
                    cur = buf.len();
                    continue;
                }
                if c == b'\n' || c == b'\r' {
                    break;
                } else if c == 0x7F || c == 0x08 {
                    if cur > 0 {
                        buf.remove(cur - 1);
                        cur -= 1;
                        redraw_at(&buf, cur);
                    }
                } else if c >= 0x20 {
                    if cur == buf.len() {
                        buf.push(c);
                        cur += 1;
                        let single = [c];
                        out(&single);
                    } else {
                        buf.insert(cur, c);
                        cur += 1;
                        redraw_at(&buf, cur);
                    }
                }
            }
            Ok(_) => {}
            Err(_) => {
                let _ = yield_now();
            }
        }
    }
    history_push(&buf);
    out(b"\n");
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_smart_case_match() {
        // 全小写前缀：模糊匹配（忽略大小写）
        assert!(smart_case_match(b"ver", b"version"));
        assert!(smart_case_match(b"ver", b"VERSION"));
        assert!(smart_case_match(b"ps", b"ps"));
        assert!(smart_case_match(b"ps", b"PS_FLAG"));
        assert!(smart_case_match(b"cat", b"cat"));
        assert!(smart_case_match(b"ls", b"ls"));

        // 含有大写前缀：严格区分大小写
        assert!(smart_case_match(b"Ver", b"Version"));
        assert!(!smart_case_match(b"Ver", b"version"));
        assert!(smart_case_match(b"PS", b"PS_FLAG"));
        assert!(!smart_case_match(b"PS", b"ps"));
    }
}
