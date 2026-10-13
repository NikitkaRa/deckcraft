//! Audio decoding to interleaved f32 PCM.
//!
//! Opus (Ogg, WebM / Matroska) uses our own decoder; everything else — MP3, AAC (M4A / MP4 / MOV /
//! ADTS), ALAC, FLAC, WAV, AIFF, CAF, Vorbis, PCM in MP4 / Matroska — goes through symphonia
//! (MPL-2.0, used unmodified), with gapless trimming on.
//!
//! [`decode`] drains a whole file for callers that asked for every sample (tests, short clips).
//! Playback uses [`AudioDecoder::pull`] so it can stop after a bounded chunk and when cancelled.

use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::{Bytes, Container, MediaError, Result};

/// Decoded audio: interleaved f32 samples in −1..1.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pcm {
    pub rate: u32,
    pub channels: u16,
    pub samples: Vec<f32>,
}

impl Pcm {
    /// Sample frames (samples per channel).
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1) as usize
    }
    pub fn duration(&self) -> f64 {
        self.frames() as f64 / self.rate.max(1) as f64
    }
    /// Sample `frame` of channel `ch` (0 past the end).
    pub fn at(&self, frame: usize, ch: usize) -> f32 {
        let c = self.channels.max(1) as usize;
        self.samples.get(frame * c + ch.min(c - 1)).copied().unwrap_or(0.0)
    }
    /// Root-mean-square level over all samples.
    pub fn rms(&self) -> f32 {
        if self.samples.is_empty() {
            return 0.0;
        }
        (self.samples.iter().map(|s| s * s).sum::<f32>() / self.samples.len() as f32).sqrt()
    }
}

/// Bytes as a symphonia source without copying them.
struct Shared(Bytes);

impl AsRef<[u8]> for Shared {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// Decode the audio of `bytes` (an audio file, or the audio track of a video) to PCM.
///
/// This keeps every sample. Playback does not call it: a long file is pulled in chunks so the
/// PCM never has to exist all at once.
pub fn decode(bytes: &Bytes) -> Result<Pcm> {
    let mut dec = AudioDecoder::open(bytes)?;
    let cancel = AtomicBool::new(false);
    let mut samples = Vec::new();
    loop {
        let n = dec.pull(16_384, &mut samples, &cancel)?;
        if n == 0 {
            break;
        }
    }
    if dec.rate == 0 {
        return Err(MediaError::Corrupt("no audio could be decoded".into()));
    }
    Ok(Pcm { rate: dec.rate, channels: dec.channels.max(1), samples })
}

/// Duration in seconds, discarding samples as they are counted.
///
/// Files with no duration in the container (raw MP3, ADTS) used to decode into one buffer just to
/// measure the length. The scratch buffer is reused, so the PCM does not grow with the file.
pub(crate) fn measure_duration(bytes: &Bytes) -> Option<f64> {
    let mut dec = AudioDecoder::open(bytes).ok()?;
    let cancel = AtomicBool::new(false);
    let mut scratch = Vec::new();
    let mut frames = 0u64;
    loop {
        scratch.clear();
        match dec.pull(4_096, &mut scratch, &cancel) {
            Ok(0) => break,
            Ok(n) => frames = frames.saturating_add(n as u64),
            Err(_) if frames > 0 => break,
            Err(_) => return None,
        }
    }
    let rate = dec.rate.max(1);
    Some(frames as f64 / rate as f64)
}

/// A decoder that yields at most `max_frames` interleaved frames per [`AudioDecoder::pull`].
pub(crate) struct AudioDecoder {
    kind: Kind,
    rate: u32,
    channels: u16,
    eof: bool,
}

enum Kind {
    OpusOgg(OpusOgg),
    OpusMkv(OpusMkv),
    Sym(SymDec),
}

impl AudioDecoder {
    pub(crate) fn open(bytes: &Bytes) -> Result<AudioDecoder> {
        match Container::sniff(bytes) {
            Container::Asf => Err(MediaError::Unsupported("Windows Media Audio (WMA)".into())),
            Container::Ogg => match open_ogg_opus(bytes) {
                Some(r) => r,
                None => open_symphonia(bytes, "ogg"),
            },
            Container::Matroska => match open_mkv_opus(bytes) {
                Some(r) => r,
                None => open_symphonia(bytes, "mkv"),
            },
            c => open_symphonia(bytes, c.name()),
        }
    }

