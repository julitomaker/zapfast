//! Who reads voice messages, videos and pictures: this computer (Whisper and
//! macOS Vision, see `transcribe` and `ocr`), an AI provider's API, or an AI
//! command-line tool installed here (Claude Code, Codex, Gemini CLI).
//!
//! Anything but the local engines sends the file to that provider, so they
//! are off until chosen in Settings > Transcription. Keys live in the OS
//! keyring (`crate::keys`), shared with Projects' analysis.

use std::{
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use base64::Engine as _;
use serde::{Deserialize, Serialize};

/// What turns speech into text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeechEngine {
    /// Whisper on this computer.
    #[default]
    Local,
    OpenAi,
    Groq,
    Gemini,
    GeminiCli,
}

impl SpeechEngine {
    pub const EVERY: [Self; 5] = [
        Self::Local,
        Self::OpenAi,
        Self::Groq,
        Self::Gemini,
        Self::GeminiCli,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Local => "This computer (Whisper)",
            Self::OpenAi => "OpenAI API",
            Self::Groq => "Groq API",
            Self::Gemini => "Gemini API",
            Self::GeminiCli => "Gemini CLI",
        }
    }

    pub fn key_account(self) -> Option<&'static str> {
        match self {
            Self::OpenAi => Some("OpenAi"),
            Self::Groq => Some("Groq"),
            Self::Gemini => Some("Gemini"),
            _ => None,
        }
    }

    pub fn default_model(self) -> &'static str {
        match self {
            Self::OpenAi => "gpt-4o-transcribe",
            Self::Groq => "whisper-large-v3-turbo",
            Self::Gemini => "gemini-2.5-flash",
            _ => "",
        }
    }
}

/// What reads pictures: their text only (this computer), or their text and
/// what they show (an AI model).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageEngine {
    /// macOS Vision on this computer: the text only.
    #[default]
    Local,
    ClaudeCode,
    Anthropic,
    OpenAi,
    OpenRouter,
    Gemini,
    CodexCli,
    GeminiCli,
}

impl ImageEngine {
    pub const EVERY: [Self; 8] = [
        Self::Local,
        Self::ClaudeCode,
        Self::CodexCli,
        Self::GeminiCli,
        Self::Anthropic,
        Self::OpenAi,
        Self::Gemini,
        Self::OpenRouter,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Local => "This computer (text only)",
            Self::ClaudeCode => "Claude Code CLI",
            Self::Anthropic => "Anthropic API",
            Self::OpenAi => "OpenAI API",
            Self::OpenRouter => "OpenRouter API",
            Self::Gemini => "Gemini API",
            Self::CodexCli => "Codex CLI",
            Self::GeminiCli => "Gemini CLI",
        }
    }

    pub fn key_account(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => Some("Anthropic"),
            Self::OpenAi => Some("OpenAi"),
            Self::OpenRouter => Some("OpenRouter"),
            Self::Gemini => Some("Gemini"),
            _ => None,
        }
    }

    pub fn default_model(self) -> &'static str {
        match self {
            Self::Anthropic => "claude-haiku-5-5",
            Self::OpenAi => "gpt-4.1-mini",
            Self::OpenRouter => "google/gemini-2.5-flash",
            Self::Gemini => "gemini-2.5-flash",
            _ => "",
        }
    }
}

/// The engines chosen in Settings, with the models typed for them (empty
/// means the engine's default).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaEngines {
    pub speech: SpeechEngine,
    pub speech_model: String,
    pub image: ImageEngine,
    pub image_model: String,
}

impl MediaEngines {
    fn speech_model(&self) -> String {
        Some(self.speech_model.trim())
            .filter(|m| !m.is_empty())
            .unwrap_or(self.speech.default_model())
            .to_owned()
    }

    fn image_model(&self) -> String {
        Some(self.image_model.trim())
            .filter(|m| !m.is_empty())
            .unwrap_or(self.image.default_model())
            .to_owned()
    }

    /// A short name of what read a message, stored with its transcript.
    pub fn speech_name(&self) -> String {
        match self.speech {
            SpeechEngine::Local => crate::transcribe::MODEL_FILE.to_owned(),
            SpeechEngine::GeminiCli => "gemini-cli".to_owned(),
            engine => format!("{engine:?}:{}", self.speech_model()),
        }
    }

    pub fn image_name(&self) -> String {
        match self.image {
            ImageEngine::Local => "vision".to_owned(),
            ImageEngine::ClaudeCode | ImageEngine::CodexCli | ImageEngine::GeminiCli => {
                format!("{:?}", self.image)
            }
            engine => format!("{engine:?}:{}", self.image_model()),
        }
    }
}

const SPEECH_PROMPT: &str = "Transcribí literalmente este audio, en su idioma original. Devolvé solo la transcripción, sin comentarios.";
const IMAGE_PROMPT: &str = "Describí en español, en una o dos oraciones, qué muestra esta imagen. Si tiene texto, transcribilo completo después de una línea que diga 'Texto:'. Devolvé solo eso.";
/// Longest piece of sound sent at once: APIs cap uploads near 25 MB.
const CHUNK_SECONDS: usize = 8 * 60;

