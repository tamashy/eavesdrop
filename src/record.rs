//! Recording lifecycle: the chosen devices (`Recorder`) and one recording (`Take`).
//!
//! A take opens the capture streams right away (so meters are live) and writes
//! nothing until its clock is started. Stopping a take closes the streams
//! immediately and finishes the file (mixdown) on a background thread, so the
//! next take can be opened right after.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use cpal::{Device, SupportedStreamConfig};

use crate::{Meter, Track, clock::Clock, config::Format, dev_name, mixdown, start_track};

pub struct Input {
    dev: Device,
    cfg: SupportedStreamConfig,
    pub name: String,
}

impl Input {
    pub fn new(dev: Device, cfg: SupportedStreamConfig) -> Self {
        let name = dev_name(&dev);
        Self { dev, cfg, name }
    }
}

pub struct Recorder {
    pub system: Input,
    pub mic: Option<Input>,
    pub out_dir: PathBuf,
    pub mic_gain: f32,
    pub keep_tracks: bool,
    pub format: Format,
}

pub struct Take {
    pub clock: Arc<Clock>,
    pub system: Arc<Meter>,
    pub mic: Option<Arc<Meter>>,
    system_track: Track,
    mic_track: Option<Track>,
    work_dir: PathBuf,
    out_dir: PathBuf,
    mic_gain: f32,
    keep_tracks: bool,
    format: Format,
}

pub enum Outcome {
    /// Never started: nothing written.
    Discarded,
    /// Thrown away with `Take::discard`: nothing saved.
    Aborted,
    Saved {
        path: PathBuf,
        recorded: Duration,
        /// Separate system / mic tracks (with --keep-tracks).
        extra: Vec<PathBuf>,
        warnings: Vec<String>,
    },
}

/// A take being finished in the background.
pub type Saving = JoinHandle<Result<Outcome>>;

