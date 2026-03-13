pub use log::LevelFilter;

use crate::config::Config;
use env_logger::Target;
use std::io::Write;
use std::path::PathBuf;

#[cfg(feature = "debug")]
pub fn init_logger(_level: LevelFilter) {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open("./stderr.log")
        .expect("cannot create log file");

    let file = Box::new(file);

    env_logger::Builder::from_default_env()
        .filter_module("fuser", LevelFilter::Info)
        .filter_level(LevelFilter::Debug)
        .target(Target::Pipe(Box::new(file)))
        .format(|buf, record| writeln!(buf, "{}: {}", record.level(), record.args()))
        .init();
}

#[cfg(not(feature = "debug"))]
pub fn init_logger(level: LevelFilter) {
    env_logger::Builder::from_default_env()
        .filter_module("fuser", LevelFilter::Info)
        .filter_level(level)
        .target(Target::Stderr)
        .format(|buf, record| writeln!(buf, "{}: {}", record.level(), record.args()))
        .init();
}

pub fn log_level_from_config(root: &PathBuf) -> LevelFilter {
    let config_path = root.join(".sandbox").join("config.toml");
    if config_path.exists() {
        Config::from_file(&config_path)
            .ok()
            .map(|c| c.log_level_filter())
            .unwrap_or(LevelFilter::Info)
    } else {
        LevelFilter::Info
    }
}
