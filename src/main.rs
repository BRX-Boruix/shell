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
    // `PATH` 内置默认（仅当用户/环境未定义时写入一次）。必须在任何分发之前——
    // `--run=` 的非交互路径同样要用 PATH 查找（如 `tcc hello.c -o hello`）。
    crate::env::init_path_default();
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

/// 后台作业输出文件目录（/scratch 的符号链接，启动期已确保存在）。
const JOB_OUT_DIR: &[u8] = b"/tmp";

/// 判断命令词序列里是否已含输出重定向操作符。
///
/// 有则**尊重显式重定向**——用户明确写了 `>`/`>>` 时，作业输出的处置策略
/// 已经由用户决定，自动追加反而会**覆盖**用户的意图（后面的 `>` 会取代
/// 前面的，内层 `split_redirects` 按「最后一条生效」之前的语义是全部生效、
/// 但两条 stdout 重定向只有一条真正留在 fd 1 上）。
fn has_stdout_redirect(cmd: &[u8]) -> bool {
    // 逐词扫描太重（这里只有一条字节串）；够用的近似：找独立的 " > "/" >> " 
    // 边界。为避免误伤文件名里的 `>`，检查空格包围的形态。
    let mut i = 0usize;
    while i + 1 < cmd.len() {
        if cmd[i] == b' ' && (cmd[i + 1] == b'>') {
            // " >" 或 " >>"（后跟空格或串尾）。
            let after = cmd.get(i + 2);
            match after {
                None => return true, // "cmd >"（内层会报语法错，但策略上不再追加）
                Some(b' ') => return true,
                Some(_) => {}
            }
        }
        i += 1;
    }
    false
}

fn spawn_background(cmd: &[u8]) {
    // ==== J-TOKEN-C（ADR-043 决策 2 / §2.1）：令牌移交的用户态策略 ====
    //
    // **策略**：后台作业**不持前台输出令牌**——其输出默认**转存文件**
    // （`/tmp/job<N>.out`，截断式），前台交互输出保持无混杂。
    // 用户显式写了 `>`/`>>` 时尊重用户的选择（见 [`has_stdout_redirect`]）。
    //
    // **为何这是用户态的事**（ADR-043 §2.1 成文约束）：处置通道是「文件」，
    // 由 shell（用户态）分配与持有；内核 console 只照单直出，**不提供队列**，
    // 本改动**零内核改动**。这与 `jobs` 表（用户态维护）同族。
    //
    // **移交语义**：前台子进程运行期间（`exec_via_path` 的等待循环）输出令牌
    // 事实归它——shell 自己不写；后台作业从 spawn 起就被本策略**剥夺**直写
    // console 的通道（fd 1 指向文件），其 `console_owner` 真值（fd1 节点）
    // 因继承 shell 的 fd 表而仍是 shell 的 pid——该真值如实反映「令牌没给
    // 后台」，策略与真值自洽。
    let redirected: alloc::vec::Vec<u8> = if has_stdout_redirect(cmd) {
        cmd.to_vec()
    } else {
        let mut v = cmd.to_vec();
        v.extend_from_slice(b" > ");
        v.extend_from_slice(JOB_OUT_DIR);
        v.extend_from_slice(b"/job");
        // 作业号在 job_add 之后才知道，但 exec 在前——先按「下一个槽位」预估。
        // 竞态窗口（两次并发 spawn_background）在本 shell 的单线程 REPL 中
        // 不存在：spawn_background 只从 repl_loop 串行调用。
        let n = crate::commands::next_job_index();
        let mut nb = [0u8; 24];
        v.extend_from_slice(util::u64_to_dec(n as u64, &mut nb));
        v.extend_from_slice(b".out");
        v
    };
    match exec(PROG_SHELL, &redirected) {
        Ok(pid) => {
            let idx = job_add(pid as u32, cmd);
            let mut b = [0u8; 24];
            out(b"[");
            out(u64_to_dec(idx as u64, &mut b));
            out(b"] ");
            out(u64_to_dec(pid, &mut b));
            out(b" ");
            out(cmd);
            if !has_stdout_redirect(cmd) {
                out(b"  [output: /tmp/job");
                out(u64_to_dec(idx as u64, &mut b));
                out(b".out]");
            }
            out(b"\n");
        }
        Err(_) => {
            out(b"boruix: background exec failed\n");
        }
    }
}
