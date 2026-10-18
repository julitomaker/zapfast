//! What this computer can do for transcription, and which engines it
//! suggests: Whisper here when the chip and memory run it well, an API when
//! they would make it slow, and an AI command-line tool for pictures when
//! one is installed (it uses the person's own subscription).

use std::path::Path;

use crate::media_ai::{ImageEngine, SpeechEngine};

/// The model, its working memory and some room to spare.
const DISK_NEEDED: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct Machine {
    pub chip: String,
    pub apple_silicon: bool,
    pub memory_gb: u64,
    pub cores: usize,
    /// Free space where the model would be stored, in bytes.
    pub free_disk: Option<u64>,
    /// The AI command-line tools found (claude, codex, gemini).
    pub clis: Vec<&'static str>,
    pub model_ready: bool,
}

impl Machine {
    /// Reads this computer's chip, memory, cores, free disk and AI tools.
    pub fn detect(state_dir: &Path) -> Self {
        let sysctl = |name: &str| {
            std::process::Command::new("/usr/sbin/sysctl")
                .args(["-n", name])
                .output()
                .ok()
                .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        let memory = sysctl("hw.memsize")
            .and_then(|bytes| bytes.parse::<u64>().ok())
            .unwrap_or(0);
        let apple_silicon = cfg!(target_arch = "aarch64")
            || sysctl("hw.optional.arm64").is_some_and(|value| value == "1");
        Self {
            chip: sysctl("machdep.cpu.brand_string")
                .unwrap_or_else(|| if apple_silicon { "Apple" } else { "Intel" }.to_owned()),
            apple_silicon,
            memory_gb: (memory + (1 << 29)) >> 30,
            cores: std::thread::available_parallelism().map_or(1, usize::from),
            free_disk: free_space(state_dir),
            clis: ["claude", "codex", "gemini"]
                .into_iter()
                .filter(|name| crate::media_ai::find_cli(name).is_some())
                .collect(),
            model_ready: crate::transcribe::model_ready(state_dir),
        }
    }

    /// One line: "Apple M5 · 16 GB · 230 GB free".
    pub fn summary(&self, free_word: &str) -> String {
        let mut parts = vec![self.chip.clone()];
        if self.memory_gb > 0 {
            parts.push(format!("{} GB", self.memory_gb));
        }
        if let Some(free) = self.free_disk {
            parts.push(format!("{} GB {free_word}", free >> 30));
        }
        parts.join(" · ")
    }

    fn room_for_model(&self) -> bool {
        self.model_ready || self.free_disk.is_none_or(|free| free >= DISK_NEEDED)
    }
}

/// What to suggest, and why, as message ids for gettext.
#[derive(Clone, Debug, PartialEq)]
pub struct Advice {
    pub speech: SpeechEngine,
    pub speech_reason: &'static str,
    pub image: ImageEngine,
    pub image_reason: &'static str,
}

pub fn advise(machine: &Machine) -> Advice {
    let (speech, speech_reason) = if !machine.room_for_model() {
        (
            SpeechEngine::Gemini,
            "There is not enough free disk for the local model (about 2 GB). Gemini transcribes in the cloud, free with your Google account.",
        )
    } else if machine.apple_silicon && machine.memory_gb >= 8 {
        (
            SpeechEngine::Local,
            "Your Mac's Apple chip runs Whisper on its GPU: fast, free and private. Nothing leaves this computer.",
        )
    } else if machine.apple_silicon {
        (
            SpeechEngine::Gemini,
            "With less than 8 GB of memory the local model would slow your Mac down. Gemini transcribes in the cloud, free with your Google account.",
        )
    } else if machine.cores >= 8 && machine.memory_gb >= 16 {
        (
            SpeechEngine::Local,
            "Your Intel Mac can run Whisper on its processor, free and private, though slower than on an Apple chip. If it feels slow, switch to Gemini.",
        )
    } else {
        (
            SpeechEngine::Gemini,
            "On this Intel Mac the local model would be slow. Gemini transcribes in the cloud in seconds, free with your Google account.",
        )
    };
    let (image, image_reason) = match machine.clis.first() {
        Some(&"claude") => (
            ImageEngine::ClaudeCode,
            "Claude Code is installed: it describes pictures and reads their text with your Claude subscription.",
        ),
        Some(&"codex") => (
            ImageEngine::CodexCli,
            "Codex is installed: it describes pictures and reads their text with your ChatGPT subscription.",
        ),
        Some(_) => (
            ImageEngine::GeminiCli,
            "Gemini CLI is installed: it describes pictures and reads their text with your Google account.",
        ),
        None => (
            ImageEngine::Local,
            "macOS reads the text in pictures on this computer, free. Install Claude Code, Codex or Gemini CLI to also get descriptions.",
        ),
    };
    Advice {
        speech,
        speech_reason,
        image,
        image_reason,
    }
}

/// Free bytes on the volume that holds `path` (or its nearest parent).
fn free_space(path: &Path) -> Option<u64> {
    let existing = path.ancestors().find(|p| p.exists())?;
    let output = std::process::Command::new("/bin/df")
        .args(["-k"])
        .arg(existing)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().nth(1)?;
    let available: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
    Some(available * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(apple: bool, memory: u64, cores: usize, free_gb: u64) -> Machine {
        Machine {
            chip: String::new(),
            apple_silicon: apple,
            memory_gb: memory,
            cores,
            free_disk: Some(free_gb << 30),
            clis: Vec::new(),
            model_ready: false,
        }
    }

    #[test]
    fn apple_silicon_with_memory_and_room_stays_local() {
        assert_eq!(
            advise(&machine(true, 16, 10, 50)).speech,
            SpeechEngine::Local
        );
        assert_eq!(advise(&machine(true, 8, 8, 50)).speech, SpeechEngine::Local);
    }

    #[test]
    fn small_or_old_or_full_machines_go_to_an_api() {
        assert_eq!(
            advise(&machine(true, 4, 8, 50)).speech,
            SpeechEngine::Gemini
        );
        assert_eq!(
            advise(&machine(false, 8, 4, 50)).speech,
            SpeechEngine::Gemini
        );
        assert_eq!(
            advise(&machine(true, 16, 10, 1)).speech,
            SpeechEngine::Gemini
        );
        assert_eq!(
            advise(&machine(false, 32, 12, 50)).speech,
            SpeechEngine::Local
        );
        let mut ready = machine(true, 16, 10, 1);
        ready.model_ready = true;
        assert_eq!(advise(&ready).speech, SpeechEngine::Local);
    }

    #[test]
    fn an_installed_cli_reads_pictures() {
        let mut with_claude = machine(true, 16, 10, 50);
        assert_eq!(advise(&with_claude).image, ImageEngine::Local);
        with_claude.clis = vec!["claude"];
        assert_eq!(advise(&with_claude).image, ImageEngine::ClaudeCode);
    }
}
