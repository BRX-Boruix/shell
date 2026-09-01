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

### 已知限制（S09 诚实降级）
- **变长浮点实参（`%.2f` / `%a` / `%e` / `%g`）在真实内核上受限**：`c_variadic`
  （Rust nightly 特性）在 `x86_64-unknown-none` 目标的 `va_arg` 读取 `f64` 变长实参时
  得到错误值（整型/指针变长实参正常）。这是**工具链/ABI 级限制**（`libccheck` 的 `snprintf2
  pad+float`、`snprintf %a` 两项失败；init 自检的 `snprintf FAIL` 同源），**非 libc 格式化
  引擎缺陷**——格式化引擎（`emit_fixed`/`render_hexfloat`）已由宿主测试覆盖通过
  （`test_float_fixed_basic`/`test_hexfloat_known_values` 等 17 项）。
- 浮点格式化引擎本身正确；仅变长浮点**实参传入**被工具链限制。修复方向：跟进 Rust `c_variadic`
  对 `x86_64-unknown-none` 的浮点 `va_arg` 支持，或改用非变长浮点接口。

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
  time/clock、fopen/fwrite/fread（经 VFS 真实文件往返）、fscanf `%lc/%ls` 宽字符、errno 机制。

### 回滚/降级
- 该命令位于 `src/libc_check.rs`，分发入口在 `commands.rs` 的 `run_builtin`。
  移除 `libccheck` 分发与 `src/libc_check.rs` 即可，不影响既有命令。

## 说明
shell 是典型可替换组件，不是内核的一部分。
