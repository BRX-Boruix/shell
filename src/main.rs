//! BORUIX 用户态 shell（命令行解释器，独立可执行程序）。
//!
//! 由 init（PID 1）经 `exec` 系统调用加载运行为 PID 2。libsys 提供 `_start`
//! 入口，本文件导出 `user_main`（进程入口）。
//!
//! 模块划分（按职责解耦，依赖单向无环）：
//! - `util`     : 输出辅助与通用字节/文本工具
//! - `env`      : 纯字符串变量表 + `export`/`env` 命令
//! - `tokenize` : 引号感知分词与 `$VAR` 展开
//! - `commands` : 内建命令与命令分发
//! - `json_tree`: JSON 树状可视化渲染（解析在 libsys，ADR-024）
//! - `main`     : 入口、REPL 循环、行读取（含历史与 Tab 补全）

#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]

#[cfg(test)]
extern crate std;

extern crate alloc;

mod commands;
mod env;
mod json_tree;
mod libc_check;
mod linehost;
mod tokenize;
mod util;

use crate::commands::{exec_line, job_add};
use crate::env::last_status;
use crate::linehost::ShellHost;
use crate::util::{out, outln, u64_to_dec};
use libline::{EditAction, Editor, read_line};
use libsys::{exec, nr::PROG_SHELL};

use alloc::vec::Vec;

/// 标准输入文件描述符。
const STDIN: u64 = 0;


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
///
/// **L-2**：行编辑已改由 `libline` 提供（本文件不再内联编辑器）。
/// 本循环只负责：把 stdin 字节喂给 `ByteSource`、把 `libline` 的决定译为 shell 动作。
fn repl_loop() {
    let mut editor = Editor::new();
    let mut src = StdinSource::default();
    let mut host = ShellHost;
    loop {
        let action = read_line(&mut editor, &mut src, &mut host, |h| {
            // 提示符内容属 shell（用户名 + cwd），故由本层提供；
            // 经 host 的 `write` 输出，与编辑器自己的重绘走同一条通道。
            h.write(b"");
            crate::util::prompt();
        });
        match action {
            EditAction::Submitted(line) => {
                out(b"\n");
                if line.is_empty() {
                    continue;
                }
                match background_split(&line) {
                    Some(cmd) => spawn_background(cmd),
                    None => exec_line(&line),
                }
            }
            EditAction::Interrupted => {
                // L-4 会在此处向前台 child 投递 SIGINT；本点只恢复行。
                out(b"\n");
            }
            EditAction::Eof(_) => {
                out(b"\n");
                continue;
            }
            EditAction::Continue => {}
        }
    }
}

/// stdin 输入源：`ByteSource` + 从 fd 0 取字节的补充逻辑。
///
/// **为何要这层包装**：`ByteSource` 只会解码已被推入的字节，取字节是
/// **调用方**的事（键盘 vs 事件总线由调用方决定）。`libline` 在队列排空时
/// 回调 `InputSource::refill`，本类型就在那里真正调 `read`——这样
/// 「怎么取」留在 shell，「怎么编辑」留在库，两侧互不知道对方细节。
#[derive(Default)]
struct StdinSource {
    inner: libline::ByteSource,
}

impl libline::InputSource for StdinSource {
    fn next_item(&mut self) -> libline::InputItem {
        self.inner.next_item()
    }

    /// 从 fd 0 尽力取字节（非阻塞：无键可读即停）。
    ///
    /// 返回「是否新增了字节」——`libline` 据此决定立刻重试还是让出 CPU。
    /// `WouldBlock`（键盘空闲）与 `Ok(0)`（诚实 EOF）都如实报「没有新增」，
    /// **不**伪装成有数据。
    fn refill(&mut self) -> bool {
        let mut got = false;
        loop {
            let mut one = [0u8; 1];
            match libsys::read(STDIN, &mut one) {
                Ok(1) => {
                    self.inner.push_bytes(&one);
                    got = true;
                }
                _ => return got,
            }
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
