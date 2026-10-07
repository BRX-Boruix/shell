//! 内建命令实现与命令分发。
//!
//! 各 `cmd_*` 对应一条 shell 内建命令；`exec_echo` 处理 `echo`；
//! `exec_line` 负责分词、参数重建并把命令名分派到对应实现。
//! 支持基础文件操作：`ls`, `cat`, `mkdir`, `touch`, `rm` 以及 `--json` 输出模式。

/// 所有内建命令名（供 Tab 补全使用）。
pub(crate) const COMMANDS: &[&[u8]] = &[
    b"echo", b"help", b"now", b"time", b"uptime", b"version", b"uname", b"cpu", b"sleep",
    b"clear", b"env", b"export", b"unset", b"ps", b"kill", b"signal", b"alias", b"unalias",
    b"which", b"jobs", b"jobout", b"ls", b"cat", b"mkdir", b"touch", b"rm", b"tree", b"jtree", b"cd",
    b"pwd", b"pipe", b"libccheck", b"poweroff", b"reboot", b"uiodemo", b"driver", b"selftest",
];

/// 返回内建命令名列表（供补全遍历）。
pub(crate) fn command_names() -> &'static [&'static [u8]] {
    COMMANDS
}

/// 【R13 后续（实测定案）】文件打开失败的**诚实错误文案**：按 errno 类别
/// 区分，不再一律显示 "No such file or directory"。实测成因：alice 读
/// 0600 root 属主的 /config/shadow.json 被 DAC 正确拒绝（内核
/// check_access 返回 EACCES=13，libsys read_to_end 如实透传），但本 shell
/// 曾把所有 Err(_) 统一显示成 NotFound——用户无法区分"文件不存在"与
/// "无权读取"，且后者正是权限模型工作的证据，被文案吞掉了。
/// 文案纪律（本仓无 strerror 表，S09 不编造可读字符串）：NotFound 与
/// PermissionDenied 用 POSIX 惯用短语（这两类占文件访问失败的绝大多数，
/// 且文案在 man page 语义上稳定）；其余类别打 `errno=N`（与本文件
/// kill/acee2e/driver 的既有风格一致）。
fn file_error_text(e: libsys::Error) -> &'static [u8] {
    match e {
        libsys::Error::NotFound => b"No such file or directory",
        libsys::Error::PermissionDenied => b"Permission denied",
        _ => b"error (see errno)",
    }
}

/// 打印 `<prefix><path>': <file_error_text>\n`（供 ls/jtree 等共用）。
fn print_file_error(prefix: &[u8], path: &[u8], e: libsys::Error) {
    out(prefix);
    out(path);
    out(b"': ");
    out(file_error_text(e));
    let mut b = [0u8; 24];
    out(b" (errno ");
    out(u64_to_dec(e.to_errno() as u64, &mut b));
    out(b")\n");
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

/// 预览下一个作业号（不登记）。供 `spawn_background` 在 `job_add` 之前
/// 构造输出文件名用——文件名必须进命令行，而命令行先于登记执行。
/// 单线程 REPL 串行调用保证「预览值 = 随后 `job_add` 的返回值」。
pub(crate) fn next_job_index() -> usize {
    let jobs = JOBS.lock();
    jobs.len() + 1
}

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

/// `jobout <n>`：读第 n 个后台作业的输出文件并回显到前台（J-TOKEN-C）。
///
/// 这是「后台输出按策略处置」的**读回**通道：后台输出默认转存
/// `/tmp/job<n>.out`（见 `main.rs::spawn_background`），本命令把它（或其
/// 自 `--tail` 之前的全部内容）取回前台。文件不存在 = 作业尚无输出
/// 或用户显式重定向到了别处——两种情况都如实回显空并返回 0（文件缺失
/// 不算命令失败：作业刚 spawn 还没写属常态）。
fn cmd_jobout(arg: &[u8]) -> u8 {
    let arg = trim_bytes(arg);
    let Some(n) = parse_u64(arg) else {
        out(b"usage: jobout <job-number>\n");
        return 2;
    };
    if n == 0 || job_pid(n as usize).is_none() {
        out(b"jobout: no such job\n");
        return 1;
    }
    let mut path: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    path.extend_from_slice(b"/tmp/job");
    let mut nb = [0u8; 24];
    path.extend_from_slice(u64_to_dec(n, &mut nb));
    path.extend_from_slice(b".out");
    // 打开失败（不存在）→ 空输出、成功返回：语义见上。
    let path_str = match core::str::from_utf8(&path) { Ok(s) => s, Err(_) => return 1 };
    let Ok(fd) = open(path_str, OpenFlags::READ_ONLY, Permissions::read_write()) else {
        return 0;
    };
    let mut buf = [0u8; 512];
    loop {
        match libsys::read(fd, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(k) => out(&buf[..k]),
        }
    }
    let _ = libsys::close(fd);
    0
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
        // 渲染行由 libsys::job_lines 单点定义——与宿主测试断言的是**同一段**
        // 逻辑，杜绝「测试测的和实际打印的是两套」（S06）。
        let lines = libsys::job_lines(&alive, job.pid, i + 1);
        let live = alive.iter().any(|p| p.pid == job.pid);
        for line in &lines {
            if line.is_root {
                out(b"[");
                out(u64_to_dec(line.job as u64, &mut b));
                out(b"] ");
                out(u64_to_dec(line.pid as u64, &mut b));
                if live {
                    out(b" Running    ");
                } else {
                    out(b" Done       ");
                }
                out(&job.cmd);
                out(b"\n");
            } else {
                out("    \u{2514} ".as_bytes());
                out(u64_to_dec(line.pid as u64, &mut b));
                out(b"\n");
            }
        }
    }
    0
}

use libsys::nr::{INFO_BOOT_MS, INFO_CPU_COUNT, INFO_VERSION};
use libsys::{
    chdir, close, chmod, dup2, exec_path, getcwd, info, kill, mkdir, now, open, pipe_create, ps,
    read, read_dir, read_to_end, read_wall_clock, sleep, sync_create, sync_delete, sync_wake,
    unlink, waitpid_any, waitpid_any_timeout, write, yield_now, Error, OpenFlags, Permissions,
    PsEntry, STDIN, STDOUT,
    power_off, reboot,
    driver_claim, driver_query, driver_register, driver_unregister,
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
        (b"which", b"locate a builtin/alias/PATH command"),
        (b"jobs", b"list background jobs"),
        (b"jobout", b"show background job output"),
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
        (b"tty", b"report whether fd 0/1/2 are terminals (isatty)"),
    ];
    // 列表与查找是**语法/查找**能力，不是内建命令，故单列说明（否则用户在
    // help 里找不到 `&&` 与 `PATH` 的任何线索）。
    out(b"lists:   A && B   run B only if A succeeded (exit 0)\n");
    out(b"         A || B   run B only if A failed\n");
    out(b"         A ; B    run B unconditionally\n");
    out(b"lookup:  a word without '/' that is not a builtin is searched in $PATH\n");
    out(b"         default PATH: ");
    out(crate::env::DEFAULT_PATH);
    out(b"\n");
    out(b"         override with: export PATH=/dir1:/dir2\n");
    out(b"         each entry is tried as NAME, then NAME.elf\n");
    out(b"\n");
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
/// `poweroff`：请求 ACPI 软关机（S5）。成功后机器断电，永不返回；
/// 电源管理不可用时打印错误并返回 1。
fn cmd_poweroff(arg: &[u8]) -> u8 {
    if !trim_bytes(arg).is_empty() {
        out(b"poweroff: usage: poweroff (no args)\n");
        return 1;
    }
    out(b"poweroff: powering down ...\n");
    match power_off() {
        Ok(()) => 0, // 永不达（成功即断电）
        Err(e) => {
            out(b"poweroff: failed: ");
            out(e.to_string().as_bytes());
            out(b"\n");
            1
        }
    }
}

/// `reboot`：请求系统重启。成功后机器复位，永不返回；不可用时打印错误并返回 1。
fn cmd_reboot(arg: &[u8]) -> u8 {
    if !trim_bytes(arg).is_empty() {
        out(b"reboot: usage: reboot (no args)\n");
        return 1;
    }
    out(b"reboot: restarting ...\n");
    match reboot() {
        Ok(()) => 0, // 永不达（成功即复位）
        Err(e) => {
            out(b"reboot: failed: ");
            out(e.to_string().as_bytes());
            out(b"\n");
            1
        }
    }
}

