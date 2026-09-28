# shell

BORUIX 的**用户 shell**——命令行解释器，一个独立的用户态程序。

[English](README.en.md)

## 支持的命令

### 文件与目录

| 命令 | 说明 |
| --- | --- |
| `ls` | 列目录（支持 `--json` 输出） |
| `cat` | 显示文件内容 |
| `mkdir` | 建目录 |
| `touch` | 建空文件 |
| `rm` | 删除 |
| `tree` | 树状列出目录 |
| `cd` / `pwd` | 切换与显示当前目录 |

### 环境与变量

| 命令 | 说明 |
| --- | --- |
| `export` / `env` / `unset` | 设置、列出、删除环境变量 |
| `alias` / `unalias` | 命令别名 |
| `which` | 查询命令来源 |

### 进程与作业

| 命令 | 说明 |
| --- | --- |
| `ps` | 列出进程 |
| `kill` / `signal` | 发送信号、列出信号 |
| `jobs` / `jobout` | 查看后台作业及其输出 |
| `pipe` | 演示管道：创建、写入、读出 |

### 系统信息

| 命令 | 说明 |
| --- | --- |
| `now` / `time` / `uptime` | 时间与运行时长 |
| `version` / `uname` | 版本信息 |
| `cpu` | CPU 信息 |
| `sleep` | 等待 |
| `clear` | 清屏 |
| `poweroff` / `reboot` | 关机、重启（需权限） |
| `help` | 命令列表 |

### 驱动管理

| 命令 | 说明 |
| --- | --- |
| `install` / `load` | 安装、加载驱动 |
| `list` / `status` | 列出驱动、查询状态 |
| `driver` / `uiodemo` | 驱动演示与调试 |

### 内置验收命令

这些命令直接调用对应的验收程序，用于在真机上确认各子系统工作正常：

| 命令 | 对应程序 |
| --- | --- |
| `acee2e` | [`acee2e`](https://github.com/BRX-Boruix/acee2e) —— 访问控制 |
| `synce2e` | [`synce2e`](https://github.com/BRX-Boruix/synce2e) —— 同步原语 |
| `trave2e` | [`trave2e`](https://github.com/BRX-Boruix/trave2e) —— 权限降级 |
| `audioe2e` | [`audioe2e`](https://github.com/BRX-Boruix/audioe2e) —— 音频 |
| `libccheck` | [`libc`](https://github.com/BRX-Boruix/libc) —— C 库接口 |
| `selftest` | [`selftest`](https://github.com/BRX-Boruix/selftest) —— 系统自检 |

## 行语法

| 语法 | 含义 |
| --- | --- |
| `&` | 后台运行（行尾） |
| `>` / `>>` | 输出重定向 |
| `"` | 引号，保护其中的空格 |
| `$VAR` | 变量展开 |

**不支持 `|` 管道语法。** 名为 `pipe` 的命令是一个演示程序，它自己创建管道并读写，用于验证管道
系统调用，而不是 shell 的语法。

## 行编辑

输入行支持**历史记录**与 **Tab 补全**，由 [`libline`](https://github.com/BRX-Boruix/libline) 组件提供。

## 使用方法

`init` 启动后自动运行，无需手动调用。也可以带命令行参数运行，直接执行一条命令后退出——系统
初始化阶段用它执行自检。

```
$ echo hello
hello
```

## 文件结构

```
shell/
├── Cargo.toml      # 包定义
├── build.rs        # 注入链接脚本
├── linker.ld       # 用户态段布局
└── src/
    ├── main.rs     # 入口与 REPL 循环
    ├── commands.rs # 内建命令与分发
    ├── tokenize.rs # 引号感知分词与变量展开
    ├── env.rs      # 环境变量表
    ├── json_tree.rs# JSON 树状渲染
    ├── linehost.rs # 与行编辑组件的对接
    ├── libc_check.rs # C 库接口验收命令
    └── util.rs     # 输出与通用工具
```

各模块依赖单向、无环。JSON 由 [`libsys`](https://github.com/BRX-Boruix/libsys) 解析，本程序只负责呈现。

## 相关项目

- [`init`](https://github.com/BRX-Boruix/init) —— 启动本程序
- [`login`](https://github.com/BRX-Boruix/login) —— 登录认证
- [`libline`](https://github.com/BRX-Boruix/libline) —— 行编辑、历史与补全
- [`coreutils`](https://github.com/BRX-Boruix/coreutils) —— 独立的外部命令程序

## 许可

MIT License，版权归 Yang Borui 所有。详见 [LICENSE](LICENSE)。
