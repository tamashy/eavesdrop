//! eavesdrop — record audio locally: what plays on the computer (system audio)
//! plus your microphone, mixed into one Opus (or WAV) file.
//!
//! It captures the system's audio output via OS loopback, so it works with any
//! app that plays sound, without integrating with it.
//!
//!  * macOS 14.6+ : Core Audio process tap (needs "System Audio Recording" permission)
//!  * Windows     : WASAPI loopback
//!  * Linux       : PulseAudio / PipeWire monitor source

mod browser;
mod clock;
mod config;
mod input;
mod library;
mod meta;
mod meter;
mod opus;
mod player;
mod record;
mod theme;
mod ui;

use clock::Clock;

use std::{
    fs::File,
    io::{BufReader, IsTerminal},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// Live status of one track, shared between the audio threads and the UI.
#[derive(Default)]
pub struct Meter {
    /// Peak level (f32 bits) since the UI last read it.
    peak: AtomicU32,
    /// Bytes written to the track file so far.
    pub bytes: AtomicU64,
    /// When set, silence is written instead of the input (mic mute).
    pub muted: AtomicBool,
    /// Last stream error, if any.
    pub error: Mutex<Option<String>>,
}

impl Meter {
    fn record_peak(&self, p: f32) {
        let _ = self
            .peak
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
                (p > f32::from_bits(old)).then_some(p.to_bits())
            });
    }
    /// Returns the peak since the previous call and resets it.
    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.peak.swap(0, Ordering::Relaxed))
    }
}

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use cpal::{
    Device, FromSample, Host, Sample, SampleFormat, SizedSample, Stream, SupportedStreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Record audio locally: what plays on the computer + your microphone",
    args_conflicts_with_subcommands = true
)]
struct Opt {
    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// List audio devices and exit
    #[arg(long)]
    list: bool,

    /// Output folder (default: `output_dir` from the config file, else ~/Recordings/eavesdrop)
    #[arg(short, long, global = true)]
    out: Option<PathBuf>,

    /// Config file (default: ~/.config/eavesdrop/config.toml)
    #[arg(long, value_name = "PATH", global = true)]
    config: Option<PathBuf>,

    /// Color theme (default, catppuccin-mocha, catppuccin-latte, nord, …; `t` cycles in the app)
    #[arg(long, value_name = "NAME", global = true)]
    theme: Option<String>,

    /// Microphone to use (part of its name or id). Default: system default input
    #[arg(long)]
    mic: Option<String>,

    /// Output device to capture (part of its name or id). Default: system default output
    #[arg(long)]
    system: Option<String>,

    /// Don't record the microphone (only system audio)
    #[arg(long)]
    no_mic: bool,

    /// Microphone volume in the final mix (1.0 = unchanged)
    #[arg(long, default_value_t = 1.0)]
    mic_gain: f32,

    /// File format of the recording (default: `format` from the config file, else opus)
    #[arg(long, value_enum, global = true)]
    format: Option<config::Format>,

    /// Also keep the separate system / mic tracks (WAV) next to the mixed file
    #[arg(long)]
    keep_tracks: bool,

    /// Stop automatically after this many minutes
    #[arg(long)]
    minutes: Option<u64>,

    /// Plain text output instead of the live terminal UI (starts recording immediately)
    #[arg(long)]
    no_tui: bool,

    /// Start recording right away instead of waiting for Space in the terminal UI
    #[arg(long)]
    now: bool,

    /// What the recording is about; saved next to it (edit later with `e` in the app)
    #[arg(long, value_name = "TEXT")]
    about: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Browse recordings and play them back
    Play {
        /// Recording to play right away (default: pick one from the list)
        file: Option<PathBuf>,
    },
    /// Print the recordings in the output folder
    Recordings,
}

/// Recordings folder: --out, else `output_dir` from the config, else ~/Recordings/eavesdrop.
fn recordings_dir(opt: &Opt, cfg: &config::Config) -> Result<PathBuf> {
    match opt.out.clone().or(cfg.output_dir.clone()) {
        Some(p) => Ok(p),
        None => Ok(dirs::home_dir()
            .ok_or_else(|| anyhow!("cannot determine home directory, use --out"))?
            .join("Recordings")
            .join("eavesdrop")),
    }
}

