# shell

BORUIX's user shell: a command interpreter, an independent user-space program.

[简体中文](README.md)

Started by the system init process as the second user program; line editing, history and Tab
completion come from [`libline`](https://github.com/BRX-Boruix/libline).

## Usage

Runs automatically after boot; no manual start needed. It can also run with a command-line argument —
execute that one command and exit. The system boot sequence uses this to run self-tests.

## Line syntax

- `&` — a trailing ampersand runs the line in the background; `jobs` and `jobout` show jobs and their output
- `>` and `>>` — output redirection (truncate and append)
- `"` — quotes protect the spaces inside them
- `$VAR` — variable expansion

**There is no `|` pipeline syntax.** The command named `pipe` is a demonstration program that creates
a pipe itself and reads and writes it, verifying the pipe system calls — it is not shell syntax.

## Built-in commands

Files and directories: `ls` (with `--json`), `cat`, `mkdir`, `touch`, `rm`, `tree`, `jtree`, `cd`, `pwd`

Environment and variables: `export`, `env`, `unset`, `alias`, `unalias`, `which`

Processes and signals: `ps`, `kill`, `signal`, `jobs`, `jobout`, `pipe`, `tty`, `echo`, `sleep`, `clear`

System information: `now`, `time`, `uptime`, `version`, `uname`, `cpu`, `help`, `poweroff`, `reboot` (privileged)

Driver management: `install`, `load`, `list`, `status`, `driver`, `uiodemo`

Built-in acceptance commands: `selftest`, `libccheck`, `acee2e`, `synce2e`, `trave2e`, `audioe2e` —
these invoke the corresponding acceptance programs to confirm subsystems work on real hardware.

External commands run by path: a command word containing `/` is handed to the system for loading
via the VFS path — `/programs/xxx.elf` and `/volumes/.../3p/xxx.elf` on the data disk both work.
Exit codes follow shell convention, with file-not-found and load-failure reported separately. See
[`coreutils`](https://github.com/BRX-Boruix/coreutils) for the standalone external commands.

## Known limitations

- External commands run by path from `/programs` and the data disk; see [`coreutils`](https://github.com/BRX-Boruix/coreutils)
- A word with no `/` that is not a built-in is treated as a built-in name and reported as unknown
- No script files; interactive line-by-line input only

## Building

```bash
cargo build --release
```

## Repository layout

```
shell/
├── Cargo.toml      # package manifest
├── build.rs        # injects the linker script
├── linker.ld       # user-space segment layout
└── src/
    ├── main.rs     # entry and interactive loop
    ├── commands.rs # built-ins and dispatch
    ├── tokenize.rs # quote-aware tokenising and variable expansion
    ├── env.rs      # environment variable table
    ├── json_tree.rs # JSON tree rendering
    ├── linehost.rs # interface to the line-editing component
    ├── libc_check.rs # the C library acceptance command
    └── util.rs     # output and general helpers
```

## Related projects

- [`init`](https://github.com/BRX-Boruix/init) — starts this program
- [`login`](https://github.com/BRX-Boruix/login) — login authentication
- [`libline`](https://github.com/BRX-Boruix/libline) — line editing, history, completion
- [`coreutils`](https://github.com/BRX-Boruix/coreutils) — standalone external commands

## License

MIT License, copyright Yang Borui. See [LICENSE](LICENSE).
