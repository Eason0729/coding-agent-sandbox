In-memory closure table plus direct-child index for the syncing server.

## Responsibilities

- Track path ancestry and descendant relationships for sandbox metadata.
- Provide fast direct-child lookup for `ReadDirAll`.
- Provide fast subtree lookup for rename and whiteout traversal.
- Remain serializable so the server can persist and restore state.
