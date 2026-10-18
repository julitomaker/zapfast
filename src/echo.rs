//! Echo cancellation for calls on a speaker: the other side's voice comes out
//! of the speaker, reaches the microphone and would go back to them (and into
//! the recording) a moment later.
//!
//! WebRTC's AEC3, through `sonora` (a pure Rust port, so it builds the same on
//! Apple Silicon and Intel), learns that path: every frame played is handed
//! over as the reference (`played`), and every frame recorded is cleaned
//! against it (`heard`) before the call and the tape get it. Noise
//! suppression and a high-pass filter go with it, as in WebRTC calls.
//!
//! One canceller serves one call, shared by its microphone and speaker pumps.
//! While the other side has been silent for a while there is no echo to take
//! out, and the microphone frame goes through untouched.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use sonora::{
    AudioProcessing, Config, StreamConfig,
    config::{EchoCanceller, HighPassFilter, NoiseSuppression, NoiseSuppressionLevel},
};

/// The engine's clock (see `call_audio::RATE`).
const RATE: usize = 16_000;
/// WebRTC processes 10 ms at a time.
const CHUNK: usize = RATE / 100;
/// How long after the other side last made a sound its echo may still arrive.
const TAIL: Duration = Duration::from_millis(1_500);
/// A played frame quieter than this (peak) is silence, not something to echo.
const AUDIBLE: i16 = 200;

struct State {
    apm: AudioProcessing,
    /// When the other side last made a sound.
    last_played: Option<Instant>,
}

/// The canceller and the played frames waiting for it. Kept apart so the
/// speaker side only ever touches a short queue, never the canceller.
struct Shared {
    state: Mutex<State>,
    played: Mutex<VecDeque<Vec<i16>>>,
}

/// One call's echo canceller; clones share it. `Echo::off()` does nothing.
#[derive(Clone, Default)]
pub struct Echo(Option<Arc<Shared>>);

impl Echo {
    /// A canceller for a new call.
    pub fn new() -> Self {
        let config = Config {
            echo_canceller: Some(EchoCanceller::default()),
            noise_suppression: Some(NoiseSuppression {
                level: NoiseSuppressionLevel::Low,
                ..NoiseSuppression::default()
            }),
            high_pass_filter: Some(HighPassFilter::default()),
            ..Default::default()
        };
        let mut apm = AudioProcessing::builder()
            .config(config)
            .capture_config(StreamConfig::new(RATE as u32, 1))
            .render_config(StreamConfig::new(RATE as u32, 1))
            .build();
        // Played frames are handed over as the device plays them, so what is
        // left is the device's own output and input latency; AEC3 finds the
        // exact delay itself.
        let _ = apm.set_stream_delay_ms(40);
        Self(Some(Arc::new(Shared {
            state: Mutex::new(State {
                apm,
                last_played: None,
            }),
            played: Mutex::new(VecDeque::new()),
        })))
    }

    /// No cancellation, for streams outside a call (and tests).
    pub fn off() -> Self {
        Self(None)
    }

    /// Whether this canceller does anything.
    pub fn is_on(&self) -> bool {
        self.0.is_some()
    }

    /// Hands over a frame as it starts to play (the other side). Cheap and
    /// never waits: it may run on the audio device's own thread. If the
    /// queue is busy that instant, the frame is skipped rather than waited for.
    pub fn played(&self, frame: &[i16]) {
        let Some(shared) = &self.0 else {
            return;
        };
        if let Ok(mut queue) = shared.played.try_lock() {
            // A microphone that stopped reading must not let this grow.
            if queue.len() > 200 {
                queue.pop_front();
            }
            queue.push_back(frame.to_vec());
        }
    }

