Implement `cas init` — create the `.sandbox/` directory tree and generate persistent identity metadata.

## Implementation Notes (Divergences from Original Spec)

### access.log Location

The original spec placed `access.log` at `.sandbox/access.log`, but the syncing server expects it at `.sandbox/data/access.log`. The implementation follows the server's expectation.

**Steps:**
1. Fail if `.sandbox/` already exists (no implicit reset)
2. Create `.sandbox/data/objects/`
3. Generate random `shm_name` (`cas-` + alphanumeric)
4. Write default `config.toml` (empty lists)
5. Create empty `access.log`
6. Create `.gitignore` to ignore `.sandbox/data/` contents

## Data Layout

```
<project-root>/
└── .sandbox/
    ├── data/
    │   ├── metadata.bin      postcard: SandboxMetadata (shm_name, abi_version, next_id)
    │   ├── data.bin          postcard: HashMap<path, FileMetadata>
    │   ├── objects/          raw content blobs: objects/<shard>/<id_hex> (shard = low byte)
    │   └── access.log        first-access audit log (plain text) - NOTE: in data/, not .sandbox/
    ├── config.toml           whitelist/ignorelist/blocklist glob arrays
    ├── .gitignore            git ignore for access.log and data
    └── daemon.sock           unix socket (present only while daemon is alive)
```
