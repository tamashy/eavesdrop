//! Per-recording metadata, kept in a small TOML file next to the recording:
//! `eavesdrop_2026-11-08_10-00-00.opus` → `eavesdrop_2026-11-08_10-00-00.meta.toml`.
//!
//! ```toml
//! description = """
//! Ideas for the onboarding redesign.
//! Try the shorter sign-up flow first."""
//! ```
//!
//! A sidecar file (instead of tags inside the audio file) keeps the recording untouched
//! and is easy to read, edit, grep or sync with other tools.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    /// What the recording is about (free text, may span several lines).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

impl Meta {
    fn is_empty(&self) -> bool {
        self.description.is_empty()
    }
}

/// Metadata file belonging to `wav`.
pub fn path_for(wav: &Path) -> PathBuf {
    wav.with_extension("meta.toml")
}

/// Loads the metadata of `wav`; a missing file means no metadata.
pub fn load(wav: &Path) -> Result<Meta> {
    let path = path_for(wav);
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("invalid {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Meta::default()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Saves the metadata of `wav`. Empty metadata removes the file.
pub fn save(wav: &Path, meta: &Meta) -> Result<()> {
    let path = path_for(wav);
    if meta.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("cannot remove {}", path.display()))
            }
            _ => Ok(()),
        };
    }
    // Write to a temp file and rename it over, so a crash never leaves half a file.
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, toml::to_string(meta)?)
        .and_then(|()| std::fs::rename(&tmp, &path))
        .with_context(|| format!("cannot write {}", path.display()))
}

/// Sets the description of `wav` (trailing whitespace trimmed).
pub fn set_description(wav: &Path, text: &str) -> Result<()> {
    let mut meta = load(wav).unwrap_or_default();
    meta.description = text.trim_end().to_string();
    save(wav, &meta)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("eavesdrop-meta-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn sidecar_path() {
        assert_eq!(
            path_for(Path::new("/r/eavesdrop_1.wav")),
            PathBuf::from("/r/eavesdrop_1.meta.toml")
        );
    }

    #[test]
    fn roundtrip_multiline_and_clear() {
        let dir = tmp_dir("rt");
        let wav = dir.join("a.wav");
        assert_eq!(load(&wav).unwrap(), Meta::default());

        let text = "Ideas for \"onboarding\"\nTry it first ✓";
        set_description(&wav, &format!("{text}\n\n")).unwrap();
        assert_eq!(load(&wav).unwrap().description, text);

        set_description(&wav, "  ").unwrap();
        assert!(!path_for(&wav).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unknown_fields_are_tolerated() {
        let m: Meta = toml::from_str("description = \"x\"\nfuture = 1").unwrap();
        assert_eq!(m.description, "x");
    }
}