    pub(crate) fn rate(&self) -> u32 {
        self.rate
    }

    pub(crate) fn channels(&self) -> u16 {
        self.channels
    }

    /// Append up to `max_frames` interleaved frames to `out`. `Ok(0)` is the end, or a stop when
    /// `cancel` is set (that stop does not finish the stream: a later pull with `cancel` clear
    /// continues).
    pub(crate) fn pull(&mut self, max_frames: usize, out: &mut Vec<f32>, cancel: &AtomicBool) -> Result<usize> {
        if self.eof || max_frames == 0 || cancel.load(Ordering::Acquire) {
            return Ok(0);
        }
        let (wrote, done) = match &mut self.kind {
            Kind::OpusOgg(s) => s.pull(max_frames, out, cancel)?,
            Kind::OpusMkv(s) => s.pull(max_frames, out, cancel)?,
            Kind::Sym(s) => s.pull(max_frames, out, cancel)?,
        };
        if done {
            self.eof = true;
        }
        self.sync_meta();
        Ok(wrote)
    }

    fn sync_meta(&mut self) {
        let (rate, channels) = match &self.kind {
            Kind::OpusOgg(s) => (48_000, s.cur.channels),
            Kind::OpusMkv(s) => (48_000, s.cur.channels),
            Kind::Sym(s) => (s.rate, s.channels),
        };
        if rate > 0 {
            self.rate = rate;
        }
        if channels > 0 {
            self.channels = channels;
        }
    }
}

fn sym_err(e: SymError) -> MediaError {
    match e {
        SymError::Unsupported(what) => MediaError::Unsupported(what.to_string()),
        other => MediaError::Corrupt(other.to_string()),
    }
}

/// Opus packets still to read, plus samples held back so a pull can stop mid-packet.
struct OpusCursor {
    dec: deckcraft_opus::Decoder,
    index: usize,
    count: usize,
    /// Playable frames after end-trim. `0` means the container gave no length (keep every frame).
    total_frames: u64,
    produced: u64,
    pending: Vec<f32>,
    pending_at: usize,
    channels: u16,
}

struct OpusOgg {
    bytes: Bytes,
    file: deckcraft_ogg::OggFile,
    stream: usize,
    cur: OpusCursor,
}

struct OpusMkv {
    bytes: Bytes,
    file: deckcraft_matroska::MkvFile,
    track: usize,
    cur: OpusCursor,
}

struct SymDec {
    format: Box<dyn symphonia::core::formats::FormatReader>,
    decoder: Box<dyn symphonia::core::codecs::Decoder>,
    track_id: u32,
    buf: Option<SampleBuffer<f32>>,
    pending: Vec<f32>,
    pending_at: usize,
    rate: u32,
    channels: u16,
    /// A sample has been emitted. A later hard error then ends the stream instead of failing it.
    emitted: bool,
}

fn open_ogg_opus(bytes: &Bytes) -> Option<Result<AudioDecoder>> {
    let src: &[u8] = bytes;
    let file = deckcraft_ogg::open(src).ok()?;
    let s = file.stream_of(deckcraft_ogg::Codec::Opus)?;
    let (head, count) = {
        let stream = file.streams.get(s)?;
        let head = stream.headers.first()?.clone();
        (head, stream.packets.len())
    };
    Some((|| {
        let mut dec = deckcraft_opus::Decoder::new(&head).map_err(|e| MediaError::Corrupt(e.to_string()))?;
        dec.set_trim_pre_skip(true);
        let channels = dec.channels() as u16;
        let total_frames = file.streams.get(s).map(|st| deckcraft_ogg::OpusTiming::of(st, dec.pre_skip() as u32).total.max(0) as u64).unwrap_or(0);
        Ok(AudioDecoder {
            kind: Kind::OpusOgg(OpusOgg {
                bytes: bytes.clone(),
                file,
                stream: s,
                cur: OpusCursor { dec, index: 0, count, total_frames, produced: 0, pending: Vec::new(), pending_at: 0, channels },
            }),
            rate: 48_000,
            channels,
            eof: false,
        })
    })())
}

fn open_mkv_opus(bytes: &Bytes) -> Option<Result<AudioDecoder>> {
    let src: &[u8] = bytes;
    let file = deckcraft_matroska::open(src).ok()?;
    let (ti, head) = file.tracks.iter().enumerate().find_map(|(i, t)| match &t.codec {
        deckcraft_matroska::Codec::Opus { head } => Some((i, head.clone())),
        _ => None,
    })?;
    Some((|| {
        let mut dec = deckcraft_opus::Decoder::new(&head).map_err(|e| MediaError::Corrupt(e.to_string()))?;
        dec.set_trim_pre_skip(true);
        let channels = dec.channels() as u16;
        let count = file.tracks.get(ti).map(|t| t.samples.len()).unwrap_or(0);
        Ok(AudioDecoder {
            kind: Kind::OpusMkv(OpusMkv {
                bytes: bytes.clone(),
                file,
                track: ti,
                cur: OpusCursor { dec, index: 0, count, total_frames: 0, produced: 0, pending: Vec::new(), pending_at: 0, channels },
            }),
            rate: 48_000,
            channels,
            eof: false,
        })
    })())
}

fn open_symphonia(bytes: &Bytes, ext: &str) -> Result<AudioDecoder> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(Shared(bytes.clone()))), MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    match ext {
        "unknown" => {}
        "mp4" => {
            hint.with_extension("mp4");
        }
        e => {
            hint.with_extension(e);
        }
    }
    let opts = FormatOptions { enable_gapless: true, ..Default::default() };
    let probed = symphonia::default::get_probe().format(&hint, mss, &opts, &MetadataOptions::default()).map_err(|e| match e {
        SymError::Unsupported(_) => MediaError::Unsupported("this audio format".into()),
        other => sym_err(other),
    })?;
    let format = probed.format;
    let codecs = symphonia::default::get_codecs();
    // The first track we can decode (a video's audio track follows its video track).
    let (track_id, decoder) = format
        .tracks()
        .iter()
        .filter(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .find_map(|t| codecs.make(&t.codec_params, &DecoderOptions::default()).ok().map(|d| (t.id, d)))
        .ok_or(MediaError::Missing("audio track we can decode"))?;
    Ok(AudioDecoder {
        kind: Kind::Sym(SymDec { format, decoder, track_id, buf: None, pending: Vec::new(), pending_at: 0, rate: 0, channels: 0, emitted: false }),
        rate: 0,
        channels: 0,
        eof: false,
    })
}

