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
                // `^C` 在空闲提示符处被按下：作废当前行即可。
                //
                // **为何这里不发信号**：此刻 **没有前台子进程**——
                // shell 正在自己读键盘。`libline` 的
                // `interrupt_target` 在前台等待期间才设为 `Some(child)`，
                // 而那段时间里 shell 压根不在 `repl_loop`（在
                // `exec_via_path` 里），故那一路由 `probe_interrupt` 处理。
                // 两边合起来才是完整的「随时可打断」。
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

    /// 从 fd 0 取一个字节，**键盘空闲时原地重试**（不返回 `false` 去让出 CPU）。
    ///
    /// # 为何必须重试而不是让出（§6.7 的真实缺陷，L-4 实测复现）
    ///
    /// 本内核的键盘阻塞是**单等待者**语义：`read` 空读经 `block_for_kbd`
    /// 以 CAS 登记 `KBD_WAITER` 并挂起自己；键盘中断经 `wake_kbd` 把
    /// **登记过的那个 pid** 放回就绪队列（`kernel/crates/task/src/scheduler.rs`
    /// 的 `block_for_kbd` / `wake_kbd`）。
    ///
    /// 而 `libsys::yield_now()` 走 `SYS_TASK_WAIT(0,0)`——**纯让出，不登记
    /// `KBD_WAITER`**。若本方法在无键可读时返回 `false`，`libline` 就
    /// `yield_now()` 空转：此时**没人登记等待**，`wake_kbd` 无处可唤醒，
    /// 击键只能躺在键盘队列里直到下一次 `read` 恰被调用。
    ///
    /// **实测症状**（真实 QEMU + 真实 PS/2 按键）：以 0.3s/键的节奏输入
    /// `echo hi`，屏幕上只出现 `echo h`——**最后一键丢失**；再按回车
    /// **不执行**。L-2 用 0.14s/键时侥幸没踩到（节奏越慢越容易丢），
    /// 所以这个缺陷此前一直潜伏。
    ///
    /// 修法 = 在 `WouldBlock` 上 `continue`（保持在内核里登记为键盘等待者）。
    /// 这与 L-3 对 `login` 的修法一致，理由见 `docs/TODO/terminal-input.md` §6.7。
    fn refill(&mut self) -> bool {
        let mut one = [0u8; 1];
        loop {
            match libsys::read(STDIN, &mut one) {
                Ok(1) => {
                    self.inner.push_bytes(&one);
                    return true;
                }
                // 键盘空闲：**继续重试**，不要把控制权让出去。
                Err(libsys::Error::WouldBlock) => continue,
                // 真错误 / 诚实 EOF：如实报「没有新增字节」。
                _ => return false,
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
