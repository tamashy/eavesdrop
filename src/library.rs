//! Recordings in the output folder.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, bail};

use crate::{meta, opus};

/// File extensions listed as recordings.
const EXTENSIONS: &[&str] = &["opus", "wav"];

fn is_recording(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| EXTENSIONS.iter().any(|e| x.eq_ignore_ascii_case(e)))
}

pub struct Recording {
    pub path: PathBuf,
    pub name: String,
    pub modified: Option<SystemTime>,
    pub size: u64,
    /// None if the file can't be read (e.g. still being written).
    pub duration: Option<Duration>,
    /// What the recording is about (from its `.meta.toml`), empty if none.
    pub description: String,
}

impl Recording {
    /// First line of the description.
    pub fn summary(&self) -> &str {
        self.description.lines().next().unwrap_or("")
    }
}

/// Recordings (`.opus`, `.wav`) directly in `dir`, newest first. A missing folder yields an empty list.
pub fn scan(dir: &Path) -> Result<Vec<Recording>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", dir.display())),
    };
    let mut recs: Vec<Recording> = entries
        .filter_map(|e| e.ok())
        .filter(|e| is_recording(&e.path()))
        .map(|e| {
            let path = e.path();
            let meta = e.metadata().ok();
            Recording {
                name: path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into(),
                modified: meta.as_ref().and_then(|m| m.modified().ok()),
                size: meta.map(|m| m.len()).unwrap_or(0),
                duration: duration(&path),
                description: meta::load(&path).map(|m| m.description).unwrap_or_default(),
                path,
            }
        })
        .collect();
    recs.sort_by(|a, b| {
        b.modified
            .cmp(&a.modified)
            .then_with(|| b.name.cmp(&a.name))
    });
    Ok(recs)
}

fn duration(path: &Path) -> Option<Duration> {
    if opus::is_opus(path) {
        return opus::duration(path);
    }
    let r = hound::WavReader::open(path).ok()?;
    let rate = r.spec().sample_rate;
    (rate > 0).then(|| Duration::from_secs_f64(r.duration() as f64 / rate as f64))
}

/// Characters not allowed in file names on at least one supported OS.
const FORBIDDEN: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];

/// Checks a new name typed by the user; returns it trimmed and without a
/// recording extension (`.opus`, `.wav`).
pub fn validate_name(name: &str) -> Result<String> {
    let name = name.trim();
    let name = EXTENSIONS
        .iter()
        .find_map(|ext| {
            let i = name.len().checked_sub(ext.len() + 1)?;
            let tail = name.get(i..)?;
            (tail.starts_with('.') && tail[1..].eq_ignore_ascii_case(ext))
                .then(|| name[..i].trim_end())
        })
        .unwrap_or(name);
    if name.is_empty() {
        bail!("name can't be empty");
    }
    if let Some(c) = name
        .chars()
        .find(|c| FORBIDDEN.contains(c) || c.is_control())
    {
        bail!("name can't contain {c:?}");
    }
    if name.starts_with('.') || name.ends_with('.') {
        bail!("name can't start or end with a dot");
    }
    Ok(name.to_string())
}

/// Renames a recording (and its metadata file) within its folder, keeping its
/// extension. Returns the new path.
pub fn rename(path: &Path, new_name: &str) -> Result<PathBuf> {
    let name = validate_name(new_name)?;
    let ext = path.extension().unwrap_or_default().to_string_lossy();
    let file = format!("{name}.{ext}");
    let dest = path.with_file_name(&file);
    if dest == path {
        return Ok(dest);
    }
    // On case-insensitive file systems a case-only rename "exists" already.
    let same_file = dest.to_string_lossy().to_lowercase() == path.to_string_lossy().to_lowercase();
    if dest.exists() && !same_file {
        bail!("{file} already exists");
    }
    std::fs::rename(path, &dest).with_context(|| format!("cannot rename to {}", dest.display()))?;
    let (old_meta, new_meta) = (meta::path_for(path), meta::path_for(&dest));
    if old_meta.exists() {
        std::fs::rename(&old_meta, &new_meta)
            .with_context(|| format!("renamed recording, but not {}", old_meta.display()))?;
    }
    Ok(dest)
}