impl OpusOgg {
    fn pull(&mut self, max_frames: usize, out: &mut Vec<f32>, cancel: &AtomicBool) -> Result<(usize, bool)> {
        let OpusOgg { bytes, file, stream, cur } = self;
        cur.pull(max_frames, out, cancel, |i| file.read_packet(&bytes[..], *stream, i).ok())
    }
}

impl OpusMkv {
    fn pull(&mut self, max_frames: usize, out: &mut Vec<f32>, cancel: &AtomicBool) -> Result<(usize, bool)> {
        let OpusMkv { bytes, file, track, cur } = self;
        cur.pull(max_frames, out, cancel, |i| file.read_sample(&bytes[..], *track, i).ok())
    }
}

impl OpusCursor {
    fn pull(
        &mut self,
        max_frames: usize,
        out: &mut Vec<f32>,
        cancel: &AtomicBool,
        mut read: impl FnMut(usize) -> Option<Vec<u8>>,
    ) -> Result<(usize, bool)> {
        let mut done = false;
        let mut wrote = drain_pending(&mut self.pending, &mut self.pending_at, self.channels, max_frames, out);
        self.produced = self.produced.saturating_add(wrote as u64);
        while wrote < max_frames && !done {
            if cancel.load(Ordering::Acquire) {
                break;
            }
            if self.total_frames > 0 && self.produced >= self.total_frames {
                done = true;
                break;
            }
            if self.index >= self.count {
                done = true;
                break;
            }
            let i = self.index;
            self.index += 1;
            let Some(packet) = read(i) else { continue };
            let planes = match self.dec.decode(Some(&packet)) {
                Ok(p) => p,
                Err(e) => {
                    log::debug!("opus packet {i}: {e}");
                    continue;
                }
            };
            let room = max_frames - wrote;
            let mut cap = room;
            if self.total_frames > 0 {
                let left = self.total_frames.saturating_sub(self.produced) as usize;
                cap = cap.min(left);
            }
            let n = take_planes(&planes, cap, out, &mut self.pending);
            wrote += n;
            self.produced = self.produced.saturating_add(n as u64);
            if self.total_frames > 0 && self.produced >= self.total_frames {
                self.pending.clear();
                self.pending_at = 0;
                done = true;
            }
        }
        Ok((wrote, done))
    }
}