/// Simple one-line progress for non-interactive use (piped output, --no-tui).
fn plain_progress(
    clock: &Clock,
    limit: Option<Duration>,
    stop: &mpsc::Receiver<()>,
    system: &Meter,
    mic: Option<&Meter>,
) {
    use std::io::Write;
    println!("● Recording… press Ctrl+C to stop");
    loop {
        let t = clock.recorded();
        let db = |p: f32| {
            if p > 0.0 {
                format!("{:>4.0} dB", 20.0 * p.log10())
            } else {
                "  -∞ dB".into()
            }
        };
        print!(
            "\r  {}  system {}  mic {} ",
            ui::fmt_duration(t),
            db(system.take_peak()),
            mic.map(|m| db(m.take_peak()))
                .unwrap_or_else(|| "  off  ".into())
        );
        std::io::stdout().flush().ok();
        if limit.is_some_and(|l| t >= l) {
            break;
        }
        match stop.recv_timeout(Duration::from_millis(500)) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            _ => break,
        }
    }
    println!();
}

fn main() -> std::process::ExitCode {
    let result = run();
    if let Err(e) = &result {
        eprintln!("Error: {e:#}");
    }
    pause_if_own_console();
    if result.is_ok() {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

/// Windows: when started by double-click, the console window belongs to this
/// process alone and would close immediately on exit, hiding the output.
#[cfg(windows)]
fn pause_if_own_console() {
    let mut pids = [0u32; 2];
    // SAFETY: the buffer is valid for `pids.len()` entries.
    let n =
        unsafe { windows_sys::Win32::System::Console::GetConsoleProcessList(pids.as_mut_ptr(), 2) };
    if n == 1 {
        println!("\nPress Enter to close…");
        let _ = std::io::stdin().read_line(&mut String::new());
    }
}

#[cfg(not(windows))]
fn pause_if_own_console() {}

fn run() -> Result<()> {
    let opt = Opt::parse();
    let host = cpal::default_host();

    if opt.list {
        return list_devices(&host);
    }

    let cfg = config::Config::load(opt.config.as_deref())?;
    if let Some(name) = opt.theme.as_deref().or(cfg.theme.as_deref()) {
        theme::set(name)?;
    }

    match &opt.cmd {
        Some(Cmd::Recordings) => return browser::print_list(&recordings_dir(&opt, &cfg)?),
        Some(Cmd::Play { file }) => {
            if !std::io::stdout().is_terminal() {
                bail!("`eavesdrop play` needs a terminal (use `eavesdrop recordings` to list)");
            }
            return match file {
                // Play a given file: browse the folder it is in.
                Some(f) => {
                    let f = std::path::absolute(f)?;
                    if !f.is_file() {
                        bail!("no such file: {}", f.display());
                    }
                    let dir = f.parent().map(Path::to_path_buf).unwrap_or_default();
                    browser::run(dir, Some(f))
                }
                None => browser::run(recordings_dir(&opt, &cfg)?, None),
            };
        }
        None => {}
    }

    let out_dir = recordings_dir(&opt, &cfg)?;
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("cannot create {}", out_dir.display()))?;

    // ---- pick devices ----------------------------------------------------
    let (sys_dev, sys_cfg) = loopback_device(&host, opt.system.as_deref())?;
    println!("● System audio : {}", dev_name(&sys_dev));
    let mic = if opt.no_mic {
        None
    } else {
        match &opt.mic {
            // Explicitly requested mic: failing to open it is an error.
            Some(q) => {
                let dev = find_device(&host, q, |d| d.supports_input() && !is_monitor(d))?;
                let cfg = dev
                    .default_input_config()
                    .with_context(|| format!("cannot open microphone {}", dev_name(&dev)))?;
                println!("● Microphone   : {}", dev_name(&dev));
                Some((dev, cfg))
            }
            // Default mic: if there is none (e.g. Mac mini without a mic), continue without.
            None => match default_mic(&host) {
                Some((dev, cfg)) => {
                    println!("● Microphone   : {}", dev_name(&dev));
                    Some((dev, cfg))
                }
                None => {
                    eprintln!(
                        "⚠ No usable microphone found — recording system audio only \
                         (connect a mic or pick one with --mic, see --list)"
                    );
                    None
                }
            },
        }
    };
    let recorder = record::Recorder {
        system: record::Input::new(sys_dev, sys_cfg),
        mic: mic.map(|(d, c)| record::Input::new(d, c)),
        out_dir,
        mic_gain: opt.mic_gain,
        keep_tracks: opt.keep_tracks,
        format: opt.format.or(cfg.format).unwrap_or_default(),
    };

    let (stop_tx, stop_rx) = mpsc::channel();
    ctrlc::set_handler(move || {
        let _ = stop_tx.send(());
    })?;

    let limit = opt.minutes.map(|m| Duration::from_secs(m * 60));
    if !opt.no_tui && std::io::stdout().is_terminal() {
        // Full app: record, then keep going (browse / listen / record again).
        let pending = ui::run(ui::Config {
            recorder,
            limit,
            start_now: opt.now,
            about: opt.about.clone().unwrap_or_default(),
            stop: stop_rx,
        })?;
        if !pending.is_empty() {
            println!("■ Saving…");
        }
        let mut result = Ok(());
        for saving in pending {
            if let Err(e) = report(saving) {
                eprintln!("✗ {e:#}");
                result = Err(e);
            }
        }
        return result;
    }

    // Plain mode: one recording, starts immediately.
    let take = recorder.new_take()?;
    take.clock.resume();
    plain_progress(
        &take.clock,
        limit,
        &stop_rx,
        &take.system,
        take.mic.as_deref(),
    );
    println!(
        "■ Stopping… ({} recorded)",
        ui::fmt_duration(take.clock.recorded())
    );
    report(take.stop(opt.about.clone().unwrap_or_default()))
}

/// Waits for a take to be saved and prints the result.
fn report(saving: record::Saving) -> Result<()> {
    let outcome = saving
        .join()
        .map_err(|_| anyhow!("save thread panicked"))??;
    match outcome {
        record::Outcome::Discarded => println!("Recording was never started — nothing saved."),
        record::Outcome::Aborted => println!("✗ Recording discarded — nothing saved."),
        record::Outcome::Saved {
            path,
            recorded,
            extra,
            warnings,
        } => {
            for w in warnings {
                eprintln!("{w}");
            }
            println!(
                "✓ Saved {} ({})",
                path.display(),
                ui::fmt_duration(recorded)
            );
            for p in extra {
                println!("  + {}", p.display());
            }
            println!("  ▶ Listen: eavesdrop play \"{}\"", path.display());
        }
    }
    Ok(())
}

// ===========================================================================
// Device selection
// ===========================================================================

fn dev_name(d: &Device) -> String {
    d.description()
        .map(|x| x.name().to_string())
        .or_else(|_| d.id().map(|i| i.id().to_string()))
        .unwrap_or_else(|_| "<unknown>".into())
}

fn dev_id(d: &Device) -> String {
    d.id().map(|i| i.id().to_string()).unwrap_or_default()
}

/// PulseAudio/PipeWire "monitor" sources carry what is being played on a sink.
fn is_monitor(d: &Device) -> bool {
    cfg!(target_os = "linux") && dev_id(d).ends_with(".monitor")
}

fn matches(d: &Device, query: &str) -> bool {
    let q = query.to_lowercase();
    dev_name(d).to_lowercase().contains(&q) || dev_id(d).to_lowercase().contains(&q)
}

fn find_device(host: &Host, query: &str, filter: impl Fn(&Device) -> bool) -> Result<Device> {
    host.devices()?
        .filter(|d| filter(d) && matches(d, query))
        .next()
        .ok_or_else(|| anyhow!("no device matching \"{query}\" (see --list)"))
}

/// The system default microphone, validated. On macOS, when no input device exists,
/// the "default input" is kAudioObjectUnknown and opening it fails with '!obj',
/// so only accept it if it is in the list of real input devices and has a config.
fn default_mic(host: &Host) -> Option<(Device, SupportedStreamConfig)> {
    let dev = host.default_input_device()?;
    let id = dev.id().ok()?;
    let exists = host
        .input_devices()
        .ok()?
        .any(|d| d.id().ok().as_ref() == Some(&id) && !is_monitor(&d));
    if !exists {
        return None;
    }
    let cfg = dev.default_input_config().ok()?;
    Some((dev, cfg))
}

/// Returns the device + config to open as an *input* stream that captures
/// everything played through the speakers/headphones.
fn loopback_device(host: &Host, query: Option<&str>) -> Result<(Device, SupportedStreamConfig)> {
    if cfg!(target_os = "linux") {
        // Find "<default sink>.monitor" (or the one the user asked for).
        let monitors: Vec<Device> = host.devices()?.filter(is_monitor).collect();
        if monitors.is_empty() {
            bail!(
                "no PulseAudio/PipeWire monitor source found. Make sure PulseAudio or \
                 pipewire-pulse is running."
            );
        }
        let dev = if let Some(q) = query {
            monitors.into_iter().find(|d| matches(d, q))
        } else {
            let want = host
                .default_output_device()
                .map(|d| format!("{}.monitor", dev_id(&d)));
            let idx = monitors
                .iter()
                .position(|d| Some(dev_id(d)) == want)
                .unwrap_or(0);
            monitors.into_iter().nth(idx)
        }
        .ok_or_else(|| anyhow!("no monitor source matching the query (see --list)"))?;
        let cfg = dev.default_input_config()?;
        Ok((dev, cfg))
    } else {
        // macOS / Windows: open an input stream on the output device → cpal uses
        // a Core Audio process tap / WASAPI loopback.
        let dev = match query {
            Some(q) => find_device(host, q, |d| d.supports_output())?,
            None => host
                .default_output_device()
                .ok_or_else(|| anyhow!("no default output device"))?,
        };
        let cfg = dev.default_output_config()?;
        Ok((dev, cfg))
    }
}

fn list_devices(host: &Host) -> Result<()> {
    println!("Audio host: {}", host.id().name());
    let def_in = host.default_input_device().map(|d| dev_id(&d));
    let def_out = host.default_output_device().map(|d| dev_id(&d));
    for d in host.devices()? {
        let id = dev_id(&d);
        let kind = if is_monitor(&d) {
            "system (monitor)"
        } else {
            match (d.supports_input(), d.supports_output()) {
                (true, true) => "mic + output",
                (true, false) => "mic",
                (false, true) => "output",
                _ => "?",
            }
        };
        let mut flags = String::new();
        if Some(&id) == def_in.as_ref() {
            flags.push_str(" [default mic]");
        }
        if Some(&id) == def_out.as_ref() {
            flags.push_str(" [default output]");
        }
        println!(
            "  {:<18} {}{}\n  {:<18}   id: {}",
            kind,
            dev_name(&d),
            flags,
            "",
            id
        );
    }
    Ok(())
}

// ===========================================================================
// Recording one track
// ===========================================================================

struct Track {
    stream: Stream,
    writer: JoinHandle<Result<f32>>,
}

impl Track {
    /// Stops the stream; the returned writer finishes the file and yields the peak level.
    fn stop(self) -> JoinHandle<Result<f32>> {
        drop(self.stream);
        self.writer
    }
}

fn start_track(
    dev: &Device,
    cfg: SupportedStreamConfig,
    path: &Path,
    clock: Arc<Clock>,
    meter: Arc<Meter>,
) -> Result<Track> {
    let channels = cfg.channels();
    let rate = cfg.sample_rate();
    let (tx, rx) = mpsc::channel::<(Instant, Vec<f32>)>();

    let spec = hound::WavSpec {
        channels,
        sample_rate: rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut wav = hound::WavWriter::create(path, spec)?;

    // Writer thread: writes samples and inserts silence for gaps, so the track
    // stays aligned with wall-clock time (loopback delivers nothing while silent
    // on some platforms, e.g. Windows).
    let m = meter.clone();
    let writer = std::thread::spawn(move || -> Result<f32> {
        let meter = m;
        let ch = channels as usize;
        let mut written: u64 = 0; // frames
        let mut peak = 0f32;
        let tolerance = (rate / 5) as u64; // 200 ms
        for (at, chunk) in rx {
            // Meters stay live even when not recording (ready / paused).
            let chunk_peak = chunk.iter().fold(0f32, |a, s| a.max(s.abs()));
            meter.record_peak(chunk_peak);
            // Position in the recording; None = captured before start or while paused.
            let Some(pos) = clock.position(at) else {
                continue;
            };
            let frames = (chunk.len() / ch) as u64;
            let expected_end = (pos.as_secs_f64() * rate as f64) as u64;
            let expected_start = expected_end.saturating_sub(frames);
            if expected_start > written + tolerance {
                for _ in 0..(expected_start - written) * ch as u64 {
                    wav.write_sample(0f32)?;
                }
                written = expected_start;
            }
            if meter.muted.load(Ordering::Relaxed) {
                for _ in 0..chunk.len() {
                    wav.write_sample(0f32)?;
                }
            } else {
                peak = peak.max(chunk_peak);
                for s in chunk {
                    wav.write_sample(s)?;
                }
            }
            written += frames;
            meter
                .bytes
                .store(written * ch as u64 * 4, Ordering::Relaxed);
        }
        // Nothing captured at the end (e.g. nothing playing): pad with silence so
        // the track is as long as the recorded time.
        if clock.started() {
            let expected = (clock.recorded().as_secs_f64() * rate as f64) as u64;
            if expected > written + tolerance {
                for _ in 0..(expected - written) * ch as u64 {
                    wav.write_sample(0f32)?;
                }
            }
        }
        wav.finalize()?;
        Ok(peak)
    });

    // Report errors through the meter (printing would garble the TUI).
    let err_fn = move |e: cpal::Error| {
        if let Ok(mut slot) = meter.error.lock() {
            *slot = Some(e.to_string());
        }
    };
    let stream_cfg = cfg.config();
    let stream = match cfg.sample_format() {
        SampleFormat::F32 => build::<f32>(dev, stream_cfg, tx, err_fn)?,
        SampleFormat::I16 => build::<i16>(dev, stream_cfg, tx, err_fn)?,
        SampleFormat::I32 => build::<i32>(dev, stream_cfg, tx, err_fn)?,
        SampleFormat::U16 => build::<u16>(dev, stream_cfg, tx, err_fn)?,
        SampleFormat::I8 => build::<i8>(dev, stream_cfg, tx, err_fn)?,
        SampleFormat::U8 => build::<u8>(dev, stream_cfg, tx, err_fn)?,
        SampleFormat::F64 => build::<f64>(dev, stream_cfg, tx, err_fn)?,
        f => bail!("unsupported sample format {f}"),
    };
    stream.play()?;
    Ok(Track { stream, writer })
}

fn build<T>(
    dev: &Device,
    cfg: cpal::StreamConfig,
    tx: mpsc::Sender<(Instant, Vec<f32>)>,
    err_fn: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    Ok(dev.build_input_stream::<T, _, _>(
        cfg,
        move |data: &[T], _| {
            let buf: Vec<f32> = data.iter().map(|&s| f32::from_sample(s)).collect();
            let _ = tx.send((Instant::now(), buf));
        },
        err_fn,
        None,
    )?)
}

// ===========================================================================
// Mixdown: system + mic → one mono Opus / stereo 16-bit WAV (streaming, low memory)
// ===========================================================================

/// Streams a WAV file as stereo frames, resampled to `out_rate` (linear interp).
struct Source {
    samples: hound::WavIntoSamples<BufReader<File>, f32>,
    channels: usize,
    step: f64,
    pos: f64,
    cur: [f32; 2],
    next: Option<[f32; 2]>,
    empty: bool,
}

impl Source {
    fn open(path: &Path, out_rate: u32) -> Result<Self> {
        let reader = hound::WavReader::open(path)?;
        let spec = reader.spec();
        let mut s = Source {
            samples: reader.into_samples(),
            channels: spec.channels as usize,
            step: spec.sample_rate as f64 / out_rate as f64,
            pos: 0.0,
            cur: [0.0; 2],
            next: None,
            empty: false,
        };
        match s.read_frame() {
            Some(f) => s.cur = f,
            None => s.empty = true,
        }
        s.next = s.read_frame();
        Ok(s)
    }

    fn read_frame(&mut self) -> Option<[f32; 2]> {
        let mut f = [0f32; 2];
        for c in 0..self.channels {
            let v = self.samples.next()?.ok()?;
            if c < 2 {
                f[c] = v;
            }
        }
        if self.channels == 1 {
            f[1] = f[0];
        }
        Some(f)
    }

    fn next_frame(&mut self) -> Option<[f32; 2]> {
        if self.empty {
            return None;
        }
        while self.pos >= 1.0 {
            self.pos -= 1.0;
            self.cur = self.next?;
            self.next = self.read_frame();
        }
        let n = self.next.unwrap_or(self.cur);
        let t = self.pos as f32;
        self.pos += self.step;
        Some([
            self.cur[0] + (n[0] - self.cur[0]) * t,
            self.cur[1] + (n[1] - self.cur[1]) * t,
        ])
    }
}

/// The mixed recording being written.
enum Sink {
    Wav(hound::WavWriter<std::io::BufWriter<File>>),
    Opus(Box<opus::Writer>),
}

impl Sink {
    fn create(path: &Path, format: config::Format, rate: u32) -> Result<Self> {
        Ok(match format {
            config::Format::Wav => Sink::Wav(hound::WavWriter::create(
                path,
                hound::WavSpec {
                    channels: 2,
                    sample_rate: rate,
                    bits_per_sample: 16,
                    sample_format: hound::SampleFormat::Int,
                },
            )?),
            config::Format::Opus => Sink::Opus(Box::new(opus::Writer::create(path)?)),
        })
    }

    /// Output sample rate: the system track's for WAV, Opus's own otherwise.
    fn rate(format: config::Format, system_rate: u32) -> u32 {
        match format {
            config::Format::Wav => system_rate,
            config::Format::Opus => opus::RATE,
        }
    }

    fn write(&mut self, f: [f32; 2]) -> Result<()> {
        match self {
            Sink::Wav(w) => {
                for x in f {
                    w.write_sample((x * i16::MAX as f32) as i16)?;
                }
            }
            // Calls don't need stereo: mono halves the bitrate needed for the same quality.
            Sink::Opus(w) => w.write((f[0] + f[1]) * 0.5)?,
        }
        Ok(())
    }

    fn finish(self) -> Result<()> {
        match self {
            Sink::Wav(w) => w.finalize()?,
            Sink::Opus(w) => w.finish()?,
        }
        Ok(())
    }
}

fn mixdown(
    system: &Path,
    mic: Option<&Path>,
    mic_gain: f32,
    out: &Path,
    format: config::Format,
) -> Result<()> {
    let out_rate = Sink::rate(format, hound::WavReader::open(system)?.spec().sample_rate);
    let mut a = Source::open(system, out_rate)?;
    let mut b = match mic {
        Some(p) => Some(Source::open(p, out_rate)?),
        None => None,
    };
    let mut sink = Sink::create(out, format, out_rate)?;
    loop {
        let fa = a.next_frame();
        let fb = b.as_mut().and_then(|s| s.next_frame());
        if fa.is_none() && fb.is_none() {
            break;
        }
        let fa = fa.unwrap_or([0.0; 2]);
        let fb = fb.unwrap_or([0.0; 2]);
        let mut frame = [0f32; 2];
        for c in 0..2 {
            let x = fa[c] + fb[c] * mic_gain;
            let x = if x.abs() > 0.9 {
                x.signum() * (0.9 + (x.abs() - 0.9).tanh() * 0.1)
            } else {
                x
            };
            frame[c] = x.clamp(-1.0, 1.0);
        }
        sink.write(frame)?;
    }
    sink.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixdown_to_opus_keeps_length() {
        let dir = std::env::temp_dir().join(format!("eavesdrop-mix-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // 2 s of stereo float at 44.1 kHz (resampled to 48 kHz for Opus) + 1 s mono mic.
        let track = |name: &str, channels: u16, rate: u32, secs: u32| {
            let path = dir.join(name);
            let spec = hound::WavSpec {
                channels,
                sample_rate: rate,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            };
            let mut w = hound::WavWriter::create(&path, spec).unwrap();
            for i in 0..rate * secs * channels as u32 {
                w.write_sample((i as f32 * 0.01).sin() * 0.3).unwrap();
            }
            w.finalize().unwrap();
            path
        };
        let system = track("system.wav", 2, 44_100, 2);
        let mic = track("mic.wav", 1, 16_000, 1);

        let out = dir.join("mix.opus");
        mixdown(&system, Some(&mic), 1.0, &out, config::Format::Opus).unwrap();
        let d = opus::duration(&out).unwrap().as_secs_f64();
        assert!((d - 2.0).abs() < 0.001, "duration {d}");

        let wav = dir.join("mix.wav");
        mixdown(&system, Some(&mic), 1.0, &wav, config::Format::Wav).unwrap();
        let spec = hound::WavReader::open(&wav).unwrap().spec();
        assert_eq!((spec.channels, spec.sample_rate), (2, 44_100));
        assert!(
            std::fs::metadata(&out).unwrap().len() * 20 < std::fs::metadata(&wav).unwrap().len()
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
