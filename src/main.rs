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

use crate::commands::{command_names, exec_line, job_add};
use crate::env::{for_each_env_name, last_status};
use crate::util::{out, outln, prompt, u64_to_dec};
use libsys::{exec, read, yield_now, nr::PROG_SHELL};

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
pub extern "C" fn user_main(argc: isize, argv: *const *const u8) -> i32 {
    // 一次性命令模式：内核 `exec(shell, cmd)` 启动时 `argc>=1`，`argv[0]` 即命令行。
    // 执行该命令后退出（不进 REPL），用于后台作业。退出码沿用最后一条命令的 `$?`。
    if argc >= 1 && !argv.is_null() {
        let cmd = unsafe { *argv };
        if !cmd.is_null() {
            let mut buf = [0u8; LINE_CAP];
            let mut n = 0usize;
            unsafe {
                let mut p = cmd;
                while n < LINE_CAP && *p != 0 {
                    buf[n] = *p;
                    n += 1;
                    p = p.add(1);
                }
            }
            if n > 0 {
                exec_line(&buf[..n]);
            }
            return last_status() as i32;
        }
    }
    outln(b"BORUIX shell (PID 2)");
    repl_loop();
    0
}

/// REPL（Read-Eval-Print-Loop）主循环：读一行 → 执行。
///
/// 经 `read` 系统调用从内核键盘输入缓冲逐字符读行；缓冲空（`WouldBlock`）时
/// 让出 CPU 并重试（键盘是异步中断驱动，无输入时不忙等）。空行与 `#` 注释跳过。
/// 行尾独立的 `&`（非 `&&`）表示后台作业：另起一个 shell 进程执行该命令，
/// 当前 shell 立即返回继续交互（`spawn_background`）。
fn repl_loop() {
    let mut line = [0u8; LINE_CAP];
    loop {
        prompt();
        let n = read_line(&mut line);
        if n == 0 {
            // 读到空行（直接回车）：继续下一轮。
            continue;
        }
        let raw = &line[..n];
        match background_split(raw) {
            Some(cmd) => spawn_background(cmd),
            None => exec_line(raw),
        }
    }
}

/// 解析行尾的 `&`：若行尾（跳过空白）为单个 `&` 且前一词不是 `&&`，返回其前的
/// 命令文本（已去尾空白）；否则返回 `None`（前台执行）。引号内 `&` 不计（简化：
/// 后台作业命令一般不含引号嵌套 `&`）。
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
    // `&&` 视为非后台（脚本与/列表，本 shell 未实现，仍按前台处理）。
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

/// 启动后台作业：以 `exec(shell, cmd)` 拉起一个独立 shell 进程执行 `cmd`，
/// 登记作业表后打印 `[job] pid cmd`，立即返回（不等待）。新进程跑完自动退出。
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

/// 重绘当前行并把**硬件光标**精确停在 `cursor` 列（`0..=n`，从行首算起的字节索引）。
///
/// 采用标准硬件光标（不使用软件反色光标）：先整行重绘（`\r` + 清到行尾），再把
/// 硬件光标从行尾左移 `(n - cursor)` 格，使其落在编辑位置。这样无论终端是否支持
/// 隐藏光标（`?25l`），始终只有一个光标且位置正确；块光标本身即高亮所在字符，
/// 不会出现「光标卡在原地 / 多个光标」的问题。
fn redraw_at(buf: &[u8], n: usize, cursor: usize) {
    out(b"\r"); // 回到列首
    out(b"\x1b[K"); // 清除至行尾
    prompt();
    out(&buf[..n]); // 整行重绘，硬件光标现在在行尾
    // 左移 (n - cursor) 格，使硬件光标停在编辑位置。
    let back = n - cursor;
    if back > 0 {
        let mut seq = [0u8; 8];
        let mut i = 0;
        seq[i] = b'\x1b'; i += 1;
        seq[i] = b'['; i += 1;
        if back >= 10 {
            seq[i] = b'0' + (back / 10) as u8; i += 1;
        }
        seq[i] = b'0' + (back % 10) as u8; i += 1;
        seq[i] = b'D'; i += 1;
        out(&seq[..i]);
    }
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
        redraw_at(buf, *n, *n);
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
    redraw_at(buf, *n, *n);
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
    redraw_at(buf, *n, *n);

    // 多个候选：列出全部。
    if mcnt > 1 {
        out(b"\n");
        for m in &matches[..mcnt] {
            out(m);
            out(b" ");
        }
        out(b"\n");
        redraw_at(buf, *n, *n);
    }
}

