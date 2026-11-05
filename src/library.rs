//! Recordings in the output folder.

use std::{
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result};

pub struct Recording {
    pub path: PathBuf,
    pub name: String,
    pub modified: Option<SystemTime>,
    pub size: u64,
    /// None if the WAV header can't be read (e.g. still being written).
    pub duration: Option<Duration>,
}

/// WAV files directly in `dir`, newest first. A missing folder yields an empty list.
pub fn scan(dir: &Path) -> Result<Vec<Recording>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", dir.display())),
    };
    let mut recs: Vec<Recording> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            let p = e.path();
            p.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("wav"))
        })
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
                duration: wav_duration(&path),
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

fn wav_duration(path: &Path) -> Option<Duration> {
    let r = hound::WavReader::open(path).ok()?;
    let rate = r.spec().sample_rate;
    (rate > 0).then(|| Duration::from_secs_f64(r.duration() as f64 / rate as f64))
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
    fn lists_only_wav_with_duration() {
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

        let recs = scan(&dir).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].name, "a.wav");
        assert_eq!(recs[0].duration, Some(Duration::from_millis(2500)));
        std::fs::remove_dir_all(&dir).ok();
    }
}
