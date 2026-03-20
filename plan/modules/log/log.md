Implement `log.rs` — Logging initialization and configuration.

---

## Functions

### `init_logger(level: LevelFilter, log_path: Option<&str>)`

Initialize the `env_logger` with the given log level filter and destination.

- **Target**: stderr if `log_path` is `None`; file at `log_path` if `Some(path)`.
- **fuser module**: Always logged at `Info` level (to reduce noise from FUSE library)
- **Other modules**: Logged at the specified `level`
- **Debug feature** (`--features debug`): ignored for log destination — destination is fully controlled by `log_path` config.

### `log_level_from_config(root: &PathBuf) -> LevelFilter`

Read `logLevel` from `.sandbox/config.toml` and convert to `LevelFilter`.

- **Config path**: `{root}/.sandbox/config.toml`
- **Fallback**: Returns `LevelFilter::Info` if config file doesn't exist or `logLevel` is not specified
- **Valid values**: `error`, `warn`, `info`, `debug`, `trace`

## Dependencies

- `env_logger` — for log output
- `log` — for `LevelFilter` and logging macros
- `crate::config::Config` — for reading config file

## Note

- The module is declared as `mod inner_log;` with `#[path = "log.rs"]` in `main.rs` to avoid naming conflict with the `log` crate.
- When using the module's functions in `main.rs`, call them via `inner_log::` prefix (e.g., `inner_log::init_logger()`).
