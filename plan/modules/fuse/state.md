# fuse/state.rs and fuse/state_loader.rs — Operation State Snapshot

## Overview

This module pair owns operation input state modeling.

- `state.rs`: pure data structures used by planner/decision tests.
- `state_loader.rs`: gathers required columns from real FS and syncing daemon.

`state_loader.rs` must read daemon columns even in `Passthrough` mode so behavior tables
always receive complete state.

## Responsibilities

### `state.rs`

- Define operation-neutral state atoms:
  - access mode
  - path
  - real entry existence/type
  - fuse entry existence/type/object-id
  - whiteout marker
  - operation flags (open/read-write/truncate)
- Define operation-specific snapshots (e.g. `OpenState`, `StatState`, `ReaddirState`).
- Keep snapshot structs deterministic and easy to instantiate in unit tests
  (avoid opaque kernel-only types in decision-facing state columns).

### `state_loader.rs`

- Query daemon (`GetEntry`, `ReadDirAll`, etc.) and real FS (`symlink_metadata`, `read_dir`).
- Normalize snapshots into `state.rs` structures.
- Keep side-effect free: no mutation to real FS or daemon metadata.

## Invariants

- Snapshot load errors are explicit and surfaced to caller.
- Whiteout is represented explicitly (never inferred by absence).
- Readdir state tracks child presence on both sides and whiteout status per child name.
- Open snapshot includes enough columns to derive a deterministic transition trace
  (`need_write`, `truncate_requested`, `real_exists`, `fuse_entry`, `object_path`).