/// 从标准输入读一行（直到 `\n` 或缓冲满），返回有效长度（不含 `\n`）。
///
/// 逐字符 `read(0, &1)`：内核键盘缓冲有字符则回显并暂存；空则返回
/// `WouldBlock`（`Err`），此处先让出 CPU 再继续，避免忙等。
///
/// 支持完整行编辑：光标左右移动（`←`/`→`）、行首/行尾（`Home`/`End`）、在光标处
/// 插入与 `Backspace`/`Delete` 删除、`↑`/`↓` 历史回溯、`Tab` 补全；方向键/编辑键/F 键
/// 输出的 ANSI 转义序列在此被识别或吞掉，不污染命令行。
fn read_line(buf: &mut [u8]) -> usize {
    let mut n = 0; // 当前行长度
    let mut cur = 0; // 光标位置（0..=n，字节索引）
    let mut off: Option<usize> = None; // 历史回溯偏移（None=新鲜输入）
    let mut draft = [0u8; LINE_CAP];
    let mut draft_len = 0usize;
    let mut csi_param: u8 = 0; // CSI 数字参数（Home/End/Delete 识别用）
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
                                csi_param = 0;
                            } else if c == b'O' {
                                esc_state = 3;
                            } else {
                                esc_state = 0; // 未知引导，停止丢弃（本字节忽略）
                            }
                        }
                        2 => {
                            // CSI：参数/中间字节继续，终结字节(0x40..=0x7E)触发动作。
                            if c.is_ascii_digit() {
                                csi_param = c; // 仅取首个数字参数（1/3/4）
                            } else if (0x40..=0x7E).contains(&c) {
                                match c {
                                    b'A' => {
                                        history_prev(buf, &mut n, &mut off, &mut draft, &mut draft_len);
                                        cur = n; // 整行替换，光标置尾
                                    }
                                    b'B' => {
                                        history_next(buf, &mut n, &mut off, &mut draft, &mut draft_len);
                                        cur = n;
                                    }
                                    b'C' => {
                                        if cur < n {
                                            cur += 1;
                                        }
                                        redraw_at(buf, n, cur);
                                    }
                                    b'D' => {
                                        if cur > 0 {
                                            cur -= 1;
                                        }
                                        redraw_at(buf, n, cur);
                                    }
                                    b'H' => {
                                        cur = 0;
                                        redraw_at(buf, n, cur);
                                    }
                                    b'F' => {
                                        cur = n;
                                        redraw_at(buf, n, cur);
                                    }
                                    b'~' => {
                                        // 数字参数：1=Home 3=Delete 4=End。
                                        match csi_param {
                                            b'1' => cur = 0,
                                            b'4' => cur = n,
                                            b'3' => {
                                                if cur < n {
                                                    // 删除光标处字符（左移）。
                                                    let mut k = cur;
                                                    while k + 1 < n {
                                                        buf[k] = buf[k + 1];
                                                        k += 1;
                                                    }
                                                    n -= 1;
                                                }
                                            }
                                            _ => {}
                                        }
                                        redraw_at(buf, n, cur);
                                    }
                                    _ => {}
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
                    cur = n; // 补全发生在行尾，光标置尾
                    continue;
                }
                if c == b'\n' || c == b'\r' {
                    break; // 行结束
                } else if c == 0x7F || c == 0x08 {
                    // 退格（DEL 0x7F 或 BS 0x08）：删除光标左侧字符。
                    if cur > 0 {
                        let mut k = cur - 1;
                        while k + 1 < n {
                            buf[k] = buf[k + 1];
                            k += 1;
                        }
                        n -= 1;
                        cur -= 1;
                        redraw_at(buf, n, cur);
                    }
                } else if c >= 0x20 {
                    // 可打印字符：在光标处插入。
                    if n < buf.len() {
                        let mut k = n;
                        while k > cur {
                            buf[k] = buf[k - 1];
                            k -= 1;
                        }
                        buf[cur] = c;
                        n += 1;
                        cur += 1;
                        redraw_at(buf, n, cur);
                    }
                }
                // 其余控制字符（其它）忽略。
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
