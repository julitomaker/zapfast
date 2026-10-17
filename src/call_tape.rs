//! Recording a call made in Whats Pro from inside the call engine: our microphone as the call
//! sends it, and the other side (one person, or a group's mix) as the call plays it. Both are
//! written to one stereo WAV at the engine's 16 kHz, us on the left and them on the right.
//!
//! Tapping the engine rather than the computer's sound keeps the other voice even with
//! headphones, needs no screen recording permission, and works the same on every system.
//!
//! The two pumps that feed a [`Tape`] run on the call's own tasks, so handing a frame over never
//! blocks: it is queued for a writer thread, and dropped if that queue is full. The writer lines
//! the two sides up by the wall clock (the internal `Timeline`), so a gap on one side becomes silence on
//! that side instead of shifting the other.
//!
//! The file is written as `<path>.part` and renamed when the recording stops. Its header is
//! patched every few seconds, so a crash or a power cut leaves a file [`recover_part`] can fix.

use std::{
    collections::VecDeque,
    fs::File,
    io::{BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, Weak,
        atomic::{AtomicBool, AtomicU16, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    },
    time::{Duration, Instant},
};

/// The call engine's rate, which the recording keeps.
pub const RATE: u32 = 16_000;
/// Why a second call cannot start recording; the interface translates it.
pub const BUSY: &str = "Another call is already being recorded";
/// Why the same call cannot start recording twice.
pub const ALREADY: &str = "This call is already being recorded";

/// How many frames may wait for the writer before new ones are dropped: a few seconds of both
/// sides, far more than a writer that is keeping up ever holds.
const QUEUE: usize = 256;
/// How long the writer waits for late frames before writing a stretch of the file, 400 ms in
/// samples. More than either side's frames take to arrive, so neither is padded with silence it
/// was about to get.
const DELAY: u64 = RATE as u64 * 400 / 1000;
/// How often the writer writes what is due.
const TICK: Duration = Duration::from_millis(100);
/// How often the header is brought up to date and the file flushed, which bounds what a crash
/// loses; and how often it is also synced to the disk.
const PATCH_EVERY: Duration = Duration::from_secs(5);
const SYNC_EVERY: Duration = Duration::from_secs(30);
/// How long [`Tape::stop`] waits for the writer to close the file.
const CLOSE_WAIT: Duration = Duration::from_secs(2);
/// How much a side may hold before its oldest samples are dropped: one second.
const CAP: usize = RATE as usize;
/// How far a side's frame may start after where the side is up to before the difference is
/// filled with silence. Bigger than the jitter of either side's frames.
const SLACK: u64 = RATE as u64 / 10;
/// The size of the WAV header written here: RIFF, a 16-byte fmt chunk, then data.
const HEADER: u64 = 44;
/// Bytes per stereo sample.
const BLOCK: u64 = 4;
/// How much the live level meter falls each tick.
const DECAY: f32 = 0.75;

/// One side of the call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// Our microphone: the left channel.
    Me = 0,
    /// The other side, or the group's mix: the right channel.
    Them = 1,
}

/// Where the recording notice is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeState {
    /// None was asked for.
    Off,
    /// It is waiting for the microphone's next frames, or playing.
    Pending,
    /// We are muted: it plays when we unmute, because a muted call sends silence.
    Held,
    /// It has played in full.
    Played,
}

/// What the call screen shows of a recording in progress.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordingView {
    pub since: Instant,
    pub notice: NoticeState,
    /// Why writing stopped, if the disk failed. The call goes on.
    pub failed: Option<String>,
}

/// A recording that stopped.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    pub generation: u64,
    /// The WAV, or the `.part` if it could not be renamed.
    pub path: PathBuf,
    pub seconds: u64,
    /// The average loudness of each side, 0 to 1: a right side near zero means the other voice
    /// was not heard.
    pub levels: [f32; 2],
    /// Where in the recording the notice started, in milliseconds.
    pub notice_ms: Option<u64>,
    pub error: Option<String>,
}

/// The recording tap of one call. Clones share it.
#[derive(Clone)]
pub struct Tape(Arc<Inner>);

struct Inner {
    generation: u64,
    muted: AtomicBool,
    live: Mutex<Option<Live>>,
}

