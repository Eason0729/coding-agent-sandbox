# fuse/executor.rs — Side-effect Application

## Overview

`executor.rs` applies side effects selected by `decision.rs`.

It owns operation execution details on:

- real filesystem (`std::fs`)
- syncing daemon client (`SyncClient`)
- open backing file selection

## Responsibilities

- Execute decision variants exactly (no policy branching here).
- Translate low-level errors into `Error` values consumed by FUSE reply mapping.
- Keep operation outputs in forms required by `fs.rs` reply helpers.

## Non-responsibilities

- No behavior classification logic (belongs to `decision.rs`).
- No direct request/reply handling (belongs to `fs.rs`).
