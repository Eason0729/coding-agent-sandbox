# fuse/open_file.rs — Minimal Open Handle State

## Overview

`open_file.rs` stores only active backing handle state. It no longer owns copy-on-write materialization logic.

All content transfer is direct filesystem I/O on the backing `File`.

## FileState

```rust
enum FileState {
    PassthroughReal { file: File },
    PassthroughObject { file: File, object_id: u64 },
}
```

- `PassthroughReal`: backing file is the real host path.
- `PassthroughObject`: backing file is object-store path on real FS, keyed by `object_id` from syncing daemon.

## OpenFile

```rust
struct OpenFile {
    state: FileState,
}
```

`OpenFile` does not keep the logical FUSE path.

## Methods

- `read_at(offset, size, ..)`:
  - seek + read on backing file.
- `write_at(offset, data, ..)`:
  - seek + write on backing file.
- `copy_from(offset, len, ..)`:
  - seek + read range from backing file.
- `flush_to_daemon(..)`:
  - sync backing file (`sync_data`), no file-content RPC.
- `set_ranged_size(size)`:
  - truncate backing file to size.

## Non-goals in this module

- no `Cow*` states
- no `FuseOnly*` dirty temp states
- no ranged patch state
- no daemon content calls (`get_object`, `put_file`, `patch_file`)

Those concerns are removed by design.