/// A recording in progress.
struct Live {
    since: Instant,
    path: PathBuf,
    frames: SyncSender<Frame>,
    meter: Arc<Meter>,
    notice: Notice,
    /// Frames the writer had no room for.
    dropped: u64,
    /// Tells the writer to delete the file instead of keeping it.
    discarded: Arc<AtomicBool>,
    done: Receiver<Outcome>,
}

/// The recording notice still to mix into the microphone.
struct Notice {
    tone: Vec<i16>,
    played: usize,
    at_ms: Option<u64>,
}

/// What the writer shares with the taps and the interface.
#[derive(Default)]
struct Meter {
    /// Each side's level now, 0 to 1000.
    levels: [AtomicU16; 2],
    failed: AtomicBool,
    reason: Mutex<Option<String>>,
}

impl Meter {
    fn fail(&self, reason: String) {
        log::warn!("call recording stopped writing: {reason}");
        lock(&self.reason).get_or_insert(reason);
        self.failed.store(true, Ordering::Release);
    }
}

struct Frame {
    side: Side,
    at: Instant,
    samples: Vec<i16>,
}

/// What the writer reports once the file is closed.
struct Outcome {
    path: PathBuf,
    seconds: u64,
    levels: [f32; 2],
    error: Option<String>,
}

/// The call being recorded, if any: one at a time in the whole process, so two accounts' calls
/// never both claim the microphone's recording. Weak, so a tape that is dropped while recording
/// closes its file instead of holding the place forever.
static LIVE: Mutex<Option<(u64, Weak<Inner>)>> = Mutex::new(None);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

impl Tape {
    /// The tap of the call `generation`, not recording yet.
    pub fn new(generation: u64) -> Self {
        Self(Arc::new(Inner {
            generation,
            muted: AtomicBool::new(false),
            live: Mutex::new(None),
        }))
    }

    pub fn generation(&self) -> u64 {
        self.0.generation
    }

    /// A frame of our microphone, on its way to the call. Mixes in what is left of the notice
    /// unless we are muted, so the other side hears it, then keeps a copy: silence while muted,
    /// which is what the other side gets.
    pub fn me(&self, frame: &mut [i16]) {
        let mut live = lock(&self.0.live);
        let Some(live) = live.as_mut().filter(|live| !live.broken()) else {
            return;
        };
        let at = Instant::now();
        let samples = if self.0.muted.load(Ordering::Acquire) {
            vec![0; frame.len()]
        } else {
            live.mix_notice(frame, at);
            frame.to_vec()
        };
        live.send(Frame {
            side: Side::Me,
            at,
            samples,
        });
    }

    /// A frame of the other side as the call plays it.
    pub fn them(&self, frame: &[i16]) {
        let mut live = lock(&self.0.live);
        if let Some(live) = live.as_mut().filter(|live| !live.broken()) {
            live.send(Frame {
                side: Side::Them,
                at: Instant::now(),
                samples: frame.to_vec(),
            });
        }
    }

