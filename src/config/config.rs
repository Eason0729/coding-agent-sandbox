use std::fs;
use std::path::Path;

use log::LevelFilter;
use serde::Deserialize;
use thiserror::Error;

/// Configuration loaded from `.sandbox/config.toml`.
///
/// All fields are optional; missing fields default to an empty list.
///
/// # Policy (evaluation order: blacklist → whitelist → default CoW)
///
/// | List        | AccessMode   | Logged? |
/// |-------------|--------------|---------|
/// | blacklist   | FuseOnly     | No      |
/// | whitelist   | Passthrough  | No      |
/// | disableLog  | CopyOnWrite  | No      |
/// | (default)   | CopyOnWrite  | Yes     |
///
/// Implicit rules (applied after user-supplied lists):
/// * `$(pwd)` (project root) is added to the whitelist unless the user
///   explicitly placed it in the blacklist.
/// * `.sandbox` is added to the blacklist unless the user explicitly placed
///   it in the whitelist.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(
        default,
        alias = "whitelist",
        alias = "Whitelist",
        alias = "white_list"
    )]
    pub whitelist: Vec<String>,

    #[serde(
        default,
        alias = "blacklist",
        alias = "Blacklist",
        alias = "black_list"
    )]
    pub blacklist: Vec<String>,

    #[serde(
        default,
        alias = "disableLog",
        alias = "DisableLog",
        alias = "disable_log"
    )]
    pub disable_log: Vec<String>,

    #[serde(default, alias = "logLevel", alias = "LogLevel", alias = "log_level")]
    pub log_level: Option<String>,

    #[serde(default, alias = "log", alias = "Log", alias = "log_path")]
    pub log: Option<String>,
}

/// Error returned by [`Config::from_str`] and [`Config::from_file`].
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("I/O error reading config: {0}")]
    Io(#[from] std::io::Error),

    #[error("TOML parse error: {0}")]
    Toml(#[from] toml::de::Error),
}

impl Config {
    /// Parse a `Config` from a TOML string.
    pub fn from_str(s: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(s)?)
    }

    /// Read and parse a `Config` from a TOML file on disk.
    pub fn from_file(path: &Path) -> Result<Self, ConfigError> {
        let content = fs::read_to_string(path)?;
        Ok(toml::from_str(&content)?)
    }

    /// Returns the logging level filter based on config.
    /// Defaults to Info if not specified or invalid.
    pub fn log_level_filter(&self) -> LevelFilter {
        match self.log_level.as_deref() {
            Some("error") => LevelFilter::Error,
            Some("warn") => LevelFilter::Warn,
            Some("info") => LevelFilter::Info,
            Some("debug") => LevelFilter::Debug,
            Some("trace") => LevelFilter::Trace,
            _ => LevelFilter::Info,
        }
    }
}
