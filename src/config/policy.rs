use std::env;
use std::path::{Path, PathBuf};

use globset::{Error as GlobError, Glob, GlobSet, GlobSetBuilder};

use crate::config::config::Config;
use crate::fuse::policy::{AccessMode, Policy};

fn expand_path(pat: &str) -> String {
    if pat.starts_with("~/") {
        if let Some(home) = env::var_os("HOME") {
            let mut expanded = PathBuf::from(home);
            expanded.push(&pat[2..]);
            return expanded.to_string_lossy().into_owned();
        }
    }
    pat.to_string()
}

fn build_globset(patterns: &[String]) -> Result<GlobSet, GlobError> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        let expanded = expand_path(pat);
        builder.add(Glob::new(&expanded)?);
    }
    builder.build()
}

/// Concrete [`Policy`] implementation driven by a parsed [`Config`].
///
/// Glob patterns from the config are matched against **paths relative to the
/// project root**.  When [`classify`] / [`should_log`] receive an absolute
/// path (as stored in the inode table), the project root prefix is stripped
/// first.  If the path does not start with the root the original path is used
/// as-is so that FUSE-virtual paths (e.g. `/src/main.rs`) are still handled
/// sensibly.
///
/// ## Evaluation order
///
/// 1. Blacklist → [`AccessMode::FuseOnly`], no log.
/// 2. Whitelist → [`AccessMode::Passthrough`], no log.
/// 3. `disableLog` → [`AccessMode::CopyOnWrite`], no log.
/// 4. Default → [`AccessMode::CopyOnWrite`], logged.
pub struct ConfigPolicy {
    /// Absolute path of the project root (used for prefix-stripping).
    root: PathBuf,

    /// Compiled blacklist glob set.
    blacklist: GlobSet,
    /// Compiled whitelist glob set.
    whitelist: GlobSet,
    /// Compiled disableLog glob set.
    disable_log: GlobSet,

    /// Whether the project root directory itself is implicitly whitelisted.
    /// Set to `false` only when the user explicitly blacklisted it.
    root_is_whitelisted: bool,

    /// Optional extra implicit passthrough anchor (current working directory).
    /// Applied to the full subtree unless explicitly blacklisted.
    cwd: Option<PathBuf>,
    cwd_is_whitelisted: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn build_globset_builder(patterns: &[String]) -> Result<GlobSetBuilder, GlobError> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        let expanded = expand_path(pat);
        builder.add(Glob::new(&expanded)?);
    }
    Ok(builder)
}

// ─────────────────────────────────────────────────────────────────────────────
// ConfigPolicy
// ─────────────────────────────────────────────────────────────────────────────

impl ConfigPolicy {
    /// Build a `ConfigPolicy` from a parsed `Config` and the project root.
    ///
    /// Applies implicit rules:
    /// * `.sandbox/**` is added to the blacklist unless `.sandbox` already
    ///   matches the user-provided whitelist.
    /// * The project root itself (relative path `""`) is treated as passthrough
    ///   unless it matches the user-provided blacklist.
    /// * `cwd` subtree is also treated as passthrough unless it is blacklisted.
    pub fn from_config(
        config: &Config,
        root: &Path,
        cwd: Option<&Path>,
    ) -> Result<Self, GlobError> {
        // Build user-only sets for implicit-rule checks (no implicit rules yet).
        let user_whitelist = build_globset(&config.whitelist)?;
        let user_blacklist = build_globset(&config.blacklist)?;

        // Now build the final sets, starting from the user patterns.
        let mut bl_builder = build_globset_builder(&config.blacklist)?;
        let wl_builder = build_globset_builder(&config.whitelist)?;
        let dl_builder = build_globset_builder(&config.disable_log)?;

        // Implicit: .sandbox → blacklist (unless user whitelisted it).
        if !user_whitelist.is_match(".sandbox") {
            bl_builder.add(Glob::new(".sandbox")?);
            bl_builder.add(Glob::new(".sandbox/**")?);
        }

        // Implicit: pwd → whitelist flag (unless user blacklisted it).
        // We represent this as a boolean rather than a glob because matching
        // an empty/dot path with globset is unreliable.
        let root_is_whitelisted = !user_blacklist.is_match(".") && !user_blacklist.is_match("");

        let cwd = cwd.map(Path::to_path_buf);
        let cwd_is_whitelisted = cwd
            .as_ref()
            .map(|p| {
                !user_blacklist.is_match(p)
                    && !user_blacklist.is_match(p.as_os_str().to_string_lossy().as_ref())
            })
            .unwrap_or(false);

        Ok(ConfigPolicy {
            root: root.to_path_buf(),
            blacklist: bl_builder.build()?,
            whitelist: wl_builder.build()?,
            disable_log: dl_builder.build()?,
            root_is_whitelisted,
            cwd,
            cwd_is_whitelisted,
        })
    }

