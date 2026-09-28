# shell

**简体中文** | [English](#english)

BORUIX 的**命令行解释器**——你在系统里打交道最多的那个程序。

它读取并解释命令、管理环境变量、执行管道与重定向、把后台作业管起来。

```
boruix:~$ ls /config
boruix:~$ echo hello | cat
boruix:~$ sleep 30 &
```

---

## 定位

shell 是一个**普通的用户态程序**，由系统初始化进程拉起。它没有任何特殊权限，也不在内核里。

这一点是刻意的：**shell 是可替换组件**。系统与命令解释器之间通过标准接口交互，换一个 shell
不需要动内核，也不需要动其他程序。

## 交互能力

| 能力 | 说明 |
| --- | --- |
| 行编辑 | 光标移动、退格、删除、按词移动 |
| 历史记录 | 上下方向键翻阅 |
| Tab 补全 | 补全内建命令名 |
| 引号感知分词 | 正确处理引号内的空格 |
| 变量展开 | `$VAR` 形式的环境变量替换 |

分词器理解引号，所以 `echo "hello world"` 是一个参数而不是两个。

变量展开遵循与常见 shell 一致的引号规则：

| 写法 | 行为 |
| --- | --- |
| `$VAR` | 展开 |
| `"$VAR"` | 展开（双引号不阻止展开） |
| `'$VAR'` | **不展开**（单引号内是字面量） |
| `$?` | 上一条命令的退出状态 |

**展开结果不会被重新分词**——这一点很重要：如果展开后的内容再被拆成多个词，变量里带的空格或
特殊字符就会变成额外的参数，从而打开命令注入的口子。这里展开是在分词过程中直接拼接进当前词，
不会被再次拆分。

## 命令执行

### 管道

多个命令用 `|` 连接，前一个的输出作为后一个的输入：

```
ls /config | cat
```

### 重定向

| 形式 | 作用 |
| --- | --- |
| `> file` | 输出写入文件（**覆盖**原有内容） |
| `>> file` | 输出追加到文件末尾 |
| `< file` | 从文件读取输入 |

重定向可以单独使用，也可以和管道组合。组合时管道各段各自处理自己的重定向。

**一个约定**：重定向符号必须是**空白分隔的独立词**。`echo hi>f` 不会被当作重定向，而是把
`hi>f` 当作一个参数。这与管道的约定一致。

### 后台作业

命令末尾加 `&` 表示在后台运行，提示符立刻返回。用 `jobs` 查看后台作业，用 `jobout` 读回它
的输出。

后台作业的输出需要**转存**：作业在后台运行期间产生的输出不能直接写到当前终端，否则会与用户
正在输入的行互相干扰。转存策略保证输出被保留下来，之后可以完整读回。

### 前台等待与中断

前台命令运行期间，shell 仍然响应 `^C`——它会继续监听键盘，收到中断就终止前台子进程。

这件事比看起来麻烦：如果简单地阻塞等待子进程结束，那么在子进程运行期间 shell 是"聋"的，
用户按 `^C` 没有任何反应。所以等待循环必须**同时**检查子进程是否已结束、以及键盘是否有输入。

## 内建命令

内建命令直接由 shell 执行，不启动新进程。

| 类别 | 命令 |
| --- | --- |
| 输出与信息 | `echo` `help` `version` `uname` `cpu` `uptime` `now` `time` |
| 环境变量 | `env` `export` `unset` `alias` `unalias` `which` |
| 文件操作 | `ls` `cat` `mkdir` `touch` `rm` `tree` `cd` `pwd` |
| 进程与作业 | `ps` `kill` `signal` `jobs` `jobout` |
| 其他 | `sleep` `clear` `pipe` `poweroff` `reboot` `driver` `selftest` `jtree` |

文件操作类命令支持 `--json` 输出模式，便于脚本消费结构化结果，而不是去解析给人看的输出。

`tree` 以树状形式展示目录结构，`jtree` 对 JSON 做同样的可视化——两者共享同一套渲染逻辑。

## 一条关于错误信息的纪律

文件打不开时，shell **区分"文件不存在"和"无权读取"**。

这不是小事。早期版本把所有打开失败都显示成"文件不存在"，结果是：用户看到 `EACCES`（权限
不足）时得到的信息是错的，而且**权限模型正在正常工作这件事，被错误的文案掩盖了**——明明
是被正确拒绝，看起来却像是文件丢了。

现在这两类用 POSIX 惯用短语显示，其余类别如实打出错误码——**不编造可读的字符串**。

## 输出与验收命令

shell 内包含一批**验收命令**，在真实内核上端到端验证下层接口：

| 命令 | 验证对象 |
| --- | --- |
| `libccheck` | C 标准库（内存、字符串、格式化、文件流、目录、信号、权限） |
| `selftest` | 系统各子系统（信号、线程、音频） |
| `driver` | 驱动框架 |
| `uiodemo` | 用户态驱动 |

这些命令验证的是**真实调用链路**：从 shell 出发，经由各级接口，一直走到内核。它们输出逐项的
通过/失败结果与末尾汇总，退出码反映总体结论。

## 构建

```bash
cargo build --release
```

编译产物部署为 BORUIX 系统中的用户态程序，由系统初始化进程拉起。

## 文件结构

```
shell/
└── src/
    ├── main.rs        # 入口与 REPL 循环
    ├── commands.rs    # 内建命令与分发
    ├── tokenize.rs    # 引号感知分词与变量展开
    ├── env.rs         # 环境变量表
    ├── linehost.rs    # 行编辑宿主
    ├── json_tree.rs   # JSON 树状渲染
    └── libc_check.rs  # C 标准库验收
```

## 相关项目

- [`libline`](https://github.com/BRX-Boruix/libline) —— 提供行编辑、历史与补全
- [`libc`](https://github.com/BRX-Boruix/libc) —— C 标准库
- [`libsys`](https://github.com/BRX-Boruix/libsys) —— 用户态系统调用封装
- [`init`](https://github.com/BRX-Boruix/init) —— 拉起 shell

## 许可

MIT License，版权归 Yang Borui 所有。详见 [LICENSE](LICENSE)。

---

# English

[简体中文](#shell) | **English**

BORUIX's **command line interpreter** — the program you deal with most inside the system.

It reads and interprets commands, manages environment variables, runs pipelines and redirections, and
keeps background jobs under control.

```
boruix:~$ ls /config
boruix:~$ echo hello | cat
boruix:~$ sleep 30 &
```

---

## Positioning

The shell is an **ordinary user-space program**, started by the system init process. It holds no
special privilege and is not in the kernel.

This is deliberate: **the shell is a replaceable component**. The system and the command interpreter
interact through a standard interface, so swapping the shell requires touching neither the kernel nor
other programs.

## Interactive capabilities

| Capability | Notes |
| --- | --- |
| Line editing | Cursor movement, backspace, deletion, word-wise movement |
| History | Scroll with the up and down arrows |
| Tab completion | Completes builtin command names |
| Quote-aware tokenizing | Handles spaces inside quotes correctly |
| Variable expansion | `$VAR` environment variable substitution |

The tokenizer understands quotes, so `echo "hello world"` is one argument rather than two.

Variable expansion follows the quoting rules familiar from other shells:

| Form | Behaviour |
| --- | --- |
| `$VAR` | Expands |
| `"$VAR"` | Expands (double quotes do not suppress it) |
| `'$VAR'` | **Does not expand** (single quotes are literal) |
| `$?` | The exit status of the previous command |

**The expanded result is not re-tokenized** — and that matters: were the expansion re-split into
words, spaces or special characters carried in a variable would become extra arguments, opening the
door to command injection. Here the expansion is appended directly into the current word during
tokenizing and is never split again.

## Command execution

### Pipelines

Commands joined with `|`, each feeding its output to the next:

```
ls /config | cat
```

### Redirection

| Form | Effect |
| --- | --- |
| `> file` | Write output to the file (**truncating** it) |
| `>> file` | Append output to the end of the file |
| `< file` | Read input from the file |

Redirection works alone or combined with pipelines; in a pipeline each stage handles its own
redirection.

**One convention**: a redirection symbol must be a **whitespace-separated standalone word**.
`echo hi>f` is not treated as a redirection; `hi>f` becomes a single argument. This matches the
convention for `|`.

### Background jobs

Appending `&` runs a command in the background and returns the prompt immediately. Use `jobs` to list
background jobs and `jobout` to read back their output.

Background output must be **diverted**: output produced while a job runs in the background cannot be
written straight to the current terminal, or it would collide with the line the user is typing. The
diversion policy preserves that output so it can be read back in full afterwards.

### Foreground waiting and interruption

While a foreground command runs, the shell still responds to `^C` — it keeps watching the keyboard
and terminates the foreground child on interrupt.

This is trickier than it looks: blocking simply until the child exits leaves the shell "deaf" for the
duration, so pressing `^C` does nothing. The wait loop must therefore check **both** whether the child
has exited **and** whether the keyboard has input.

## Builtin commands

Builtins are executed by the shell itself, without starting a new process.

| Category | Commands |
| --- | --- |
| Output and information | `echo` `help` `version` `uname` `cpu` `uptime` `now` `time` |
| Environment | `env` `export` `unset` `alias` `unalias` `which` |
| File operations | `ls` `cat` `mkdir` `touch` `rm` `tree` `cd` `pwd` |
| Processes and jobs | `ps` `kill` `signal` `jobs` `jobout` |
| Other | `sleep` `clear` `pipe` `poweroff` `reboot` `driver` `selftest` `jtree` |

The file operation commands support a `--json` output mode, so scripts can consume structured
results instead of parsing human-readable output.

`tree` renders a directory hierarchy, and `jtree` does the same for JSON — the two share one rendering
implementation.

## A discipline about error messages

When a file cannot be opened, the shell **distinguishes "no such file" from "permission denied"**.

This is not a small matter. An earlier version displayed every open failure as "no such file",
meaning a user hitting `EACCES` (insufficient permission) got incorrect information — and **the fact
that the permission model was working correctly was hidden by the wrong message**. Being correctly
refused looked like a missing file.

The two classes now use their customary POSIX phrases, and the remaining classes print the error code
honestly — **no invented human-readable strings**.

## Output and acceptance commands

The shell carries a set of **acceptance commands** that verify lower layers end to end on a real
kernel:

| Command | Verified against |
| --- | --- |
| `libccheck` | The C standard library (memory, strings, formatting, file streams, directories, signals, permissions) |
| `selftest` | System subsystems (signals, threads, audio) |
| `driver` | The driver framework |
| `uiodemo` | User-space drivers |

These verify the **real call chain**: starting at the shell, through each layer, down to the kernel.
They print per-item pass/fail results plus a trailing summary, with the exit code reflecting the
overall verdict.

## Building

```bash
cargo build --release
```

The artifact is deployed as a user-space program in a BORUIX system, started by the system init
process.

## Layout

```
shell/
└── src/
    ├── main.rs        # entry point and the REPL loop
    ├── commands.rs    # builtin commands and dispatch
    ├── tokenize.rs    # quote-aware tokenizing and variable expansion
    ├── env.rs         # the environment variable table
    ├── linehost.rs    # the line editing host
    ├── json_tree.rs   # JSON tree rendering
    └── libc_check.rs  # C standard library acceptance
```

## Related projects

- [`libline`](https://github.com/BRX-Boruix/libline) — provides line editing, history, and completion
- [`libc`](https://github.com/BRX-Boruix/libc) — the C standard library
- [`libsys`](https://github.com/BRX-Boruix/libsys) — the user-space syscall wrapper
- [`init`](https://github.com/BRX-Boruix/init) — starts the shell

## License

MIT License, copyright Yang Borui. See [LICENSE](LICENSE).