impl SymDec {
    fn pull(&mut self, max_frames: usize, out: &mut Vec<f32>, cancel: &AtomicBool) -> Result<(usize, bool)> {
        let mut done = false;
        let ch0 = self.channels.max(1);
        let mut wrote = drain_pending(&mut self.pending, &mut self.pending_at, ch0, max_frames, out);
        if wrote > 0 {
            self.emitted = true;
        }
        while wrote < max_frames && !done {
            if cancel.load(Ordering::Acquire) {
                break;
            }
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymError::IoError(_)) | Err(SymError::ResetRequired) => {
                    done = true;
                    break;
                }
                Err(SymError::DecodeError(e)) => {
                    log::debug!("audio demux: {e}");
                    continue;
                }
                Err(e) => {
                    if self.emitted || wrote > 0 {
                        done = true;
                        break;
                    }
                    return Err(sym_err(e));
                }
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    let spec = *decoded.spec();
                    let ch = spec.channels.count() as u16;
                    if self.rate == 0 {
                        self.rate = spec.rate;
                        self.channels = ch;
                    }
                    if ch != self.channels || decoded.frames() == 0 {
                        continue;
                    }
                    let Some(need) = (decoded.capacity() as u64).checked_mul(u64::from(ch.max(1))) else {
                        continue;
                    };
                    if self.buf.as_ref().is_none_or(|b| (b.capacity() as u64) < need) {
                        self.buf = Some(SampleBuffer::<f32>::new(decoded.capacity() as u64, spec));
                    }
                    let frame_count = decoded.frames();
                    let Some(buf) = self.buf.as_mut() else { continue };
                    buf.copy_interleaved_ref(decoded);
                    let samples = buf.samples();
                    let frame_samples = frame_count.saturating_mul(ch as usize).min(samples.len());
                    let Some(slice) = samples.get(..frame_samples) else { continue };
                    let room = max_frames - wrote;
                    let n = take_interleaved(slice, ch, room, out, &mut self.pending);
                    wrote += n;
                    if n > 0 {
                        self.emitted = true;
                    }
                }
                Err(SymError::DecodeError(e)) => log::debug!("audio decode: {e}"),
                Err(SymError::IoError(_)) => continue,
                Err(e) => {
                    if self.emitted || wrote > 0 {
                        done = true;
                        break;
                    }
                    return Err(sym_err(e));
                }
            }
        }
        Ok((wrote, done))
    }
}

/// Move up to `max_frames` already-decoded frames from `pending` into `out`.
fn drain_pending(pending: &mut Vec<f32>, at: &mut usize, channels: u16, max_frames: usize, out: &mut Vec<f32>) -> usize {
    let ch = channels.max(1) as usize;
    let have = pending.len().saturating_sub(*at) / ch;
    let n = have.min(max_frames);
    let Some(samples) = n.checked_mul(ch) else { return 0 };
    let end = at.saturating_add(samples);
    if let Some(slice) = pending.get(*at..end) {
        out.extend_from_slice(slice);
        *at = end;
    }
    if *at >= pending.len() {
        pending.clear();
        *at = 0;
    }
    n
}

/// Interleave up to `max_frames` from planar Opus output. The rest stays in `pending`.
fn take_planes(planes: &[Vec<f32>], max_frames: usize, out: &mut Vec<f32>, pending: &mut Vec<f32>) -> usize {
    let ch = planes.len();
    if ch == 0 || max_frames == 0 {
        return 0;
    }
    let avail = planes.iter().map(Vec::len).min().unwrap_or(0);
    let n = avail.min(max_frames);
    out.reserve(n.saturating_mul(ch));
    for i in 0..n {
        for p in planes {
            if let Some(s) = p.get(i) {
                out.push(*s);
            }
        }
    }
    if avail > n {
        pending.clear();
        pending.reserve((avail - n).saturating_mul(ch));
        for i in n..avail {
            for p in planes {
                if let Some(s) = p.get(i) {
                    pending.push(*s);
                }
            }
        }
    }
    n
}

/// Copy up to `max_frames` interleaved frames. The rest stays in `pending` (which must be empty).
fn take_interleaved(samples: &[f32], channels: u16, max_frames: usize, out: &mut Vec<f32>, pending: &mut Vec<f32>) -> usize {
    let ch = channels.max(1) as usize;
    let avail = samples.len() / ch;
    let n = avail.min(max_frames);
    let Some(take) = n.checked_mul(ch) else { return 0 };
    if let Some(slice) = samples.get(..take) {
        out.extend_from_slice(slice);
    }
    if let Some(rest) = samples.get(take..)
        && !rest.is_empty()
    {
        pending.extend_from_slice(rest);
    }
    n
}

