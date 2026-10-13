//! The playback mixer and clock.
//!
//! The UI host owns a [`Player`] and an optional [`AudioOut`] (cpal on desktop). Each playing
//! media shape is a *voice* with its own clock (`position`, in media seconds), advanced by the
//! audio callback as it pulls samples — so video frames synced to it stay in step with what is
//! heard. Without an output device (web, CI, no sound card) [`Player::tick`] advances the clocks on
//! wall time instead, so playback, looping and video still work silently.
//!
//! Audio is not decoded into one buffer. Each voice keeps a window of PCM no larger than
//! [`Limits::ahead_bytes`]; the decoder waits when that window is full and is cancelled when the
//! voice stops or the player is dropped. Finished clips that fit in the window may be cached, up
//! to [`Limits::cache_bytes`] in total. At most [`Limits::max_jobs`] decodes run at once. On the
//! web, where there are no threads, each tick pulls one bounded chunk so a long file cannot stall
//! the page.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use crate::audio::AudioDecoder;
use crate::{Bytes, ClipParams, MediaError, Pcm};

/// An audio output device. `fill(buffer, channels, sample_rate)` is called from the device's
/// thread to fill an interleaved f32 buffer.
pub trait AudioOut {
    fn start(&mut self, fill: Box<dyn FnMut(&mut [f32], usize, u32) + Send>) -> Result<(), String>;
    fn stop(&mut self);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayState {
    /// Waiting for the audio to decode.
    Loading,
    Playing,
    Paused,
    /// Reached the end (not looping).
    Ended,
}

impl PlayState {
    pub fn name(self) -> &'static str {
        match self {
            PlayState::Loading => "loading",
            PlayState::Playing => "playing",
            PlayState::Paused => "paused",
            PlayState::Ended => "ended",
        }
    }
}

/// What to play.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VoiceSpec {
    pub clip: ClipParams,
    pub looping: bool,
    /// Media duration in seconds from the probe (0 = unknown: taken from the decoded audio).
    pub duration: f64,
    /// Decode and mix the audio (false for videos without sound, or muted).
    pub audio: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VoiceStatus {
    pub state: PlayState,
    /// Media time in seconds.
    pub position: f64,
    pub duration: f64,
    /// Why the audio is silent, if it couldn't be decoded.
    pub error: Option<String>,
}

/// Default mix rate when no device says otherwise.
const DEFAULT_RATE: u32 = 48_000;

/// PCM kept for one voice: two seconds of stereo f32. A longer file slides this window forward
/// instead of growing without a bound (an hour of that format is about 1.4 GB).
const AHEAD_BYTES: usize = 2 * 48_000 * 2 * std::mem::size_of::<f32>();

/// Reusable finished clips. A clip larger than the playback window is never cached.
const CACHE_BYTES: usize = 8 * 1024 * 1024;

/// Decodes running at the same time. Further starts wait, and stop waiting if cancelled.
const MAX_JOBS: usize = 2;

/// Frames per pull, so cancellation is noticed within one chunk.
const PULL_FRAMES: usize = 2_048;

/// Frames kept behind the playhead so a mix can interpolate the sample it is on.
const LOOKBEHIND: u64 = 2;

/// Channel count assumed for the first pull, before the file has said. A first chunk sized for
/// this many channels cannot exceed the byte cap even if the file really is that wide.
const CHANNEL_GUESS: usize = 16;

#[derive(Clone, Debug)]
struct Limits {
    cache_bytes: usize,
    ahead_bytes: usize,
    max_jobs: usize,
    pull_frames: usize,
    /// `false` on wasm and in tests that need a deterministic pump.
    threaded: bool,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { cache_bytes: CACHE_BYTES, ahead_bytes: AHEAD_BYTES, max_jobs: MAX_JOBS, pull_frames: PULL_FRAMES, threaded: crate::THREADS }
    }
}

fn lock<'a, T>(m: &'a Mutex<T>) -> MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn frames_at(t: f64, rate: u32) -> u64 {
    if rate == 0 || !t.is_finite() || t <= 0.0 {
        return 0;
    }
    let f = t * f64::from(rate);
    if f >= u64::MAX as f64 { u64::MAX } else { f as u64 }
}

fn pcm_bytes(p: &Pcm) -> usize {
    p.samples.len().saturating_mul(std::mem::size_of::<f32>())
}

fn aligned_cap(ahead_bytes: usize, channels: usize) -> usize {
    let ch = channels.max(1);
    let samples = (ahead_bytes / std::mem::size_of::<f32>()).max(ch);
    samples - (samples % ch)
}

enum SourcePull {
    Frames { rate: u32, channels: u16, samples: Vec<f32> },
    Eof,
    Cancelled,
}

/// A PCM producer pulled a few frames at a time. `pull` must return quickly once `cancel` is set:
/// the player joins the decode thread on drop.
trait PcmSource: Send {
    fn pull(&mut self, max_frames: usize, cancel: &AtomicBool) -> crate::Result<SourcePull>;
    fn restart(&mut self) -> crate::Result<()>;
}

struct FileSource {
    bytes: Bytes,
    dec: Option<AudioDecoder>,
}

impl FileSource {
    fn new(bytes: Bytes) -> FileSource {
        FileSource { bytes, dec: None }
    }
}

impl PcmSource for FileSource {
    fn pull(&mut self, max_frames: usize, cancel: &AtomicBool) -> crate::Result<SourcePull> {
        if cancel.load(Ordering::Acquire) {
            return Ok(SourcePull::Cancelled);
        }
        if self.dec.is_none() {
            self.dec = Some(AudioDecoder::open(&self.bytes)?);
        }
        let Some(dec) = self.dec.as_mut() else {
            return Err(MediaError::Corrupt("no decoder".into()));
        };
        let mut samples = Vec::new();
        let n = dec.pull(max_frames, &mut samples, cancel)?;
        if cancel.load(Ordering::Acquire) {
            return Ok(SourcePull::Cancelled);
        }
        if n == 0 {
            return Ok(SourcePull::Eof);
        }
        let ch = usize::from(dec.channels().max(1));
        let keep = n.saturating_mul(ch);
        if samples.len() > keep {
            samples.truncate(keep);
        }
        Ok(SourcePull::Frames { rate: dec.rate().max(1), channels: dec.channels().max(1), samples })
    }

    fn restart(&mut self) -> crate::Result<()> {
        self.dec = None;
        Ok(())
    }
}

enum Drive {
    Worked,
    Idle,
    Stop,
    NoSource,
}

struct LiveInner {
    rate: u32,
    channels: u16,
    /// Absolute frame of `samples[0]`.
    origin: u64,
    samples: Vec<f32>,
    /// Frames consumed from the start of this generation, including ones already dropped.
    decoded: u64,
    eof: bool,
    error: Option<String>,
}

struct LivePcm {
    inner: Mutex<LiveInner>,
    playhead: AtomicU64,
    cancel: AtomicBool,
    restart_gen: AtomicU64,
    applied_gen: AtomicU64,
    /// Held while the worker decides to sleep, so a kick cannot land between the check and the wait.
    #[cfg(not(target_arch = "wasm32"))]
    kick: Mutex<()>,
    wake: Condvar,
    /// Set only for inline decoding (wasm, tests). A worker owns the source otherwise.
    source: Mutex<Option<Box<dyn PcmSource>>>,
}

