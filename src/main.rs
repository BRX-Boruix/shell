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

#![no_std]
#![no_main]

mod commands;
mod env;
mod tokenize;
mod util;

use crate::commands::{command_names, exec_line};
use crate::env::for_each_env_name;
use crate::util::{out, outln, prompt};
use libsys::{read, write, yield_now, STDOUT};

/// 标准输入文件描述符。
const STDIN: u64 = 0;

/// 一行输入的最大字节数（与 `repl_loop` 的缓冲对齐）。
const LINE_CAP: usize = 256;
/// 命令历史保存的最大条数（环形缓冲，无文件系统依赖）。
const HIST_MAX: usize = 32;

/// 命令历史环形缓冲：每条为定长 `LINE_CAP` 字节，以首字节 0 标记结尾。
static mut HIST: [[u8; LINE_CAP]; HIST_MAX] = [[0u8; LINE_CAP]; HIST_MAX];
/// 累计压入历史的条数（可超过 `HIST_MAX`；经取模定位槽位）。
static mut HIST_COUNT: usize = 0;

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
    let mut line = [0u8; LINE_CAP];
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

/// 重绘当前行：回到行首、清到行尾、重印提示符与缓冲内容。
/// 用于历史回溯与补全后刷新显示（光标始终在行尾）。
fn redraw(buf: &[u8], n: usize) {
    out(b"\r"); // 回到列首
    out(b"\x1b[K"); // 清除至行尾
    prompt();
    out(&buf[..n]);
}

/// 把一行压入历史（空行或与前一条重复则忽略）。
fn history_push(line: &[u8]) {
    if line.is_empty() {
        return;
    }
    unsafe {
        let c = HIST_COUNT;
        let idx = c % HIST_MAX;
        let l = line.len().min(LINE_CAP);
        HIST[idx][..l].copy_from_slice(&line[..l]);
        if l < LINE_CAP {
            HIST[idx][l] = 0;
        }
        if c > 0 {
            let prev = (c - 1) % HIST_MAX;
            let mut same = true;
            for k in 0..LINE_CAP {
                if HIST[idx][k] != HIST[prev][k] {
                    same = false;
                    break;
                }
                if HIST[idx][k] == 0 {
                    break;
                }
            }
            if same {
                return; // 与最新一条相同，不重复记录
            }
        }
        HIST_COUNT = c + 1;
    }
}

/// 回溯到更旧的一条历史（`↑`）。首次离开「新鲜输入」时保存草稿，便于 `↓` 还原。
fn history_prev(
    buf: &mut [u8],
    n: &mut usize,
    off: &mut Option<usize>,
    draft: &mut [u8; LINE_CAP],
    draft_len: &mut usize,
) {
    unsafe {
        if HIST_COUNT == 0 {
            return;
        }
        let max_off = HIST_COUNT.min(HIST_MAX);
        match off {
            None => {
                let dl = (*n).min(LINE_CAP);
                draft[..dl].copy_from_slice(&buf[..dl]);
                *draft_len = dl;
                *off = Some(0);
            }
            Some(o) => {
                if *o + 1 < max_off {
                    *o += 1;
                }
            }
        }
        let o = off.unwrap();
        let idx = (HIST_COUNT - 1 - o) % HIST_MAX;
        let mut len = 0;
        while len < LINE_CAP && HIST[idx][len] != 0 {
            len += 1;
        }
        buf[..len].copy_from_slice(&HIST[idx][..len]);
        *n = len;
        redraw(buf, *n);
    }
}

/// 前进到更新的一条历史（`↓`）。到最新后还原此前保存的草稿（新鲜输入）。
fn history_next(
    buf: &mut [u8],
    n: &mut usize,
    off: &mut Option<usize>,
    draft: &mut [u8; LINE_CAP],
    draft_len: &mut usize,
) {
    match off {
        None => return,
        Some(o) => {
            if *o > 0 {
                *o -= 1;
                unsafe {
                    let idx = (HIST_COUNT - 1 - *o) % HIST_MAX;
                    let mut len = 0;
                    while len < LINE_CAP && HIST[idx][len] != 0 {
                        len += 1;
                    }
                    buf[..len].copy_from_slice(&HIST[idx][..len]);
                    *n = len;
                }
            } else {
                *off = None;
                let dl = *draft_len;
                buf[..dl].copy_from_slice(&draft[..dl]);
                *n = dl;
            }
        }
    }
    redraw(buf, *n);
}

