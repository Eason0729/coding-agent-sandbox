# fuse/open_file.rs — Minimal Open Handle State

## Overview

`open_file.rs` stores only active backing handle state. It no longer owns copy-on-write materialization logic.

All content transfer is direct filesystem I/O on the backing `File`.

Write path must commit full requested buffers (loop/write-all semantics), so
large CoW writes cannot silently become short writes.
