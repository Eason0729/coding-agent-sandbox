Implement `config/config.rs` — `Config` struct and TOML parsing.

---

## TOML Format

```toml
# Paths that should pass through to real filesystem (read+write) and does not print log access
whitelist = []

# Paths that should be hidden from the real filesystem and does not print log access
blacklist = []

# Path that does not print log access
disableLog = []

# Path by default does copyOnWrite + log access

# Logging level: error, warn, info, debug, trace (default: info)
logLevel = "info"

# Log output destination (default: ./.sandbox/cas.log)
# If omitted, logs go to stderr.
# If set to a path, logs are written to that file.
log = "./.sandbox/cas.log"
```

- All fields are optional. If omitted, the list is empty (logLevel defaults to "info").
- `log` field: `Option<String>`. Omitted means stderr; set to a path string means log to that file.

## Note
- implicitly add pwd to whitelist(If pwd does not match blacklist)
- implicitly add current working directory to whitelist(If it does not match blacklist)
- implicitly add .sandbox to blocklist(If it does not match whitelist)
- config and AccessMode are different(very important!), path are not automatically AccessMode::Passthrough when blacklist