    /// Follows the call's mute. Set it before muting the call and after unmuting it, so no
    /// voice the other side did not hear reaches the recording.
    pub fn set_muted(&self, muted: bool) {
        self.0.muted.store(muted, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn is_muted(&self) -> bool {
        self.0.muted.load(Ordering::Acquire)
    }

    /// Starts writing the call to `path`, a `.wav`, with the notice first if `notice`.
    pub fn start(&self, path: &Path, notice: bool) -> Result<(), String> {
        let mut current = lock(&LIVE);
        if let Some((generation, inner)) = current.as_ref()
            && inner.strong_count() > 0
        {
            return Err(if *generation == self.0.generation {
                ALREADY
            } else {
                BUSY
            }
            .to_owned());
        }
        let mut live = lock(&self.0.live);
        if live.is_some() {
            return Err(ALREADY.to_owned());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let part = part_path(path);
        let mut file = File::create(&part).map_err(|error| error.to_string())?;
        if let Err(error) = file.write_all(&header(0)) {
            drop(file);
            let _ = std::fs::remove_file(&part);
            return Err(error.to_string());
        }
        let since = Instant::now();
        let meter = Arc::new(Meter::default());
        let discarded = Arc::new(AtomicBool::new(false));
        let (frames, received) = mpsc::sync_channel(QUEUE);
        let (finished, done) = mpsc::channel();
        let writer = Writer {
            file: Some(BufWriter::with_capacity(1 << 16, file)),
            since,
            timeline: Timeline::default(),
            meter: Arc::clone(&meter),
            written: 0,
            peaks: [0.0; 2],
            sums: [0.0; 2],
            counts: [0; 2],
            error: None,
        };
        let (wav, keep) = (path.to_owned(), Arc::clone(&discarded));
        let spawned = std::thread::Builder::new()
            .name("call-tape".into())
            .spawn(move || {
                let outcome = writer.run(received, part, wav, &keep);
                let _ = finished.send(outcome);
            });
        if let Err(error) = spawned {
            let _ = std::fs::remove_file(part_path(path));
            return Err(error.to_string());
        }
        *live = Some(Live {
            since,
            path: path.to_owned(),
            frames,
            meter,
            notice: Notice {
                tone: if notice {
                    crate::call_tones::recording_notice()
                } else {
                    Vec::new()
                },
                played: 0,
                at_ms: None,
            },
            dropped: 0,
            discarded,
            done,
        });
        *current = Some((self.0.generation, Arc::downgrade(&self.0)));
        Ok(())
    }

    /// Stops recording: closes the file, brings its header up to date and renames it to the
    /// `.wav`. Waits up to two seconds for that. None if it was not recording, so calling it
    /// twice is harmless.
    pub fn stop(&self) -> Option<Report> {
        self.finish(false)
    }

    /// Stops recording and deletes what was recorded.
    pub fn discard(&self) {
        if let Some(report) = self.finish(true) {
            let _ = std::fs::remove_file(&report.path);
            let _ = std::fs::remove_file(part_path(&report.path));
        }
    }

    /// The recording in progress, if any.
    pub fn view(&self) -> Option<RecordingView> {
        let live = lock(&self.0.live);
        let live = live.as_ref()?;
        Some(RecordingView {
            since: live.since,
            notice: live.notice.state(self.0.muted.load(Ordering::Acquire)),
            failed: lock(&live.meter.reason).clone(),
        })
    }

    fn finish(&self, discard: bool) -> Option<Report> {
        let live = lock(&self.0.live).take()?;
        {
            let mut current = lock(&LIVE);
            if current
                .as_ref()
                .is_some_and(|(_, inner)| std::ptr::eq(inner.as_ptr(), Arc::as_ptr(&self.0)))
            {
                *current = None;
            }
        }
        let Live {
            since,
            path,
            frames,
            meter,
            notice,
            dropped,
            discarded,
            done,
        } = live;
        if dropped > 0 {
            log::warn!("call recording dropped {dropped} frames the writer had no room for");
        }
        discarded.store(discard, Ordering::Release);
        // Closing the queue is what tells the writer to finish.
        drop(frames);
        let report = match done.recv_timeout(CLOSE_WAIT) {
            Ok(outcome) => Report {
                generation: self.0.generation,
                path: outcome.path,
                seconds: outcome.seconds,
                levels: outcome.levels,
                notice_ms: notice.at_ms,
                error: outcome.error,
            },
            Err(_) => {
                log::warn!("the call recording's writer did not finish in time");
                Report {
                    generation: self.0.generation,
                    path,
                    seconds: since.elapsed().as_secs(),
                    levels: meter
                        .levels
                        .each_ref()
                        .map(|level| f32::from(level.load(Ordering::Relaxed)) / 1000.0),
                    notice_ms: notice.at_ms,
                    error: Some("the recording is still being written".to_owned()),
                }
            }
        };
        Some(report)
    }
}

impl Live {
    fn broken(&self) -> bool {
        self.meter.failed.load(Ordering::Acquire)
    }

    fn send(&mut self, frame: Frame) {
        match self.frames.try_send(frame) {
            Ok(()) | Err(TrySendError::Disconnected(_)) => {}
            Err(TrySendError::Full(_)) => self.dropped += 1,
        }
    }

    fn mix_notice(&mut self, frame: &mut [i16], at: Instant) {
        let notice = &mut self.notice;
        let rest = &notice.tone[notice.played.min(notice.tone.len())..];
        if rest.is_empty() || frame.is_empty() {
            return;
        }
        if notice.at_ms.is_none() {
            // The frame is the sound up to now, so it starts a frame's length before it.
            let start = position(self.since, at).saturating_sub(frame.len() as u64);
            notice.at_ms = Some(start * 1000 / u64::from(RATE));
        }
        for (sample, tone) in frame.iter_mut().zip(rest) {
            *sample = sample.saturating_add(*tone);
        }
        notice.played += frame.len().min(rest.len());
    }
}

impl Notice {
    fn state(&self, muted: bool) -> NoticeState {
        if self.tone.is_empty() {
            NoticeState::Off
        } else if self.played >= self.tone.len() {
            NoticeState::Played
        } else if muted {
            NoticeState::Held
        } else {
            NoticeState::Pending
        }
    }
}

/// The live level of each side of the call `generation`, 0 to 1, while it is recorded.
pub fn live_levels(generation: u64) -> Option<[f32; 2]> {
    let inner = lock(&LIVE)
        .as_ref()
        .filter(|(live, _)| *live == generation)?
        .1
        .upgrade()?;
    let live = lock(&inner.live);
    let meter = &live.as_ref()?.meter;
    Some(
        meter
            .levels
            .each_ref()
            .map(|level| f32::from(level.load(Ordering::Relaxed)) / 1000.0),
    )
}

/// Fixes the header of a `<name>.wav.part` left by a crash from the file's length, and renames
/// it to `<name>.wav`, which it returns.
pub fn recover_part(path: &Path) -> std::io::Result<PathBuf> {
    use std::io::{Error, ErrorKind};
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".part"))
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "not a .part file"))?;
    let wav = path.with_file_name(name);
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    let mut start = [0u8; 12];
    file.read_exact(&mut start)?;
    if &start[..4] != b"RIFF" || &start[8..] != b"WAVE" || file.metadata()?.len() < HEADER {
        return Err(Error::new(ErrorKind::InvalidData, "not a recording"));
    }
    // A sample cut in half by the crash is dropped.
    let data = (file.metadata()?.len() - HEADER) / BLOCK * BLOCK;
    file.set_len(HEADER + data)?;
    patch(&mut file, data)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(path, &wav)?;
    Ok(wav)
}

