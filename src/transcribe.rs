//! Local speech-to-text for voice messages, audio files and videos, with
//! whisper.cpp (Metal on Apple Silicon, the CPU on Intel Macs). Nothing leaves
//! the computer: the model is downloaded once from whisper.cpp's own model
//! repository and runs here.
//!
//! Decoding reuses the app's own codecs: OGG/Opus voice messages through
//! `voice`, MP4 sound through `video::aac_samples`, and anything else rodio
//! reads (MP3, M4A, WAV, Vorbis). Whisper wants 16 kHz mono.

use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

/// whisper.cpp's quantized large-v3-turbo: near large-v3 accuracy, several
/// times faster, about 550 MB.
pub const MODEL_FILE: &str = "ggml-large-v3-turbo-q5_0.bin";
const MODEL_URL: &str =
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin";
/// A smaller download than this is an error page, not the model.
const MODEL_MIN_BYTES: u64 = 400 * 1024 * 1024;
const RATE: u32 = 16_000;
/// Longest stretch transcribed from one file: whisper's cost grows with it.
const LONGEST_SECONDS: usize = 60 * 60;

/// Where the model lives for this app's data folder.
pub fn model_path(state_dir: &Path) -> PathBuf {
    state_dir.join("models").join(MODEL_FILE)
}

/// Whether the model is already downloaded.
pub fn model_ready(state_dir: &Path) -> bool {
    std::fs::metadata(model_path(state_dir)).is_ok_and(|meta| meta.len() >= MODEL_MIN_BYTES)
}

/// Downloads the model once, to a `.part` file renamed when complete.
pub fn ensure_model(state_dir: &Path) -> Result<PathBuf, String> {
    let path = model_path(state_dir);
    if model_ready(state_dir) {
        return Ok(path);
    }
    // One download at a time, even with several accounts.
    static DOWNLOADING: Mutex<()> = Mutex::new(());
    let _one = DOWNLOADING.lock().unwrap_or_else(|p| p.into_inner());
    if model_ready(state_dir) {
        return Ok(path);
    }
    let folder = path.parent().expect("model path has a folder");
    std::fs::create_dir_all(folder).map_err(|error| format!("model folder: {error}"))?;
    log::info!("transcription: downloading the {MODEL_FILE} model");
    let partial = path.with_extension("bin.part");
    let mut response = ureq::get(MODEL_URL)
        .call()
        .map_err(|error| format!("could not download the transcription model: {error}"))?;
    let mut reader = response.body_mut().with_config().limit(u64::MAX).reader();
    let mut file =
        std::fs::File::create(&partial).map_err(|error| format!("model file: {error}"))?;
    let mut buffer = vec![0u8; 1 << 20];
    let mut total = 0u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("model download stopped: {error}"))?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])
            .map_err(|error| format!("model file: {error}"))?;
        total += read as u64;
    }
    file.sync_all()
        .map_err(|error| format!("model file: {error}"))?;
    if total < MODEL_MIN_BYTES {
        let _ = std::fs::remove_file(&partial);
        return Err("the transcription model download was incomplete".into());
    }
    std::fs::rename(&partial, &path).map_err(|error| format!("model file: {error}"))?;
    log::info!("transcription: model ready");
    Ok(path)
}

/// Linear resampling of mono samples to 16 kHz; enough for speech.
fn resample(mono: &[f32], rate: u32) -> Vec<f32> {
    if rate == RATE || rate == 0 || mono.is_empty() {
        return mono.to_vec();
    }
    let ratio = f64::from(rate) / f64::from(RATE);
    let count = (mono.len() as f64 / ratio).floor() as usize;
    (0..count)
        .map(|index| {
            let position = index as f64 * ratio;
            let left = position.floor() as usize;
            let t = (position - left as f64) as f32;
            let a = mono[left.min(mono.len() - 1)];
            let b = mono.get(left + 1).copied().unwrap_or(a);
            a + (b - a) * t
        })
        .collect()
}

fn mono(interleaved: &[f32], channels: u16) -> Vec<f32> {
    let channels = usize::from(channels.max(1));
    interleaved
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}

