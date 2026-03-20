pub use log::LevelFilter;

use crate::config::Config;
use env_logger::Target;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;

pub fn init_logger<T: AsRef<Path>>(level: LevelFilter, log_path: Option<T>) {
    #[cfg(feature = "debug")]
    {
        eprintln!("level: {:?}", level);
        if let Some(ref x) = log_path {
            eprintln!("log_path: {:?}", x.as_ref());
        }
    }
    let target: Target = match log_path {
        Some(path) => {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(path)
                .expect("cannot create log file");
            Target::Pipe(Box::new(file))
        }
        None => Target::Stderr,
    };

    env_logger::Builder::from_default_env()
        .filter_module("fuser", LevelFilter::Info)
        .filter_level(level)
        .target(target)
        .format(|buf, record| writeln!(buf, "{}: {}", record.level(), record.args()))
        .init();
}

pub fn log_data_from_config(root: &PathBuf) -> (LevelFilter, Option<PathBuf>) {
    let config_path = root.join(".sandbox").join("config.toml");
    match Config::from_file(&config_path) {
        Ok(config) => (
            config.log_level_filter(),
            config.log.and_then(|x| PathBuf::from_str(x.as_str()).ok()),
        ),
        Err(_) => (LevelFilter::Info, None),
    }
}