/// Reads a message's speech with the chosen engine. Blocking.
pub fn speech(engines: &MediaEngines, state_dir: &Path, media: &Path) -> Result<String, String> {
    match engines.speech {
        SpeechEngine::Local => crate::transcribe::transcribe(state_dir, media),
        SpeechEngine::GeminiCli => {
            let wav = temporary_wav(&crate::transcribe::samples(media)?)?;
            let prompt = format!("{SPEECH_PROMPT} @{}", wav.path().display());
            run_cli("gemini", &["-p", &prompt], None).map(|text| text.trim().to_owned())
        }
        engine => {
            let key = key(engine.key_account())?;
            let model = engines.speech_model();
            let samples = crate::transcribe::samples(media)?;
            let mut parts = Vec::new();
            for chunk in samples.chunks(CHUNK_SECONDS * 16_000) {
                let wav = wav_bytes(chunk);
                let text = match engine {
                    SpeechEngine::Gemini => gemini(&key, &model, SPEECH_PROMPT, "audio/wav", &wav)?,
                    SpeechEngine::Groq => openai_transcription(
                        "https://api.groq.com/openai/v1/audio/transcriptions",
                        &key,
                        &model,
                        wav,
                    )?,
                    _ => openai_transcription(
                        "https://api.openai.com/v1/audio/transcriptions",
                        &key,
                        &model,
                        wav,
                    )?,
                };
                parts.push(text.trim().to_owned());
            }
            Ok(parts.join(" "))
        }
    }
}

/// Reads a picture with the chosen engine. Blocking.
pub fn image(engines: &MediaEngines, path: &Path) -> Result<String, String> {
    let shown = path.display().to_string();
    match engines.image {
        ImageEngine::Local => crate::ocr::read_text(path),
        ImageEngine::ClaudeCode => {
            let prompt = format!("{IMAGE_PROMPT}\nLa imagen está en: {shown}");
            run_cli(
                "claude",
                &[
                    "-p",
                    &prompt,
                    "--output-format",
                    "text",
                    "--allowedTools",
                    "Read",
                ],
                None,
            )
        }
        ImageEngine::CodexCli => {
            let answer = tempfile::NamedTempFile::new().map_err(|e| e.to_string())?;
            let answer_path = answer.path().display().to_string();
            run_cli(
                "codex",
                &[
                    "exec",
                    "--skip-git-repo-check",
                    "--image",
                    &shown,
                    "--output-last-message",
                    &answer_path,
                    IMAGE_PROMPT,
                ],
                None,
            )?;
            std::fs::read_to_string(answer.path()).map_err(|e| e.to_string())
        }
        ImageEngine::GeminiCli => {
            let prompt = format!("{IMAGE_PROMPT} @{shown}");
            run_cli("gemini", &["-p", &prompt], None)
        }
        engine => {
            let key = key(engine.key_account())?;
            let model = engines.image_model();
            let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
            let mime = image_mime(path, &bytes);
            match engine {
                ImageEngine::Anthropic => anthropic_image(&key, &model, mime, &bytes),
                ImageEngine::Gemini => gemini(&key, &model, IMAGE_PROMPT, mime, &bytes),
                ImageEngine::OpenRouter => openai_image(
                    "https://openrouter.ai/api/v1/chat/completions",
                    &key,
                    &model,
                    mime,
                    &bytes,
                ),
                _ => openai_image(
                    "https://api.openai.com/v1/chat/completions",
                    &key,
                    &model,
                    mime,
                    &bytes,
                ),
            }
        }
    }
    .map(|text| text.trim().to_owned())
}

fn key(account: Option<&str>) -> Result<String, String> {
    let account = account.ok_or("this engine needs no key")?;
    crate::keys::get(account)
        .ok_or_else(|| format!("Add your {account} API key in Settings > Transcription"))
}

fn http() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| e.to_string())
}

/// An API's error message, or its JSON answer.
fn json(
    response: reqwest::blocking::Response,
    provider: &str,
) -> Result<serde_json::Value, String> {
    let status = response.status();
    let body: serde_json::Value = response
        .json()
        .map_err(|e| format!("{provider}: unreadable answer ({e})"))?;
    if !status.is_success() || body.get("error").is_some() {
        let message = body["error"]["message"]
            .as_str()
            .or_else(|| body["error"].as_str())
            .unwrap_or("request refused");
        return Err(format!("{provider}: {message}"));
    }
    Ok(body)
}

