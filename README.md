# CAS — Coding Agent Sandbox

Run your claude safely, no setup, REAL sandbox

## What Is CAS?

`cas` runs untrusted code inside a controlled filesystem environment and container runtimes. It intercepts filesystem operations using FUSE and enforces per-path read/write/block policies:

- **Write**: Full access
- **Read**: Agents has full access to files, but redirect write to `./sandbox/data`(CoW).
- **Block**: Agents cannot read real files, it can only read what it write.

## Features

- **Rootless**: No root or container runtime required
- **Copy-on-write**: Writes to untracked files go to a private FUSE store
- **Config**: Change the filesystem policy whatever you want.
- **Multi-instance coordination**: Coordinate multiple concurrent sandbox instances.

## Installation

```bash
cargo install --path .
```

## Configuration

Edit `.sandbox/config.toml`:

```toml
whitelist = ["src/", "*.rs"]
blacklist = [".env", "secrets/"]
disableLog = ["*.tmp"]
logLevel = "info"
```

- **whitelist**: Paths that bypass CoW and write directly to real filesystem
- **blacklist**: Paths hidden from the sandboxed process
- **disableLog**: Paths that follow CoW but don't log first access

## Commands

```
Coding Agent Sandbox — filesystem isolation tool

Usage: cas [OPTIONS] <COMMAND>

Commands:
  init   Initialize or reset sandbox (creates if not exists, cleans if exists)
  clean  Clean data directory or initialize sandbox if not exists
  purge  Delete entire .sandbox directory
  run    Run a command inside the sandbox (auto-initializes if not exists)

Options:
  -r, --root <ROOT>  Project root directory (defaults to current directory) [default: .]
  -h, --help         Print help
```

## Requirements

> [!IMPORTANT]
> This container is linux only!

- Linux kernel with FUSE support
- Rust toolchain
