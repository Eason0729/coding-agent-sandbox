Implement `config/policy.rs` — `ConfigPolicy`, the concrete implementor of the `Policy` trait from `fuse/policy.rs`.

This module bridges the config file (parsed `Config`) with the FUSE layer's `Policy` trait.

---

## Goals

1. Implement the `Policy` trait
2. Use `globset` for efficient glob matching
