//! Optional user config file (TOML).
//!
//! Location: `~/.config/eavesdrop/config.toml` on macOS/Linux
//! (`$XDG_CONFIG_HOME/eavesdrop/config.toml` if set), `%APPDATA%\eavesdrop\config.toml`
//! on Windows. Override with `--config <path>`.
//!
//! ```toml
//! # Folder where recordings are saved. `~` expands to your home directory.
//! output_dir = "~/Documents/Recordings"
//!
//! # Color theme: default, catppuccin-mocha, catppuccin-latte, nord, … (see `theme`).
//! theme = "catppuccin-mocha"
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Folder where recordings are saved.
    pub output_dir: Option<PathBuf>,
    /// Color theme name.
    pub theme: Option<String>,
}

impl Config {
    /// Loads the config. An explicit `path` must exist; the default location is optional.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let (path, required) = match path {
            Some(p) => (p.to_path_buf(), true),
            None => match default_path() {
                Some(p) => (p, false),
                None => return Ok(Self::default()),
            },
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if !required && e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        Self::parse(&text).with_context(|| format!("invalid config {}", path.display()))
    }

    fn parse(text: &str) -> Result<Self> {
        let mut cfg: Self = toml::from_str(text)?;
        cfg.output_dir = cfg.output_dir.map(|p| expand_tilde(&p));
        if let Some(t) = &cfg.theme
            && crate::theme::find(t).is_none()
        {
            anyhow::bail!("unknown theme {t:?} (available: {})", crate::theme::names());
        }
        Ok(cfg)
    }
}

/// Default config file location.
pub fn default_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        dirs::config_dir()
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
    };
    base.map(|b| b.join("eavesdrop").join("config.toml"))
}

/// Expands a leading `~` to the home directory.
fn expand_tilde(p: &Path) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => match dirs::home_dir() {
            Some(home) => home.join(rest),
            None => p.to_path_buf(),
        },
        Err(_) => p.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config() {
        assert!(Config::parse("").unwrap().output_dir.is_none());
    }

    #[test]
    fn absolute_output_dir() {
        let cfg = Config::parse(r#"output_dir = "/tmp/rec""#).unwrap();
        assert_eq!(cfg.output_dir, Some(PathBuf::from("/tmp/rec")));
    }

    #[test]
    fn tilde_expands() {
        let cfg = Config::parse(r#"output_dir = "~/Recs""#).unwrap();
        assert_eq!(cfg.output_dir, Some(dirs::home_dir().unwrap().join("Recs")));
    }

    #[test]
    fn theme_is_validated() {
        let cfg = Config::parse(r#"theme = "catppuccin-latte""#).unwrap();
        assert_eq!(cfg.theme.as_deref(), Some("catppuccin-latte"));
        assert!(Config::parse(r#"theme = "neon""#).is_err());
    }

    #[test]
    fn unknown_key_rejected() {
        assert!(Config::parse(r#"outptu_dir = "/x""#).is_err());
    }

    #[test]
    fn missing_explicit_file_errors() {
        assert!(Config::load(Some(Path::new("/nonexistent/eavesdrop.toml"))).is_err());
    }
}
