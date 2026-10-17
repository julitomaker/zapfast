//! Records a call to a file for transcription: the microphone, the system's
//! sound (a Meet or Zoom call in the browser), or both mixed.
//!
//! The system's sound comes from ScreenCaptureKit, which asks once for the
//! Screen & System Audio Recording permission; on macOS 15 and later it also
//! records the microphone in step. Otherwise the microphone is read through
//! rodio. Each source is written to disk as it arrives, as 16 kHz mono, so a
//! long call does not grow in memory, and the tracks are mixed into one WAV
//! when the recording stops.

use std::{
    fs::File,
    io::{BufWriter, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

const RATE: u32 = 16_000;

/// What a recording listens to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Microphone,
    /// The computer's sound: the other side of a call in the browser.
    System,
    Both,
}

/// One source's samples, resampled to 16 kHz mono and appended to a file.
struct Track {
    file: BufWriter<File>,
    path: PathBuf,
    /// The source rate and how far into the next source sample resampling is.
    position: f64,
    last: f32,
}

impl Track {
    fn create(path: PathBuf) -> std::io::Result<Self> {
        Ok(Self {
            file: BufWriter::new(File::create(&path)?),
            path,
            position: 0.0,
            last: 0.0,
        })
    }

    /// Appends mono samples at `rate`, linearly resampled to 16 kHz.
    fn push(&mut self, mono: &[f32], rate: f64) {
        if mono.is_empty() || rate <= 0.0 {
            return;
        }
        let step = rate / f64::from(RATE);
        let mut out = Vec::with_capacity((mono.len() as f64 / step) as usize + 2);
        while self.position < mono.len() as f64 {
            let left = self.position.floor();
            let index = left as isize;
            let t = (self.position - left) as f32;
            let a = if index < 0 {
                self.last
            } else {
                mono[index as usize]
            };
            let b = mono.get((index + 1) as usize).copied().unwrap_or(a);
            let value = (a + (b - a) * t).clamp(-1.0, 1.0);
            out.extend_from_slice(&((value * f32::from(i16::MAX)) as i16).to_le_bytes());
            self.position += step;
        }
        self.position -= mono.len() as f64;
        self.last = *mono.last().expect("not empty");
        let _ = self.file.write_all(&out);
    }
}

#[derive(Default)]
struct Tracks {
    system: Option<Track>,
    microphone: Option<Track>,
}

/// A recording in progress. Stopping it returns the mixed WAV file.
pub struct Recording {
    started: Instant,
    folder: PathBuf,
    tracks: Arc<Mutex<Tracks>>,
    stop: Arc<AtomicBool>,
    microphone: Option<std::thread::JoinHandle<()>>,
    #[cfg(target_os = "macos")]
    capture: Option<macos::Capture>,
}

impl Recording {
    /// Starts recording `source` into `<state>/recordings/`.
    pub fn start(state_dir: &Path, source: Source) -> Result<Self, String> {
        let folder = state_dir.join("recordings");
        std::fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
        let stamp = jiff::Timestamp::now().as_second();
        let tracks = Arc::new(Mutex::new(Tracks::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let mut recording = Self {
            started: Instant::now(),
            folder: folder.clone(),
            tracks: Arc::clone(&tracks),
            stop: Arc::clone(&stop),
            microphone: None,
            #[cfg(target_os = "macos")]
            capture: None,
        };
        let wants_system = matches!(source, Source::System | Source::Both);
        let wants_microphone = matches!(source, Source::Microphone | Source::Both);
        let mut microphone_in_capture = false;
        if wants_system {
            tracks.lock().unwrap_or_else(|p| p.into_inner()).system = Some(
                Track::create(folder.join(format!("{stamp}-system.raw")))
                    .map_err(|e| e.to_string())?,
            );
            #[cfg(target_os = "macos")]
            {
                let capture = macos::Capture::start(Arc::clone(&tracks), wants_microphone)?;
                microphone_in_capture = capture.microphone;
                recording.capture = Some(capture);
            }
            #[cfg(not(target_os = "macos"))]
            return Err("Recording the computer's sound is only available on macOS".into());
        }
        if wants_microphone {
            tracks.lock().unwrap_or_else(|p| p.into_inner()).microphone = Some(
                Track::create(folder.join(format!("{stamp}-microphone.raw")))
                    .map_err(|e| e.to_string())?,
            );
            if !microphone_in_capture {
                recording.microphone =
                    Some(spawn_microphone(Arc::clone(&tracks), Arc::clone(&stop))?);
            }
        }
        Ok(recording)
    }

    pub fn elapsed(&self) -> std::time::Duration {
        self.started.elapsed()
    }

    /// Stops, mixes the tracks and returns the WAV file.
    pub fn stop(mut self) -> Result<PathBuf, String> {
        self.stop.store(true, Ordering::Relaxed);
        #[cfg(target_os = "macos")]
        if let Some(capture) = self.capture.take() {
            capture.stop();
        }
        if let Some(thread) = self.microphone.take() {
            let _ = thread.join();
        }
        let mut tracks =
            std::mem::take(&mut *self.tracks.lock().unwrap_or_else(|p| p.into_inner()));
        let mut raws = Vec::new();
        for track in [tracks.system.take(), tracks.microphone.take()]
            .into_iter()
            .flatten()
        {
            let Track { mut file, path, .. } = track;
            file.flush().map_err(|e| e.to_string())?;
            drop(file);
            raws.push(path);
        }
        let stamp = jiff::Timestamp::now()
            .to_zoned(jiff::tz::TimeZone::system())
            .strftime("%Y-%m-%d %H.%M")
            .to_string();
        let wav = self.folder.join(format!("Llamada {stamp}.wav"));
        mix(&raws, &wav)?;
        for raw in raws {
            let _ = std::fs::remove_file(raw);
        }
        Ok(wav)
    }
}

/// Mixes 16 kHz mono 16-bit tracks into one WAV, louder ones not clipping.
fn mix(raws: &[PathBuf], wav: &Path) -> Result<(), String> {
    let mut tracks = Vec::new();
    for raw in raws {
        let mut bytes = Vec::new();
        File::open(raw)
            .and_then(|mut file| file.read_to_end(&mut bytes))
            .map_err(|e| e.to_string())?;
        tracks.push(
            bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| i32::from(i16::from_le_bytes(*pair)))
                .collect::<Vec<_>>(),
        );
    }
    let length = tracks.iter().map(Vec::len).max().unwrap_or(0);
    if length == 0 {
        return Err("Nothing was recorded".into());
    }
    let mut out = BufWriter::new(File::create(wav).map_err(|e| e.to_string())?);
    let data = (length * 2) as u32;
    let mut header = Vec::with_capacity(44);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&(36 + data).to_le_bytes());
    header.extend_from_slice(b"WAVEfmt ");
    header.extend_from_slice(&16u32.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes());
    header.extend_from_slice(&1u16.to_le_bytes());
    header.extend_from_slice(&RATE.to_le_bytes());
    header.extend_from_slice(&(RATE * 2).to_le_bytes());
    header.extend_from_slice(&2u16.to_le_bytes());
    header.extend_from_slice(&16u16.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&data.to_le_bytes());
    out.write_all(&header).map_err(|e| e.to_string())?;
    for index in 0..length {
        let sum: i32 = tracks.iter().filter_map(|track| track.get(index)).sum();
        let value = sum.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
        out.write_all(&value.to_le_bytes())
            .map_err(|e| e.to_string())?;
    }
    out.flush().map_err(|e| e.to_string())
}

/// Reads the default microphone on a thread until `stop`.
fn spawn_microphone(
    tracks: Arc<Mutex<Tracks>>,
    stop: Arc<AtomicBool>,
) -> Result<std::thread::JoinHandle<()>, String> {
    let (ready_tx, ready) = std::sync::mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("call-microphone".into())
        .spawn(move || {
            use rodio::Source as _;
            let microphone = rodio::microphone::MicrophoneBuilder::new()
                .default_device()
                .and_then(|builder| builder.default_config())
                .map_err(|error| error.to_string())
                .and_then(|builder| builder.open_stream().map_err(|error| error.to_string()));
            let microphone = match microphone {
                Ok(microphone) => {
                    let _ = ready_tx.send(Ok(()));
                    microphone
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(format!("Could not open the microphone: {error}")));
                    return;
                }
            };
            let channels = usize::from(microphone.channels().get());
            let rate = f64::from(microphone.sample_rate().get());
            let mut frame = Vec::with_capacity(channels);
            let mut chunk = Vec::with_capacity(4800);
            for sample in microphone {
                frame.push(sample);
                if frame.len() == channels {
                    chunk.push(frame.iter().sum::<f32>() / channels as f32);
                    frame.clear();
                }
                if chunk.len() >= 4800 {
                    if let Some(track) = tracks
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .microphone
                        .as_mut()
                    {
                        track.push(&chunk, rate);
                    }
                    chunk.clear();
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                }
            }
            if let Some(track) = tracks
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .microphone
                .as_mut()
            {
                track.push(&chunk, rate);
            }
        })
        .map_err(|e| e.to_string())?;
    ready
        .recv()
        .map_err(|_| "The microphone did not answer".to_owned())??;
    Ok(thread)
}

