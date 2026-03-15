# CAS — Coding Agent Sandbox

A rootless filesystem sandbox for safely running untrusted programs (AI coding agents) using FUSE. Built in Rust.

## What Is CAS?

`cas` runs untrusted code inside a controlled filesystem environment without requiring root privileges or container runtimes. It intercepts filesystem operations using FUSE and enforces per-path read/write/block policies:

- **Write**: Full access
- **Read**: Agents has full access to files, but redirect write to `./sandbox/data`(CoW).
- **Block**: Agents cannot read real files, it can only read what it write.

## Features

- **Rootless**: No root or container runtime required
- **Copy-on-write**: Writes to untracked files go to a private FUSE store
- **Config**: Change the filesystem policy whatever you want.
- **Out-of-box isolation**: Isolate only what you need(Pid/filesystem), network is NOT isolated by default.
- **Multi-instance coordination**: Uses POSIX SHM to coordinate multiple concurrent sandbox instances.

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
  init   Initialize a new sandbox in the current directory
  clean  Remove FUSE data and reset SHM
  purge  Delete entire .sandbox directory
  run    Run a command inside the sandbox

Options:
  -r, --root <ROOT>  Project root directory (defaults to current directory) [default: .]
  -h, --help         Print help
```

## Requirements

- Linux kernel with FUSE support
- Rust toolchain