impl LivePcm {
    fn new() -> Arc<LivePcm> {
        Arc::new(LivePcm {
            inner: Mutex::new(LiveInner { rate: 0, channels: 0, origin: 0, samples: Vec::new(), decoded: 0, eof: false, error: None }),
            playhead: AtomicU64::new(0),
            cancel: AtomicBool::new(false),
            restart_gen: AtomicU64::new(0),
            applied_gen: AtomicU64::new(0),
            #[cfg(not(target_arch = "wasm32"))]
            kick: Mutex::new(()),
            wake: Condvar::new(),
            source: Mutex::new(None),
        })
    }

    fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
        self.wake_worker();
    }

    fn set_playhead(&self, frame: u64) {
        self.playhead.store(frame, Ordering::Release);
    }

    fn kick(&self) {
        self.wake_worker();
    }

    /// Wake the decode worker. The lock is taken first so the notify cannot slip between the
    /// worker's idle check and its wait.
    fn wake_worker(&self) {
        #[cfg(not(target_arch = "wasm32"))]
        let _held = lock(&self.kick);
        self.wake.notify_one();
    }

    fn request_restart(&self) {
        let cur = self.restart_gen.load(Ordering::Acquire);
        if cur != self.applied_gen.load(Ordering::Acquire) {
            self.kick();
            return;
        }
        self.restart_gen.fetch_add(1, Ordering::AcqRel);
        self.kick();
    }

    fn rate(&self) -> u32 {
        lock(&self.inner).rate
    }

    fn loading(&self) -> bool {
        let g = lock(&self.inner);
        g.error.is_none() && !g.eof && g.samples.is_empty()
    }

    fn error_text(&self) -> Option<String> {
        lock(&self.inner).error.clone()
    }

    fn duration_if_eof(&self) -> f64 {
        let g = lock(&self.inner);
        if g.eof && g.rate > 0 { g.decoded as f64 / f64::from(g.rate) } else { 0.0 }
    }

    #[cfg(test)]
    fn window_bytes(&self) -> usize {
        lock(&self.inner).samples.len().saturating_mul(std::mem::size_of::<f32>())
    }

    #[cfg(test)]
    fn is_eof(&self) -> bool {
        lock(&self.inner).eof
    }

    fn behind(&self, frame: u64) -> bool {
        let g = lock(&self.inner);
        !g.samples.is_empty() && frame.saturating_add(LOOKBEHIND) < g.origin
    }

    /// Enough audio to start, or the decode has finished or failed.
    fn ready(&self, ahead_bytes: usize) -> bool {
        let g = lock(&self.inner);
        if g.error.is_some() || g.eof {
            return true;
        }
        if g.rate == 0 || g.samples.is_empty() {
            return false;
        }
        let ch = usize::from(g.channels.max(1));
        let cap = (aligned_cap(ahead_bytes, ch) / ch).max(1);
        let buffered = g.samples.len() / ch;
        let end = g.origin.saturating_add(buffered as u64);
        let head = self.playhead.load(Ordering::Acquire);
        end.saturating_sub(head) >= cap as u64
    }

    /// The whole clip is in the window, so it can be cached and seeked without another decode.
    fn finished_clip(&self) -> Option<Pcm> {
        let g = lock(&self.inner);
        if !g.eof || g.origin != 0 || g.error.is_some() || g.rate == 0 || g.samples.is_empty() {
            return None;
        }
        let ch = usize::from(g.channels.max(1));
        let buffered = (g.samples.len() / ch) as u64;
        if g.decoded != buffered {
            return None;
        }
        Some(Pcm { rate: g.rate, channels: g.channels.max(1), samples: g.samples.clone() })
    }

    fn fail(&self, msg: &str) {
        let mut g = lock(&self.inner);
        g.error = Some(msg.to_string());
        g.eof = true;
    }

    fn mark_eof(&self) {
        lock(&self.inner).eof = true;
    }

    fn reset_window(&self) {
        let mut g = lock(&self.inner);
        g.samples.clear();
        g.origin = 0;
        g.decoded = 0;
        g.eof = false;
        g.error = None;
    }

    fn room(&self, limits: &Limits) -> usize {
        let g = lock(&self.inner);
        if g.eof || g.error.is_some() {
            return 0;
        }
        let ch = if g.channels == 0 { CHANNEL_GUESS } else { usize::from(g.channels.max(1)) };
        let cap_frames = (aligned_cap(limits.ahead_bytes, ch) / ch).max(1);
        let quantum = limits.pull_frames.max(1).min(cap_frames);
        let buffered = g.samples.len() / ch;
        let end = g.origin.saturating_add(buffered as u64);
        let head = self.playhead.load(Ordering::Acquire);
        if end < head.saturating_sub(LOOKBEHIND) {
            return quantum;
        }
        let ahead = end.saturating_sub(head.min(end)) as usize;
        if ahead >= cap_frames {
            return 0;
        }
        let keep_from = head.saturating_sub(LOOKBEHIND);
        let droppable = if keep_from > g.origin { (keep_from - g.origin) as usize } else { 0 };
        let droppable = droppable.min(buffered);
        let free = cap_frames.saturating_sub(buffered).saturating_add(droppable);
        (cap_frames - ahead).min(free).min(quantum)
    }

    fn append(&self, rate: u32, channels: u16, samples: &[f32], ahead_bytes: usize) {
        if samples.is_empty() {
            return;
        }
        let mut g = lock(&self.inner);
        if g.rate == 0 {
            g.rate = rate.max(1);
            g.channels = channels.max(1);
        }
        let ch = usize::from(g.channels.max(1));
        if usize::from(channels.max(1)) != ch || !samples.len().is_multiple_of(ch) {
            return;
        }
        let frames = samples.len() / ch;
        let chunk_start = g.decoded;
        g.decoded = g.decoded.saturating_add(frames as u64);
        let cap_samples = aligned_cap(ahead_bytes, ch);
        let head = self.playhead.load(Ordering::Acquire);
        let keep_from = head.saturating_sub(LOOKBEHIND);

        let buffered = g.samples.len() / ch;
        let window_end = g.origin.saturating_add(buffered as u64);
        if !g.samples.is_empty() && window_end != chunk_start {
            g.samples.clear();
            g.origin = chunk_start;
        }
        if g.samples.is_empty() && chunk_start.saturating_add(frames as u64) <= keep_from {
            g.origin = g.decoded;
            return;
        }
        let skip_frames = if g.samples.is_empty() { (keep_from.saturating_sub(chunk_start) as usize).min(frames) } else { 0 };
        let skip = skip_frames.saturating_mul(ch);
        if g.samples.is_empty() {
            g.origin = chunk_start.saturating_add(skip_frames as u64);
        }
        if let Some(tail) = samples.get(skip..) {
            g.samples.extend_from_slice(tail);
        }
        // A clip that still fits keeps its start, so a loop or a short seek does not re-decode.
        if g.origin == 0 && g.samples.len() <= cap_samples {
            return;
        }
        if keep_from > g.origin {
            let drop_frames = ((keep_from - g.origin) as usize).min(g.samples.len() / ch);
            let drop = drop_frames.saturating_mul(ch);
            if drop > 0 && drop <= g.samples.len() {
                g.samples.drain(..drop);
                g.origin = g.origin.saturating_add(drop_frames as u64);
            }
        }
        if g.samples.len() > cap_samples {
            let extra = g.samples.len() - cap_samples;
            let extra = extra - (extra % ch);
            if extra > 0 && extra <= g.samples.len() {
                g.samples.drain(..extra);
                g.origin = g.origin.saturating_add((extra / ch) as u64);
            }
        }
    }

    fn drive(&self, source: &mut dyn PcmSource, limits: &Limits) -> Drive {
        if self.cancel.load(Ordering::Acquire) {
            return Drive::Stop;
        }
        let want = self.restart_gen.load(Ordering::Acquire);
        if want != self.applied_gen.load(Ordering::Acquire) {
            if let Err(e) = source.restart() {
                self.fail(&e.to_string());
                return Drive::Stop;
            }
            self.reset_window();
            self.applied_gen.store(want, Ordering::Release);
        }
        if lock(&self.inner).error.is_some() {
            return Drive::Stop;
        }
        if lock(&self.inner).eof {
            return Drive::Idle;
        }
        let room = self.room(limits);
        if room == 0 {
            return Drive::Idle;
        }
        let generation = self.restart_gen.load(Ordering::Acquire);
        match source.pull(room, &self.cancel) {
            Ok(SourcePull::Cancelled) => Drive::Stop,
            Err(e) => {
                if self.cancel.load(Ordering::Acquire) {
                    Drive::Stop
                } else {
                    self.fail(&e.to_string());
                    Drive::Stop
                }
            }
            Ok(SourcePull::Eof) => {
                if self.restart_gen.load(Ordering::Acquire) != generation {
                    Drive::Worked
                } else {
                    self.mark_eof();
                    Drive::Idle
                }
            }
            Ok(SourcePull::Frames { samples, .. }) if samples.is_empty() => {
                self.mark_eof();
                Drive::Idle
            }
            Ok(SourcePull::Frames { rate, channels, samples }) => {
                if self.cancel.load(Ordering::Acquire) {
                    return Drive::Stop;
                }
                if self.restart_gen.load(Ordering::Acquire) != generation {
                    return Drive::Worked;
                }
                self.append(rate, channels, &samples, limits.ahead_bytes);
                Drive::Worked
            }
        }
    }

    fn pump_inline(&self, limits: &Limits) -> Drive {
        let mut slot = lock(&self.source);
        let Some(source) = slot.as_mut() else { return Drive::NoSource };
        self.drive(source.as_mut(), limits)
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn wait_idle(&self, limits: &Limits) {
        let g = lock(&self.kick);
        if self.cancel.load(Ordering::Acquire) {
            return;
        }
        if self.restart_gen.load(Ordering::Acquire) != self.applied_gen.load(Ordering::Acquire) {
            return;
        }
        if self.room(limits) > 0 || lock(&self.inner).error.is_some() {
            return;
        }
        let _ = self.wake.wait_timeout(g, std::time::Duration::from_millis(40));
    }

    fn sample(&self, frame: u64, ch: usize) -> Sample {
        let g = lock(&self.inner);
        if g.rate == 0 {
            return if g.eof { Sample::Silence } else { Sample::Missing };
        }
        let channels = usize::from(g.channels.max(1));
        let buffered = g.samples.len() / channels;
        let end = g.origin.saturating_add(buffered as u64);
        if frame < g.origin {
            return Sample::Behind;
        }
        if frame >= end {
            return if g.eof { Sample::Silence } else { Sample::Missing };
        }
        let rel = (frame - g.origin) as usize;
        let Some(idx) = rel.checked_mul(channels).and_then(|i| i.checked_add(ch.min(channels - 1))) else {
            return Sample::Silence;
        };
        Sample::Ready(g.samples.get(idx).copied().unwrap_or(0.0))
    }
}

