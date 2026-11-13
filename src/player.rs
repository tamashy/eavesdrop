//! WAV / Opus playback through the default output device, with pause, seek and volume.
//!
//! A decoder thread reads the file, converts it to stereo at the device rate
//! and keeps ~0.5 s queued; the audio callback only pops frames from that queue.

use std::{
    collections::VecDeque,
    fs::File,
    io::BufReader,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};

use crate::opus;
use cpal::{
    FromSample, SampleFormat, SizedSample, Stream,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};

type Frame = [f32; 2];
type Reader = hound::WavReader<BufReader<File>>;

/// A decodable recording.
enum Source {
    Wav(Reader),
    Opus(Box<opus::Reader>),
}

impl Source {
    fn open(path: &Path) -> Result<Self> {
        if opus::is_opus(path) {
            return Ok(Source::Opus(Box::new(opus::Reader::open(path)?)));
        }
        let reader = hound::WavReader::open(path)?;
        let spec = reader.spec();
        if spec.channels == 0 || spec.sample_rate == 0 {
            bail!("invalid WAV header");
        }
        Ok(Source::Wav(reader))
    }

    fn rate(&self) -> u32 {
        match self {
            Source::Wav(r) => r.spec().sample_rate,
            Source::Opus(_) => opus::RATE,
        }
    }

    fn frames(&self) -> u64 {
        match self {
            Source::Wav(r) => r.duration().into(),
            Source::Opus(r) => r.frames(),
        }
    }

    fn seek(&mut self, frame: u64) -> Result<()> {
        match self {
            Source::Wav(r) => Ok(r.seek(frame.min(u32::MAX.into()) as u32)?),
            Source::Opus(r) => r.seek(frame),
        }
    }

    /// Reads up to `n` frames as stereo f32.
    fn read(&mut self, n: usize, out: &mut Vec<Frame>) -> Result<()> {
        match self {
            Source::Wav(r) => read_frames(r, n, out),
            Source::Opus(r) => r.read(n, out),
        }
    }
}

/// Source frames decoded per step.
const CHUNK: usize = 2048;
pub const MAX_VOLUME: f32 = 2.0;

struct Queue {
    frames: VecDeque<Frame>,
    /// Position (in source frames) of the first queued frame.
    head: f64,
    /// Pending seek target (source frames) for the decoder.
    seek: Option<u64>,
    /// Decoder reached the end of the file.
    eof: bool,
}

struct Shared {
    queue: Mutex<Queue>,
    volume: AtomicU32,
    paused: AtomicBool,
    quit: AtomicBool,
    error: Mutex<Option<String>>,
}

pub struct Player {
    shared: Arc<Shared>,
    rate: u32,
    frames: u64,
    _stream: Stream,
    decoder: Option<JoinHandle<()>>,
}