impl Recorder {
    /// Opens the streams for a new take (in the Ready state).
    pub fn new_take(&self) -> Result<Take> {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
        let work_dir = self.out_dir.join(format!(
            ".tmp-{stamp}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&work_dir)
            .with_context(|| format!("cannot create {}", work_dir.display()))?;

        let clock = Arc::new(Clock::new(Instant::now()));
        let system = Arc::new(Meter::default());
        let opened = (|| -> Result<_> {
            let system_track = start_track(
                &self.system.dev,
                self.system.cfg,
                &work_dir.join("system.wav"),
                clock.clone(),
                system.clone(),
            )?;
            let mic = match &self.mic {
                Some(m) => {
                    let meter = Arc::new(Meter::default());
                    let track = start_track(
                        &m.dev,
                        m.cfg,
                        &work_dir.join("mic.wav"),
                        clock.clone(),
                        meter.clone(),
                    )?;
                    Some((track, meter))
                }
                None => None,
            };
            Ok((system_track, mic))
        })();
        let (system_track, mic) = match opened {
            Ok(x) => x,
            Err(e) => {
                std::fs::remove_dir_all(&work_dir).ok();
                return Err(e);
            }
        };
        let (mic_track, mic) = match mic {
            Some((t, m)) => (Some(t), Some(m)),
            None => (None, None),
        };
        Ok(Take {
            clock,
            system,
            mic,
            system_track,
            mic_track,
            work_dir,
            out_dir: self.out_dir.clone(),
            mic_gain: self.mic_gain,
            keep_tracks: self.keep_tracks,
            format: self.format,
        })
    }
}

impl Take {
    /// Where the recording will be saved: named after the moment it was started
    /// (or now, if not started yet).
    pub fn output_path(&self) -> PathBuf {
        let at = match self.clock.started_at() {
            Some(t) => {
                chrono::Local::now() - chrono::Duration::from_std(t.elapsed()).unwrap_or_default()
            }
            None => chrono::Local::now(),
        };
        self.out_dir.join(format!(
            "eavesdrop_{}.{}",
            at.format("%Y-%m-%d_%H-%M-%S"),
            self.format.extension()
        ))
    }

    /// Stops capturing now; the file is finished on a background thread.
    /// `description` (may be empty) is saved as the recording's metadata.
    pub fn stop(self, description: String) -> Saving {
        self.finish(description, true)
    }

    /// Stops capturing and throws the recording away: nothing is saved and the
    /// raw tracks are deleted (in the background, once the writers are done).
    pub fn discard(self) -> Saving {
        self.finish(String::new(), false)
    }

    fn finish(self, description: String, save: bool) -> Saving {
        // Freeze the recorded time so all tracks end at the same point.
        if self.clock.state() == crate::clock::State::Recording {
            self.clock.toggle();
        }
        let started = self.clock.started();
        let recorded = self.clock.recorded();
        let path = unique(self.output_path());
        // Dropping the streams closes the channels, which lets the writer threads finish.
        let system = self.system_track.stop();
        let mic = self.mic_track.map(Track::stop);
        let (work_dir, out_dir) = (self.work_dir, self.out_dir);
        let (mic_gain, keep_tracks, format) = (self.mic_gain, self.keep_tracks, self.format);
        std::thread::spawn(move || {
            let join = |h: JoinHandle<Result<f32>>| {
                h.join()
                    .map_err(|_| anyhow::anyhow!("writer thread panicked"))?
            };
            let system_peak = join(system)?;
            let mic_peak = mic.map(join).transpose()?;
            if !started || !save {
                std::fs::remove_dir_all(&work_dir).ok();
                return Ok(if started {
                    Outcome::Aborted
                } else {
                    Outcome::Discarded
                });
            }
            let warnings = silence_warnings(system_peak, mic_peak);

            let system_path = work_dir.join("system.wav");
            let mic_path = work_dir.join("mic.wav");
            let mic_input = mic_peak.map(|_| mic_path.as_path());
            mixdown(&system_path, mic_input, mic_gain, &path, format).with_context(|| {
                format!("mixdown failed; raw tracks kept in {}", work_dir.display())
            })?;

            let mut warnings = warnings;
            if !description.trim().is_empty()
                && let Err(e) = crate::meta::set_description(&path, &description)
            {
                warnings.push(format!("⚠ Description not saved: {e:#}"));
            }

            let mut extra = Vec::new();
            if keep_tracks {
                let stem = path.file_stem().unwrap_or_default().to_string_lossy();
                for (p, suffix) in [(&system_path, "system"), (&mic_path, "mic")] {
                    if p.exists() {
                        let dest = out_dir.join(format!("{stem}_{suffix}.wav"));
                        std::fs::rename(p, &dest)?;
                        extra.push(dest);
                    }
                }
            }
            std::fs::remove_dir_all(&work_dir).ok();
            Ok(Outcome::Saved {
                path,
                recorded,
                extra,
                warnings,
            })
        })
    }
}

fn silence_warnings(system_peak: f32, mic_peak: Option<f32>) -> Vec<String> {
    let mut w = Vec::new();
    if system_peak < 0.001 {
        w.push(format!(
            "⚠ System audio track is (almost) silent. {}",
            if cfg!(target_os = "macos") {
                "On macOS, allow your terminal app under System Settings › Privacy & Security › \
                 Screen & System Audio Recording (\"System Audio Recording Only\")."
            } else {
                "Check that the sound plays through the captured output device (see --list / --system)."
            }
        ));
    }
    if mic_peak.is_some_and(|p| p < 0.001) {
        w.push("⚠ Microphone track is silent — check microphone permission / device.".into());
    }
    w
}

/// `path`, or `path` with `_2`, `_3`, … appended if it already exists.
fn unique(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    let stem = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let ext = path
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    (2..)
        .map(|n| dir.join(format!("{stem}_{n}.{ext}")))
        .find(|p| !p.exists())
        .unwrap_or(path)
}