/// Where the recording to `path` is written until it stops.
fn part_path(path: &Path) -> PathBuf {
    let mut part = path.as_os_str().to_owned();
    part.push(".part");
    PathBuf::from(part)
}

/// A header for `data` bytes of stereo 16-bit samples at [`RATE`].
fn header(data: u32) -> [u8; HEADER as usize] {
    let mut header = [0u8; HEADER as usize];
    let fields: [&[u8]; 12] = [
        b"RIFF",
        &data.saturating_add(36).to_le_bytes(),
        b"WAVEfmt ",
        &16u32.to_le_bytes(),
        &1u16.to_le_bytes(),
        &2u16.to_le_bytes(),
        &RATE.to_le_bytes(),
        &(RATE * BLOCK as u32).to_le_bytes(),
        &(BLOCK as u16).to_le_bytes(),
        &16u16.to_le_bytes(),
        b"data",
        &data.to_le_bytes(),
    ];
    let mut at = 0;
    for field in fields {
        header[at..at + field.len()].copy_from_slice(field);
        at += field.len();
    }
    header
}

/// Writes the sizes for `data` bytes into the header of `file` and goes back to its end. A file
/// past what the header can count, some 18 hours, keeps the largest size.
fn patch(file: &mut File, data: u64) -> std::io::Result<()> {
    let data = u32::try_from(data).unwrap_or(u32::MAX - 36);
    let header = header(data);
    file.seek(SeekFrom::Start(4))?;
    file.write_all(&header[4..8])?;
    file.seek(SeekFrom::Start(40))?;
    file.write_all(&header[40..])?;
    file.seek(SeekFrom::End(0))?;
    Ok(())
}

/// Where the sound heard at `at` belongs in a recording that started at `since`, in samples.
fn position(since: Instant, at: Instant) -> u64 {
    let micros = at.saturating_duration_since(since).as_micros();
    u64::try_from(micros * u128::from(RATE) / 1_000_000).unwrap_or(u64::MAX)
}

fn rms(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    ((sum / samples.len() as f64).sqrt() / 32_768.0) as f32
}

/// Lines the two sides up by when their frames arrived, one stereo sample at a time.
///
/// Each side has a queue of samples not written yet. A frame is placed where its arrival says
/// it belongs: if that is past the end of its side's queue by more than [`SLACK`], the side
/// missed something (a microphone that changed, a speaker that stalled) and the difference is
/// filled with silence, so what follows stays in step with the other side. Writing takes a
/// sample from each side, or silence from one that has nothing. A side that gets more at once
/// than it can play (the burst after a stall) keeps only its last second.
#[derive(Default)]
pub(crate) struct Timeline {
    /// How many stereo samples have been written.
    out: u64,
    lanes: [VecDeque<i16>; 2],
    /// Samples dropped because a side held too much.
    trimmed: u64,
}