/// `uiodemo [dev_name]`：演示**用户态驱动（UIO）**全流程——向内核注册认领一个真实
/// 设备、claim 授权后把该设备 MMIO 窗口真实映射进本进程、从映射地址读一个硬件寄存器
/// （volatile 读，证明用户态真的触达了硬件），最后注销。默认认领 QEMU 的 e1000 网卡
/// `pci-ethernet-00-03-0`（PCI BAR 有 MMIO 窗口，可被 UIO 认领）。
fn cmd_uiodemo(arg: &[u8]) -> u8 {
    const DEFAULT_DEV: &[u8] = b"pci-ethernet-00-03-0";
    let name = trim_bytes(arg);
    let name: &[u8] = if name.is_empty() { DEFAULT_DEV } else { name };
    let name_str = core::str::from_utf8(name).unwrap_or("?");
    out(b"uiodemo: target device = ");
    out(name);
    out(b"\n");

    // 1. query：查设备绑定状态（JSON）。
    match driver_query(name_str) {
        Ok(json) => {
            out(b"uiodemo: query -> ");
            out(json.as_bytes());
            out(b"\n");
        }
        Err(e) => {
            out(b"uiodemo: driver_query failed: ");
            out(e.to_string().as_bytes());
            out(b"\n");
            return 1;
        }
    }

    // 2. register：本进程认领该设备，拿 uio_id。
    let uio_id = match driver_register(name_str) {
        Ok(id) => id,
        Err(e) => {
            out(b"uiodemo: driver_register failed: ");
            out(e.to_string().as_bytes());
            out(b"\n");
            return 1;
        }
    };
    out(b"uiodemo: registered (uio_id=");
    {
        let mut b = [0u8; 24];
        out(u64_to_dec(uio_id, &mut b));
    }
    out(b")\n");

    // 3. claim：授权映射 MMIO 窗口，返回用户虚拟地址。
    let va = match driver_claim(uio_id) {
        Ok(v) => v,
        Err(e) => {
            out(b"uiodemo: driver_claim failed: ");
            out(e.to_string().as_bytes());
            out(b"\n");
            let _ = driver_unregister(uio_id);
            return 1;
        }
    };
    out(b"uiodemo: claimed, device MMIO mapped at user 0x");
    {
        let mut b = [0u8; 24];
        out(u64_to_dec(va, &mut b));
    }
    out(b" (dec)\n");

    // 4. 从映射地址 volatile 读一个 32 位硬件寄存器（offset 0），证明用户态触达硬件。
    let reg: u32 = unsafe { core::ptr::read_volatile(va as *const u32) };
    out(b"uiodemo: read dev reg[0] = ");
    {
        let mut b = [0u8; 24];
        out(u64_to_dec(reg as u64, &mut b));
    }
    out(b" (dec)\n");

    // 5. unregister：释放认领。
    match driver_unregister(uio_id) {
        Ok(()) => out(b"uiodemo: unregistered ok\n"),
        Err(e) => {
            out(b"uiodemo: driver_unregister failed: ");
            out(e.to_string().as_bytes());
            out(b"\n");
            return 1;
        }
    }
    out(b"uiodemo: PASS - userspace driver registered/claimed/mapped/read a real device\n");
    0
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

    let mut buf = [PsEntry::EMPTY; 32];
    match ps(&mut buf) {
        Ok(n) => {
            let mut entries: Vec<PsEntry> = buf[..n].to_vec();
            // 内核按分桶顺序返回，**不是** pid 序。此处按 pid 排序，使同一次
            // 输出可复现（同样的进程集合得到同样的行序）——否则行序随桶分布
            // 漂移，无法比对、无法验收。
            entries.sort_unstable_by_key(|p| p.pid);
            let entries = &entries[..];
            out(b"PID   PPID  STATE\n");
            let mut b = [0u8; 24];
            for e in entries {
                // 缩进反映与父的关系：init(ppid=0) 顶格，其子进程缩进——这就是
                // ADR-043 支柱 1 的进程树在人类可读输出里的可见形态。
                let depth = libsys::job_depth(entries, e.pid);
                let mut d = 0u32;
                while d < depth && d < 8 {
                    out(b"  ");
                    d += 1;
                }
                out(u64_to_dec(e.pid as u64, &mut b));
                out(b"   ");
                out(u64_to_dec(e.ppid as u64, &mut b));
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

/// `which NAME`：查找命令类型（别名 → 内建 → `PATH` 中的程序）。
///
/// `PATH` 命中时打印**解析出的完整路径**（POSIX `which` 的输出形态），
/// 与真正执行共用 `path_candidate` 同一套规则——不重复实现查找。
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
    // `PATH` 查找：命中则打印**解析出的完整路径**。
    if let Some(cand) = path_candidate(name) {
        out(&cand);
        out(b"\n");
        return 0;
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
        Err(e) => {
            print_file_error(b"ls: cannot access '", path.as_bytes(), e);
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
        Err(e) => {
            out(b"cat: ");
            out(a);
            out(b": ");
            out(file_error_text(e));
            let mut b = [0u8; 24];
            out(b" (errno ");
            out(u64_to_dec(e.to_errno() as u64, &mut b));
            out(b")\n");
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
/// `acee2e`：显式 ACE 通道（A2-6 / ADR-040 §3.5.1 G4）的**真实用户态**验收（协调端）。
///
/// 流程：建 `/scratch/acee2e.txt`（0644）→ `exec_path("/programs/acee2e.elf")` 派生子
/// 进程 → `waitpid_any()` 收退出码，断言 0。子进程经 libsys 封装（`set_aces`/
/// `get_aces`/`aces_count`）穿越 syscall 边界读写 ACE，覆盖验收 #19（写入的策略经
/// 求值生效）与 #20（ABI 往返保真），并含容量不足不截断、畸形如实拒绝且不留半套、
/// 清空、以及"写 ACE 不动 classic 三段"五类边界。
///
/// 为何必须走子进程而非 shell 内联：ACE 的授权面是**节点属主或 CAP_OWNER**，而
/// 强制判定发生在内核；用独立进程走完整 exec→syscall→exit 链路，才验证到真实
/// 用户链路（§3.5.4「不得止于单测」）。子进程退出码即失败项编号，便于定位。
///
/// **依赖 `/programs/acee2e.elf` 存在**（liveCD 内嵌 payload 提供，ADR-028 单源）。
/// 缺失时如实报错，不假定成功。定位为开发/验收期诊断，非生产特性。
fn cmd_acee2e() -> u8 {
    let mut b = [0u8; 24];
    // 1. 前置：目标文件必须存在（mkdir 忽略已存在）。子进程只写 ACE，不建文件，
    //    故此处失败即如实上报——不替它兜底。
    let _ = mkdir("/scratch", Permissions::all());
    match open("/scratch/acee2e.txt", OpenFlags::CREATE_OR_TRUNCATE, Permissions::all()) {
        Ok(fd) => { let _ = close(fd); }
        Err(_) => {
            out(b"acee2e: cannot create /scratch/acee2e.txt\n");
            return 1;
        }
    }
    // 明确设为 0644：子进程断言 "set_aces 未扰动 classic 三段" 需已知基线。
    let _ = chmod("/scratch/acee2e.txt", 0o644);
    // 2. 派生测试子进程。
    let child = match exec_path("/programs/acee2e.elf", &[]) {
        Ok(pid) => pid,
        Err(e) => {
            out(b"acee2e: spawn failed errno=");
            out(u64_to_dec(e.to_errno() as u64, &mut b));
            out(b" (/programs/acee2e.elf missing; liveCD supplies it from the built-in\n");
            out(b"        payload)\n");
            return 1;
        }
    };
    out(b"acee2e: spawned pid=");
    out(u64_to_dec(child, &mut b));
    out(b"\n");
    // 3. 收退出码。0 = 全部通过；非零 = 子进程内首个失败项编号。
    //    纪律（实测得出，非推断）：`exec_path` 是**非阻塞**派生（立即返回 pid），
    //    而 `waitpid_any` 在"子进程尚未退出、且当下无法切换"时如实返回 WouldBlock
    //    （errno 11）——**不是**错误，只是"还没有可收的结果"。
    //    故固定次数的 yield 循环是不够的（子进程工作量不定长：实测其跑完需数百次
    //    调度机会，固定额度要么不够、要么白等）。正确形态是**按 WouldBlock 重试**，
    //    直到收到真实退出码或遇到非 WouldBlock 的硬错误。
    //    这与 synce2e 的差别在子进程工作量：synce2e 的等待端一进去就阻塞，父进程
    //    几次 yield 即可；acee2e 的子进程要跑完十余个 syscall，故需循环重试。
    let mut attempt: u32 = 0;
    let wr = loop {
        match waitpid_any() {
            Ok(w) => break w,
            Err(Error::WouldBlock) => {
                // 让出调度机会给子进程，然后重试。上限只为防"子进程永不退出"
                // 造成的无界挂起（如实失败，不静默吞掉）。
                attempt += 1;
                if attempt > 2_000_000 {
                    out(b"acee2e: child did not exit (retry budget exhausted)\n");
                    return 1;
                }
                let _ = yield_now();
            }
            Err(e) => {
                out(b"acee2e: waitpid failed errno=");
                out(u64_to_dec(e.to_errno() as u64, &mut b));
                out(b"\n");
                return 1;
            }
        }
    };
    // 4. 断言退出码与 pid。
    {
        if wr.pid != child {
            out(b"acee2e: waitpid pid=");
            out(u64_to_dec(wr.pid, &mut b));
            out(b" (expected the spawned child)\n");
            return 1;
        }
        if wr.code != 0 {
            out(b"acee2e: FAIL exit=");
            out(u64_to_dec(wr.code, &mut b));
            out(b" (see child output above)\n");
            return 1;
        }
        out(b"acee2e: child exit=0 -- ALL OK (real user-space ACE channel)\n");
    }
    0
}

/// `trave2e`：目录遍历权限（A2-3 / ADR-040 §3.4）的**真实用户态**验收（协调端）。
///
/// 流程：`exec_path("/programs/trave2e.elf")` 派生子进程 → 收退出码断言 0。
/// 子进程在**真实进程上下文**中：建夹具（0700 目录 + 0644 文件）→ 属主穿越成功 →
/// 经 `identity_set` **真实降级**为 uid 2002 → 穿越无 x 目录必须 EACCES → 由属主补 x
/// → 非属主再试应成功（对照）→ 恢复身份并清理。
///
/// 与内核停机测试 `[test-traverse]` 的分工：后者跑在内核态、直接构造 ProcessIdentity，
/// 证明**检查逻辑**正确；本命令走真实用户链路（真实 PCB 身份经 syscall 变更、真实
/// 用户指针、真实路径解析），证明**该检查在真实进程上可达且生效**——二者互补，
/// 缺一不能声称"用户态确实被拦住"（§3.5.4「不得止于单测」）。
///
/// **依赖 `/programs/trave2e.elf` 存在**（liveCD 内嵌 payload，ADR-028 单源）。
fn cmd_trave2e() -> u8 {
    let mut b = [0u8; 24];
    let child = match exec_path("/programs/trave2e.elf", &[]) {
        Ok(pid) => pid,
        Err(e) => {
            out(b"trave2e: spawn failed errno=");
            out(u64_to_dec(e.to_errno() as u64, &mut b));
            out(b" (/programs/trave2e.elf missing; liveCD supplies it from the built-in\n");
            out(b"         payload)\n");
            return 1;
        }
    };
    out(b"trave2e: spawned pid=");
    out(u64_to_dec(child, &mut b));
    out(b"\n");
    // 收退出码：按 WouldBlock 重试（同 acee2e 纪律——`exec_path` 非阻塞派生，
    // 子进程工作量不定长，固定次数 yield 不够；WouldBlock 不是错误，只是"还没结果"）。
    let mut attempt: u32 = 0;
    let wr = loop {
        match waitpid_any() {
            Ok(w) => break w,
            Err(Error::WouldBlock) => {
                attempt += 1;
                if attempt > 2_000_000 {
                    out(b"trave2e: child did not exit (retry budget exhausted)\n");
                    return 1;
                }
                let _ = yield_now();
            }
            Err(e) => {
                out(b"trave2e: waitpid failed errno=");
                out(u64_to_dec(e.to_errno() as u64, &mut b));
                out(b"\n");
                return 1;
            }
        }
    };
    if wr.pid != child {
        out(b"trave2e: waitpid pid=");
        out(u64_to_dec(wr.pid, &mut b));
        out(b" (expected the spawned child)\n");
        return 1;
    }
    if wr.code != 0 {
        out(b"trave2e: FAIL exit=");
        out(u64_to_dec(wr.code, &mut b));
        out(b" (see child output above)\n");
        return 1;
    }
    out(b"trave2e: child exit=0 -- ALL OK (real user-space traversal check)\n");
    0
}

/// `synce2e`：SYNC 域（ADR-032）端到端**阻塞往返**测试（协调端）。
///
/// 流程：create 同步字(id,0) → `exec_path("/programs/synce2e.elf", "waiter:<id>")`
/// 派生子进程（等待端）→ sleep 让子进程在 `sync_wait` 上真实阻塞 →
/// `sync_wake(id,42,1)` 唤醒并预置值 42 → `waitpid_any()` 收子进程退出码断言为 0。
/// 若 `sync_wake` 返回 1，则证明子进程确实被登记为等待者并阻塞过（真实切换往返）。
///
/// **依赖 `/programs/synce2e.elf` 存在**，来源随启动模式而异：
/// liveCD 模式由内核内嵌 payload 提供（ADR-028 单源）；
/// 安装模式由 `systemdisk.img` 的 EXT2 `/programs` 提供
/// （SDK `build --systemdisk` 会写入 `synce2e.elf`，实测确认存在）。
///
/// 因此**两种模式下该命令都可用**。
///
/// > 原注释称"安装模式下不保证存在"，源于把 ADR-029 的"不做 payload 兜底"
/// > 误读为"盘上没有这个文件"（2026-09-12 实测纠正）。
/// > 「不兜底」= 不用内嵌副本遮蔽盘上的内容，不等于盘上没有内容。
/// >
/// > 真正的不确定性来自另一处：**自定义（非 SDK 产）系统盘**上装了什么
/// > 就有什么。故失败仍须如实报错，不能假定必然成功。
/// 定位为开发/验收期诊断，非生产特性。
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
            // 路径不存在时如实报错并给出两条可能的来源（不假定是哪一种）。
            out(b"synce2e: spawn waiter failed errno=");
            out(u64_to_dec(e.to_errno() as u64, &mut b));
            out(b" (/programs/synce2e.elf missing; liveCD supplies it from the built-in\n");
            out(b"         payload, installed mode from the systemdisk.img EXT2 /programs)\n");
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

/// 
/// `audioe2e`：AUDIO 域（plan_audio_vfs.md 批次二）端到端**阻塞往返**测试（协调端）。
///
/// **为何需要它**：内核启动期测试（`test_audio_pipe_a2`）直接调节点与 ring，
/// 无法触达两条关键路径——AUDIO 域四个 syscall 包装本身、以及真正的
/// 阻塞-唤醒往返（`block_for_audio`/`wake_audio` 需要真实进程上下文切换）。
/// 本命令经**真实 syscall** 跑完整往返。
///
/// **顺序是本测试的核心**：
///   1. 先派生 consumer —— 它 attach 后在**空** ring 上调 fetch，真正入睡；
///   2. 父进程再经 VFS 写入一帧 PCM —— 写路径的 notify 唤醒 consumer；
///   3. consumer 醒来逐字节校验、commit、detach，退出 0。
///
/// 若先写数据再派生 consumer，fetch 会立刻拿到数据，**阻塞路径根本没被走到**
/// ——测试会"通过"却什么也没验证（这正是本命令顺序不可调换的原因）。
///
/// **liveCD 专属测试命令（ADR-029）**：`/programs/audioe2e.elf` 依赖内核内嵌
/// 测试 payload；安装模式下 `/programs` 是磁盘 root 的普通目录、不做兜底。
fn cmd_audioe2e() -> u8 {
    let mut b = [0u8; 24];

    // ---- 1. 先派生 consumer，让它 attach 并在空 ring 上阻塞 ----
    let consumer = match exec_path("/programs/audioe2e.elf", b"consumer") {
        Ok(pid) => pid,
        Err(e) => {
            out(b"audioe2e: spawn consumer failed errno=");
            out(u64_to_dec(e.to_errno() as u64, &mut b));
            out(b" (liveCD-only test payload required, ADR-029)\n");
            return 1;
        }
    };
    out(b"audioe2e: spawned consumer pid=");
    out(u64_to_dec(consumer, &mut b));
    out(b"\n");
    // 保持父就绪（yield 而非 sleep）：子进程阻塞时若无就绪同伴可切，
    // block_current_with 会 NotSwitched，阻塞路径就走不到。
    for _ in 0..3000 {
        let _ = yield_now();
    }

    // ---- 2. 父进程经 VFS 写入一帧 PCM，唤醒阻塞中的 consumer ----
    // 与 consumer 端**逐字节一致**的确定性填充（写错则校验失败如实报错）。
    const FRAME: usize = 256;
    let mut frame = [0u8; FRAME];
    for (i, v) in frame.iter_mut().enumerate() {
        *v = ((i * 37) ^ (i >> 3)) as u8;
    }
    // 经**真实 VFS syscall** 打开并写入（shell 是用户态程序，不经内核内部 API）。
    let fd = match open(
        "/devices/audio/dsp",
        OpenFlags::READ_WRITE,
        Permissions::read_write(),
    ) {
        Ok(f) => f,
        Err(_) => { out(b"audioe2e: open dsp failed\n"); return 1; }
    };
    match write(fd, &frame) {
        Ok(n) if n == FRAME => {
            out(b"audioe2e: wrote 256-byte frame (should wake blocked consumer)\n");
        }
        Ok(n) => {
            out(b"audioe2e: short write ");
            out(u64_to_dec(n as u64, &mut b));
            out(b" (expected 256)\n");
            return 1;
        }
        Err(_) => {
            out(b"audioe2e: write failed (consumer attached?)\n");
            return 1;
        }
    }
    // 让被唤醒的 consumer 跑完校验/commit/detach。
    for _ in 0..3000 {
        let _ = yield_now();
    }

    // ---- 3. 收 consumer 退出码断言 0 ----
    match waitpid_any() {
        Ok(wr) if wr.pid == consumer && wr.code == 0 => {
            out(b"audioe2e: consumer exit=0 (blocked, woke, verified, committed)\n");
        }
        Ok(wr) => {
            out(b"audioe2e: consumer pid=");
            out(u64_to_dec(wr.pid, &mut b));
            out(b" exit=");
            out(u64_to_dec(wr.code, &mut b));
            out(b" (expected pid=");
            out(u64_to_dec(consumer, &mut b));
            out(b" exit=0)\n");
            return 1;
        }
        Err(_) => { out(b"audioe2e: waitpid(consumer) failed\n"); return 1; }
    }
    out(b"audioe2e: ALL OK (audio pipe blocking round-trip)\n");
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
            Err(e) => {
                print_file_error(b"jtree: cannot read '", json_text.as_bytes(), e);
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
        Err(e) => {
            out(b"cd: ");
            out(file_error_text(e));
            let mut b = [0u8; 24];
            out(b" (errno ");
            out(u64_to_dec(e.to_errno() as u64, &mut b));
            out(b"): ");
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
                cloexec: false,
                exclusive: false,
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
                cloexec: false,
                exclusive: false,
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
// ==================== 命令列表（`&&` / `||` / `;`）====================

/// 命令列表分隔符（POSIX 的 AND-OR 列表与顺序列表）。
///
/// **必须是独立的词**（前后有空白）——与既有 `|` 完全同一纪律：`tokenize_line`
/// 按空白切词，故 `a && b` 里的 `&&` 是独立的词，而 `a&&b` 是一个词、不会被当作
/// 分隔符。这个选择顺带让**引号天然安全**：`echo "a;b"` 分词后是一个词
/// `"a;b"`，不会被误切——若改为在原始行上找 `;`，就必须自己重做一遍引号处理。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ListSep {
    /// 列表首项（无前驱分隔符）。
    First,
    /// `;`：无条件执行。
    Always,
    /// `&&`：仅当**前一项成功**（退出码 0）时执行。
    OnSuccess,
    /// `||`：仅当**前一项失败**（退出码非 0）时执行。
    OnFailure,
}

/// 把一个词判为列表分隔符（纯函数，边界可穷举）。
pub(crate) fn list_sep_of(word: &[u8]) -> Option<ListSep> {
    match word {
        b";" => Some(ListSep::Always),
        b"&&" => Some(ListSep::OnSuccess),
        b"||" => Some(ListSep::OnFailure),
        _ => None,
    }
}

/// 执行命令列表 `A && B ; C || D`（POSIX 短路语义）。
///
/// 返回**最后实际执行**的那一项的退出状态——被短路跳过的项不改变它。这正是
/// POSIX「列表的退出状态 = 最后一条命令的状态」，也是 `&&`/`||` 能与 `$?` 互相
/// 组合的基础（`a && b || c` 的常见写法依赖它）。
///
/// **空项如实报语法错误**（退出码 2）而不是静默跳过：`a && && b` 若被静默跳过，
/// 用户会以为整行执行成功。唯一例外是**尾随 `;`**（`a ;` 合法，POSIX 允许）。
fn exec_list(words: &[Vec<u8>]) -> u8 {
    // 切段为 `(前导分隔符, 词区间)`。
    let mut items: Vec<(ListSep, &[Vec<u8>])> = Vec::new();
    let mut start = 0usize;
    let mut sep = ListSep::First;
    for (i, w) in words.iter().enumerate() {
        if let Some(s) = list_sep_of(w) {
            items.push((sep, &words[start..i]));
            sep = s;
            start = i + 1;
        }
    }
    items.push((sep, &words[start..]));

    let last = items.len().saturating_sub(1);
    let mut status = 0u8;
    for (i, (sep, seg)) in items.iter().enumerate() {
        if seg.is_empty() {
            // 尾随 `;`：POSIX 允许（`a ;` 等价 `a`）。
            if i == last && *sep == ListSep::Always {
                continue;
            }
            out(b"boruix: syntax error: empty command in list\n");
            return 2;
        }
        let run = match sep {
            ListSep::First | ListSep::Always => true,
            ListSep::OnSuccess => status == 0,
            ListSep::OnFailure => status != 0,
        };
        if run {
            status = exec_words(seg);
        }
    }
    status
}

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
    // 命令列表（`&&` / `||` / `;`）：先于管道切分——它们是**保留分隔符**，
    // 不作为普通命令/参数（与既有 `|` 同一纪律）。
    let status = exec_list(&final_words);
    set_last_status(status);
}

/// 执行一条**已分词**的命令（单命令或管道），返回退出状态。
///
/// 从 `exec_line` 抽出：命令列表（`exec_list`）需要对每一项单独执行、并按退出码
/// 决定是否短路，故「执行一项」必须是可独立调用的单元。
fn exec_words(words: &[Vec<u8>]) -> u8 {
    // 管道 `|`：若存在独立的 `|` 词，按管道执行（pipe-features A3）。
    // 先于单命令分发——`|` 是保留分隔符，不作为普通命令/参数。
    if words.iter().any(|w| w.as_slice() == b"|") {
        return exec_pipeline(words);
    }
    // 单命令路径：先剥离重定向（> / >> / <），剩下的才是指令词。
    let (clean_words, redirs) = match split_redirects(words) {
        Ok(v) => v,
        Err(()) => {
            out(b"boruix: malformed redirect\n");
            return 1;
        }
    };
    if clean_words.is_empty() {
        // 只有重定向没有命令（如 > f）——无可执行指令，如实报错。
        out(b"boruix: missing command before redirect\n");
        return 1;
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

    // 分发：内建名优先；含 `/` 走 VFS 路径装载；其余按 `PATH` 查找
    // （判据集中在 `run_command` 一处）。
    if redirs.is_empty() {
        run_command(name, arg)
    } else {
        match with_redirects(&redirs, || run_command(name, arg)) {
            Ok(s) => s,
            Err(()) => 1, // open/dup2 失败已报错
        }
    }
}

/// 分发单条命令：内建名走内建，含 `/` 的走 VFS 路径装载。
///
/// 供 `exec_line` 与管道各段共用，故管道里也能写 `/programs/xxx.elf`。
///
/// 退出码约定沿用 shell 惯例（与 `run_builtin` 的 127 一致）：
///   * 127 —— 命令未找到（未知内建名）；
///   * 126 —— 找到了但无法执行（不存在 / 非 ELF / 权限不足）。
/// `tty`\uff1a报告 fd 0/1/2 是否终端\uff08**J-TOKEN-A \u2261 T-ISATTY \u771f\u503c\u9a8c\u6536**\uff09\u3002
///
/// \u8fd9\u662f `libc::isatty` \u7684\u7528\u6237\u53ef\u89c1\u9762\uff1a\u8d70\u771f\u5b9e `SYS_STREAM_FSTAT`\uff0c
/// \u771f\u503c\u53d6\u81ea\u8282\u70b9\u81ea\u8ff0\u3002\u91cd\u5b9a\u5411\uff08`> file`\uff09\u4f1a\u7528 `dup2` \u628a fd 1
/// \u6362\u6210\u666e\u901a\u6587\u4ef6\u8282\u70b9\uff0c\u6545\u672c\u547d\u4ee4\u5fc5\u987b\u5982\u5b9e\u6539\u53e3\u2014\u2014
/// \u65e7\u786c\u7f16\u7801 `fd \u2208 {0,1,2} \u2192 1` \u6c38\u8fdc\u8bf4\u4e0d\u5230\u8fd9\u4e00\u70b9\u3002
fn cmd_tty() -> u8 {
    let mut b = [0u8; 24];
    for (fd, name) in [(0u64, &b"stdin"[..]), (1, &b"stdout"[..]), (2, &b"stderr"[..])] {
        out(name);
        out(b": ");
        match libsys::fstat(fd) {
            Ok(info) => {
                if info.is_terminal != 0 {
                    out(b"isatty=1\n");
                } else {
                    out(b"isatty=0\n");
                }
            }
            Err(_) => out(b"isatty=0 (fstat failed)\n"),
        }
    }
    let _ = &mut b;
    0
}

/// 分发单条命令：**内建名 → 内建；含 `/` → VFS 路径装载；其余 → `PATH` 查找**。
///
/// 供 `exec_words` 与管道各段共用，故管道里也能写 `/programs/xxx.elf`。
///
/// 退出码约定沿用 shell 惯例（与 `run_builtin` 的 127 一致）：
///   * 127 —— 命令未找到（未知内建名，且 `PATH` 各目录都没有）；
///   * 126 —— 找到了但无法执行（不存在 / 非 ELF / 权限不足 / 是目录）。
///
/// **`PATH` 查找为什么放在这里而不是 `classify_command` 里**：`classify_command`
/// 只回答「这个词看起来像路径还是像名字」这一**纯语法**问题（可穷举）；「名字该
/// 去哪里找」依赖运行期的 `PATH` 变量，属**策略**，故留在分发层。
fn run_command(name: &[u8], arg: &[u8]) -> u8 {
    if name.is_empty() {
        return 0;
    }
    // 内建名优先，避免「内建名恰好含斜杠」时被路径规则遮蔽（既有纪律）。
    if is_builtin(name) {
        return run_builtin(name, arg);
    }
    // 含 `/` 的词交给内核解析（相对路径也走这里：内核会与 cwd 拼接）。
    if classify_command(name) == Dispatch::Path {
        return exec_via_path(name, arg);
    }
    // 既非内建、又不含 `/` → 按 `PATH` 逐目录查找（POSIX 语义）。
    //
    // 修复前这里直接落到 `run_builtin` 的 unknown command 分支（127），于是
    // 「按名字调用一个不在内建表里的程序」在 shell 里**根本不可能**——
    // 3P6-1 的 `tcc hello.c -o hello` 正是卡在这一条。
    exec_via_search_path(name, arg)
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
        b"jobout" => cmd_jobout(arg),
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
        b"tty" => cmd_tty(),
        b"synce2e" => cmd_synce2e(),
        b"acee2e" => cmd_acee2e(),
        b"trave2e" => cmd_trave2e(),
        b"audioe2e" => cmd_audioe2e(),
        b"libccheck" => crate::libc_check::cmd_libccheck(arg),
        b"poweroff" => cmd_poweroff(arg),
        b"reboot" => cmd_reboot(arg),
        b"uiodemo" => cmd_uiodemo(arg),
        b"driver" => cmd_driver(arg),
        b"selftest" => cmd_selftest(arg),
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
    // 与 `exec_line` 用同一个分发器：管道中也能写路径程序。
    if redirs.is_empty() {
        run_command(name, arg)
    } else {
        match with_redirects(&redirs, || run_command(name, arg)) {
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
// ============================================================================
// driver 命令：运行时驱动安装 / 装载 / 枚举（ADR-037 决策 2/3/4/5，runtime-driver.md
// PRE-3 + P1-2/P1-3/P1-4/P1-5）。
//
// 驱动 = 普通 no_std 用户 ELF + UIO 认领。装载走既有 exec_path（spawn 子进程，父进程
// 不退出）+ driver_register/claim（System-only，PRE-1 门禁）。设备名经 exec 的 cmd
// 字符串（= 子进程 argv[0]）传入驱动 ELF，由驱动内部解析。
//
// 子命令：
//   driver install <srcpath> <name> [dev]  把 srcpath 的驱动 ELF 装进 /modules/<name>/
//                                          （写 driver.elf + manifest.json），dev 缺省
//                                          pci-ethernet-00-03-0。
//   driver list                           枚举 /modules/*/，真实读各 manifest.json 列出。
//   driver load <name>                    读 manifest、把 binds[0] 解析为设备名、
//                                          spawn /modules/<name>/driver.elf（设备名作 cmd）
//                                          并 waitpid 收其退出码。
//   driver status [dev]                   driver_query(dev) 打印绑定态 JSON（看 uio_claimed）。
//
// 诚实契约：每步以底层真实成败为准（read_to_end / write / spawn / waitpid 真值），
// 不伪造"已安装/已认领"。
// ============================================================================

/// 驱动名合法字符白名单（[A-Za-z0-9_-]），禁止 '/' 与 '.' 防路径穿越。
fn valid_driver_name(name: &[u8]) -> bool {
    if name.is_empty() || name.len() > 48 {
        return false;
    }
    name.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// binds 条目合法白名单：具体设备名（含 '.' ':' '-' '_' 字母数字）或以 '\*'
/// 结尾的通配类别前缀（如 "pci-vga-*"）。不允许 '/' 防路径穿越、不允许 '\*' 出现在
/// 非结尾（单一尾部通配）。
fn valid_bind(b: &[u8]) -> bool {
    if b.is_empty() || b.len() > 64 {
        return false;
    }
    let body = if b[b.len() - 1] == b'*' { &b[..b.len() - 1] } else { b };
    if body.is_empty() {
        return false;
    }
    body.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c == b'.' || c == b':')
}

/// 把空格分隔的 arg 切成词（返回 owned，首词为子命令，其后为参数）。
fn split_words(arg: &[u8]) -> Vec<Vec<u8>> {
    let mut words: Vec<Vec<u8>> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut in_word = false;
    for &c in arg {
        if c == b' ' || c == b'\t' {
            if in_word {
                words.push(core::mem::take(&mut cur));
                in_word = false;
            }
        } else {
            cur.push(c);
            in_word = true;
        }
    }
    if in_word {
        words.push(cur);
    }
    words
}

/// 写整个字节序列到文件（create/truncate + write + close）。
fn write_file(path: &str, data: &[u8]) -> Result<(), libsys::Error> {
    let fd = open(path, OpenFlags::CREATE_OR_TRUNCATE, Permissions::read_write())?;
    let mut off = 0usize;
    while off < data.len() {
        let n = write(fd, &data[off..])?;
        if n == 0 {
            break;
        }
        off += n;
    }
    let _ = close(fd);
    Ok(())
}

/// 把一个文件的权限收紧为 system_only（仅 System 可读/可 exec）。shell 是 System。
///
/// A1-7 修复：门禁位经 `GATE_SYSTEM_BIT`（wire bit9）显式写入——旧形态
/// `system_only: true` 经 `to_bits()` 输出 bit3（≤0o7 区），A1-1 线格式迁移后
/// 被内核兼容层误展开为 classic 段（静默失效），此处归位为 ABI 常量直通。
fn make_system_only(path: &str) -> Result<(), libsys::Error> {
    chmod(
        path,
        0o755 | libsys::GATE_SYSTEM_BIT,
    )
}

/// 构造并写 manifest.json：{"name":<name>,"binary":"driver.elf","binds":[<dev>]}。
///
/// name/dev 均经 valid_driver_name 白名单校验（[A-Za-z0-9_-]，设备名另含 ':' '.'），
/// 不含 '"'、'\\'、控制符，故手写 JSON 无需转义、无注入面。读取方（driver load / list）
/// 用 libsys::json::JsonParser 解析同一 schema。
fn write_manifest(dir: &str, name: &str, dev: &str) -> Result<(), libsys::Error> {
    let mut s = alloc::string::String::from("{\"name\":\"");
    s.push_str(name);
    s.push_str("\",\"binary\":\"driver.elf\",\"binds\":[\"");
    s.push_str(dev);
    s.push_str("\"]}");
    write_file(&alloc::format!("{dir}/manifest.json"), s.as_bytes())
}

/// driver install <srcpath> <name> [dev]
fn driver_install(words: &[Vec<u8>]) -> u8 {
    if words.len() < 2 {
        out(b"driver install: usage: driver install <srcpath> <name> [dev]\n");
        return 1;
    }
    let src = words[0].as_slice();
    let name = words[1].as_slice();
    let dev: &[u8] = if words.len() >= 3 {
        words[2].as_slice()
    } else {
        b"pci-ethernet-00-03-0"
    };
    if !valid_driver_name(name) {
        out(b"driver install: invalid driver name (allow [A-Za-z0-9_-], no '/' or '.')\n");
        return 1;
    }
    if !valid_bind(dev) {
        out(b"driver install: invalid bind (device name or trailing-* class, e.g. pci-vga-*)\n");
        return 1;
    }
    let src_str = match core::str::from_utf8(src) {
        Ok(p) => p,
        Err(_) => { out(b"driver install: bad src path\n"); return 1; }
    };
    let name_str = core::str::from_utf8(name).unwrap_or("?");
    let dev_str = core::str::from_utf8(dev).unwrap_or("?");
    let dir = alloc::format!("/modules/{name_str}");
    // 1. 校验源 ELF 可读（read_to_end 真值）。
    let elf = match read_to_end(src_str) {
        Ok(b) => b,
        Err(_) => {
            out(b"driver install: cannot read source ELF: ");
            out(src);
            out(b"\n");
            return 1;
        }
    };
    // 2. 建目录（已存在则容忍，幂等）。
    let _ = mkdir(&dir, Permissions::all());
    // 3. 写 driver.elf + manifest.json。
    let elf_path = alloc::format!("{dir}/driver.elf");
    if write_file(&elf_path, &elf).is_err() {
        out(b"driver install: write driver.elf failed\n");
        return 1;
    }
    if write_manifest(&dir, name_str, dev_str).is_err() {
        out(b"driver install: write manifest.json failed\n");
        return 1;
    }
    // 4. 收紧为 system_only（System-only 写 / system_only 可执行，ADR-037 决策 5）。
    if make_system_only(&elf_path).is_err() {
        out(b"driver install: mark driver.elf system_only failed\n");
        return 1;
    }
    let mpath = alloc::format!("{dir}/manifest.json");
    let _ = make_system_only(&mpath);
    let _ = make_system_only(&dir);
    out(b"driver install: installed -> /modules/");
    out(name);
    out(b" (binary driver.elf, binds=[");
    out(dev);
    out(b"])\n");
    0
}

/// 从 manifest.json 解析出 binds[0]（设备名/通配）。失败返回 None。
fn manifest_first_bind(name: &str) -> Option<alloc::vec::Vec<u8>> {
    let path = alloc::format!("/modules/{name}/manifest.json");
    let bytes = read_to_end(&path).ok()?;
    let text = core::str::from_utf8(&bytes).ok()?;
    let mut parser = libsys::json::JsonParser::new(text);
    let val = parser.parse().ok()?;
    // 顶层 object，找 binds 数组，取 [0]。
    if let libsys::json::JsonValue::Object(fields) = val {
        for (k, v) in fields {
            if k == "binds" {
                if let libsys::json::JsonValue::Array(arr) = v {
                    if let Some(libsys::json::JsonValue::String(s)) = arr.into_iter().next() {
                        return Some(s.into_bytes());
                    }
                }
            }
        }
    }
    None
}

/// 从 /devices/list 解析一个 binds 条目（具体设备名或 "prefix*" 通配类别）→ 返回
/// 第一个匹配的真实 DriverHub 设备名。读 /devices/list 真值，不伪造；无匹配返回 None。
///
/// 通配只做**尾部单个 '*'**（valid_bind 已保证）：把前缀与每个设备名比对。
fn resolve_bind(bind: &[u8]) -> Option<alloc::vec::Vec<u8>> {
    let Ok(list) = read_to_end("/devices/list") else {
        return None;
    };
    let Ok(text) = core::str::from_utf8(&list) else { return None; };
    let mut parser = libsys::json::JsonParser::new(text);
    let Ok(val) = parser.parse() else { return None; };
    let wild = bind.last() == Some(&b'*');
    let prefix = if wild { &bind[..bind.len() - 1] } else { bind };
    if let libsys::json::JsonValue::Array(items) = val {
        for it in items {
            if let libsys::json::JsonValue::Object(fields) = it {
                for (k, v) in fields {
                    if k == "name" {
                        if let libsys::json::JsonValue::String(s) = v {
                            let nb = s.as_bytes();
                            let hit = if wild { nb.starts_with(prefix) } else { nb == bind };
                            if hit {
                                return Some(s.into_bytes());
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

/// driver load <name>：spawn 驱动 ELF（设备名作 cmd）并 waitpid 收回。
fn driver_load(words: &[Vec<u8>]) -> u8 {
    if words.is_empty() {
        out(b"driver load: usage: driver load <name>\n");
        return 1;
    }
    let name = words[0].as_slice();
    if !valid_driver_name(name) {
        out(b"driver load: invalid driver name\n");
        return 1;
    }
    let name_str = core::str::from_utf8(name).unwrap_or("?");
    // 1. 读 manifest 取 binds[0]（具体设备名或通配类别）。
    let Some(bind) = manifest_first_bind(name_str) else {
        out(b"driver load: no manifest or no binds for ");
        out(name);
        out(b"\n");
        return 1;
    };
    // 1b. 通配 binds 解析为 /devices/list 中真实匹配的设备（具体名原样直通）。
    let Some(dev) = resolve_bind(&bind) else {
        out(b"driver load: bind matches no real unclaimed device: ");
        out(&bind);
        out(b"\n");
        return 1;
    };
    // 2. spawn /modules/<name>/driver.elf，把设备名作 cmd（= 子进程 argv[0]）。
    let elf_path = alloc::format!("/modules/{name_str}/driver.elf");
    let mut b = [0u8; 24];
    let child = match exec_path(&elf_path, &dev) {
        Ok(pid) => pid,
        Err(e) => {
            out(b"driver load: spawn driver failed errno=");
            out(u64_to_dec(e.to_errno() as u64, &mut b));
            out(b"\n");
            return 1;
        }
    };
    out(b"driver load: spawned driver pid=");
    out(u64_to_dec(child, &mut b));
    out(b" device=");
    out(&dev);
    out(b"\n");
    // 3. 收退出码（单核上 waitpid 可能瞬时 WouldBlock/NotFound，轮询）。
    let mut code: u64 = 0;
    let mut reap = false;
    for _ in 0..10_000_000u32 {
        match waitpid_any() {
            Ok(wr) if wr.pid == child => { code = wr.code; reap = true; break; }
            Ok(_) => { /* 其它已收子进程，忽略继续 */ }
            Err(libsys::error::Error::WouldBlock) | Err(libsys::error::Error::NotFound) => {
                let _ = yield_now();
            }
            Err(_) => { let _ = yield_now(); }
        }
    }
    if !reap {
        out(b"driver load: TIMEOUT waiting for driver pid\n");
        return 1;
    }
    out(b"driver load: driver pid=");
    out(u64_to_dec(child, &mut b));
    out(b" exit=");
    out(u64_to_dec(code, &mut b));
    out(b"\n");
    code as u8
}

/// driver list：枚举 /modules/*/ 真实读 manifest。
fn driver_list() -> u8 {
    let entries = match read_dir("/modules") {
        Ok(e) => e,
        Err(_) => { out(b"driver list: cannot read /modules\n"); return 1; }
    };
    let mut found = false;
    for e in entries {
        if e.node_type != "dir" {
            continue;
        }
        let mpath = alloc::format!("/modules/{}/manifest.json", e.name);
        match read_to_end(&mpath) {
            Ok(bytes) => {
                found = true;
                out(b"  ");
                out(e.name.as_bytes());
                out(b": ");
                out(&bytes);
                out(b"\n");
            }
            Err(_) => {
                // 有目录无 manifest：如实列出目录名但标注无声明。
                found = true;
                out(b"  ");
                out(e.name.as_bytes());
                out(b": (no manifest.json)\n");
            }
        }
    }
    if !found {
        out(b"driver list: no drivers installed in /modules\n");
        return 0;
    }
    0
}

/// driver status [dev]：driver_query 打印绑定态 JSON。
fn driver_status(words: &[Vec<u8>]) -> u8 {
    let dev: &[u8] = if words.is_empty() { b"pci-ethernet-00-03-0" } else { words[0].as_slice() };
    let dev_str = core::str::from_utf8(dev).unwrap_or("?");
    match driver_query(dev_str) {
        Ok(json) => {
            out(b"driver status ");
            out(dev);
            out(b": ");
            out(json.as_bytes());
            out(b"\n");
            0
        }
        Err(_) => {
            out(b"driver status: query failed for ");
            out(dev);
            out(b"\n");
            1
        }
    }
}

/// driver 主分发：<install|load|list|status> [args...]
/// `selftest` 命令：按需运行开机自检（原 init 启动序列整体迁入 /programs/selftest.elf）。
///
/// 用法：selftest [audio|thread|quick]。无参 = 全量（含 SIGKILL 风暴，耗时最长）。
/// 实现走 exec_path + waitpid_any 收尸，与用户手敲外部程序同一条真实路径。
fn cmd_selftest(arg: &[u8]) -> u8 {
    let mut b = [0u8; 24];
    // 组参数：原样透传（空参 = 全量）。
    let mut cmd: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    cmd.extend_from_slice(arg);
    let child = match exec_path("/programs/selftest.elf", &cmd) {
        Ok(pid) => pid,
        Err(e) => {
            out(b"selftest: spawn failed errno=");
            out(u64_to_dec(e.to_errno() as u64, &mut b));
            out(b"\n");
            return 1;
        }
    };
    out(b"selftest: spawned pid=");
    out(u64_to_dec(child, &mut b));
    out(b"\n");
    // exec 返回时子进程可能尚未进入就绪队列：此刻 waitpid 会因
    // "系统内无其他可运行进程"被拒绝（WouldBlock）。与 cmd_audioe2e 同法，
    // 先让出若干轮给子进程起跑，再进入收尸阻塞。
    for _ in 0..100 {
        let _ = yield_now();
    }
    // 收尸并回显真实退出码（0=全绿）。
    // EAGAIN（WouldBlock, errno 11）重试：exec 返回时子进程可能尚未进入
    // 就绪队列，内核在"系统内无其他可运行进程"时**如实拒绝阻塞**（不猜测
    // 等待）。让出后重试即可——子进程一经调度，下一次 waitpid 就走真正的
    // 阻塞路径；这不是忙等，是内核诚实语义的正确用法。
    let st = loop {
        match waitpid_any() {
            Ok(wr) => {
                out(b"selftest: reaped pid=");
                out(u64_to_dec(wr.pid, &mut b));
                out(b" exit=");
                out(u64_to_dec(wr.code as u64, &mut b));
                out(b"\n");
                if wr.pid != child {
                    out(b"selftest: NOTE reaped unrelated pid (orphan)\n");
                }
                break if wr.code == 0 { 0 } else { 1 };
            }
            Err(e) if e.to_errno() == 11 => {
                let _ = yield_now();
            }
            Err(_) => {
                out(b"selftest: waitpid failed\n");
                break 1;
            }
        }
    };
    st
}


fn cmd_driver(arg: &[u8]) -> u8 {
    let a = trim_bytes(arg);
    if a.is_empty() {
        out(b"driver: subcommands: install <src> <name> [dev] | load <name> | list | status [dev]\n");
        return 1;
    }
    let words = split_words(a);
    if words.is_empty() {
        return 1;
    }
    match words[0].as_slice() {
        b"install" => driver_install(&words[1..]),
        b"load" => driver_load(&words[1..]),
        b"list" => driver_list(),
        b"status" => driver_status(&words[1..]),
        other => {
            out(b"driver: unknown subcommand: ");
            out(other);
            out(b"\n");
            1
        }
    }
}


// ==================== 路径执行（`/path/to/prog.elf`）====================

/// 命令行首词应该如何分发。
///
/// **为何要单独抽成纯函数**：判定规则（什么算路径、什么算内建名）可以穷举，
/// 而「真的去执行」不行。分离后就能在宿主上覆盖全部边界，
/// 而不是只能在 QEMU 上敲几条例试（S23/S29）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dispatch {
    /// 走内建命令表。
    Builtin,
    /// 走 VFS 路径装载（用户给了明确的路径）。
    Path,
    /// 空词，什么都不做。
    Empty,
}

/// 判定命令行首词的分发方式。
///
/// **判据是「含 `/`」而不是「以 `/` 开头」**，理由是相对路径：
/// 内核 `sys_exec` 会把相对路径与进程 cwd 拼接（见 `absolute_path`），
/// 所以 `prog.elf`（无斜杠）与 `./prog.elf`（有斜杠）在**内核**看来一样。
/// 若只认前导斜杠，`./prog.elf` 会被当成内建名而报 unknown command ——
/// 那恰恰是 Unix 用户最先试的写法。含斜杠即交给内核解析，
/// 是否存在、是否可执行由内核如实回答。
///
/// **为何不以「文件是否存在」为准**：那需要先探测文件系统，会给每个
/// 未知内建名多加一次无谓的 VFS 查询，并引入 TOCTOU 窗口
/// （探测与执行之间文件可能消失）。让内核在装载时判定更直接。
pub(crate) fn classify_command(word: &[u8]) -> Dispatch {
    if word.is_empty() {
        return Dispatch::Empty;
    }
    if word.contains(&b'/') {
        return Dispatch::Path;
    }
    Dispatch::Builtin
}

/// 该词是否为已知内建命令名。
///
/// **消歧用**：含 `/` 的词优先按路径处理，但若它恰好是内建名仍应走内建 ——
/// 保留这一层是为了让规则不依赖「内建名恰好都不含斜杠」这一巧合。
pub(crate) fn is_builtin(word: &[u8]) -> bool {
    COMMANDS.iter().any(|c| *c == word)
}

/// 前台等待期间**非阻塞**探一次键盘：拿到 `^C`（0x03）就向 `child` 投递 `SIGINT`。
///
/// # 为什么在前台等待里做这件事（L-4）
///
/// 前台等待的**时间片**（§6.11 裁决 B）：有界 waitpid 每次最多等这么久。
///
/// 选 10ms 的理由：与调度 tick 同量级，因此 `^C` 的响应延迟在人类可感知范围内；
/// 同时轮询频率不致过高而浪费 CPU（每片仅一次系统调用）。
const WAIT_SLICE_NS: u64 = 10_000_000;

/// 前台子进程运行中，shell 若只闷头 `waitpid_any()`，键盘就没有读者：
/// 用户按的 `^C` 会**留在键盘队列里**，直到子进程自己结束、下一条命令
/// 开始读行时才被取走——届时它会立刻中断**那一行**（表现为「Ctrl-C 延迟
/// 生效，而且把下一行输入打错」）。要让它当场生效，只能在等待循环里照看键盘。
///
/// # 为什么是「探」而不是「阻塞读」
///
/// 子进程（如 `cat`）**有权读自己的 stdin**——前台进程共享同一个键盘。
/// shell 若在这里阻塞读，就会与子进程抢输入。故本函数只做一次**非阻塞**探：
///
/// # 裁决甲（§6.12.5）：本函数曾与自己的文档矛盾
///
/// 上面这句「只做一次**非阻塞**探」是**本函数最初的意图**，但旧实现用的是
/// 阻塞 `read`——文档与代码**直接矛盾**，而这不是笔误、是**实现缺陷**：
/// 交互 stdin 空读会让内核登记 `KBD_WAITER` 并把本进程切走。
///
/// 后果实测（真实 QEMU + 真实 PS/2 按键）：前台子进程被 `^C` 杀死后，
/// shell 回到等待循环开头、又立刻阻塞在这里，**永远走不到 `waitpid`**；
/// 提示符永不回来，且此后对任何按键毫无反应（连字符回显都没有）。
///
/// 现改用 [`libsys::read_nonblocking`]：无键可读时内核**不登记等待者**，
/// 如实返回 `WouldBlock`，本函数立刻返回。
/// 「探键」与「等待」从此彻底解耦——这正是裁决甲要达成的效果。
///
/// 副作用（正向）：旧实现每次空探都会阻塞切走，使「等待循环 → 行编辑」
/// 的交接窗口异常地长，窗口内的击键大量落进无人登记的空档。
/// 现在探键不再切走进程，交接是**同步**完成的。
///
/// shell 若在这里阻塞读，就会与子进程抢输入。故本函数只做一次**非阻塞**探：
/// 无键可读就立刻返回，绝不消费任何**非 `0x03`** 的字节（那些属于子进程）。
///
/// # 为什么目标必须是 `pid` 而不是 `waitpid_any` 收到的 pid
///
/// ADR-046 §1.2 已裁定：`waitpid_any()` **可能收尸到无关孤儿**，
/// 故「杀掉我正在等的那个」语义不成立；`exec_path()` 返回的 `child` pid
/// 才是精确目标。本函数只接受前者。
///
/// # 失败语义
///
/// `kill` 失败（典型：子进程恰在此刻退出，返回 `NotFound`）**不报错**——
/// 那是真实竞态，而且用户要的是「中断这件事发生」，已经发生。静默即可。
fn probe_interrupt(child: u64) {
    let mut one = [0u8; 1];
    match libsys::read_nonblocking(STDIN, &mut one) {
        Ok(1) if one[0] == 0x03 => {
            let mut sink = [0u8; 1];
            let _ = read(STDIN, &mut sink);
            let _ = kill(child, libsys::signal::SIGINT as u64);
        }
        _ => {}
    }
}

// ==================== `PATH` 查找 ====================

/// 当前 `PATH` 的目录列表（`:` 分隔；空项忽略）。
///
/// 取值顺序：用户 `export PATH=...`（或环境带入）优先，否则内置默认
/// （见 `env::DEFAULT_PATH`，由 `env::init_path_default` 在 shell 启动时写入）。
///
/// `PATH` 被 `unset` 时返回**空列表**——**不偷偷恢复默认值**：POSIX 下未设
/// PATH 就是「没有可搜索目录，只有内建可用」，如实报错（见 `exec_via_search_path`）。
pub(crate) fn path_dirs() -> Vec<Vec<u8>> {
    let raw = match crate::env::env_get(b"PATH") {
        Some(v) => v,
        None => return Vec::new(),
    };
    let mut dirs = Vec::new();
    for part in raw.split(|&c| c == b':') {
        let part = trim_bytes(part);
        if !part.is_empty() {
            dirs.push(part.to_vec());
        }
    }
    dirs
}

/// `PATH` 里**第一个存在且不是目录**的候选路径。
///
/// **单点**：`which`（打印解析结果）与真正执行（`exec_via_search_path`）共用同一套
/// 解析规则——两处各写一份必然分叉（S13：同一个判断只允许一个真相来源）。
///
/// **两级尝试 `name` → `name.elf`**：BORUIX 的可执行文件统一带 `.elf`
/// （`ls` 正是用它作为「可执行」标记，见 `cmd_ls` 的文档），而 POSIX 习惯不带
/// 扩展名。用户敲 `tcc` 与敲 `tcc.elf` 应当命中同一个文件——3P6-1 的验收原文
/// 就是 `tcc hello.c -o hello`（不带扩展名）。**顺序固定为精确名优先**，
/// 故 `.elf` 只是后备，不会遮蔽同名的精确文件；名字本身已以 `.elf` 结尾时
/// 不再追加（省一次无意义的 stat）。
///
/// **跳过目录**：POSIX 的 PATH 查找只接受普通文件，同名目录不应遮蔽后面的真程序
/// （`node_type` 判据来自 `libsys::StatInfo::TYPE_DIR`，不手写魔数）。
/// 非 UTF-8 的 PATH 项同样跳过（它不可能是可执行路径），但**不改写成别的路径**
/// ——那会把一次失败伪装成成功（S09）。
///
/// **为什么这里可以探测文件是否存在**（`classify_command` 的文档曾明确反对探测）：
/// 那条纪律针对的是**每个未知命令名**（含所有手误输入）——为它探测会给每次打错
/// 多加一次 VFS 查询且无收益。这里是**已经确定要查找**的 PATH 候选（目录数 ≤ 几），
/// 探测只用于**选择交给谁执行**，能否执行仍由内核装载时判定；残余 TOCTOU 窗口的
/// 后果有限：探测到存在而执行时消失 → `exec` 如实报 ENOENT。
fn path_candidate(name: &[u8]) -> Option<Vec<u8>> {
    // 精确名优先；名字本身不以 `.elf` 结尾时再试一次 `name.elf`。
    let rounds = if name.ends_with(b".elf") { 1 } else { 2 };
    for dir in path_dirs() {
        for k in 0..rounds {
            let mut cand: Vec<u8> = Vec::with_capacity(dir.len() + 1 + name.len() + 4);
            cand.extend_from_slice(&dir);
            cand.push(b'/');
            cand.extend_from_slice(name);
            if k == 1 {
                cand.extend_from_slice(b".elf");
            }
            let Ok(cand_str) = core::str::from_utf8(&cand) else {
                continue;
            };
            match libsys::stat(cand_str) {
                // 目录不遮蔽后续候选（POSIX：PATH 查找只接受普通文件）。
                Ok(info) if info.node_type == libsys::StatInfo::TYPE_DIR => continue,
                Ok(_) => return Some(cand),
                Err(_) => continue,
            }
        }
    }
    None
}

/// 按 `PATH` 逐目录查找 `name` 并执行（POSIX 语义）。
///
/// 全部候选都不存在 → 127，并**如实列出搜索过的目录**——「PATH 没配对」与
/// 「程序真的不存在」必须可区分（批次六 R2：失败原因必须区分）。`PATH` 未设时
/// 单独给一句话说明「只有内建可用」，而不是让用户对着 not found 猜。
fn exec_via_search_path(name: &[u8], arg: &[u8]) -> u8 {
    if path_dirs().is_empty() {
        out(b"boruix: command not found: ");
        out(name);
        out(b" (PATH is unset or empty; only built-ins are available)\n");
        return 127;
    }
    if let Some(cand) = path_candidate(name) {
        // 交给既有路径执行器：它负责三类失败的区分报告与前台等待。
        return exec_via_path(&cand, arg);
    }
    out(b"boruix: command not found: ");
    out(name);
    out(b" (searched: ");
    for (i, d) in path_dirs().iter().enumerate() {
        if i > 0 {
            out(b":");
        }
        out(d);
    }
    out(b")\n");
    127
}

/// 执行一个 VFS 路径指向的程序，如实报告三类不同的失败。
///
/// **三类失败必须分开报**（这是本功能的重点，不是装饰）：
///
/// 1. **文件不存在**（ENOENT）：路径打错了，或该文件不在当前启动模式下；
/// 2. **不是可执行镜像**（ENOEXEC）：文件在，但内容不是可装载的 ELF；
/// 3. **内核拒绝装载**（EACCES/EISDIR/ENOTDIR/E2BIG/EFAULT 等）：权限、
///    对象类型、命令行过长、地址非法。
///
/// 笼统报一句 exec failed 会把这三类混在一起，而它们的排查方向完全不同 ——
/// 与批次六 R2「失败原因必须区分」是同一条纪律。
///
/// 退出码沿用 shell 惯例：126 = 找到了但无法执行。
fn exec_via_path(path: &[u8], arg: &[u8]) -> u8 {
    let path_str = match core::str::from_utf8(path) {
        Ok(s) => s,
        Err(_) => {
            out(b"boruix: path is not valid UTF-8: ");
            out(path);
            out(b"\n");
            return 126;
        }
    };
    match exec_path(path_str, arg) {
        Ok(pid) => {
            // 前台语义：等它跑完再回提示符（与内建命令一致）。
            let mut b = [0u8; 24];
            out(b"boruix: started pid=");
            out(u64_to_dec(pid, &mut b));
            out(b"\n");
            // 前台语义：等它跑完再回提示符。
            //
            // **`WouldBlock` 不是失败，必须重试。** 内核 `sys_task_wait` 在
            // "目标子进程仍在运行，但**本核**就绪队列里没有别的进程可切" 时
            // 返回 `WouldBlock`（拒绝阻塞，以免让出后无人可运行）。这在多核下
            // 很常见：子进程 home 在别的核、或此刻所有可运行进程都在别的核上。
            //
            // 旧实现把 `WouldBlock` 当致命错误直接 `return 126` —— 于是**丢下
            // 仍在运行的子进程**自己退出。init 的 supervisor 循环随即重拉一个
            // shell，键盘缓冲被多个将死的 shell 并发 `read`，输入被瓜分
            // （`clear` 变成 `r` / `clea`）——"命令对不对全靠运气"的真正来源。
            //
            // **L-4：等待期间同时照看键盘。** 前台子进程运行中，shell 若只是
            // 闷头 `waitpid_any`，`^C` 就永远没人读——用户按键进键盘队列后
            // 无人消费，直到子进程自己结束才被下一条命令读走（键盘缓冲区里
            // 躺着一个 0x03，会在下一次 `read_line` 里立刻中断那一行，
            // 表现为"Ctrl-C 延迟生效且打错行"）。
            //
            // 故每次重问之前**非阻塞**探一次 stdin：只有拿到 `0x03` 才动作。
            // 其余字节**原样留在队列里**交给子进程——前台程序（如 `cat`）
            // 有权读自己的 stdin，shell **不得**替它消费。
            loop {
                // ---- 顺序：**先探键盘，再等子进程**（裁决甲后此顺序重新安全） ----
                //
                // 为何这个顺序曾经不安全：`probe_interrupt` 内部用的是**阻塞**
                // `read`，交互 stdin 空读会让内核登记 `KBD_WAITER` 并把本进程
                // 切走。于是「先探键」等于「先阻塞」——子进程被 `^C` 杀死后，
                // shell 回到循环开头又立刻挂在探键上，**永远走不到 `waitpid`**。
                // 实测症状：提示符永不回来，此后对任何按键毫无反应。
                //
                // 裁决甲（§6.12.5）之后 `probe_interrupt` 改用
                // `libsys::read_nonblocking`：无键可读时内核**不登记等待者**，
                // 如实返回 `WouldBlock`，本函数立刻返回。
                // 故「探键」不再有任何阻塞可能，本顺序**重新成立且更自然**：
                // 每次等待前先照看一次键盘，然后交给有界等待。
                //
                // **不再需要**「先用极短超时查死活」那种绕法——那会在计时器
                // 粒度上引入竞态（1ns 定时器立即到期），并让 `NotFound` 这种
                // 瞬态结果被误当成「子进程已结束」而抛弃仍在运行的子进程。
                // 现在只有一条等待路径、一种超时值，语义单一（S13）。
                probe_interrupt(pid);
                match waitpid_any_timeout(WAIT_SLICE_NS) {
                    Ok(wr) => break wr.code as u8,
                    Err(Error::WouldBlock) => {
                        // 真超时 / 内核拒绝阻塞：**都应重试**，
                        // 回到循环开头再探一次键盘。
                        let _ = yield_now();
                        continue;
                    }
                    // `NotFound` = 内核 `waitpid_inner` 报「无子进程可等」。
                    //
                    // **刻意与 `WouldBlock` 同等对待（重试）而非当成完成**——
                    // 这是实测纠正的一个错误假设：`waitpid_any` 的 `NotFound`
                    // 可能在子进程**仍在运行**时瞬态出现（实测：子进程
                    // state 仍为 Running 的同时本分支被判为 `NotFound`）。把它当成
                    // 「收工」会让 shell **抛弃仍在运行的前台子进程**直接回提示符，
                    // 而子进程还在跑——提示符看似回来了，行为完全错乱。
                    //
                    // 正确做法：当作「此刻没收到」重试。真正的结束由
                    // `Ok(wr)` 分支交付（它带真实退出码），那才是可靠信号。
                    Err(Error::NotFound) => {
                        let _ = yield_now();
                        continue;
                    }
                    Err(e) => {
                        out(b"boruix: wait failed: ");
                        out(e.to_string().as_bytes());
                        out(b"\n");
                        break 126;
                    }
                }
            }
        }
        Err(e) => {
            report_exec_error(path, e);
            126
        }
    }
}

/// 把装载失败映射成**具体、可行动**的提示。
///
/// 本函数的全部意义在于：不把不同性质的失败压扁成同一句话。
fn report_exec_error(path: &[u8], e: libsys::Error) {
    use libsys::Error;
    out(b"boruix: cannot execute ");
    out(path);
    out(b": ");
    match e {
        Error::NotFound => {
            out(b"no such file (ENOENT)\n");
            // 提示 /programs 的**两种**来源，不假定当前是哪一种。
            //
            // 原措辞只说 liveCD 一种来源，并称安装模式下"没有兜底"，
            // 会被读成"安装模式下 /programs 是空的"——这是错的：
            // 安装模式下 /programs 是 systemdisk.img 的 EXT2 目录，
            // SDK `build --systemdisk` 会把全部用户程序写进去（实测确认）。
            // shell 无法可靠区分当前模式，故只陈述事实，由用户自行判断。
            out(b"boruix: note: /programs contents depend on how you booted:\n");
            out(b"boruix:       - liveCD (ISO): the built-in payload embedded in the kernel\n");
            out(b"boruix:       - installed (hard disk): the EXT2 /programs on that disk,\n");
            out(b"boruix:         written by `python main.py build --systemdisk`\n");
        }
        Error::ExecFormat => {
            out(b"not a loadable ELF image (ENOEXEC)\n");
        }
        Error::PermissionDenied => {
            out(b"permission denied (EACCES)\n");
        }
        Error::IsDirectory => {
            out(b"is a directory, not a program (EISDIR)\n");
        }
        Error::NotDirectory => {
            out(b"a path component is not a directory (ENOTDIR)\n");
        }
        Error::ArgListTooLong => {
            out(b"command line too long (E2BIG)\n");
        }
        Error::BadAddress => {
            out(b"bad user address (EFAULT)\n");
        }
        Error::NameTooLong => {
            out(b"path too long (ENAMETOOLONG)\n");
        }
        other => {
            out(other.to_string().as_bytes());
            out(b" (errno=");
            let mut b = [0u8; 24];
            out(u64_to_dec(other.to_errno() as u64, &mut b));
            out(b")\n");
        }
    }
}
