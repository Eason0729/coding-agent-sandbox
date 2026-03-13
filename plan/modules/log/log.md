Implement `log.rs` — Logging initialization and configuration.

---

## Functions

### `init_logger(level: LevelFilter)`

Initialize the `env_logger` with the given log level filter.

- **Target**: stderr (or pipe to file if `debug` feature is enabled)
- **fuser module**: Always logged at `Info` level (to reduce noise from FUSE library)
- **Other modules**: Logged at the specified `level`
- **Debug feature**: When compiled with `--features debug`, logs are also written to `/tmp/test.txt`

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