impl Timeline {
    /// Queues `samples` of `side`, the sound up to position `end`.
    pub(crate) fn push(&mut self, side: Side, end: u64, samples: &[i16]) {
        let lane = &mut self.lanes[side as usize];
        let start = end.saturating_sub(samples.len() as u64);
        let head = self.out + lane.len() as u64;
        if start > head + SLACK {
            let gap = usize::try_from(start - head).unwrap_or(usize::MAX).min(CAP);
            lane.extend(std::iter::repeat_n(0, gap));
        }
        lane.extend(samples);
        if lane.len() > CAP {
            let excess = lane.len() - CAP;
            lane.drain(..excess);
            self.trimmed += excess as u64;
        }
    }

    /// Writes interleaved stereo samples into `out` until `due` have been written.
    pub(crate) fn take(&mut self, due: u64, out: &mut Vec<i16>) {
        let count = due.saturating_sub(self.out);
        out.reserve(usize::try_from(count * 2).unwrap_or(0));
        for _ in 0..count {
            for lane in &mut self.lanes {
                out.push(lane.pop_front().unwrap_or(0));
            }
        }
        self.out += count;
    }

    /// Where the longer side's queue ends: everything received, written.
    pub(crate) fn end(&self) -> u64 {
        self.out + self.lanes.iter().map(VecDeque::len).max().unwrap_or(0) as u64
    }

    #[cfg(test)]
    fn queued(&self, side: Side) -> usize {
        self.lanes[side as usize].len()
    }
}

/// The thread that writes a recording.
struct Writer {
    /// None once the disk failed.
    file: Option<BufWriter<File>>,
    since: Instant,
    timeline: Timeline,
    meter: Arc<Meter>,
    /// Stereo samples handed to the file.
    written: u64,
    peaks: [f32; 2],
    /// Each side's loudness summed over its samples, for the average.
    sums: [f64; 2],
    counts: [u64; 2],
    error: Option<String>,
}

impl Writer {
    fn run(
        mut self,
        frames: Receiver<Frame>,
        part: PathBuf,
        path: PathBuf,
        discarded: &AtomicBool,
    ) -> Outcome {
        let mut next_tick = Instant::now() + TICK;
        let mut next_patch = self.since + PATCH_EVERY;
        let mut next_sync = self.since + SYNC_EVERY;
        loop {
            match frames.recv_timeout(next_tick.saturating_duration_since(Instant::now())) {
                Ok(frame) => self.receive(frame),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            let now = Instant::now();
            if now < next_tick {
                continue;
            }
            next_tick = now + TICK;
            for (peak, level) in self.peaks.iter_mut().zip(&self.meter.levels) {
                *peak *= DECAY;
                level.store((*peak * 1000.0).round() as u16, Ordering::Relaxed);
            }
            self.write_until(position(self.since, now).saturating_sub(DELAY));
            if now >= next_sync {
                next_sync = now + SYNC_EVERY;
                next_patch = now + PATCH_EVERY;
                self.patch(true);
            } else if now >= next_patch {
                next_patch = now + PATCH_EVERY;
                self.patch(false);
            }
        }
        let end = self
            .timeline
            .end()
            .max(position(self.since, Instant::now()));
        self.write_until(end);
        self.patch(true);
        self.file = None;
        let levels = [0, 1].map(|side| {
            if self.counts[side] == 0 {
                0.0
            } else {
                (self.sums[side] / self.counts[side] as f64) as f32
            }
        });
        if self.timeline.trimmed > 0 {
            log::debug!(
                "call recording dropped {} samples a side held too long",
                self.timeline.trimmed
            );
        }
        let mut outcome = Outcome {
            path: path.clone(),
            seconds: self.written / u64::from(RATE),
            levels,
            error: self.error.take(),
        };
        if discarded.load(Ordering::Acquire) {
            let _ = std::fs::remove_file(&part);
        } else if let Err(error) = std::fs::rename(&part, &path) {
            log::warn!("could not rename the call recording: {error}");
            outcome.path = part;
            outcome.error.get_or_insert(error.to_string());
        }
        outcome
    }

    fn receive(&mut self, frame: Frame) {
        if self.file.is_none() {
            return;
        }
        let side = frame.side as usize;
        let level = rms(&frame.samples);
        self.peaks[side] = self.peaks[side].max(level);
        self.meter.levels[side].store(
            (self.peaks[side] * 1000.0).round() as u16,
            Ordering::Relaxed,
        );
        self.sums[side] += f64::from(level) * frame.samples.len() as f64;
        self.counts[side] += frame.samples.len() as u64;
        self.timeline
            .push(frame.side, position(self.since, frame.at), &frame.samples);
    }

    fn write_until(&mut self, due: u64) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let mut samples = Vec::new();
        self.timeline.take(due, &mut samples);
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        match file.write_all(&bytes) {
            Ok(()) => self.written += samples.len() as u64 / 2,
            Err(error) => self.fail(error),
        }
    }