#[cfg(target_os = "macos")]
mod macos {
    use std::{
        ffi::c_void,
        ptr::NonNull,
        sync::{Arc, Mutex, mpsc},
        time::Duration,
    };

    use block2::RcBlock;
    use dispatch2::DispatchQueue;
    use objc2::{
        AllocAnyThread, DefinedClass, define_class, msg_send,
        rc::Retained,
        runtime::{NSObject, NSObjectProtocol, ProtocolObject},
    };
    use objc2_core_media::{CMAudioFormatDescriptionGetStreamBasicDescription, CMSampleBuffer};
    use objc2_foundation::{NSArray, NSError};
    use objc2_screen_capture_kit::{
        SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutput,
        SCStreamOutputType,
    };

    use super::Tracks;

    pub struct Capture {
        stream: Retained<SCStream>,
        _output: Retained<Output>,
        /// Whether ScreenCaptureKit records the microphone too (macOS 15+).
        pub microphone: bool,
    }

    // The stream is only started and stopped, from one thread at a time.
    unsafe impl Send for Capture {}

    pub struct Ivars {
        tracks: Arc<Mutex<Tracks>>,
    }

    define_class!(
        #[unsafe(super(NSObject))]
        #[name = "WhatsProCallAudioOutput"]
        #[ivars = Ivars]
        pub struct Output;

        unsafe impl NSObjectProtocol for Output {}

        unsafe impl SCStreamOutput for Output {
            #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
            unsafe fn did_output(
                &self,
                _stream: &SCStream,
                buffer: &CMSampleBuffer,
                kind: SCStreamOutputType,
            ) {
                let Some((mono, rate)) = (unsafe { samples(buffer) }) else {
                    return;
                };
                let mut tracks = self
                    .ivars()
                    .tracks
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                let track = if kind == SCStreamOutputType::Microphone {
                    tracks.microphone.as_mut()
                } else if kind == SCStreamOutputType::Audio {
                    tracks.system.as_mut()
                } else {
                    None
                };
                if let Some(track) = track {
                    track.push(&mono, rate);
                }
            }
        }
    );

