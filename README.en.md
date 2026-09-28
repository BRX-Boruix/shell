# shell

BORUIX's **user shell** — a command line interpreter, a standalone user-space program.

[简体中文](README.md)

## Supported commands

### Files and directories

| Command | Description |
| --- | --- |
| `ls` | List a directory (supports `--json` output) |
| `cat` | Show file contents |
| `mkdir` | Create a directory |
| `touch` | Create an empty file |
| `rm` | Remove |
| `tree` | List a directory as a tree |
| `cd` / `pwd` | Change and show the current directory |

### Environment and variables

| Command | Description |
| --- | --- |
| `export` / `env` / `unset` | Set, list, and delete environment variables |
| `alias` / `unalias` | Command aliases |
| `which` | Find where a command comes from |

### Processes and jobs

| Command | Description |
| --- | --- |
| `ps` | List processes |
| `kill` / `signal` | Send signals, list signals |
| `jobs` / `jobout` | Inspect background jobs and their output |
| `pipe` | Demonstrate a pipe: create, write, read back |

### System information

| Command | Description |
| --- | --- |
| `now` / `time` / `uptime` | Time and uptime |
| `version` / `uname` | Version information |
| `cpu` | CPU information |
| `sleep` | Wait |
| `clear` | Clear the screen |
| `poweroff` / `reboot` | Power off, reboot (privileged) |
| `help` | Command list |

### Driver management

| Command | Description |
| --- | --- |
| `install` / `load` | Install and load drivers |
| `list` / `status` | List drivers, query status |
| `driver` / `uiodemo` | Driver demonstration and debugging |

### Built-in acceptance commands

These invoke the corresponding acceptance programs directly, for confirming on real hardware that each subsystem works:

| Command | Program |
| --- | --- |
| `acee2e` | [`acee2e`](https://github.com/BRX-Boruix/acee2e) — access control |
| `synce2e` | [`synce2e`](https://github.com/BRX-Boruix/synce2e) — synchronisation primitives |
| `trave2e` | [`trave2e`](https://github.com/BRX-Boruix/trave2e) — privilege dropping |
| `audioe2e` | [`audioe2e`](https://github.com/BRX-Boruix/audioe2e) — audio |
| `libccheck` | [`libc`](https://github.com/BRX-Boruix/libc) — C library interfaces |
| `selftest` | [`selftest`](https://github.com/BRX-Boruix/selftest) — system self-test |

## Line syntax

| Syntax | Meaning |
| --- | --- |
| `&` | Run in the background (at end of line) |
| `>` / `>>` | Output redirection |
| `"` | Quoting, protecting spaces inside |
| `$VAR` | Variable expansion |

**The `|` pipe syntax is not supported.** The command named `pipe` is a demonstration program that creates a pipe itself and reads and writes it, to verify the pipe system calls — it is not shell syntax.

## Line editing

Input lines support **history** and **Tab completion**, provided by the [`libline`](https://github.com/BRX-Boruix/libline) component.

## Usage

Started automatically by `init`. It can also be run with a command line argument to execute a single command and exit — system initialisation uses that to run self-tests.

```
$ echo hello
hello
```

## Layout

```
shell/
├── Cargo.toml      # package definition
├── build.rs        # injects the linker script
├── linker.ld       # user-space section layout
└── src/
    ├── main.rs     # entry point and the REPL loop
    ├── commands.rs # built-in commands and dispatch
    ├── tokenize.rs # quote-aware tokenising and variable expansion
    ├── env.rs      # the environment variable table
    ├── json_tree.rs# JSON tree rendering
    ├── linehost.rs # integration with the line editing component
    ├── libc_check.rs # the C library interface acceptance command
    └── util.rs     # output and general utilities
```

Module dependencies run one way, without cycles. JSON is parsed by [`libsys`](https://github.com/BRX-Boruix/libsys); this program only renders it.

## Related projects

- [`init`](https://github.com/BRX-Boruix/init) — starts this program
- [`login`](https://github.com/BRX-Boruix/login) — login authentication
- [`libline`](https://github.com/BRX-Boruix/libline) — line editing, history, and completion
- [`coreutils`](https://github.com/BRX-Boruix/coreutils) — standalone external command programs

## License

MIT License, copyright Yang Borui. See [LICENSE](LICENSE).