enum Sample {
    Ready(f32),
    /// Not decoded yet. The clock waits.
    Missing,
    /// The playhead moved behind the window. Decode again from the start.
    Behind,
    Silence,
}

#[cfg(not(target_arch = "wasm32"))]
struct JobGate {
    max: usize,
    running: Mutex<usize>,
    cv: Condvar,
}

#[cfg(not(target_arch = "wasm32"))]
impl JobGate {
    fn new(max: usize) -> JobGate {
        JobGate { max: max.max(1), running: Mutex::new(0), cv: Condvar::new() }
    }

    fn enter(&self, cancel: &AtomicBool) -> bool {
        let mut g = lock(&self.running);
        loop {
            if cancel.load(Ordering::Acquire) {
                return false;
            }
            if *g < self.max {
                *g += 1;
                return true;
            }
            let (ng, _) = self.cv.wait_timeout(g, std::time::Duration::from_millis(30)).unwrap_or_else(|e| e.into_inner());
            g = ng;
        }
    }

    fn leave(&self) {
        {
            let mut g = lock(&self.running);
            *g = g.saturating_sub(1);
        }
        self.cv.notify_one();
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn worker_loop(mut source: Box<dyn PcmSource>, live: Arc<LivePcm>, limits: Limits, gate: Arc<JobGate>) {
    if !gate.enter(&live.cancel) {
        return;
    }
    loop {
        if live.cancel.load(Ordering::Acquire) {
            break;
        }
        match live.drive(&mut *source, &limits) {
            Drive::Stop | Drive::NoSource => break,
            Drive::Idle => live.wait_idle(&limits),
            Drive::Worked => {}
        }
    }
    gate.leave();
}

enum Audio {
    Full(Arc<Pcm>),
    Live(Arc<LivePcm>),
}

struct Voice {
    key: u64,
    media_key: u64,
    audio: Option<Audio>,
    spec: VoiceSpec,
    t: f64,
    state: PlayState,
}

impl Voice {
    fn duration(&self) -> f64 {
        if self.spec.duration > 0.0 {
            return self.spec.duration;
        }
        match &self.audio {
            Some(Audio::Full(p)) => p.duration(),
            Some(Audio::Live(l)) => l.duration_if_eof(),
            None => 0.0,
        }
    }

    fn loading(&self) -> bool {
        match &self.audio {
            Some(Audio::Live(l)) => l.loading(),
            _ => false,
        }
    }

    fn error_text(&self) -> Option<String> {
        match &self.audio {
            Some(Audio::Live(l)) => l.error_text(),
            _ => None,
        }
    }

    fn live(&self) -> Option<&Arc<LivePcm>> {
        match &self.audio {
            Some(Audio::Live(l)) => Some(l),
            _ => None,
        }
    }

    fn publish_playhead(&self) {
        let Some(live) = self.live() else { return };
        live.set_playhead(frames_at(self.t, live.rate()));
        live.kick();
    }

    fn needs_restart_at(&self, t: f64) -> bool {
        let Some(live) = self.live() else { return false };
        live.behind(frames_at(t, live.rate()))
    }
}

/// The voices, shared with the audio callback.
#[derive(Default)]
struct Mixer {
    voices: Vec<Voice>,
}

impl Mixer {
    /// Mix `frames` frames at `rate` into `out` (interleaved, `channels` wide; `None`: advance the
    /// clocks only).
    fn mix(&mut self, mut out: Option<&mut [f32]>, frames: usize, channels: usize, rate: u32) {
        if let Some(o) = out.as_deref_mut() {
            o.fill(0.0);
        }
        let step = 1.0 / rate.max(1) as f64;
        let channels = channels.max(1);
        for v in &mut self.voices {
            if v.state != PlayState::Playing {
                continue;
            }
            v.publish_playhead();
            if v.loading() {
                continue;
            }
            let dur = v.duration();
            let (start, end) = v.spec.clip.range(dur);
            if v.t < start {
                v.t = start;
            }
            let audible = out.is_some() && v.audio.is_some() && v.error_text().is_none();
            if audible {
                mix_audible(v, out.as_deref_mut(), frames, channels, step, start, end, dur);
            } else {
                advance_silent(v, frames, step, start, end);
            }
            v.publish_playhead();
        }
        if let Some(o) = out {
            for s in o.iter_mut() {
                *s = s.clamp(-1.0, 1.0);
            }
        }
    }
}

fn advance_silent(v: &mut Voice, frames: usize, step: f64, start: f64, end: f64) {
    let mut left = frames as f64 * step;
    while left > 0.0 {
        let room = end - v.t;
        if left < room {
            v.t += left;
            break;
        }
        left -= room.max(0.0);
        if v.spec.looping && end.is_finite() && end - start > 1e-3 {
            v.t = start;
            if v.needs_restart_at(start) {
                if let Some(live) = v.live() {
                    live.set_playhead(frames_at(start, live.rate()));
                    live.request_restart();
                }
                break;
            }
        } else {
            v.t = end;
            v.state = PlayState::Ended;
            break;
        }
    }
}

fn mix_audible(v: &mut Voice, out: Option<&mut [f32]>, frames: usize, channels: usize, step: f64, start: f64, end: f64, dur: f64) {
    let Some(o) = out else { return };
    let full = if let Some(Audio::Full(pcm)) = &v.audio { Some(Arc::clone(pcm)) } else { None };
    let live = if let Some(Audio::Live(live)) = &v.audio { Some(Arc::clone(live)) } else { None };
    if let Some(pcm) = full {
        mix_full(v, &pcm, o, frames, channels, step, start, end, dur);
    } else if let Some(live) = live {
        mix_live(v, &live, o, frames, channels, step, start, end, dur);
    }
}

fn mix_full(v: &mut Voice, pcm: &Pcm, o: &mut [f32], frames: usize, channels: usize, step: f64, start: f64, end: f64, dur: f64) {
    let src_ch = usize::from(pcm.channels.max(1));
    let n_src = pcm.frames();
    for f in 0..frames {
        if v.t >= end {
            if v.spec.looping {
                v.t = start;
            } else {
                v.state = PlayState::Ended;
                v.t = end;
                break;
            }
        }
        let g = v.spec.clip.gain(v.t, dur);
        let x = v.t * f64::from(pcm.rate);
        let k = x as usize;
        if g > 0.0 && k < n_src {
            let fr = (x - k as f64) as f32;
            for c in 0..channels {
                let sc = if src_ch == 1 {
                    0
                } else if c < src_ch {
                    c
                } else {
                    continue;
                };
                let a = pcm.at(k, sc);
                let b = if k + 1 < n_src { pcm.at(k + 1, sc) } else { a };
                if let Some(s) = o.get_mut(f * channels + c) {
                    *s += (a + (b - a) * fr) * g;
                }
            }
        }
        v.t += step;
    }
}

fn mix_live(v: &mut Voice, live: &LivePcm, o: &mut [f32], frames: usize, channels: usize, step: f64, start: f64, end: f64, dur: f64) {
    let rate = live.rate().max(1);
    let src_ch = {
        let g = lock(&live.inner);
        usize::from(g.channels.max(1))
    };
    for f in 0..frames {
        if v.t >= end {
            if v.spec.looping {
                v.t = start;
                if live.behind(frames_at(start, rate)) {
                    live.set_playhead(frames_at(start, rate));
                    live.request_restart();
                    break;
                }
            } else {
                v.state = PlayState::Ended;
                v.t = end;
                break;
            }
        }
        let g = v.spec.clip.gain(v.t, dur);
        let x = v.t * f64::from(rate);
        let k = if x >= u64::MAX as f64 { u64::MAX } else { x as u64 };
        let fr = (x - k as f64) as f32;
        let mut missing = false;
        if g > 0.0 {
            for c in 0..channels {
                let sc = if src_ch == 1 {
                    0
                } else if c < src_ch {
                    c
                } else {
                    continue;
                };
                let a = match live.sample(k, sc) {
                    Sample::Ready(s) => s,
                    Sample::Silence => 0.0,
                    Sample::Missing | Sample::Behind => {
                        missing = true;
                        break;
                    }
                };
                let b = match live.sample(k.saturating_add(1), sc) {
                    Sample::Ready(s) => s,
                    _ => a,
                };
                if let Some(s) = o.get_mut(f * channels + c) {
                    *s += (a + (b - a) * fr) * g;
                }
            }
        }
        if missing {
            if live.behind(k) {
                live.set_playhead(k);
                live.request_restart();
            }
            break;
        }
        v.t += step;
    }
}

/// The voices, shared with the audio callback.
pub struct Player {
    out: Option<Box<dyn AudioOut>>,
    streaming: bool,
    mixer: Arc<Mutex<Mixer>>,
    last_tick: Option<f64>,
    idle_since: Option<f64>,
    /// Finished clips small enough to replay without decoding again.
    cache: HashMap<u64, Arc<Pcm>>,
    limits: Limits,
    #[cfg(not(target_arch = "wasm32"))]
    gate: Arc<JobGate>,
    #[cfg(not(target_arch = "wasm32"))]
    workers: Vec<std::thread::JoinHandle<()>>,
}

impl Player {
    /// A player over `out` (`None`: silent, clocked by [`Player::tick`]).
    pub fn new(out: Option<Box<dyn AudioOut>>) -> Player {
        Player::with_limits(out, Limits::default())
    }

    fn with_limits(out: Option<Box<dyn AudioOut>>, limits: Limits) -> Player {
        Player {
            out,
            streaming: false,
            mixer: Arc::new(Mutex::new(Mixer::default())),
            last_tick: None,
            idle_since: None,
            cache: HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            gate: Arc::new(JobGate::new(limits.max_jobs)),
            #[cfg(not(target_arch = "wasm32"))]
            workers: Vec::new(),
            limits,
        }
    }

    pub fn has_output(&self) -> bool {
        self.out.is_some()
    }

    fn with<R>(&self, f: impl FnOnce(&mut Mixer) -> R) -> R {
        let mut g = lock(&self.mixer);
        f(&mut g)
    }

    fn audio_for(&mut self, bytes: &Bytes) -> Audio {
        let live = LivePcm::new();
        self.start_job(Box::new(FileSource::new(bytes.clone())), Arc::clone(&live));
        Audio::Live(live)
    }

    fn start_job(&mut self, source: Box<dyn PcmSource>, live: Arc<LivePcm>) {
        #[cfg(not(target_arch = "wasm32"))]
        if self.limits.threaded && crate::THREADS {
            let limits = self.limits.clone();
            let gate = Arc::clone(&self.gate);
            let live_worker = Arc::clone(&live);
            match std::thread::Builder::new().name("deckcraft-audio-decode".into()).spawn(move || worker_loop(source, live_worker, limits, gate)) {
                Ok(h) => self.workers.push(h),
                Err(_) => live.fail("couldn't start the decoder"),
            }
            return;
        }
        *lock(&live.source) = Some(source);
    }

    fn start_voice(&mut self, key: u64, media_key: u64, mut audio: Option<Audio>, spec: VoiceSpec, from: Option<f64>) {
        let replaced = self.with(|m| {
            if let Some(v) = m.voices.iter_mut().find(|v| v.key == key) {
                v.media_key = media_key;
                v.spec = spec;
                let old = if v.audio.is_none() {
                    v.audio = audio.take();
                    None
                } else {
                    audio.take()
                };
                if let Some(t) = from {
                    v.t = t;
                } else if v.state == PlayState::Ended {
                    v.t = spec.clip.range(v.duration()).0;
                }
                v.state = PlayState::Playing;
                old
            } else {
                let t = from.unwrap_or(spec.clip.range(spec.duration).0);
                m.voices.push(Voice { key, media_key, audio: audio.take(), spec, t, state: PlayState::Playing });
                None
            }
        });
        if let Some(old) = replaced {
            self.release(old);
        }
        self.sync_playheads();
        self.ensure_stream();
        self.kick_inline(key);
    }

    /// One chunk for this voice. A tick pulls the next, so opening a long file cannot block the UI.
    fn kick_inline(&mut self, key: u64) {
        if self.limits.threaded {
            return;
        }
        let live = self.with(|m| m.voices.iter().find(|v| v.key == key).and_then(|v| v.live().cloned()));
        if let Some(live) = live {
            let _ = live.pump_inline(&self.limits);
        }
    }

    /// Play voice `key` (a shape) from `from` seconds (default: the trimmed start). Resumes a
    /// paused voice when `from` is `None`.
    pub fn play(&mut self, key: u64, media_key: u64, bytes: &Bytes, spec: VoiceSpec, from: Option<f64>) {
        self.promote();
        let has = self.with(|m| m.voices.iter().any(|v| v.key == key && v.audio.is_some()));
        let audio = if spec.audio && !has {
            if let Some(pcm) = self.cache.get(&media_key).cloned() { Some(Audio::Full(pcm)) } else { Some(self.audio_for(bytes)) }
        } else {
            None
        };
        self.start_voice(key, media_key, audio, spec, from);
    }

    #[cfg(test)]
    fn play_source(&mut self, key: u64, media_key: u64, source: Box<dyn PcmSource>, spec: VoiceSpec, from: Option<f64>) {
        let live = LivePcm::new();
        self.start_job(source, Arc::clone(&live));
        let replaced = self.with(|m| {
            if let Some(v) = m.voices.iter_mut().find(|v| v.key == key) {
                let old = v.audio.replace(Audio::Live(Arc::clone(&live)));
                v.media_key = media_key;
                v.spec = spec;
                if let Some(t) = from {
                    v.t = t;
                }
                v.state = PlayState::Playing;
                old
            } else {
                let t = from.unwrap_or(spec.clip.range(spec.duration).0);
                m.voices.push(Voice { key, media_key, audio: Some(Audio::Live(live)), spec, t, state: PlayState::Playing });
                None
            }
        });
        if let Some(old) = replaced {
            self.release(old);
        }
        self.sync_playheads();
        self.kick_inline(key);
    }

    fn release(&self, audio: Audio) {
        if let Audio::Live(l) = audio {
            l.cancel();
        }
    }

    fn sync_playheads(&self) {
        self.with(|m| {
            for v in &m.voices {
                v.publish_playhead();
            }
        });
    }

    fn pump(&mut self, jobs: usize) -> bool {
        self.sync_playheads();
        let lives = self.with(|m| m.voices.iter().filter_map(|v| v.live().cloned()).collect::<Vec<_>>());
        let jobs = jobs.max(1);
        let mut used = 0;
        let mut did = false;
        for live in lives {
            if used >= jobs {
                break;
            }
            if live.cancel.load(Ordering::Acquire) {
                continue;
            }
            if matches!(live.pump_inline(&self.limits), Drive::Worked) {
                did = true;
                used += 1;
            }
        }
        did
    }

    /// Pull until the inline decoders are idle. Tests use this; a tick does one round.
    #[cfg(test)]
    fn service(&mut self) -> bool {
        let did = self.pump(self.limits.max_jobs.max(1));
        self.promote();
        did
    }

    fn cached_pcm_bytes(&self) -> usize {
        self.cache.values().fold(0usize, |n, p| n.saturating_add(pcm_bytes(p)))
    }

    fn evict_until(&mut self, budget: usize) {
        while self.cached_pcm_bytes() > budget {
            let victim = self.cache.iter().filter(|(_, p)| Arc::strong_count(p) == 1).max_by_key(|(_, p)| pcm_bytes(p)).map(|(k, _)| *k);
            let Some(k) = victim else { return };
            self.cache.remove(&k);
        }
    }

    fn store_cache(&mut self, key: u64, pcm: Arc<Pcm>) -> bool {
        let add = pcm_bytes(&pcm);
        if add == 0 || add > self.limits.cache_bytes {
            return false;
        }
        if self.cache.get(&key).is_some_and(|old| Arc::ptr_eq(old, &pcm)) {
            return true;
        }
        self.evict_until(self.limits.cache_bytes.saturating_sub(add));
        if self.cached_pcm_bytes().saturating_add(add) > self.limits.cache_bytes {
            return false;
        }
        self.cache.insert(key, pcm);
        true
    }

    fn promote(&mut self) {
        let ready: Vec<(u64, Pcm)> =
            self.with(|m| m.voices.iter().filter_map(|v| v.live().and_then(|l| l.finished_clip()).map(|p| (v.media_key, p))).collect());
        for (key, pcm) in ready {
            let pcm = Arc::new(pcm);
            if !self.store_cache(key, Arc::clone(&pcm)) {
                continue;
            }
            self.with(|m| {
                for v in &mut m.voices {
                    if v.media_key != key {
                        continue;
                    }
                    match v.audio.take() {
                        Some(Audio::Live(l)) if l.finished_clip().is_some() => {
                            l.cancel();
                            v.audio = Some(Audio::Full(Arc::clone(&pcm)));
                        }
                        other => v.audio = other,
                    }
                }
            });
        }
    }

    fn ensure_stream(&mut self) {
        if self.streaming {
            return;
        }
        let Some(out) = self.out.as_mut() else { return };
        let m = Arc::clone(&self.mixer);
        let fill = Box::new(move |buf: &mut [f32], channels: usize, rate: u32| match m.lock() {
            Ok(mut g) => {
                let frames = buf.len() / channels.max(1);
                g.mix(Some(buf), frames, channels, rate)
            }
            Err(_) => buf.fill(0.0),
        });
        match out.start(fill) {
            Ok(()) => self.streaming = true,
            Err(e) => {
                log::warn!("audio output unavailable ({e}); media plays silently");
                self.out = None;
            }
        }
    }

    pub fn pause(&mut self, key: u64) {
        self.with(|m| {
            if let Some(v) = m.voices.iter_mut().find(|v| v.key == key && v.state == PlayState::Playing) {
                v.state = PlayState::Paused;
            }
        });
    }

    /// Remove voice `key` and cancel a decode nothing else is waiting on.
    pub fn stop(&mut self, key: u64) {
        let audio = self.with(|m| m.voices.iter().position(|v| v.key == key).map(|i| m.voices.remove(i).audio));
        if let Some(Some(a)) = audio {
            self.release(a);
        }
    }

    pub fn stop_all(&mut self) {
        let audios = self.with(|m| m.voices.drain(..).filter_map(|v| v.audio).collect::<Vec<_>>());
        for a in audios {
            self.release(a);
        }
    }

    /// Move voice `key` to `t` seconds (clamped to its played range).
    pub fn seek(&mut self, key: u64, t: f64) {
        self.with(|m| {
            if let Some(v) = m.voices.iter_mut().find(|v| v.key == key) {
                let (s, e) = v.spec.clip.range(v.duration());
                v.t = t.clamp(s, if e.is_finite() { e } else { f64::MAX });
                if v.state == PlayState::Ended {
                    v.state = PlayState::Paused;
                }
                if let Some(live) = v.live().cloned() {
                    let frame = frames_at(v.t, live.rate());
                    live.set_playhead(frame);
                    if live.behind(frame) {
                        live.request_restart();
                    } else {
                        live.kick();
                    }
                }
            }
        });
    }

    pub fn status(&self, key: u64) -> Option<VoiceStatus> {
        self.with(|m| {
            m.voices.iter().find(|v| v.key == key).map(|v| VoiceStatus {
                state: if v.state == PlayState::Playing && v.loading() { PlayState::Loading } else { v.state },
                position: v.t,
                duration: v.duration(),
                error: v.error_text(),
            })
        })
    }

    /// Keys of all voices (playing, paused or ended).
    pub fn keys(&self) -> Vec<u64> {
        self.with(|m| m.voices.iter().map(|v| v.key).collect())
    }

    pub fn any_playing(&self) -> bool {
        self.with(|m| m.voices.iter().any(|v| v.state == PlayState::Playing))
    }

    /// Call every frame with the host's clock (seconds): runs the voices' clocks when there is no
    /// audio device, and releases the device after a few idle seconds.
    pub fn tick(&mut self, now: f64) {
        #[cfg(not(target_arch = "wasm32"))]
        if self.limits.threaded {
            self.workers.retain(|h| !h.is_finished());
        }
        if !self.limits.threaded {
            let _ = self.pump(self.limits.max_jobs.max(1));
        }
        self.promote();
        let dt = self.last_tick.map(|l| (now - l).clamp(0.0, 0.25)).unwrap_or(0.0);
        self.last_tick = Some(now);
        if !self.streaming && dt > 0.0 {
            let frames = (dt * DEFAULT_RATE as f64).round() as usize;
            self.with(|m| m.mix(None, frames, 2, DEFAULT_RATE));
        }
        if self.streaming {
            if self.any_playing() {
                self.idle_since = None;
            } else if now - *self.idle_since.get_or_insert(now) > 3.0 {
                if let Some(o) = self.out.as_mut() {
                    o.stop();
                }
                self.streaming = false;
                self.idle_since = None;
            }
        }
    }

    fn settled(&self, key: u64) -> bool {
        self.with(|m| {
            let Some(v) = m.voices.iter().find(|v| v.key == key) else { return true };
            match &v.audio {
                None => true,
                Some(Audio::Full(_)) => true,
                Some(Audio::Live(l)) => l.ready(self.limits.ahead_bytes),
            }
        })
    }

    /// Block until voice `key`'s audio has decoded far enough to play (tests, offline rendering).
    pub fn wait_loaded(&self, key: u64, timeout: std::time::Duration) -> bool {
        let t0 = web_now();
        loop {
            if self.settled(key) {
                return true;
            }
            if web_now() - t0 > timeout.as_secs_f64() || !crate::THREADS {
                return self.settled(key);
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Mix into `buf` directly (offline rendering and tests; the device callback does the same).
    pub fn render(&self, buf: &mut [f32], channels: usize, rate: u32) {
        let frames = buf.len() / channels.max(1);
        self.with(|m| m.mix(Some(buf), frames, channels, rate));
    }

    fn cancel_lives(&mut self) {
        let lives = self.with(|m| m.voices.iter().filter_map(|v| v.live().cloned()).collect::<Vec<_>>());
        for l in lives {
            l.cancel();
        }
    }

    #[cfg(test)]
    fn retained_pcm_bytes(&self) -> usize {
        let parts: Vec<(usize, Option<usize>)> = self.with(|m| {
            m.voices
                .iter()
                .map(|v| match &v.audio {
                    Some(Audio::Live(l)) => (l.window_bytes(), None),
                    Some(Audio::Full(p)) => (pcm_bytes(p), Some(Arc::as_ptr(p) as usize)),
                    None => (0, None),
                })
                .collect()
        });
        let mut cached = Vec::new();
        let mut n = 0usize;
        for p in self.cache.values() {
            n = n.saturating_add(pcm_bytes(p));
            cached.push(Arc::as_ptr(p) as usize);
        }
        for (bytes, ptr) in parts {
            match ptr {
                Some(p) if cached.contains(&p) => {}
                _ => n = n.saturating_add(bytes),
            }
        }
        n
    }

    #[cfg(test)]
    fn stream_open(&self, key: u64) -> Option<(usize, bool)> {
        self.with(|m| {
            m.voices.iter().find(|v| v.key == key).and_then(|v| {
                let l = v.live()?;
                Some((l.window_bytes(), l.is_eof()))
            })
        })
    }
}

/// Seconds on a monotonic clock where there is one (std's Instant panics on wasm32).
fn web_now() -> f64 {
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::sync::OnceLock;
        static T0: OnceLock<std::time::Instant> = OnceLock::new();
        T0.get_or_init(std::time::Instant::now).elapsed().as_secs_f64()
    }
    #[cfg(target_arch = "wasm32")]
    {
        0.0
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        if let Some(o) = self.out.as_mut() {
            o.stop();
        }
        self.cancel_lives();
        #[cfg(not(target_arch = "wasm32"))]
        for h in self.workers.drain(..) {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, AtomicUsize};

    fn spec(duration: f64, looping: bool) -> VoiceSpec {
        VoiceSpec { clip: ClipParams::default(), looping, duration, audio: false }
    }

    fn audible(duration: f64) -> VoiceSpec {
        VoiceSpec { clip: ClipParams::default(), looping: false, duration, audio: true }
    }

    fn inline(ahead_bytes: usize, cache_bytes: usize, max_jobs: usize, pull_frames: usize) -> Limits {
        Limits { cache_bytes, ahead_bytes, max_jobs, pull_frames, threaded: false }
    }

    fn until_idle(p: &mut Player) {
        for _ in 0..20_000 {
            if !p.service() {
                return;
            }
        }
        panic!("decode did not settle");
    }

    struct Synth {
        left: u64,
        rate: u32,
        channels: u16,
        pulled: Arc<AtomicU64>,
        entered: Arc<AtomicUsize>,
        max_inside: Arc<AtomicUsize>,
        /// When set, `pull` waits until this is true or `cancel` is set, and counts a pull only
        /// if it was released rather than cancelled.
        release: Option<Arc<AtomicBool>>,
    }

    impl Synth {
        fn endless(pulled: Arc<AtomicU64>) -> Synth {
            Synth {
                left: 1_000_000,
                rate: 1_000,
                channels: 1,
                pulled,
                entered: Arc::new(AtomicUsize::new(0)),
                max_inside: Arc::new(AtomicUsize::new(0)),
                release: None,
            }
        }
    }

    impl PcmSource for Synth {
        fn pull(&mut self, max_frames: usize, cancel: &AtomicBool) -> crate::Result<SourcePull> {
            let inside = self.entered.fetch_add(1, Ordering::AcqRel) + 1;
            self.max_inside.fetch_max(inside, Ordering::Relaxed);
            let result = self.pull_inner(max_frames, cancel);
            self.entered.fetch_sub(1, Ordering::AcqRel);
            result
        }

        fn restart(&mut self) -> crate::Result<()> {
            Ok(())
        }
    }

    impl Synth {
        fn pull_inner(&mut self, max_frames: usize, cancel: &AtomicBool) -> crate::Result<SourcePull> {
            if let Some(release) = &self.release {
                while !cancel.load(Ordering::Acquire) && !release.load(Ordering::Acquire) {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                if cancel.load(Ordering::Acquire) {
                    return Ok(SourcePull::Cancelled);
                }
            } else if cancel.load(Ordering::Acquire) {
                return Ok(SourcePull::Cancelled);
            }
            if self.left == 0 {
                return Ok(SourcePull::Eof);
            }
            let n = (max_frames as u64).min(self.left) as usize;
            self.left -= n as u64;
            self.pulled.fetch_add(n as u64, Ordering::Relaxed);
            let ch = usize::from(self.channels.max(1));
            Ok(SourcePull::Frames { rate: self.rate, channels: self.channels, samples: vec![0.25; n.saturating_mul(ch)] })
        }
    }

    #[test]
    fn wall_clock_advances_pauses_and_seeks() {
        let mut p = Player::new(None);
        let b: Bytes = Arc::new(vec![]);
        p.play(1, 1, &b, spec(2.0, false), None);
        p.tick(10.0);
        p.tick(10.2);
        p.tick(10.4);
        let s = p.status(1).map(|s| s.position).unwrap_or(-1.0);
        assert!((s - 0.4).abs() < 1e-3, "{s}");
        p.pause(1);
        p.tick(10.6);
        assert!((p.status(1).map(|s| s.position).unwrap_or(-1.0) - 0.4).abs() < 1e-3);
        assert_eq!(p.status(1).map(|s| s.state), Some(PlayState::Paused));
        p.seek(1, 1.5);
        p.play(1, 1, &b, spec(2.0, false), None);
        p.tick(10.8);
        p.tick(11.0);
        p.tick(11.2);
        p.tick(11.4);
        let st = p.status(1);
        assert_eq!(st.as_ref().map(|s| s.state), Some(PlayState::Ended));
        assert_eq!(st.map(|s| s.position), Some(2.0));
        // Playing an ended voice starts over.
        p.play(1, 1, &b, spec(2.0, false), None);
        assert_eq!(p.status(1).map(|s| s.position), Some(0.0));
    }

    #[test]
    fn looping_wraps_within_the_trimmed_range() {
        let mut p = Player::new(None);
        let b: Bytes = Arc::new(vec![]);
        let mut s = spec(1.0, true);
        s.clip.trim_start = 0.5;
        p.play(7, 7, &b, s, None);
        p.tick(0.0);
        for i in 1..=4 {
            p.tick(i as f64 * 0.2);
        }
        // 0.8 s played in a 0.5 s loop starting at 0.5: 0.5 + (0.8 mod 0.5) = 0.8.
        let pos = p.status(7).map(|s| s.position).unwrap_or(-1.0);
        assert!((pos - 0.8).abs() < 1e-3, "{pos}");
        assert_eq!(p.status(7).map(|s| s.state), Some(PlayState::Playing));
    }

    #[test]
    fn mixer_applies_volume_fade_and_channel_mapping() {
        let pcm = Arc::new(Pcm { rate: 10, channels: 1, samples: vec![1.0; 10] });
        let clip = ClipParams { fade_in: 0.5, volume: 0.5, ..Default::default() };
        let mut m = Mixer {
            voices: vec![Voice {
                key: 1,
                media_key: 1,
                audio: Some(Audio::Full(pcm)),
                spec: VoiceSpec { clip, looping: false, duration: 1.0, audio: true },
                t: 0.0,
                state: PlayState::Playing,
            }],
        };
        let mut buf = vec![0.0f32; 2 * 12];
        m.mix(Some(&mut buf), 12, 2, 10);
        // Frame 0: fade starts at 0; frame 3: 0.3/0.5 × 0.5 = 0.3; both channels carry the mono source.
        assert_eq!(buf[0], 0.0);
        assert!((buf[6] - 0.3).abs() < 1e-5 && (buf[7] - 0.3).abs() < 1e-5, "{buf:?}");
        assert!((buf[16] - 0.5).abs() < 1e-5);
        // Past the end: silence, and the voice has ended.
        assert_eq!(buf[22], 0.0);
        assert_eq!(m.voices[0].state, PlayState::Ended);
    }

    #[test]
    fn device_clock_drives_position() {
        struct Fake(Arc<Mutex<Option<Box<dyn FnMut(&mut [f32], usize, u32) + Send>>>>);
        impl AudioOut for Fake {
            fn start(&mut self, fill: Box<dyn FnMut(&mut [f32], usize, u32) + Send>) -> Result<(), String> {
                *lock(&self.0) = Some(fill);
                Ok(())
            }
            fn stop(&mut self) {}
        }
        let cb = Arc::new(Mutex::new(None));
        let mut p = Player::new(Some(Box::new(Fake(Arc::clone(&cb)))));
        let b: Bytes = Arc::new(vec![]);
        p.play(3, 3, &b, spec(10.0, false), None);
        // Wall time alone doesn't move a device-clocked voice…
        p.tick(0.0);
        p.tick(0.2);
        assert_eq!(p.status(3).map(|s| s.position), Some(0.0));
        // …the device pulling 4800 frames at 48 kHz does.
        let mut buf = vec![0.0; 4800 * 2];
        if let Some(f) = lock(&cb).take().as_mut() {
            f(&mut buf, 2, 48_000);
        }
        let pos = p.status(3).map(|s| s.position).unwrap_or(-1.0);
        assert!((pos - 0.1).abs() < 1e-6, "{pos}");
    }

    #[test]
    fn long_stream_stays_inside_the_pcm_budget() {
        let pulled = Arc::new(AtomicU64::new(0));
        let mut p = Player::with_limits(None, inline(256, 256, 1, 32));
        p.play_source(1, 1, Box::new(Synth::endless(Arc::clone(&pulled))), audible(1_000.0), None);
        until_idle(&mut p);
        let n = pulled.load(Ordering::Relaxed);
        assert!(n > 0 && n <= 64, "decoded {n} frames, budget is 64");
        assert!(p.retained_pcm_bytes() <= 256, "retained {}", p.retained_pcm_bytes());
        let (bytes, eof) = p.stream_open(1).unwrap_or((0, true));
        assert!(!eof, "a million-frame source must not be finished");
        assert!(bytes <= 256 && bytes > 0, "{bytes}");
        assert!(!p.service());
        assert_eq!(pulled.load(Ordering::Relaxed), n);
        // Moving the clock slides the window; it does not decode the rest of the file.
        p.tick(0.0);
        p.tick(0.05);
        until_idle(&mut p);
        let n2 = pulled.load(Ordering::Relaxed);
        assert!(n2 > n && n2 < 10_000, "{n} -> {n2}");
        assert!(p.retained_pcm_bytes() <= 256, "retained {}", p.retained_pcm_bytes());
    }

    #[test]
    fn stop_and_drop_cancel_further_decode() {
        let pulled = Arc::new(AtomicU64::new(0));
        let mut p = Player::with_limits(None, inline(256, 256, 1, 32));
        p.play_source(1, 1, Box::new(Synth::endless(Arc::clone(&pulled))), audible(1_000.0), None);
        assert!(p.service());
        let n = pulled.load(Ordering::Relaxed);
        assert!(n > 0);
        p.stop(1);
        assert!(!p.service());
        assert_eq!(pulled.load(Ordering::Relaxed), n);
        assert_eq!(p.retained_pcm_bytes(), 0);

        p.play_source(2, 2, Box::new(Synth::endless(Arc::clone(&pulled))), audible(1_000.0), None);
        assert!(p.service());
        let n = pulled.load(Ordering::Relaxed);
        drop(p);
        assert_eq!(pulled.load(Ordering::Relaxed), n);
    }

    #[test]
    fn one_pump_starts_only_the_job_budget() {
        let a = Arc::new(AtomicU64::new(0));
        let b = Arc::new(AtomicU64::new(0));
        let mut p = Player::with_limits(None, inline(256, 256, 1, 32));
        p.play_source(1, 1, Box::new(Synth::endless(Arc::clone(&a))), audible(1_000.0), None);
        // `play_source` already pulled one chunk for the first voice.
        let first = a.load(Ordering::Relaxed);
        assert!(first > 0);
        p.play_source(2, 2, Box::new(Synth::endless(Arc::clone(&b))), audible(1_000.0), None);
        // The second start is the one job this call is allowed to run.
        assert_eq!(a.load(Ordering::Relaxed), first);
        assert!(b.load(Ordering::Relaxed) > 0);
        let before_a = a.load(Ordering::Relaxed);
        let before_b = b.load(Ordering::Relaxed);
        assert!(p.service());
        let advanced = (a.load(Ordering::Relaxed) > before_a) as u8 + (b.load(Ordering::Relaxed) > before_b) as u8;
        assert_eq!(advanced, 1, "a {} -> {}, b {} -> {}", before_a, a.load(Ordering::Relaxed), before_b, b.load(Ordering::Relaxed));
    }

    #[test]
    fn repeated_starts_do_not_accumulate_pcm() {
        let pulled = Arc::new(AtomicU64::new(0));
        let mut p = Player::with_limits(None, inline(256, 256, 1, 32));
        for _ in 0..30 {
            p.play_source(1, 1, Box::new(Synth::endless(Arc::clone(&pulled))), audible(1_000.0), None);
            let _ = p.service();
            assert!(p.retained_pcm_bytes() <= 256, "retained {}", p.retained_pcm_bytes());
            p.stop(1);
            assert_eq!(p.retained_pcm_bytes(), 0);
        }
        assert!(pulled.load(Ordering::Relaxed) < 30 * 200, "pulled {}", pulled.load(Ordering::Relaxed));
    }

    #[test]
    fn cache_keeps_only_a_byte_budget_and_not_a_playing_clip() {
        fn short(n: u64) -> Synth {
            Synth {
                left: n,
                rate: 1_000,
                channels: 1,
                pulled: Arc::new(AtomicU64::new(0)),
                entered: Arc::new(AtomicUsize::new(0)),
                max_inside: Arc::new(AtomicUsize::new(0)),
                release: None,
            }
        }
        // 40 mono frames = 160 bytes. Three of them do not fit in 400 bytes.
        let mut p = Player::with_limits(None, inline(2_000, 400, 2, 64));
        for key in 1..=3 {
            p.play_source(key, key, Box::new(short(40)), audible(0.04), None);
            until_idle(&mut p);
            p.stop(key);
        }
        let cached = p.cached_pcm_bytes();
        assert!((160..=400).contains(&cached), "cached {cached}");
        assert_ne!(cached, 480);

        let mut p = Player::with_limits(None, inline(2_000, 400, 2, 64));
        p.play_source(1, 1, Box::new(short(40)), audible(0.04), None);
        p.play_source(2, 2, Box::new(short(40)), audible(0.04), None);
        until_idle(&mut p);
        assert_eq!(p.cached_pcm_bytes(), 320);
        p.play_source(3, 3, Box::new(short(40)), audible(0.04), None);
        until_idle(&mut p);
        // The two playing clips are in use, so the third is not stored over them.
        assert_eq!(p.cached_pcm_bytes(), 320);
        assert!(p.status(1).is_some_and(|s| s.error.is_none()));
        assert!(p.status(2).is_some_and(|s| s.error.is_none()));
    }

    fn wav_mono(rate: u32, samples: &[f32]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes()).collect();
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

    #[test]
    fn a_long_wav_is_not_decoded_to_the_end() {
        let samples: Vec<f32> = (0..8_000).map(|i| ((i as f32) * 0.01).sin() * 0.2).collect();
        let bytes: Bytes = Arc::new(wav_mono(8_000, &samples));
        let mut p = Player::with_limits(None, inline(400, 400, 1, 50));
        p.play(1, 1, &bytes, audible(1.0), None);
        until_idle(&mut p);
        let (window, eof) = p.stream_open(1).unwrap_or((usize::MAX, true));
        assert!(!eof, "one second of audio must not be exhausted under a 400-byte window");
        assert!(window <= 400 && window > 0, "{window}");
        assert!(p.retained_pcm_bytes() <= 400, "retained {}", p.retained_pcm_bytes());
    }

    #[test]
    fn a_short_wav_still_reaches_the_mixer() {
        let samples: Vec<f32> = (0..400).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
        let bytes: Bytes = Arc::new(wav_mono(8_000, &samples));
        let mut p = Player::with_limits(None, inline(64 * 1024, 64 * 1024, 1, 256));
        p.play(1, 1, &bytes, audible(0.05), None);
        until_idle(&mut p);
        let mut buf = vec![0.0f32; 100 * 2];
        p.render(&mut buf, 2, 8_000);
        let rms = (buf.iter().map(|s| s * s).sum::<f32>() / buf.len() as f32).sqrt();
        assert!(rms > 0.05, "{rms}");
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn a_blocked_decode_stops_when_the_voice_does() {
        let release = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(AtomicUsize::new(0));
        let max_inside = Arc::new(AtomicUsize::new(0));
        let pulled = Arc::new(AtomicU64::new(0));
        let mk = |pulled: Arc<AtomicU64>, entered: Arc<AtomicUsize>, release: Arc<AtomicBool>| Synth {
            left: 1_000_000,
            rate: 1_000,
            channels: 1,
            pulled,
            entered,
            max_inside: Arc::clone(&max_inside),
            release: Some(release),
        };
        let entered2 = Arc::new(AtomicUsize::new(0));
        let pulled2 = Arc::new(AtomicU64::new(0));
        let mut p = Player::with_limits(None, Limits { cache_bytes: 256, ahead_bytes: 256, max_jobs: 1, pull_frames: 32, threaded: true });
        p.play_source(1, 1, Box::new(mk(Arc::clone(&pulled), Arc::clone(&entered), Arc::clone(&release))), audible(1_000.0), None);
        p.play_source(2, 2, Box::new(mk(Arc::clone(&pulled2), Arc::clone(&entered2), Arc::clone(&release))), audible(1_000.0), None);
        let start = std::time::Instant::now();
        while entered.load(Ordering::Relaxed) == 0 && start.elapsed() < std::time::Duration::from_secs(2) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        std::thread::sleep(std::time::Duration::from_millis(80));
        assert_eq!(entered.load(Ordering::Relaxed), 1, "the second decode must wait for a job slot");
        assert_eq!(entered2.load(Ordering::Relaxed), 0);
        assert_eq!(pulled.load(Ordering::Relaxed), 0, "cancel must win before samples are kept");
        drop(p);
        assert_eq!(pulled.load(Ordering::Relaxed), 0);
        assert_eq!(pulled2.load(Ordering::Relaxed), 0);
        assert!(max_inside.load(Ordering::Relaxed) <= 1);
    }
}