    /// Brings the header up to date, and syncs the file to the disk if `sync`.
    fn patch(&mut self, sync: bool) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let data = self.written * BLOCK;
        let patched = file.flush().and_then(|()| {
            let file = file.get_mut();
            patch(file, data)?;
            if sync { file.sync_data() } else { Ok(()) }
        });
        if let Err(error) = patched {
            self.fail(error);
        }
    }

    /// Stops writing after the disk failed, keeping what reached it: the header is made to
    /// match the file's length, as [`recover_part`] would.
    fn fail(&mut self, error: std::io::Error) {
        let reason = error.to_string();
        self.meter.fail(reason.clone());
        self.error.get_or_insert(reason);
        let Some(file) = self.file.take() else {
            return;
        };
        let (mut file, _unwritten) = file.into_parts();
        let kept = file
            .metadata()
            .map(|metadata| metadata.len().saturating_sub(HEADER) / BLOCK * BLOCK);
        if let Ok(data) = kept {
            let _ = file.set_len(HEADER + data);
            let _ = patch(&mut file, data);
            let _ = file.sync_data();
            self.written = data / BLOCK;
        }
    }
}

/// Serializes the tests, here and elsewhere, that start a recording: only one may be live in
/// the process. A test holds it for its whole length, across its awaits too: each test has a
/// runtime of its own, so nothing else on that runtime waits for it.
#[cfg(test)]
pub(crate) fn exclusive() -> Exclusive {
    static ALONE: Mutex<()> = Mutex::new(());
    Exclusive {
        _alone: lock(&ALONE),
    }
}

/// What [`exclusive`] hands a test, until it ends.
#[cfg(test)]
pub(crate) struct Exclusive {
    _alone: MutexGuard<'static, ()>,
}