/// Tab 补全：补全光标处（行尾）的当前词。
///
/// - 词以 `$` 开头 → 补全环境变量名（保留 `$`）；
/// - 行首词 → 补全内建命令名（补全后追加空格）；
/// - 其它位置的词 → 补全环境变量名。
///
/// 唯一匹配直接替换；多个匹配先扩展到最长公共前缀，并列出全部候选。
fn tab_complete(buf: &mut [u8], n: &mut usize) {
    // 当前正在输入的词：以空白分隔的最后一个词。
    let mut ws = 0usize;
    for k in 0..*n {
        if buf[k] == b' ' || buf[k] == b'\t' {
            ws = k + 1;
        }
    }
    let word = &buf[ws..*n];
    let (prefix, with_dollar, is_cmd) = if word.first() == Some(&b'$') {
        (&word[1..], true, false)
    } else if ws == 0 {
        (word, false, true)
    } else {
        (word, false, false)
    };

    // 收集匹配候选。
    let mut matches: [&[u8]; 48] = [&b""[..]; 48];
    let mut mcnt = 0usize;
    if is_cmd {
        for name in command_names() {
            if name.len() >= prefix.len() && &name[..prefix.len()] == prefix {
                matches[mcnt] = name;
                mcnt += 1;
            }
        }
    } else {
        for_each_env_name(|name| {
            if mcnt < matches.len()
                && name.len() >= prefix.len()
                && &name[..prefix.len()] == prefix
            {
                matches[mcnt] = name;
                mcnt += 1;
            }
        });
    }
    if mcnt == 0 {
        return; // 无匹配：静默
    }

    // 最长公共前缀（所有候选都包含 prefix，故 lcp >= prefix.len()）。
    let mut lcp = prefix.len();
    'outer: while lcp < matches[0].len() {
        let b = matches[0][lcp];
        for m in &matches[1..mcnt] {
            if lcp >= m.len() || m[lcp] != b {
                break 'outer;
            }
        }
        lcp += 1;
    }

    // 构造替换文本。
    let mut rb = [0u8; 80];
    let mut rl = 0usize;
    if with_dollar {
        rb[rl] = b'$';
        rl += 1;
    }
    if mcnt == 1 {
        let c = matches[0];
        let take = c.len().min(rb.len() - rl);
        rb[rl..rl + take].copy_from_slice(&c[..take]);
        rl += take;
        if is_cmd {
            // 命令补全后补一个空格，方便继续输入参数。
            if rl < rb.len() {
                rb[rl] = b' ';
                rl += 1;
            }
        }
    } else {
        let take = lcp.min(rb.len() - rl);
        rb[rl..rl + take].copy_from_slice(&matches[0][..take]);
        rl += take;
    }

    // 替换当前词。
    if ws + rl <= buf.len() {
        buf[ws..ws + rl].copy_from_slice(&rb[..rl]);
        *n = ws + rl;
    }
    redraw(buf, *n);

    // 多个候选：列出全部。
    if mcnt > 1 {
        out(b"\n");
        for m in &matches[..mcnt] {
            out(m);
            out(b" ");
        }
        out(b"\n");
        redraw(buf, *n);
    }
}

/// 从标准输入读一行（直到 `\n` 或缓冲满），返回有效长度（不含 `\n`）。
///
/// 逐字符 `read(0, &1)`：内核键盘缓冲有字符则回显并暂存；空则返回
/// `WouldBlock`（`Err`），此处先让出 CPU 再继续，避免忙等。
///
/// 支持：退格（删除行尾字符）、`↑`/`↓` 历史回溯、Tab 补全；方向键/编辑键/F 键
/// 输出的 ANSI 转义序列在此被识别或吞掉，不污染命令行。
fn read_line(buf: &mut [u8]) -> usize {
    let mut n = 0;
    let mut off: Option<usize> = None; // 历史回溯偏移（None=新鲜输入）
    let mut draft = [0u8; LINE_CAP];
    let mut draft_len = 0usize;
    // 转义序列丢弃状态机：键盘驱动对方向键/编辑键/F 键输出 ANSI 序列（如 `↑`→
    // `\x1b[A`，Insert→`\x1b[2~`，F1→`\x1bOP`）。此处按状态机解析或吞掉。
    // 0=普通 1=已遇 ESC 2=CSI 参数/中间字节 3=SS3 单字节终结。
    let mut esc_state: u8 = 0;
    while n < buf.len() {
        let mut one = [0u8; 1];
        match read(STDIN, &mut one) {
            Ok(got) if got == 1 => {
                let c = one[0];
                // 正处于转义序列中：按状态机处理剩余字节。
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
                            // CSI：终结字节(0x40..=0x7E)触发动作；参数/中间字节继续。
                            if (0x40..=0x7E).contains(&c) {
                                match c {
                                    b'A' => history_prev(buf, &mut n, &mut off, &mut draft, &mut draft_len),
                                    b'B' => history_next(buf, &mut n, &mut off, &mut draft, &mut draft_len),
                                    _ => {} // ←/→/Home/End 等：暂忽略
                                }
                                esc_state = 0;
                            } else if !(0x20..=0x3F).contains(&c) {
                                esc_state = 0;
                            }
                        }
                        3 => {
                            esc_state = 0; // SS3：下一字节即终结（F1-F4 等，忽略）
                        }
                        _ => {}
                    }
                    continue;
                }
                if c == 0x1B {
                    // ESC：开始解析转义序列。
                    esc_state = 1;
                    continue;
                }
                if c == 0x09 {
                    // Tab：补全当前词。
                    tab_complete(buf, &mut n);
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
    history_push(&buf[..n]);
    out(b"\n");
    n
}