/// The recording and its metadata file, if any.
fn files_of(path: &Path) -> Vec<PathBuf> {
    let mut files = vec![path.to_path_buf()];
    let m = meta::path_for(path);
    if m.exists() {
        files.push(m);
    }
    files
}

/// Moves a recording (and its metadata) to the system Trash / Recycle Bin.
pub fn trash(path: &Path) -> Result<()> {
    #[allow(unused_mut)]
    let mut ctx = trash::TrashContext::default();
    // macOS: the default asks Finder via AppleScript, which is slow and can block on
    // an "allow to control Finder" prompt; the system call needs no permission.
    #[cfg(target_os = "macos")]
    {
        use trash::macos::{DeleteMethod, TrashContextExtMacos};
        ctx.set_delete_method(DeleteMethod::NsFileManager);
    }
    ctx.delete_all(files_of(path))
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Deletes a recording (and its metadata) permanently.
pub fn delete(path: &Path) -> Result<()> {
    for f in files_of(path) {
        std::fs::remove_file(&f).with_context(|| format!("cannot delete {}", f.display()))?;
    }
    Ok(())
}

pub fn fmt_modified(t: Option<SystemTime>) -> String {
    t.map(|t| {
        chrono::DateTime::<chrono::Local>::from(t)
            .format("%Y-%m-%d %H:%M")
            .to_string()
    })
    .unwrap_or_else(|| "?".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_dir_is_empty() {
        assert!(
            scan(Path::new("/nonexistent/eavesdrop"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn name_validation() {
        assert_eq!(
            validate_name("  Standup 2026.WAV ").unwrap(),
            "Standup 2026"
        );
        assert_eq!(validate_name("call.opus").unwrap(), "call");
        assert_eq!(validate_name("a.b").unwrap(), "a.b");
        assert_eq!(
            validate_name("Встреча с командой").unwrap(),
            "Встреча с командой"
        );
        for bad in [
            "", "  ", ".wav", ".OPUS", "a/b", "a\\b", "x:y", "q?", ".hidden", "end.",
        ] {
            assert!(validate_name(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn rename_moves_metadata_and_refuses_overwrite() {
        let dir = std::env::temp_dir().join(format!("eavesdrop-ren-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.wav");
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(dir.join("taken.wav"), b"y").unwrap();
        meta::set_description(&a, "about a").unwrap();

        assert!(rename(&a, "taken").is_err());
        let b = rename(&a, "Planning notes").unwrap();
        assert_eq!(b, dir.join("Planning notes.wav"));
        assert!(!a.exists() && b.exists());
        assert_eq!(meta::load(&b).unwrap().description, "about a");
        assert!(!meta::path_for(&a).exists());

        let c = dir.join("c.opus");
        std::fs::write(&c, b"z").unwrap();
        assert_eq!(rename(&c, "Call").unwrap(), dir.join("Call.opus"));

        delete(&b).unwrap();
        assert!(!b.exists() && !meta::path_for(&b).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lists_only_recordings_with_duration() {
        let dir = std::env::temp_dir().join(format!("eavesdrop-lib-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".tmp-x")).unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 1000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(dir.join("a.wav"), spec).unwrap();
        for _ in 0..2500 {
            w.write_sample(0i16).unwrap();
        }
        w.finalize().unwrap();
        let mut o = opus::Writer::create(&dir.join("b.opus")).unwrap();
        for _ in 0..opus::RATE / 2 {
            o.write(0.0).unwrap();
        }
        o.finish().unwrap();
        // Make the WAV the newest so the order is deterministic.
        let f = std::fs::File::options()
            .append(true)
            .open(dir.join("a.wav"))
            .unwrap();
        f.set_modified(SystemTime::now() + Duration::from_secs(60))
            .unwrap();

        let recs = scan(&dir).unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[1].name, "b.opus");
        assert_eq!(recs[1].duration, Some(Duration::from_millis(500)));
        assert_eq!(recs[0].name, "a.wav");
        assert_eq!(recs[0].duration, Some(Duration::from_millis(2500)));
        assert_eq!(recs[0].description, "");
        meta::set_description(&dir.join("a.wav"), "Line one\nline two").unwrap();
        let recs = scan(&dir).unwrap();
        assert_eq!(recs.len(), 2, "metadata file must not be listed");
        assert_eq!(recs[0].summary(), "Line one");
        std::fs::remove_dir_all(&dir).ok();
    }
}