/// The left and the right channel of a WAV this module wrote, for tests elsewhere.
#[cfg(test)]
pub(crate) fn read_channels(path: &Path) -> [Vec<i16>; 2] {
    let wav = std::fs::read(path).unwrap();
    let mut sides = [Vec::new(), Vec::new()];
    for (at, pair) in wav[HEADER as usize..].as_chunks::<2>().0.iter().enumerate() {
        sides[at % 2].push(i16::from_le_bytes(*pair));
    }
    sides
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: u64 = RATE as u64;

    /// Feeds both sides 20 ms frames of `me` and `them` from `from` to `to` (in samples), writing
    /// what is due every 100 ms as the writer would. The microphone is silent while `mic_gap`.
    fn feed(
        timeline: &mut Timeline,
        out: &mut Vec<i16>,
        span: std::ops::Range<u64>,
        mic_gap: std::ops::Range<u64>,
        mark: u64,
    ) {
        let frame = SECOND / 50;
        let mut end = span.start + frame;
        while end <= span.end {
            let value = |base: i16| if end == mark { 7 } else { base };
            if !mic_gap.contains(&end) {
                timeline.push(Side::Me, end, &[value(100); 320]);
            }
            timeline.push(Side::Them, end, &[value(200); 320]);
            if end.is_multiple_of(SECOND / 10) {
                timeline.take(end.saturating_sub(DELAY), out);
            }
            end += frame;
        }
    }

    fn channel(stereo: &[i16], side: Side) -> Vec<i16> {
        stereo
            .iter()
            .skip(side as usize)
            .step_by(2)
            .copied()
            .collect()
    }

    #[test]
    fn a_gap_on_the_microphone_is_silence_and_the_sides_stay_in_step() {
        let mut timeline = Timeline::default();
        let mut out = Vec::new();
        let mark = 4 * SECOND;
        feed(
            &mut timeline,
            &mut out,
            0..6 * SECOND,
            SECOND..3 * SECOND,
            mark,
        );
        timeline.take(timeline.end(), &mut out);
        let (me, them) = (channel(&out, Side::Me), channel(&out, Side::Them));
        assert_eq!(me.len(), them.len());
        // The two seconds the microphone missed are silent on the left, the right went on.
        let gap = (SECOND + SECOND / 10) as usize..(3 * SECOND - SECOND / 10) as usize;
        assert!(me[gap.clone()].iter().all(|&s| s == 0));
        assert!(them[gap].iter().all(|&s| s == 200));
        // The frame marked on both sides at four seconds lands at the same place on each.
        let left = me.iter().position(|&s| s == 7).unwrap();
        let right = them.iter().position(|&s| s == 7).unwrap();
        assert_eq!(left, right);
        assert_eq!(left as u64, mark - 320);
        assert!(me[(3 * SECOND) as usize + 400..].iter().all(|&s| s != 0));
    }

    #[test]
    fn a_burst_of_more_than_a_second_keeps_its_last_second() {
        let mut timeline = Timeline::default();
        // Two seconds of the other side drained at once after a stall.
        for index in 0..100i16 {
            timeline.push(Side::Them, 320, &[index; 320]);
        }
        assert_eq!(timeline.queued(Side::Them), CAP);
        assert_eq!(timeline.trimmed, SECOND);
        let mut out = Vec::new();
        timeline.take(timeline.end(), &mut out);
        let them = channel(&out, Side::Them);
        assert_eq!(them.len(), CAP);
        assert_eq!((them[0], them[CAP - 1]), (50, 99), "the oldest went first");
        assert!(channel(&out, Side::Me).iter().all(|&s| s == 0));
    }

    fn read_wav(path: &Path) -> Vec<u8> {
        std::fs::read(path).unwrap()
    }

    fn samples_of(wav: &[u8]) -> Vec<i16> {
        wav[HEADER as usize..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| i16::from_le_bytes(*pair))
            .collect()
    }

    #[test]
    fn muted_the_left_is_silent_and_the_header_is_right() {
        let _alone = exclusive();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("call.wav");
        let tape = Tape::new(41);
        tape.start(&path, false).unwrap();
        assert!(part_path(&path).exists(), "written as .part first");
        tape.set_muted(true);
        for _ in 0..10 {
            let mut frame = vec![1_000i16; 960];
            tape.me(&mut frame);
            assert!(
                frame.iter().all(|&s| s == 1_000),
                "the call's frame is untouched"
            );
            tape.them(&[2_000; 320]);
        }
        let view = tape.view().unwrap();
        assert_eq!(view.notice, NoticeState::Off);
        assert_eq!(view.failed, None);
        let report = tape.stop().unwrap();
        assert_eq!(report.path, path);
        assert_eq!(report.generation, 41);
        assert_eq!(report.error, None);
        assert!(!part_path(&path).exists());
        assert!(tape.stop().is_none(), "stopping twice is harmless");
        assert!(tape.view().is_none());

        let wav = read_wav(&path);
        let length = wav.len() as u32;
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(
            u32::from_le_bytes(wav[4..8].try_into().unwrap()),
            length - 8
        );
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), RATE);
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(
            u32::from_le_bytes(wav[40..44].try_into().unwrap()),
            length - 44
        );
        let samples = samples_of(&wav);
        let (me, them) = (channel(&samples, Side::Me), channel(&samples, Side::Them));
        assert!(me.iter().all(|&s| s == 0), "muted, our voice is not kept");
        assert_eq!(them.iter().filter(|&&s| s == 2_000).count(), 3_200);
        assert_eq!(report.levels[0], 0.0);
        assert!(report.levels[1] > 0.05, "{:?}", report.levels);
    }

    #[test]
    fn the_notice_waits_for_the_microphone_to_open() {
        let _alone = exclusive();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("call.wav");
        let tape = Tape::new(42);
        tape.set_muted(true);
        tape.start(&path, true).unwrap();
        let mut frame = vec![0i16; 960];
        tape.me(&mut frame);
        assert!(frame.iter().all(|&s| s == 0), "muted, the notice is held");
        assert_eq!(tape.view().unwrap().notice, NoticeState::Held);

        tape.set_muted(false);
        assert_eq!(tape.view().unwrap().notice, NoticeState::Pending);
        let tone = crate::call_tones::recording_notice();
        let mut heard = Vec::new();
        while heard.len() < tone.len() {
            let mut frame = vec![0i16; 960];
            tape.me(&mut frame);
            heard.extend(frame);
        }
        assert_eq!(
            &heard[..tone.len()],
            &tone[..],
            "the call carries the notice"
        );
        assert_eq!(tape.view().unwrap().notice, NoticeState::Played);
        let mut after = vec![5i16; 960];
        tape.me(&mut after);
        assert!(after.iter().all(|&s| s == 5), "it plays once");

        let report = tape.stop().unwrap();
        assert!(report.notice_ms.is_some_and(|ms| ms < 1_000), "{report:?}");
        let samples = samples_of(&read_wav(&path));
        let me = channel(&samples, Side::Me);
        let loudest = me.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(loudest > 7_000, "the recording keeps the notice: {loudest}");
    }

    #[test]
    fn one_call_is_recorded_at_a_time() {
        let _alone = exclusive();
        let dir = tempfile::tempdir().unwrap();
        let first = Tape::new(43);
        let second = Tape::new(44);
        first.start(&dir.path().join("a.wav"), false).unwrap();
        assert_eq!(
            second.start(&dir.path().join("b.wav"), false),
            Err(BUSY.to_owned())
        );
        assert_eq!(
            first.start(&dir.path().join("c.wav"), false),
            Err(ALREADY.to_owned())
        );
        assert!(live_levels(43).is_some());
        assert!(live_levels(44).is_none());
        first.stop().unwrap();
        assert!(live_levels(43).is_none());

        let path = dir.path().join("b.wav");
        second.start(&path, false).unwrap();
        second.them(&[3_000; 320]);
        second.discard();
        assert!(!path.exists() && !part_path(&path).exists(), "discarded");
        assert!(second.view().is_none());

        // A tape dropped while recording closes its file and frees the place.
        let dropped = Tape::new(45);
        let path = dir.path().join("d.wav");
        dropped.start(&path, false).unwrap();
        drop(dropped);
        Tape::new(46)
            .start(&dir.path().join("e.wav"), false)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !path.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(path.exists(), "kept as a .wav");
    }

    #[test]
    fn a_start_that_cannot_write_leaves_nothing_live() {
        let _alone = exclusive();
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, b"x").unwrap();
        let tape = Tape::new(47);
        assert!(tape.start(&blocker.join("call.wav"), true).is_err());
        assert!(tape.view().is_none());
        let other = Tape::new(48);
        other.start(&dir.path().join("call.wav"), false).unwrap();
        other.discard();
    }

    #[test]
    fn the_live_levels_follow_each_side() {
        let _alone = exclusive();
        let dir = tempfile::tempdir().unwrap();
        let tape = Tape::new(49);
        tape.start(&dir.path().join("call.wav"), false).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut levels = None;
        while Instant::now() < deadline {
            tape.them(&[16_000; 320]);
            levels = live_levels(49);
            if levels.is_some_and(|[_, them]| them > 0.3) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let [me, them] = levels.unwrap();
        assert_eq!(me, 0.0);
        assert!(them > 0.3, "{them}");
        tape.discard();
    }

    #[test]
    fn a_part_left_by_a_crash_opens_after_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("call-1-2.wav.part");
        // The header as written at the start, then a second and a bit, cut mid-sample.
        let mut bytes = header(0).to_vec();
        for index in 0..RATE as i32 + 10 {
            let value = ((index % 64) * 100) as i16;
            bytes.extend(value.to_le_bytes());
            bytes.extend((-value).to_le_bytes());
        }
        bytes.push(1);
        std::fs::write(&part, &bytes).unwrap();

        let wav = recover_part(&part).unwrap();
        assert_eq!(wav, dir.path().join("call-1-2.wav"));
        assert!(!part.exists());
        let length = std::fs::metadata(&wav).unwrap().len();
        assert_eq!(length, HEADER + (u64::from(RATE) + 10) * BLOCK);

        use rodio::Source as _;
        let decoder = rodio::Decoder::try_from(File::open(&wav).unwrap()).unwrap();
        assert_eq!(decoder.channels().get(), 2);
        assert_eq!(decoder.sample_rate().get(), RATE);
        assert_eq!(decoder.count(), (RATE as usize + 10) * 2);

        let other = dir.path().join("notes.txt");
        std::fs::write(&other, b"hello").unwrap();
        assert!(recover_part(&other).is_err());
    }
}
