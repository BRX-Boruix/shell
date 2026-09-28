# shell

BORUIX 的用户 shell：命令行解释器，一个独立的用户态程序。

[English](README.en.md)

由系统初始化进程启动，作为第二个用户进程运行；行编辑、历史与 Tab 补全由 [`libline`](https://github.com/BRX-Boruix/libline) 提供。

## 使用方法

开机后自动运行，无需手动调用。也可以带命令行参数运行——直接执行一条命令后退出，系统初始化阶段
用它执行自检。

## 行语法

- `A | B | C`——管道：前一段的输出接后一段的输入，支持多段
- `>` 与 `>>`——输出重定向（覆盖与追加）；`<`——输入重定向
- `&`——行尾表示后台运行，`jobs` 与 `jobout` 查看作业与其输出
`"`——引号保护其中的空格
- `$VAR` 与 `$?`——变量展开与上一条命令的退出状态
- `#`——行内注释，`#` 起到行尾被忽略

名为 `pipe` 的内建命令是一个演示程序：它自己创建管道并读写，用于直接验证管道系统调用。

## 内建命令

文件与目录：`ls`（支持 `--json`）、`cat`、`mkdir`、`touch`、`rm`、`tree`、`jtree`、`cd`、`pwd`

环境与变量：`export`、`env`、`unset`、`alias`、`unalias`、`which`

进程与信号：`ps`、`kill`、`signal`、`jobs`、`jobout`、`pipe`、`tty`、`echo`、`sleep`、`clear`

系统信息：`now`、`time`、`uptime`、`version`、`uname`、`cpu`、`help`、`poweroff`、`reboot`（需权限）

驱动管理：`install`、`load`、`list`、`status`、`driver`、`uiodemo`

内置验收命令：`selftest`、`libccheck`、`acee2e`、`synce2e`、`trave2e`、`audioe2e`——直接调用对应的
验收程序，在真机上确认子系统工作正常。

外部命令按路径执行：命令词含 `/` 时交给系统按 VFS 路径装载，`/programs/xxx.elf` 与数据盘上的
`/volumes/.../3p/xxx.elf` 都可以；退出码沿用 shell 惯例，找不到文件与装载失败分开报告。见
[`coreutils`](https://github.com/BRX-Boruix/coreutils) 提供的独立外部命令。

## 已知限制

- 不含 `/` 且不是内建的词按内建名处理，报「未知命令」
- 管道为顺序语义：前一段跑完再跑下一段，不是并发执行
- 没有脚本文件执行，只有交互式逐行输入

## 构建

```bash
cargo build --release
```

## 文件结构

```
shell/
├── Cargo.toml      # 包定义
├── build.rs        # 注入链接脚本
├── linker.ld       # 用户态段布局
└── src/
    ├── main.rs     # 入口与交互循环
    ├── commands.rs # 内建命令与分发
    ├── tokenize.rs # 引号感知分词与变量展开
    ├── env.rs      # 环境变量表
    ├── json_tree.rs # JSON 树状渲染
    ├── linehost.rs # 与行编辑组件的对接
    ├── libc_check.rs # C 库接口验收命令
    └── util.rs     # 输出与通用工具
```

## 相关项目

- [`init`](https://github.com/BRX-Boruix/init) —— 启动本程序
- [`login`](https://github.com/BRX-Boruix/login) —— 登录认证
- [`libline`](https://github.com/BRX-Boruix/libline) —— 行编辑、历史与补全
- [`coreutils`](https://github.com/BRX-Boruix/coreutils) —— 独立的外部命令程序

## 许可

MIT License，版权归 Yang Borui 所有。详见 [LICENSE](LICENSE)。