    /// Strip the project-root prefix from `path`, returning the relative
    /// sub-path.  Falls back to `path` itself when the prefix is absent (e.g.
    /// absolute paths outside the project root like `/bin/bash`).  The result
    /// is non-absolute iff the path lives under the project root.
    fn rel<'a>(&self, path: &'a Path) -> &'a Path {
        path.strip_prefix(&self.root).unwrap_or(path)
    }
}

impl Policy for ConfigPolicy {
    fn classify(&self, path: &Path) -> AccessMode {
        let rel = self.rel(path);

        // A non-absolute relative path means strip_prefix succeeded → path is
        // under (or equal to) the project root.
        let under_root = !rel.is_absolute();

        // Blacklist first (applies everywhere, including inside project root).
        if self.blacklist.is_match(rel) {
            return AccessMode::FuseOnly;
        }

        // Explicit user whitelist.
        if self.whitelist.is_match(rel) {
            return AccessMode::Passthrough;
        }

        // Implicit: the entire project root tree is passthrough unless the user
        // explicitly blacklisted it.
        if under_root && self.root_is_whitelisted {
            return AccessMode::Passthrough;
        }

        if self.cwd_is_whitelisted {
            if let Some(cwd) = &self.cwd {
                if path == cwd || path.starts_with(cwd) {
                    return AccessMode::Passthrough;
                }
            }
        }

        AccessMode::CopyOnWrite
    }

    fn should_log(&self, path: &Path) -> bool {
        let rel = self.rel(path);
        let under_root = !rel.is_absolute();

        // Never log blacklisted paths (FuseOnly).
        if self.blacklist.is_match(rel) {
            return false;
        }
        // Never log explicit whitelist (Passthrough).
        if self.whitelist.is_match(rel) {
            return false;
        }
        // Never log implicit project-root passthrough.
        if under_root && self.root_is_whitelisted {
            return false;
        }
        // Never log implicit current-working-directory passthrough.
        if self.cwd_is_whitelisted {
            if let Some(cwd) = &self.cwd {
                if path == cwd || path.starts_with(cwd) {
                    return false;
                }
            }
        }
        // Never log paths where logging is explicitly disabled.
        if self.disable_log.is_match(rel) {
            return false;
        }
        // Remaining paths are CopyOnWrite outside the project root → log them.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::ConfigPolicy;
    use crate::config::config::Config;
    use crate::fuse::policy::{AccessMode, Policy};
    use std::env;
    use std::path::Path;

    fn empty_config() -> Config {
        Config {
            whitelist: Vec::new(),
            blacklist: Vec::new(),
            disable_log: Vec::new(),
            log_level: None,
            log: None,
        }
    }

    #[test]
    fn implicit_cwd_is_passthrough() {
        let cfg = empty_config();
        let root = Path::new("/project/eason/cas");
        let cwd = Path::new("/home/eason/.cargo");
        let policy = ConfigPolicy::from_config(&cfg, root, Some(cwd)).expect("policy");

        assert_eq!(
            policy.classify(Path::new("/home/eason/.cargo")),
            AccessMode::Passthrough
        );
        assert_eq!(
            policy.classify(Path::new("/home/eason/.cargo/registry/index")),
            AccessMode::Passthrough
        );
        assert!(!policy.should_log(Path::new("/home/eason/.cargo/registry/index")));
    }

    #[test]
    fn blacklist_still_wins_over_implicit_cwd() {
        let mut cfg = empty_config();
        cfg.blacklist = vec!["/home/eason/.cargo/**".to_string()];

        let root = Path::new("/project/eason/cas");
        let cwd = Path::new("/home/eason/.cargo");
        let policy = ConfigPolicy::from_config(&cfg, root, Some(cwd)).expect("policy");

        assert_eq!(
            policy.classify(Path::new("/home/eason/.cargo/registry/index")),
            AccessMode::FuseOnly
        );
        assert!(!policy.should_log(Path::new("/home/eason/.cargo/registry/index")));
    }
}
