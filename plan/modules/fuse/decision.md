# fuse/decision.rs — Pure Behavior Tables

## Overview

`decision.rs` converts loaded operation state into explicit behavior variants.
No filesystem or daemon I/O is allowed in this module.

## Responsibilities

- Define per-operation decision enums (e.g. `StatDecision`, `OpenDecision`, `UnlinkDecision`, `ReaddirDecision`).
- Encode the complete behavior matrix for `Passthrough`, `FuseOnly`, and `CopyOnWrite`.
- Explicitly model whiteout handling.
- Expose pure functions `decide_*` that map state snapshot to decision enum.
- Expose a pure open-transition extractor that emits ordered transition steps for
  timing-sensitive `open` flow (`classify -> ensure/open -> copy-up -> whiteout-delete`).

## Passthrough readdir policy

For child-name collision where both real and non-whiteout fuse entry exist:

- classify as `DontCare` at policy level
- runtime may select deterministic winner for stable output ordering

Whiteout remains strict and explicit: whiteout masks matching child in merged view.

## Testability contract

All `decide_*` functions are unit-testable with plain data fixtures and must not require
FUSE mount, kernel state, or daemon socket.

Open transition extraction must be unit-testable as pure data transformation.
The transition test matrix should be exhaustive across:

- access mode
- `need_write`
- `truncate_requested`
- real entry existence
- fuse entry type/object-id columns