/// A media file's sound as 16 kHz mono samples.
pub fn samples(path: &Path) -> Result<Vec<f32>, String> {
    let mut head = [0u8; 12];
    let mut file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let read = file.read(&mut head).map_err(|error| error.to_string())?;
    let head = &head[..read];
    let mut samples = if head.starts_with(b"OggS") {
        let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
        match crate::voice::decode(&bytes) {
            Ok(decoded) => resample(&decoded, crate::voice::RATE),
            Err(_) => rodio_samples(path)?,
        }
    } else if head.len() >= 8 && &head[4..8] == b"ftyp" {
        match crate::video::aac_samples(path) {
            Ok((interleaved, channels, rate)) => resample(&mono(&interleaved, channels), rate),
            Err(_) => rodio_samples(path)?,
        }
    } else {
        rodio_samples(path)?
    };
    samples.truncate(LONGEST_SECONDS * RATE as usize);
    if samples.is_empty() {
        return Err("the file has no sound".into());
    }
    Ok(samples)
}

fn rodio_samples(path: &Path) -> Result<Vec<f32>, String> {
    use rodio::Source;
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let decoder = rodio::Decoder::try_from(file).map_err(|error| error.to_string())?;
    let (channels, rate) = (decoder.channels().get(), decoder.sample_rate().get());
    let interleaved: Vec<f32> = decoder.collect();
    Ok(resample(&mono(&interleaved, channels), rate))
}

/// The loaded model, kept between transcriptions: loading it takes seconds.
static CONTEXT: Mutex<Option<(PathBuf, whisper_rs::WhisperContext)>> = Mutex::new(None);

/// Frees the loaded model. Call before the process exits: ggml's Metal
/// device is torn down by an exit handler, which aborts while a context
/// still holds it.
pub fn unload() {
    CONTEXT.lock().unwrap_or_else(|p| p.into_inner()).take();
}

/// Transcribes a media file, downloading the model on first use. Blocking:
/// run it off the interface and the async runtime.
pub fn transcribe(state_dir: &Path, media: &Path) -> Result<String, String> {
    let model = ensure_model(state_dir)?;
    let audio = samples(media)?;
    let mut loaded = CONTEXT.lock().unwrap_or_else(|p| p.into_inner());
    if loaded.as_ref().is_none_or(|(path, _)| *path != model) {
        let context = whisper_rs::WhisperContext::new_with_params(
            &model,
            whisper_rs::WhisperContextParameters::default(),
        )
        .map_err(|error| format!("could not load the transcription model: {error}"))?;
        *loaded = Some((model, context));
    }
    let (_, context) = loaded.as_ref().expect("loaded above");
    let mut state = context
        .create_state()
        .map_err(|error| format!("transcription: {error}"))?;
    let mut params =
        whisper_rs::FullParams::new(whisper_rs::SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(Some("auto"));
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
    params.set_n_threads(threads as i32);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_special(false);
    params.set_print_timestamps(false);
    params.set_suppress_blank(true);
    state
        .full(params, &audio)
        .map_err(|error| format!("transcription failed: {error}"))?;
    let mut text = String::new();
    for index in 0..state.full_n_segments() {
        if let Some(segment) = state.get_segment(index)
            && let Ok(piece) = segment.to_str_lossy()
        {
            text.push_str(piece.trim());
            text.push(' ');
        }
    }
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampling_to_16k_keeps_the_duration() {
        let one_second = vec![0.5f32; 48_000];
        assert_eq!(resample(&one_second, 48_000).len(), 16_000);
        assert_eq!(resample(&one_second[..16_000], 16_000).len(), 16_000);
        assert_eq!(mono(&[1.0, 0.0, 0.5, 0.5], 2), vec![0.5, 0.5]);
    }

    #[test]
    fn a_short_download_is_not_a_model() {
        let root = tempfile::tempdir().unwrap();
        assert!(!model_ready(root.path()));
        std::fs::create_dir_all(root.path().join("models")).unwrap();
        std::fs::write(model_path(root.path()), b"<html>").unwrap();
        assert!(!model_ready(root.path()));
    }
}