fn openai_transcription(url: &str, key: &str, model: &str, wav: Vec<u8>) -> Result<String, String> {
    let part = reqwest::blocking::multipart::Part::bytes(wav)
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| e.to_string())?;
    let form = reqwest::blocking::multipart::Form::new()
        .text("model", model.to_owned())
        .text("response_format", "json")
        .part("file", part);
    let response = http()?
        .post(url)
        .bearer_auth(key)
        .multipart(form)
        .send()
        .map_err(|e| e.to_string())?;
    Ok(json(response, "transcription API")?["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned())
}

fn gemini(
    key: &str,
    model: &str,
    prompt: &str,
    mime: &str,
    bytes: &[u8],
) -> Result<String, String> {
    let body = serde_json::json!({
        "contents": [{"parts": [
            {"text": prompt},
            {"inline_data": {"mime_type": mime, "data": base64::engine::general_purpose::STANDARD.encode(bytes)}}
        ]}]
    });
    let response = http()?
        .post(format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent"
        ))
        .header("x-goog-api-key", key)
        .json(&body)
        .send()
        .map_err(|e| e.to_string())?;
    let answer = json(response, "Gemini")?;
    Ok(answer["candidates"][0]["content"]["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|part| part["text"].as_str())
        .collect::<Vec<_>>()
        .join(""))
}

fn anthropic_image(key: &str, model: &str, mime: &str, bytes: &[u8]) -> Result<String, String> {
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 2000,
        "messages": [{"role": "user", "content": [
            {"type": "image", "source": {"type": "base64", "media_type": mime,
                "data": base64::engine::general_purpose::STANDARD.encode(bytes)}},
            {"type": "text", "text": IMAGE_PROMPT}
        ]}]
    });
    let response = http()?
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .map_err(|e| e.to_string())?;
    let answer = json(response, "Anthropic")?;
    Ok(answer["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join(""))
}

fn openai_image(
    url: &str,
    key: &str,
    model: &str,
    mime: &str,
    bytes: &[u8],
) -> Result<String, String> {
    let data = format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    );
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": IMAGE_PROMPT},
            {"type": "image_url", "image_url": {"url": data}}
        ]}]
    });
    let response = http()?
        .post(url)
        .bearer_auth(key)
        .json(&body)
        .send()
        .map_err(|e| e.to_string())?;
    Ok(
        json(response, "image API")?["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    )
}

fn image_mime(path: &Path, bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("heic"))
    {
        "image/heic"
    } else {
        "image/jpeg"
    }
}

/// 16-bit mono WAV at 16 kHz.
fn wav_bytes(samples: &[f32]) -> Vec<u8> {
    let data = samples.len() * 2;
    let mut out = Vec::with_capacity(44 + data);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((36 + data) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&16_000u32.to_le_bytes());
    out.extend_from_slice(&32_000u32.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data as u32).to_le_bytes());
    for sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

fn temporary_wav(samples: &[f32]) -> Result<tempfile::NamedTempFile, String> {
    let mut file = tempfile::Builder::new()
        .suffix(".wav")
        .tempfile()
        .map_err(|e| e.to_string())?;
    file.write_all(&wav_bytes(samples))
        .map_err(|e| e.to_string())?;
    Ok(file)
}

/// Where a command-line tool is installed, including the copies Whats Pro
/// installs itself (src/ai_tools.rs).
pub fn find_cli(name: &str) -> Option<PathBuf> {
    crate::ai_tools::find(name)
}

fn run_cli(name: &str, args: &[&str], stdin: Option<&str>) -> Result<String, String> {
    let binary =
        find_cli(name).ok_or_else(|| format!("{name} is not installed on this computer"))?;
    // The tool may be a script that needs node and friends.
    let path = crate::ai_tools::path();
    let mut child = std::process::Command::new(&binary)
        .args(args)
        .env("PATH", path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("{name}: {e}"))?;
    if let Some(input) = stdin
        && let Some(mut pipe) = child.stdin.take()
    {
        let _ = pipe.write_all(input.as_bytes());
    }
    drop(child.stdin.take());
    let output = child
        .wait_with_output()
        .map_err(|e| format!("{name}: {e}"))?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        let line = error
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("");
        return Err(format!("{name} stopped ({}): {line}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wav_header_matches_its_samples() {
        let wav = wav_bytes(&[0.0, 1.0, -1.0]);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(wav.len(), 44 + 6);
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 6);
        assert_eq!(i16::from_le_bytes([wav[46], wav[47]]), i16::MAX);
    }

    #[test]
    fn engines_default_to_this_computer_and_name_their_models() {
        let engines = MediaEngines::default();
        assert_eq!(engines.speech, SpeechEngine::Local);
        assert_eq!(engines.image, ImageEngine::Local);
        let groq = MediaEngines {
            speech: SpeechEngine::Groq,
            ..MediaEngines::default()
        };
        assert_eq!(groq.speech_name(), "Groq:whisper-large-v3-turbo");
        let parsed: MediaEngines =
            serde_json::from_str(r#"{"speech":"open_ai","image":"claude_code"}"#).unwrap();
        assert_eq!(parsed.speech, SpeechEngine::OpenAi);
        assert_eq!(parsed.image, ImageEngine::ClaudeCode);
    }

    #[test]
    fn pictures_are_sniffed() {
        assert_eq!(image_mime(Path::new("a.jpg"), b"\x89PNG...."), "image/png");
        assert_eq!(
            image_mime(Path::new("a.jpg"), b"\xff\xd8\xff"),
            "image/jpeg"
        );
    }
}