    /// Cleans a microphone frame in place of the other side's echo. Frames
    /// are processed in 10 ms pieces; a trailing piece shorter than that is
    /// left as it is.
    pub fn heard(&self, frame: &mut [i16]) {
        let Some(shared) = &self.0 else {
            return;
        };
        let played: Vec<Vec<i16>> = shared
            .played
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .drain(..)
            .collect();
        let mut state = shared.state.lock().unwrap_or_else(|p| p.into_inner());
        // What was played since the last microphone frame goes in first, in
        // order, as AEC3 expects.
        let mut scratch = [0i16; CHUNK];
        let mut pending: Vec<i16> = Vec::new();
        for frame in played {
            if frame
                .iter()
                .any(|sample| sample.unsigned_abs() >= AUDIBLE.unsigned_abs())
            {
                state.last_played = Some(Instant::now());
            }
            pending.extend_from_slice(&frame);
        }
        for chunk in pending.chunks(CHUNK) {
            if chunk.len() == CHUNK {
                let _ = state.apm.process_render_i16(chunk, &mut scratch);
            }
        }
        let echo_possible = state.last_played.is_some_and(|at| at.elapsed() < TAIL);
        let mut cleaned = [0i16; CHUNK];
        for chunk in frame.chunks_mut(CHUNK) {
            if chunk.len() != CHUNK {
                continue;
            }
            // Always run, so the filter keeps following the room; only use
            // the result while an echo can be there.
            if state.apm.process_capture_i16(chunk, &mut cleaned).is_ok() && echo_possible {
                chunk.copy_from_slice(&cleaned);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(len: usize, start: usize) -> Vec<i16> {
        (0..len)
            .map(|i| {
                let t = (start + i) as f32 / RATE as f32;
                ((t * 440.0 * std::f32::consts::TAU).sin() * 8_000.0) as i16
            })
            .collect()
    }

    fn energy(frame: &[i16]) -> f64 {
        frame.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / frame.len() as f64
    }

    #[test]
    fn off_and_silence_leave_the_microphone_alone() {
        let mut frame = tone(960, 0);
        let before = frame.clone();
        Echo::off().heard(&mut frame);
        assert_eq!(frame, before);
        let echo = Echo::new();
        echo.played(&vec![0; 320]);
        echo.heard(&mut frame);
        assert_eq!(
            frame, before,
            "nothing was played, so there is no echo to take out"
        );
    }

    #[test]
    fn an_echo_of_what_was_played_is_reduced() {
        let echo = Echo::new();
        let mut first = 0.0;
        let mut last = 0.0;
        // Four seconds: the far end plays a tone, and the microphone hears it
        // back, quieter and 40 ms late, as from a speaker.
        let delay = 640;
        let mut played_so_far: Vec<i16> = vec![0; delay];
        for frame_index in 0..(4 * RATE / 960) {
            let start = frame_index * 960;
            let far = tone(960, start);
            for piece in far.chunks(320) {
                echo.played(piece);
            }
            played_so_far.extend_from_slice(&far);
            let mut mic: Vec<i16> = played_so_far[start..start + 960]
                .iter()
                .map(|s| s / 3)
                .collect();
            let raw = energy(&mic);
            echo.heard(&mut mic);
            let ratio = energy(&mic) / raw.max(1.0);
            if frame_index == 2 {
                first = ratio;
            }
            last = ratio;
        }
        assert!(
            last < 0.25,
            "the echo is mostly gone once learnt: {first} -> {last}"
        );
    }
    #[test]
    fn your_voice_survives_while_the_other_side_talks() {
        let echo = Echo::new();
        let delay = 640;
        let mut played_so_far: Vec<i16> = vec![0; delay];
        let mut kept = Vec::new();
        for frame_index in 0..(6 * RATE / 960) {
            let start = frame_index * 960;
            let far = tone(960, start);
            for piece in far.chunks(320) {
                echo.played(piece);
            }
            played_so_far.extend_from_slice(&far);
            // From the third second on, you speak (a lower tone) over the echo.
            let speaking = frame_index >= 3 * RATE / 960;
            let near: Vec<i16> = (0..960)
                .map(|i| {
                    if !speaking {
                        return 0;
                    }
                    let t = (start + i) as f32 / RATE as f32;
                    ((t * 220.0 * std::f32::consts::TAU).sin() * 6_000.0) as i16
                })
                .collect();
            let mut mic: Vec<i16> = played_so_far[start..start + 960]
                .iter()
                .zip(&near)
                .map(|(echo, voice)| echo / 3 + voice)
                .collect();
            echo.heard(&mut mic);
            if speaking && frame_index >= 4 * RATE / 960 {
                kept.push(energy(&mic) / energy(&near).max(1.0));
            }
        }
        let average = kept.iter().sum::<f64>() / kept.len() as f64;
        println!("your voice kept while both talk: {average:.3}");
        assert!(average > 0.2, "your own voice must get through: {average}");
    }
}
