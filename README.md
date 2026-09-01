# BORUIX shell

交互式命令行解释器（用户态程序）。

## 职责
- 读取并解释用户输入命令
- 调用 `base` 提供的命令与系统调用
- 支持脚本、管道、重定向等（逐步扩展）

## 依赖
- `libc` / `libsys`
- `base`（基础命令）

## 重定向运维手册（milestone: `>`/`>>`/`<`）

### 如何验证
- 输出重定向：`echo hi > /tmp/a` → `cat /tmp/a` 输出 `hi`；`echo x > /tmp/a` 后文件被截断（原内容清空）。
- 追加重定向：`echo a >> /tmp/a` 两次 → 文件含两行 `a`（不截断）。
- 输入重定向：`cat < /etc/motd` 输出文件内容。
- 管道 + 重定向组合：`echo hi > /tmp/f | cat` → 左段写入文件，右段从管道读到 EOF（空）。
- 缺路径报错：`echo hi >` → `boruix: malformed redirect`，不执行命令。

### 常见失败模式
- 目标路径不可写/目录不存在：`boruix: cannot open '<path>' for redirect`，命令不执行（POSIX 语义）。
- 路径非 UTF-8：`boruix: redirect: invalid path`。
- 备份槽被占/超上限：`boruix: redirect: stdio backup failed` / `boruix: dup2 backup failed`。
- **已知限制**：重定向符须为空白分隔的独立词，`echo hi>f` 不会被当作重定向（与管道 `|` 同约定）。

### 已知限制 / 规避（S09 诚实降级）
- **变长浮点实参（`%.2f` / `%a` / `%e` / `%g`）**：本目标 `x86_64-unknown-none` 的
  `c_variadic`（nightly 特性）在**调用侧**代码生成存在 ABI 缺陷——变长 `double` 实参被放进
  通用寄存器（GPR）而非 XMM，且 `%al=0`，导致标准的 `va_arg`（走 `fp_offset`/XMM 槽）读到垃圾值。
  **已在 libc 规避**（`stdio.rs` 的 `next_float_arg_gp`）：实测确认调用方把全部变长实参（整型/
  指针/浮点）按序放入 GPR，故统一经 `gp_offset`/`reg_save_area` 读取可正确还原。真机验证：
  `libccheck` `snprintf2 pad+float`/`snprintf %a`/`snprintf %n` 全过、init 自检 `snprintf OK`，
  `[libccheck] passed=71 failed=0`。
- 该规避依赖当前编译器把变长实参放入 GPR 的（缺陷）行为；若上游修复 `c_variadic`，应改回标准
  `ap.next_arg::<f64>()` 并移除 `next_float_arg_gp`（其注释已标注）。浮点格式化引擎本身由宿主测试
  覆盖（`test_float_fixed_basic`/`test_hexfloat_known_values` 等 17 项）。

### 回滚/降级
- 该功能位于内建命令分发路径（`exec_line` / `run_pipeline_stage` 剥离重定向后分发）。
  若需回滚，恢复 `src/commands.rs` 中 `split_redirects`/`with_redirects` 的调用即可，
  不影响既有管道与单命令执行。

## libc 验证命令（`libccheck`）

在真实内核上端到端验收 libc 的 C ABI（S06 真实数据链路），调用栈：
`libccheck` → `libc_check::cmd_libccheck` → `libc::*`（malloc/string/printf/strtol/time/FILE 流）
→ `libsys` syscall → 内核。

### 如何验证
- 在 shell 输入 `libccheck`，逐项输出 `<检查名>: OK/FAIL`，末尾汇总
  `[libccheck] passed=N failed=M`，退出码 0（全过）或 1（有失败）。
- 覆盖：malloc/realloc/free 堆复用、posix_memalign（64/256 对齐）+ 对齐指针上 realloc、strlen/strcmp/strcpy、
  strspn/strcspn/strpbrk/strncat/strtok、strtok_r（多上下文交错）、snprintf（整数/宽度/浮点/截断）、strtol/atoi、
  time/clock、fopen/fwrite/fread（经 VFS 真实文件往返）、fscanf `%lc/%ls` 宽字符、errno 机制、
  mkdir/remove（目录与文件创建删除）、opendir/readdir/closedir（目录遍历）、signal/sigaction/raise（真实处理器投递 + sigreturn）、
  fcntl（F_DUPFD/F_GETFD）、stat/fstat/chmod/rename（经 VFS + INode::metadata/set_permissions）。
  真机：`[libccheck] passed=103 failed=0`。

### 回滚/降级
- 该命令位于 `src/libc_check.rs`，分发入口在 `commands.rs` 的 `run_builtin`。
  移除 `libccheck` 分发与 `src/libc_check.rs` 即可，不影响既有命令。

## 说明
shell 是典型可替换组件，不是内核的一部分。