    impl Output {
        fn new(tracks: Arc<Mutex<Tracks>>) -> Retained<Self> {
            let this = Self::alloc().set_ivars(Ivars { tracks });
            unsafe { msg_send![super(this), init] }
        }
    }

    /// A sample buffer's sound as mono samples and its rate. Float or 16-bit,
    /// interleaved or not: only the first channel of planar sound is read.
    unsafe fn samples(buffer: &CMSampleBuffer) -> Option<(Vec<f32>, f64)> {
        let format = unsafe { buffer.format_description() }?;
        let description = unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(&format) };
        let description = unsafe { description.as_ref() }?;
        let block = unsafe { buffer.data_buffer() }?;
        let length = unsafe { block.data_length() };
        if length == 0 {
            return None;
        }
        let mut bytes = vec![0u8; length];
        let status = unsafe {
            block.copy_data_bytes(
                0,
                length,
                NonNull::new(bytes.as_mut_ptr().cast::<c_void>())?,
            )
        };
        if status != 0 {
            return None;
        }
        let frames = usize::try_from(unsafe { buffer.num_samples() }).ok()?;
        let channels = description.mChannelsPerFrame.max(1) as usize;
        const FLOAT: u32 = 1;
        const NON_INTERLEAVED: u32 = 1 << 5;
        let float = description.mFormatFlags & FLOAT != 0;
        let planar = description.mFormatFlags & NON_INTERLEAVED != 0;
        let values: Vec<f32> = if float && description.mBitsPerChannel == 32 {
            bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b))
                .collect()
        } else if description.mBitsPerChannel == 16 {
            bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| f32::from(i16::from_le_bytes(*b)) / f32::from(i16::MAX))
                .collect()
        } else {
            return None;
        };
        let mono = if channels == 1 {
            values
        } else if planar {
            values.into_iter().take(frames).collect()
        } else {
            values
                .chunks_exact(channels)
                .map(|frame| frame.iter().sum::<f32>() / channels as f32)
                .collect()
        };
        Some((mono, description.mSampleRate))
    }

    impl Capture {
        pub fn start(tracks: Arc<Mutex<Tracks>>, microphone: bool) -> Result<Self, String> {
            let (sender, answer) = mpsc::channel();
            let sender = Mutex::new(Some(sender));
            let handler = RcBlock::new(
                move |content: *mut SCShareableContent, error: *mut NSError| {
                    let result = if let Some(content) = unsafe { content.as_ref() } {
                        Ok(unsafe {
                            Retained::retain(content as *const _ as *mut SCShareableContent)
                        })
                    } else {
                        let message = unsafe { error.as_ref() }
                            .map(|error| error.localizedDescription().to_string())
                            .unwrap_or_else(|| "no answer".into());
                        Err(message)
                    };
                    if let Some(sender) = sender.lock().unwrap_or_else(|p| p.into_inner()).take() {
                        let _ = sender.send(result);
                    }
                },
            );
            unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
            let content = answer
                .recv_timeout(Duration::from_secs(60))
                .map_err(|_| "macOS did not answer whether the computer's sound may be recorded".to_owned())?
                .map_err(|error| {
                    format!("Allow Whats Pro in System Settings > Privacy & Security > Screen & System Audio Recording ({error})")
                })?
                .ok_or("macOS gave no screen to record")?;
            let displays = unsafe { content.displays() };
            let display = displays
                .firstObject()
                .ok_or("No display to record the sound of")?;
            let filter = unsafe {
                SCContentFilter::initWithDisplay_excludingWindows(
                    SCContentFilter::alloc(),
                    &display,
                    &NSArray::new(),
                )
            };
            let config = unsafe { SCStreamConfiguration::new() };
            unsafe {
                config.setCapturesAudio(true);
                config.setExcludesCurrentProcessAudio(true);
                config.setSampleRate(48_000);
                config.setChannelCount(1);
                // The picture is not wanted: the smallest frame, rarely.
                config.setWidth(2);
                config.setHeight(2);
            }
            let microphone =
                microphone && config.respondsToSelector(objc2::sel!(setCaptureMicrophone:));
            if microphone {
                unsafe { config.setCaptureMicrophone(true) };
            }
            let stream = unsafe {
                SCStream::initWithFilter_configuration_delegate(
                    SCStream::alloc(),
                    &filter,
                    &config,
                    None,
                )
            };
            let output = Output::new(tracks);
            let queue = DispatchQueue::new("whatspro.call-audio", None);
            let protocol = ProtocolObject::from_ref(&*output);
            for kind in [SCStreamOutputType::Audio, SCStreamOutputType::Screen]
                .into_iter()
                .chain(microphone.then_some(SCStreamOutputType::Microphone))
            {
                unsafe {
                    stream.addStreamOutput_type_sampleHandlerQueue_error(
                        protocol,
                        kind,
                        Some(&queue),
                    )
                }
                .map_err(|error| error.localizedDescription().to_string())?;
            }
            let (started_tx, started) = mpsc::channel();
            let started_tx = Mutex::new(Some(started_tx));
            let done = RcBlock::new(move |error: *mut NSError| {
                let result = match unsafe { error.as_ref() } {
                    None => Ok(()),
                    Some(error) => Err(error.localizedDescription().to_string()),
                };
                if let Some(tx) = started_tx.lock().unwrap_or_else(|p| p.into_inner()).take() {
                    let _ = tx.send(result);
                }
            });
            unsafe { stream.startCaptureWithCompletionHandler(Some(&done)) };
            started
                .recv_timeout(Duration::from_secs(60))
                .map_err(|_| "The recording did not start".to_owned())?
                .map_err(|error| format!("The recording did not start: {error}"))?;
            Ok(Self {
                stream,
                _output: output,
                microphone,
            })
        }

        pub fn stop(self) {
            let (tx, rx) = mpsc::channel();
            let tx = Mutex::new(Some(tx));
            let done = RcBlock::new(move |_error: *mut NSError| {
                if let Some(tx) = tx.lock().unwrap_or_else(|p| p.into_inner()).take() {
                    let _ = tx.send(());
                }
            });
            unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&done)) };
            let _ = rx.recv_timeout(Duration::from_secs(5));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_resample_to_16k_and_mix_into_one_wav() {
        let root = tempfile::tempdir().unwrap();
        let mut a = Track::create(root.path().join("a.raw")).unwrap();
        let mut b = Track::create(root.path().join("b.raw")).unwrap();
        // One second at 48 kHz in two chunks, half a second at 16 kHz.
        a.push(&vec![0.25; 24_000], 48_000.0);
        a.push(&vec![0.25; 24_000], 48_000.0);
        b.push(&vec![0.5; 8_000], 16_000.0);
        let paths = [a.path.clone(), b.path.clone()];
        drop((a, b));
        let wav = root.path().join("out.wav");
        mix(&paths, &wav).unwrap();
        let bytes = std::fs::read(&wav).unwrap();
        let samples = (bytes.len() - 44) / 2;
        assert!((15_990..=16_010).contains(&samples), "{samples}");
        let first = i16::from_le_bytes([bytes[44], bytes[45]]);
        assert!(
            (24_000..=24_700).contains(&first),
            "both tracks are summed: {first}"
        );
    }
}
