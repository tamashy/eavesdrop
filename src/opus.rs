//! Ogg Opus (`.opus`) files, the compact format recordings are saved in.
//!
//! Recordings are mono, 48 kHz, ~32 kb/s: about 14 MB per hour, where a 48 kHz
//! stereo 16-bit WAV takes ~690 MB. Files are gapless (pre-skip and end-trim set
//! per RFC 7845), so they decode to exactly the samples that were written.

use std::{
    fs::File,
    io::{BufReader, BufWriter, Read, Seek, SeekFrom},
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use opus_pure::{
    Application, MAX_PACKET_BYTES, MAX_PACKET_SAMPLES, OggOpusReader, OggOpusWriter, OggPacket,
    OpusDecoder, OpusEncoder, OpusHead,
};

/// Sample rate files are encoded and decoded at.
pub const RATE: u32 = 48_000;
/// Samples per packet (20 ms).
const FRAME: usize = 960;
const BITRATE: i32 = 32_000;
/// Decoded and discarded before a seek target so the decoder has converged
/// (80 ms, as RFC 7845 §4.6 recommends).
const PRE_ROLL: u64 = 3840;

/// True for paths with an `.opus` extension.
pub fn is_opus(path: &Path) -> bool {
    path.extension()
        .is_some_and(|x| x.eq_ignore_ascii_case("opus"))
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// Streams mono 48 kHz samples into an `.opus` file.
pub struct Writer {
    enc: OpusEncoder,
    ogg: OggOpusWriter<BufWriter<File>>,
    pre_skip: u64,
    frame: Vec<f32>,
    buf: Vec<u8>,
    /// Encoded but not yet written: the last packet carries the end-trim.
    held: Option<Vec<u8>>,
    /// Samples written by the caller.
    samples: u64,
}

impl Writer {
    pub fn create(path: &Path) -> Result<Self> {
        let mut enc = OpusEncoder::new(RATE as i32, 1, Application::Audio)?;
        enc.bitrate_bps = BITRATE;
        let head = OpusHead::for_encoder(&enc, RATE);
        let pre_skip = u64::from(head.pre_skip);
        let file =
            File::create(path).with_context(|| format!("cannot create {}", path.display()))?;
        Ok(Self {
            enc,
            ogg: OggOpusWriter::new(BufWriter::new(file), head)?,
            pre_skip,
            frame: Vec::with_capacity(FRAME),
            buf: vec![0; MAX_PACKET_BYTES],
            held: None,
            samples: 0,
        })
    }

    pub fn write(&mut self, sample: f32) -> Result<()> {
        self.samples += 1;
        self.push(sample)
    }

    fn push(&mut self, sample: f32) -> Result<()> {
        self.frame.push(sample);
        if self.frame.len() == FRAME {
            let n = self.enc.encode(&self.frame, FRAME, &mut self.buf)?;
            self.frame.clear();
            if let Some(prev) = self.held.replace(self.buf[..n].to_vec()) {
                self.ogg.write_packet(&prev)?;
            }
        }
        Ok(())
    }

    /// Flushes the encoder and writes the end of the stream.
    pub fn finish(mut self) -> Result<()> {
        // The encoder runs `pre_skip` samples behind: feed that much silence so
        // the last real samples come out, then pad to a whole frame.
        for _ in 0..self.pre_skip {
            self.push(0.0)?;
        }
        while !self.frame.is_empty() {
            self.push(0.0)?;
        }
        if let Some(last) = self.held.take() {
            // The final granule marks where the audio ends; the padding is trimmed.
            let end = self.pre_skip + self.samples;
            let duration = end - self.ogg.granule() as u64;
            self.ogg
                .write_packet_with_duration(&last, duration as u32)?;
        }
        let w = self.ogg.finish()?;
        w.into_inner()
            .map_err(|e| anyhow!("cannot write file: {}", e.error()))?
            .sync_all()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// Decodes an `.opus` file to stereo frames at 48 kHz, with seeking.
pub struct Reader {
    ogg: OggOpusReader<BufReader<File>>,
    dec: OpusDecoder,
    channels: usize,
    pre_skip: u64,
    /// Audio length in samples.
    len: u64,
    /// Granule position (48 kHz, pre-skip included) where the next packet starts.
    pos: u64,
    /// Output before this granule position is dropped (pre-skip / seek target).
    from: u64,
    /// Packets ending at or before this granule position aren't even decoded.
    skip_until: u64,
    packet: OggPacket,
    block: Vec<f32>,
    pending: Vec<[f32; 2]>,
    taken: usize,
}

impl Reader {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path)?;
        let end = final_granule(&mut file)?;
        file.seek(SeekFrom::Start(0))?;
        let ogg = OggOpusReader::new(BufReader::new(file))?;
        let head = ogg.head();
        let channels = head.channel_count as usize;
        if channels == 0 {
            bail!("invalid Opus header");
        }
        let pre_skip = u64::from(head.pre_skip);
        Ok(Self {
            dec: head.decoder(RATE as i32)?,
            channels,
            pre_skip,
            len: end.saturating_sub(pre_skip),
            pos: 0,
            from: pre_skip,
            skip_until: 0,
            packet: OggPacket::default(),
            block: vec![0.0; MAX_PACKET_SAMPLES * channels],
            pending: Vec::new(),
            taken: 0,
            ogg,
        })
    }

    /// Length in frames at [`RATE`].
    pub fn frames(&self) -> u64 {
        self.len
    }

    /// Continues reading from `frame`.
    ///
    /// Opus has no index, so this goes back to the start and skips packets
    /// without decoding them, which takes milliseconds even for long files.
    pub fn seek(&mut self, frame: u64) -> Result<()> {
        self.ogg.rewind()?;
        self.dec = self.ogg.head().decoder(RATE as i32)?;
        self.pos = 0;
        self.from = self.pre_skip + frame.min(self.len);
        self.skip_until = self.from.saturating_sub(PRE_ROLL);
        self.pending.clear();
        self.taken = 0;
        Ok(())
    }

    /// Appends up to `n` frames to `out`; fewer only at the end of the file.
    pub fn read(&mut self, n: usize, out: &mut Vec<[f32; 2]>) -> Result<()> {
        let mut want = n;
        while want > 0 {
            if self.taken == self.pending.len() && !self.decode_next()? {
                break;
            }
            let avail = &self.pending[self.taken..];
            let k = avail.len().min(want);
            out.extend_from_slice(&avail[..k]);
            self.taken += k;
            want -= k;
        }
        Ok(())
    }

    /// Decodes packets until one yields audio. False at the end of the stream.
    fn decode_next(&mut self) -> Result<bool> {
        let end = self.pre_skip + self.len;
        self.pending.clear();
        self.taken = 0;
        while self.pos < end && self.ogg.read_packet_into(&mut self.packet)? {
            let start = self.pos;
            let n = opus_pure::packet::samples_48k(&self.packet.data)? as u64;
            self.pos += n;
            if start + n <= self.skip_until {
                continue;
            }
            let got = self
                .dec
                .decode(&self.packet.data, MAX_PACKET_SAMPLES, &mut self.block)?
                as u64;
            let (lo, hi) = (self.from.max(start), (start + got).min(end));
            let ch = self.channels;
            for t in lo..hi.max(lo) {
                let i = (t - start) as usize * ch;
                let l = self.block[i];
                let r = if ch > 1 { self.block[i + 1] } else { l };
                self.pending.push([l, r]);
            }
            if !self.pending.is_empty() {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Audio length of an `.opus` file, read from its header and last page only.
pub fn duration(path: &Path) -> Option<Duration> {
    let mut file = File::open(path).ok()?;
    let end = final_granule(&mut file).ok()?;
    file.seek(SeekFrom::Start(0)).ok()?;
    let pre_skip = OggOpusReader::new(BufReader::new(file))
        .ok()?
        .head()
        .pre_skip;
    let samples = end.saturating_sub(u64::from(pre_skip));
    Some(Duration::from_secs_f64(samples as f64 / RATE as f64))
}

/// Granule position of the last page: the end of the audio, pre-skip included.
fn final_granule(file: &mut File) -> Result<u64> {
    // An Ogg page is at most 27 + 255 + 255 * 255 bytes.
    const MAX_PAGE: u64 = 27 + 255 + 255 * 255;
    let size = file.metadata()?.len();
    let start = size.saturating_sub(MAX_PAGE);
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((size - start) as usize);
    file.read_to_end(&mut buf)?;
    last_page_granule(&buf).ok_or_else(|| anyhow!("incomplete Opus file (no final page)"))
}

/// Finds the last complete Ogg page in `buf` that carries a granule position.
fn last_page_granule(buf: &[u8]) -> Option<u64> {
    let mut i = buf.len().checked_sub(27)?;
    loop {
        if &buf[i..i + 4] == b"OggS" && buf[i + 4] == 0 {
            let nseg = buf[i + 26] as usize;
            let body = buf
                .get(i + 27..i + 27 + nseg)
                .map(|t| t.iter().map(|&s| s as usize).sum::<usize>());
            let granule = i64::from_le_bytes(buf[i + 6..i + 14].try_into().ok()?);
            if let Some(body) = body
                && i + 27 + nseg + body <= buf.len()
                && granule >= 0
            {
                return Some(granule as u64);
            }
        }
        i = i.checked_sub(1)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(i: u64) -> f32 {
        (i as f32 * 440.0 * std::f32::consts::TAU / RATE as f32).sin() * 0.5
    }

    fn write_tone(path: &Path, samples: u64) {
        let mut w = Writer::create(path).unwrap();
        for i in 0..samples {
            w.write(tone(i)).unwrap();
        }
        w.finish().unwrap();
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("eavesdrop-opus-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn round_trip_is_gapless_and_compact() {
        let dir = temp("rt");
        let path = dir.join("t.opus");
        let samples = RATE as u64 * 3 + 123; // not a whole number of frames
        write_tone(&path, samples);

        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size < 20_000, "3 s at 32 kb/s should be ~12 KB, got {size}");
        assert_eq!(
            duration(&path).unwrap(),
            Duration::from_secs_f64(samples as f64 / RATE as f64)
        );

        let mut r = Reader::open(&path).unwrap();
        assert_eq!(r.frames(), samples);
        let mut out = Vec::new();
        r.read(usize::MAX, &mut out).unwrap();
        assert_eq!(out.len() as u64, samples);
        // Lossy, but the tone must come back aligned (no delay) and at its level.
        let err: f32 = out[4800..9600]
            .iter()
            .enumerate()
            .map(|(k, f)| (f[0] - tone(4800 + k as u64)).powi(2))
            .sum::<f32>()
            / 4800.0;
        assert!(err < 0.01, "mean squared error {err}");
        assert_eq!(out[100][0], out[100][1], "mono plays on both channels");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn seek_lands_on_the_target_sample() {
        let dir = temp("seek");
        let path = dir.join("t.opus");
        let samples = RATE as u64 * 4;
        write_tone(&path, samples);

        let mut r = Reader::open(&path).unwrap();
        let mut whole = Vec::new();
        r.read(usize::MAX, &mut whole).unwrap();

        for target in [RATE as u64 * 2 + 17, 10, samples - 5] {
            r.seek(target).unwrap();
            let mut out = Vec::new();
            r.read(480, &mut out).unwrap();
            assert_eq!(out.len() as u64, 480.min(samples - target));
            let err: f32 = out
                .iter()
                .enumerate()
                .map(|(k, f)| (f[0] - tone(target + k as u64)).powi(2))
                .sum::<f32>()
                / out.len() as f32;
            assert!(err < 0.01, "seek to {target}: mean squared error {err}");
        }
        // Seeking past the end yields nothing.
        r.seek(samples + 1000).unwrap();
        let mut out = Vec::new();
        r.read(100, &mut out).unwrap();
        assert!(out.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_recording_is_valid() {
        let dir = temp("empty");
        let path = dir.join("t.opus");
        write_tone(&path, 0);
        assert_eq!(duration(&path).unwrap(), Duration::ZERO);
        let mut out = Vec::new();
        Reader::open(&path).unwrap().read(10, &mut out).unwrap();
        assert!(out.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn truncated_file_has_no_duration() {
        assert_eq!(last_page_granule(b"OggS\0junk"), None);
        assert_eq!(last_page_granule(b""), None);
    }
}
