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
//! - `main`    : 入口、REPL 循环、行读取

#![no_std]
#![no_main]

mod commands;
mod env;
mod tokenize;
mod util;

use crate::commands::exec_line;
use crate::util::{out, outln, prompt};
use libsys::{read, write, yield_now, STDOUT};

/// 标准输入文件描述符。
const STDIN: u64 = 0;

/// shell 入口（libsys `_start` 调用）：输出横幅并进入 REPL 循环。
/// 返回退出码。
#[unsafe(no_mangle)]
pub extern "C" fn user_main(_argc: isize, _argv: *const *const u8) -> i32 {
    outln(b"BORUIX shell (PID 2)");
    repl_loop();
    0
}

/// REPL（Read-Eval-Print-Loop）主循环：读一行 → 执行。
///
/// 经 `read` 系统调用从内核键盘输入缓冲逐字符读行；缓冲空（`WouldBlock`）时
/// 让出 CPU 并重试（键盘是异步中断驱动，无输入时不忙等）。空行与 `#` 注释跳过。
fn repl_loop() {
    let mut line = [0u8; 256];
    loop {
        prompt();
        let n = read_line(&mut line);
        if n == 0 {
            // 读到空行（直接回车）：继续下一轮。
            continue;
        }
        exec_line(&line[..n]);
    }
}

/// 从标准输入读一行（直到 `\n` 或缓冲满），返回有效长度（不含 `\n`）。
///
/// 逐字符 `read(0, &1)`：内核键盘缓冲有字符则回显并暂存；空则返回
/// `WouldBlock`（`Err`），此处先让出 CPU 再继续，避免忙等。
fn read_line(buf: &mut [u8]) -> usize {
    let mut n = 0;
    // 转义序列丢弃状态机：键盘驱动对方向键/编辑键/F 键输出 ANSI 序列（如 `↑`→
    // `\x1b[A`，Insert→`\x1b[2~`，F1→`\x1bOP`）。此处吞掉整个序列，避免污染命令行
    // （行编辑留作后续；现阶段这些键仅被忽略而不报错）。
    // 0=普通 1=已遇 ESC 2=CSI 参数/中间字节 3=SS3 单字节终结。
    let mut esc_state: u8 = 0;
    while n < buf.len() {
        let mut one = [0u8; 1];
        match read(STDIN, &mut one) {
            Ok(got) if got == 1 => {
                let c = one[0];
                // 正处于转义序列中：按状态机吞掉剩余字节。
                if esc_state != 0 {
                    match esc_state {
                        1 => {
                            // ESC 后：期待引导字节 '['(CSI) 或 'O'(SS3)。
                            if c == b'[' {
                                esc_state = 2;
                            } else if c == b'O' {
                                esc_state = 3;
                            } else {
                                esc_state = 0; // 未知引导，停止丢弃（本字节忽略）
                            }
                        }
                        2 => {
                            // CSI：参数(0x30..=0x3F)/中间(0x20..=0x2F)继续，
                            // 终结字节(0x40..=0x7E)结束；其余视为畸形停止。
                            if (0x40..=0x7E).contains(&c) {
                                esc_state = 0;
                            } else if !(0x20..=0x3F).contains(&c) {
                                esc_state = 0;
                            }
                        }
                        3 => {
                            esc_state = 0; // SS3：下一字节即终结
                        }
                        _ => {}
                    }
                    continue;
                }
                if c == 0x1B {
                    // ESC：开始丢弃整个转义序列。
                    esc_state = 1;
                    continue;
                }
                if c == b'\n' || c == b'\r' {
                    break; // 行结束
                } else if c == 0x7F || c == 0x08 {
                    // 退格（DEL 0x7F 或 BS 0x08）：删除上一个已输入字符，
                    // 并向终端发送擦除序列（BS + 空格 + BS）刷新光标。
                    if n > 0 {
                        n -= 1;
                        out(b"\x08 \x08");
                    }
                } else {
                    // 回显 + 暂存
                    let _ = write(STDOUT, &one);
                    buf[n] = c;
                    n += 1;
                }
            }
            Ok(_) => {
                // 读到 0 字节：无更多数据，继续尝试。
            }
            Err(_) => {
                // WouldBlock / 其它：键盘尚未就绪或缓冲空，让出 CPU 重试。
                let _ = yield_now();
            }
        }
    }
    out(b"\n");
    n
}
