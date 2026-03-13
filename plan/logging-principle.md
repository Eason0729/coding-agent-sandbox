# Logging Principle

## Logging Targets

- stderr: Runtime debugging, troubleshooting (via `log` crate)
- ./.sandbox/access.log: log to remind user

## Log Level

- Configurable via `logLevel` in config.toml
- Levels: error, warn, info, debug, trace (default: info)
- Set via `RUST_LOG` environment variable or config file

## Logger Initialization

- Logger initialized in main.rs before any command execution
- Uses `env_logger` or similar with filter set from config
- All error/warn/info/debug/trace calls go through `log` crate macros

## Migration from eprintln!

- Replace all `eprintln!` with appropriate `log::error!`, `log::warn!`, etc.
- Keep stderr output for critical errors that should always be visible
- Access log (.sandbox/access.log) remains separate for file access records
