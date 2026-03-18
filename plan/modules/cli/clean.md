Implement `cas clean` — clean data directory or initialize sandbox if not exists

## Behavior

`cas clean` has two behaviors:
- If `.sandbox/` does not exist: initialize sandbox with default config
- If `.sandbox/` exists: remove fuse data and reset SHM

## Options

- `--force`, `-f`: Force clean even if daemon is running (default: false)

## Daemon Check

When not using `--force`:
1. Check if `daemon.sock` exists
2. Try to connect to the socket to verify daemon is running
3. If daemon is running, abort with error: "daemon is running. Use --force to clean anyway"

## Steps (when cleaning)

1. Load metadata to get `shm_name` before removing data
2. Remove `.sandbox/data/` (includes `metadata.bin`, `data.bin`, `objects/`, `access.log`)
3. Attempt to unlink the SHM segment (if it exists)
4. Remove `daemon.sock` if present

## Steps (when initializing)

1. Create `.sandbox/data/objects/`
2. Generate random `shm_name` (`cas-` + alphanumeric)
3. Write default `config.toml` (empty lists)
4. Create empty `access.log`
5. Create `.gitignore` to ignore `.sandbox/data/` contents

## Implementation Notes

- If the SHM segment doesn't exist (already cleaned or never used), the unlink operation is silently ignored
- The metadata is loaded first to get the `shm_name` before the data directory is deleted