/// Sample rate, channels and frame count from symphonia's container probe (no decoding).
pub(crate) fn probe_symphonia(bytes: &Bytes, ext: &str) -> Option<(String, u32, u16, Option<u64>)> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(Shared(bytes.clone()))), MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    if ext != "unknown" {
        hint.with_extension(ext);
    }
    let opts = FormatOptions { enable_gapless: true, ..Default::default() };
    let probed = symphonia::default::get_probe().format(&hint, mss, &opts, &MetadataOptions::default()).ok()?;
    let codecs = symphonia::default::get_codecs();
    let t = probed.format.tracks().iter().find(|t| t.codec_params.codec != CODEC_TYPE_NULL)?;
    let p = &t.codec_params;
    let name = codecs.get_codec(p.codec).map(|d| d.short_name.to_string()).unwrap_or_else(|| "unknown".into());
    Some((name, p.sample_rate.unwrap_or(0), p.channels.map(|c| c.count() as u16).unwrap_or(0), p.n_frames))
}

/// Interleaved PCM resampled (linear) to `rate`.
pub fn resample(pcm: &Pcm, rate: u32) -> Pcm {
    if pcm.rate == rate || pcm.rate == 0 || rate == 0 {
        return pcm.clone();
    }
    let c = pcm.channels.max(1) as usize;
    let n_in = pcm.frames();
    let n_out = (n_in as u64 * rate as u64 / pcm.rate as u64) as usize;
    let step = pcm.rate as f64 / rate as f64;
    let mut out = Vec::with_capacity(n_out * c);
    for i in 0..n_out {
        let x = i as f64 * step;
        let k = x as usize;
        let f = (x - k as f64) as f32;
        for ch in 0..c {
            let a = pcm.at(k, ch);
            let b = if k + 1 < n_in { pcm.at(k + 1, ch) } else { a };
            out.push(a + (b - a) * f);
        }
    }
    Pcm { rate, channels: pcm.channels, samples: out }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_keeps_duration_and_shape() {
        let pcm = Pcm { rate: 100, channels: 1, samples: (0..100).map(|i| i as f32 / 100.0).collect() };
        let r = resample(&pcm, 200);
        assert_eq!(r.frames(), 200);
        assert!((r.duration() - 1.0).abs() < 1e-9);
        assert!((r.at(101, 0) - 0.505).abs() < 1e-3);
    }

    #[test]
    fn wma_is_reported_unsupported() {
        let mut b = crate::ASF_HEADER.to_vec();
        b.extend_from_slice(&[0; 64]);
        assert!(matches!(decode(&std::sync::Arc::new(b)), Err(MediaError::Unsupported(_))));
    }

    fn wav(rate: u32, samples: &[f32]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| ((s * 32767.0) as i16).to_le_bytes()).collect();
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * 2).to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.extend_from_slice(&data);
        b
    }

    fn collect(bytes: &Bytes, chunk: usize) -> Vec<f32> {
        let mut dec = AudioDecoder::open(bytes).expect("decoder");
        let cancel = AtomicBool::new(false);
        let mut out = Vec::new();
        loop {
            let n = dec.pull(chunk, &mut out, &cancel).expect("pull");
            if n == 0 {
                break;
            }
        }
        out
    }

    #[test]
    fn small_pulls_match_one_drain_and_cancel_does_not_consume() {
        let samples: Vec<f32> = (0..800).map(|i| (i as f32 / 800.0) - 0.5).collect();
        let bytes: Bytes = std::sync::Arc::new(wav(8_000, &samples));
        let whole = collect(&bytes, 10_000);
        let chunked = collect(&bytes, 7);
        assert_eq!(chunked, whole);
        assert!(whole.len() > 100);

        let mut dec = AudioDecoder::open(&bytes).expect("decoder");
        let cancel = AtomicBool::new(true);
        let mut out = Vec::new();
        assert_eq!(dec.pull(10_000, &mut out, &cancel).expect("cancelled pull"), 0);
        assert!(out.is_empty());
        let cancel = AtomicBool::new(false);
        let n = dec.pull(50, &mut out, &cancel).expect("pull after cancel");
        assert!(n > 0 && n <= 50, "{n}");
    }
}
