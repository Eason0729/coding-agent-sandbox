Implement `cas purge` — delete entire .sandbox directory

## Steps

1. Fail if `.sandbox/` does not exist (not initialized)
2. Remove the entire `.sandbox/` directory recursively

## Implementation Notes

- This is a destructive operation that completely removes all sandbox data including:
  - Configuration
  - Metadata
  - FUSE data
  - Object store
  - Access logs
  - SHM segment
  - Daemon socket
- Use `cas clean` instead if you want to reset the sandbox while preserving configuration
