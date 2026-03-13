Implement `fuse/policy.rs` — the `AccessMode` enum and `Policy` trait consumed by `CasFuseFs`.

---

## `AccessMode`

There are exactly three access modes:

- **Passthrough**: reads and writes go directly to the real FS. Used for paths on the whitelist (e.g. `project_root`).
- **HideReal**: reads are served from the fuse store only (real FS is invisible); writes go to fuse store. Used for `.sandbox/`. An empty fuse store means all reads return ENOENT.
- **CopyOnWrite**: if data exists in the fuse store, serve it; otherwise read from the real FS (and on first write, copy into fuse store). This is the default for all paths not otherwise classified. First access is logged.

---

## `Policy` Trait

- `classify(path: &Path) -> AccessMode`
- `should_log(path: &Path) -> bool`