impl Player {
    /// Opens `path` and starts playing it.
    pub fn open(path: &Path, volume: f32) -> Result<Self> {
        let reader =
            Source::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        let (rate, frames) = (reader.rate(), reader.frames());

        let host = cpal::default_host();
        let dev = host
            .default_output_device()
            .ok_or_else(|| anyhow!("no default output device"))?;
        let cfg = dev.default_output_config()?;
        let dev_rate = cfg.sample_rate();
        let step = rate as f64 / dev_rate as f64;

        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                frames: VecDeque::new(),
                head: 0.0,
                seek: None,
                eof: false,
            }),
            volume: AtomicU32::new(volume.clamp(0.0, MAX_VOLUME).to_bits()),
            paused: AtomicBool::new(false),
            quit: AtomicBool::new(false),
            error: Mutex::new(None),
        });

        let decoder = {
            let shared = shared.clone();
            let target = (dev_rate / 2) as usize;
            std::thread::spawn(move || decode(reader, shared, step, target))
        };

        let err_shared = shared.clone();
        let err_fn = move |e: cpal::Error| {
            if let Ok(mut slot) = err_shared.error.lock() {
                *slot = Some(e.to_string());
            }
        };
        let sc = cfg.config();
        let s = shared.clone();
        let stream = match cfg.sample_format() {
            SampleFormat::F32 => build::<f32>(&dev, sc, s, step, err_fn)?,
            SampleFormat::I16 => build::<i16>(&dev, sc, s, step, err_fn)?,
            SampleFormat::I32 => build::<i32>(&dev, sc, s, step, err_fn)?,
            SampleFormat::U16 => build::<u16>(&dev, sc, s, step, err_fn)?,
            SampleFormat::I8 => build::<i8>(&dev, sc, s, step, err_fn)?,
            SampleFormat::U8 => build::<u8>(&dev, sc, s, step, err_fn)?,
            SampleFormat::F64 => build::<f64>(&dev, sc, s, step, err_fn)?,
            f => bail!("unsupported output sample format {f}"),
        };
        stream.play()?;

        Ok(Self {
            shared,
            rate,
            frames,
            _stream: stream,
            decoder: Some(decoder),
        })
    }

    pub fn duration(&self) -> Duration {
        Duration::from_secs_f64(self.frames as f64 / self.rate as f64)
    }

    pub fn position(&self) -> Duration {
        if self.finished() {
            return self.duration();
        }
        let head = self.queue().head.min(self.frames as f64);
        Duration::from_secs_f64(head / self.rate as f64)
    }

    /// Played to the end of the file.
    pub fn finished(&self) -> bool {
        let q = self.queue();
        q.eof && q.seek.is_none() && q.frames.is_empty()
    }

    pub fn paused(&self) -> bool {
        self.shared.paused.load(Ordering::Relaxed)
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Relaxed);
    }

    /// Pause / resume. At the end of the file, starts again from the beginning.
    pub fn toggle(&self) {
        if self.finished() {
            self.seek(Duration::ZERO);
            self.shared.paused.store(false, Ordering::Relaxed);
        } else {
            self.shared.paused.fetch_xor(true, Ordering::Relaxed);
        }
    }

    pub fn seek(&self, to: Duration) {
        let frame = ((to.as_secs_f64() * self.rate as f64) as u64).min(self.frames);
        let mut q = self.queue();
        q.frames.clear();
        q.head = frame as f64;
        q.seek = Some(frame);
        q.eof = false;
    }

    /// Seeks relative to the current position (negative = backward).
    pub fn seek_by(&self, secs: f64) {
        let pos = (self.position().as_secs_f64() + secs).max(0.0);
        self.seek(Duration::from_secs_f64(pos).min(self.duration()));
    }

    /// Seeks to a fraction (0..1) of the file.
    pub fn seek_ratio(&self, r: f64) {
        self.seek(self.duration().mul_f64(r.clamp(0.0, 1.0)));
    }

    pub fn set_volume(&self, v: f32) {
        self.shared
            .volume
            .store(v.clamp(0.0, MAX_VOLUME).to_bits(), Ordering::Relaxed);
    }

    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().ok().and_then(|e| e.clone())
    }

    fn queue(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.shared.queue.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shared.quit.store(true, Ordering::Relaxed);
        if let Some(d) = self.decoder.take() {
            let _ = d.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Decoder thread
// ---------------------------------------------------------------------------

fn decode(mut reader: Source, shared: Arc<Shared>, step: f64, target: usize) {
    let set_error = |e: String| {
        if let Ok(mut slot) = shared.error.lock() {
            *slot = Some(e);
        }
    };
    let mut rs = Resampler::new(step);
    let mut src = Vec::with_capacity(CHUNK);
    let mut out = Vec::with_capacity(CHUNK * 2);
    while !shared.quit.load(Ordering::Relaxed) {
        let (seek, room) = {
            let mut q = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            let room = !q.eof && q.frames.len() < target;
            (q.seek.take(), room)
        };
        if let Some(frame) = seek {
            if let Err(e) = reader.seek(frame) {
                set_error(format!("seek failed: {e}"));
            }
            rs = Resampler::new(step);
        } else if !room {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }

        src.clear();
        out.clear();
        let eof = match reader.read(CHUNK, &mut src) {
            Ok(()) => src.len() < CHUNK,
            Err(e) => {
                set_error(format!("read failed: {e}"));
                true
            }
        };
        rs.process(&src, &mut out);

        let mut q = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        if q.seek.is_some() {
            continue; // a newer seek arrived while decoding: drop this chunk
        }
        q.frames.extend(out.drain(..));
        q.eof = eof;
    }
}

/// Reads up to `n` frames as stereo f32.
fn read_frames(r: &mut Reader, n: usize, out: &mut Vec<Frame>) -> Result<()> {
    let spec = r.spec();
    let ch = spec.channels as usize;
    match spec.sample_format {
        hound::SampleFormat::Float => fill(r.samples::<f32>(), ch, n, out),
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32;
            fill(
                r.samples::<i32>().map(|s| s.map(|v| v as f32 * scale)),
                ch,
                n,
                out,
            )
        }
    }
}

fn fill(
    mut it: impl Iterator<Item = hound::Result<f32>>,
    ch: usize,
    n: usize,
    out: &mut Vec<Frame>,
) -> Result<()> {
    for _ in 0..n {
        let mut f = [0f32; 2];
        for c in 0..ch {
            let Some(v) = it.next() else { return Ok(()) };
            let v = v?;
            if let Some(slot) = f.get_mut(c) {
                *slot = v;
            }
        }
        if ch == 1 {
            f[1] = f[0];
        }
        out.push(f);
    }
    Ok(())
}

/// Streaming linear-interpolation resampler (source rate → device rate).
struct Resampler {
    step: f64,
    /// Position relative to `last` (index 0); chunk frame `i` is index `i + 1`.
    pos: f64,
    last: Option<Frame>,
}

impl Resampler {
    fn new(step: f64) -> Self {
        Self {
            step,
            pos: 0.0,
            last: None,
        }
    }

    fn process(&mut self, mut input: &[Frame], out: &mut Vec<Frame>) {
        let last = match self.last {
            Some(f) => f,
            None => match input.split_first() {
                Some((f, rest)) => {
                    input = rest;
                    *f
                }
                None => return,
            },
        };
        let at = |j: usize| if j == 0 { last } else { input[j - 1] };
        let n = input.len();
        while self.pos < n as f64 {
            let i = self.pos as usize;
            let t = (self.pos - i as f64) as f32;
            let (a, b) = (at(i), at(i + 1));
            out.push([a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]);
            self.pos += self.step;
        }
        self.last = Some(input.last().copied().unwrap_or(last));
        self.pos -= n as f64;
    }
}

// ---------------------------------------------------------------------------
// Output stream
// ---------------------------------------------------------------------------

fn build<T>(
    dev: &cpal::Device,
    cfg: cpal::StreamConfig,
    shared: Arc<Shared>,
    step: f64,
    err_fn: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let ch = cfg.channels as usize;
    Ok(dev.build_output_stream::<T, _, _>(
        cfg,
        move |data: &mut [T], _| {
            let vol = f32::from_bits(shared.volume.load(Ordering::Relaxed));
            let paused = shared.paused.load(Ordering::Relaxed);
            // Never block the audio thread; output silence if the decoder holds the lock.
            let mut q = if paused {
                None
            } else {
                shared.queue.try_lock().ok()
            };
            for out in data.chunks_mut(ch) {
                let f = match q.as_mut().and_then(|q| {
                    let f = q.frames.pop_front()?;
                    q.head += step;
                    Some(f)
                }) {
                    Some(f) => [f[0] * vol, f[1] * vol],
                    None => [0.0; 2],
                };
                if ch == 1 {
                    out[0] = T::from_sample((f[0] + f[1]) * 0.5);
                } else {
                    for (c, s) in out.iter_mut().enumerate() {
                        *s = T::from_sample(f.get(c).copied().unwrap_or(0.0).clamp(-1.0, 1.0));
                    }
                }
            }
        },
        err_fn,
        None,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(n: usize) -> Vec<Frame> {
        (0..n).map(|i| [i as f32, -(i as f32)]).collect()
    }

    #[test]
    fn same_rate_passes_through_across_chunks() {
        let mut rs = Resampler::new(1.0);
        let mut out = Vec::new();
        let input = ramp(10);
        rs.process(&input[..4], &mut out);
        rs.process(&input[4..], &mut out);
        assert_eq!(out, input[..9].to_vec()); // last frame waits for its successor
    }

    #[test]
    fn upsample_interpolates() {
        let mut rs = Resampler::new(0.5);
        let mut out = Vec::new();
        rs.process(&ramp(3), &mut out);
        let left: Vec<f32> = out.iter().map(|f| f[0]).collect();
        assert_eq!(left, vec![0.0, 0.5, 1.0, 1.5]);
    }

    #[test]
    fn downsample_keeps_ratio() {
        let mut rs = Resampler::new(2.0);
        let mut out = Vec::new();
        for c in ramp(101).chunks(7) {
            rs.process(c, &mut out);
        }
        let left: Vec<f32> = out.iter().map(|f| f[0]).collect();
        assert_eq!(left, (0..50).map(|i| (i * 2) as f32).collect::<Vec<_>>());
    }

    #[test]
    fn fill_mono_and_int_scaling() {
        let mut out = Vec::new();
        fill([0.5f32, -0.25].into_iter().map(Ok), 1, 10, &mut out).unwrap();
        assert_eq!(out, vec![[0.5, 0.5], [-0.25, -0.25]]);
    }

    #[test]
    fn plays_back_written_file_positions() {
        let dir = std::env::temp_dir().join(format!("eavesdrop-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 8000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..16000 {
            w.write_sample((i % 100) as i16).unwrap();
        }
        w.finalize().unwrap();

        let mut r = hound::WavReader::open(&path).unwrap();
        assert_eq!(r.duration(), 8000);
        r.seek(4000).unwrap();
        let mut out = Vec::new();
        read_frames(&mut r, CHUNK, &mut out).unwrap();
        assert_eq!(out.len(), CHUNK);
        assert!((out[0][0] - 0.0).abs() < 1e-6); // sample 8000 % 100 == 0
        std::fs::remove_dir_all(&dir).ok();
    }
}
