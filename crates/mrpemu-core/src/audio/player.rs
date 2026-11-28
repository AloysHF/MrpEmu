use std::sync::{Arc, Mutex, OnceLock};

use rustysynth_ext::{
    MidiFile, MidiFileSequencer, SoundFont, SoundFontError, Synthesizer, SynthesizerSettings,
};

use super::{AudioSink, PcmClip};

const SOUNDFONT_BYTES: &[u8] = include_bytes!("../../assets/FluidR3Mono_GM.sf3");
const SOUNDFONT_LICENSE: &str = include_str!("../../assets/FluidR3Mono_License.md");
static SOUNDFONT: OnceLock<Result<Arc<SoundFont>, SoundFontError>> = OnceLock::new();

/// One active guest sound, rendered as normalized stereo PCM at the output rate.
/// Clones share playback state, so one can receive guest commands while another
/// renders samples on the same thread or an audio thread.
#[derive(Clone)]
pub struct AudioPlayer {
    sample_rate: u32,
    playback: Arc<Mutex<Option<Playback>>>,
}

enum Playback {
    Wave(WavePlayback),
    Midi(Box<MidiFileSequencer>),
}

struct WavePlayback {
    clip: PcmClip,
    position: f64,
    looped: bool,
}

impl AudioPlayer {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            playback: Arc::new(Mutex::new(None)),
        }
    }

    /// Overwrites the buffer with stereo output, including silence when idle.
    pub fn render(&self, output: &mut [[f32; 2]]) {
        output.fill([0.0; 2]);
        let Ok(mut playback) = self.playback.lock() else {
            return;
        };
        match playback.as_mut() {
            Some(Playback::Wave(wave)) => wave.render(output, self.sample_rate),
            Some(Playback::Midi(sequencer)) => {
                let mut left = [0.0; 128];
                let mut right = [0.0; 128];
                for chunk in output.chunks_mut(left.len()) {
                    let frames = chunk.len();
                    sequencer.render(&mut left[..frames], &mut right[..frames]);
                    for ((frame, left), right) in chunk.iter_mut().zip(&left).zip(&right) {
                        *frame = [left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0)];
                    }
                }
            }
            None => {}
        }
    }

    fn replace(&self, playback: Option<Playback>) {
        if let Ok(mut active) = self.playback.lock() {
            *active = playback;
        }
    }
}

impl WavePlayback {
    fn render(&mut self, output: &mut [[f32; 2]], output_rate: u32) {
        let channels = usize::from(self.clip.channels);
        let frames = self.clip.samples.len() / channels;
        let step = f64::from(self.clip.sample_rate) / f64::from(output_rate);
        for frame in output {
            if self.position >= frames as f64 {
                if !self.looped {
                    break;
                }
                self.position %= frames as f64;
            }
            let source = self.position as usize * channels;
            *frame = [
                self.clip.samples[source],
                self.clip.samples[source + channels - 1],
            ]
            .map(|sample| f32::from(sample) / 32768.0);
            self.position += step;
        }
    }
}

impl AudioSink for AudioPlayer {
    fn play_bgm(&mut self, clip: PcmClip, looped: bool) {
        if !matches!(clip.channels, 1 | 2)
            || clip.samples.len() < usize::from(clip.channels)
            || clip.sample_rate == 0
            || self.sample_rate == 0
        {
            return;
        }
        self.replace(Some(Playback::Wave(WavePlayback {
            clip,
            position: 0.0,
            looped,
        })));
    }

    fn play_midi(&mut self, mut data: &[u8], looped: bool) {
        let bytes = data.len();
        // Load and prepare the sequence before locking the render state.
        let file = match MidiFile::new(&mut data) {
            Ok(file) => Arc::new(file),
            Err(error) => {
                log::warn!("MIDI playback failed: {error}");
                return;
            }
        };
        let font = match SOUNDFONT.get_or_init(|| {
            log::trace!("Embedded MIDI sound bank license:\n{SOUNDFONT_LICENSE}");
            let mut bytes = SOUNDFONT_BYTES;
            SoundFont::new(&mut bytes).map(Arc::new)
        }) {
            Ok(font) => font,
            Err(error) => {
                log::warn!("failed to load the built-in sound bank: {error}");
                return;
            }
        };
        let settings = SynthesizerSettings::new(self.sample_rate as i32);
        match Synthesizer::new(font, &settings) {
            Ok(synth) => {
                let mut sequencer = Box::new(MidiFileSequencer::new(synth));
                sequencer.play(&file, looped);
                self.replace(Some(Playback::Midi(sequencer)));
                log::debug!("playing MIDI ({bytes} bytes, loop={looped})");
            }
            Err(error) => log::warn!("failed to start the MIDI synthesizer: {error}"),
        }
    }

    fn play_effect(&mut self, clip: PcmClip) {
        self.play_bgm(clip, false);
    }

    fn stop_bgm(&mut self) {
        self.replace(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_and_loops_midi_through_the_shared_player() {
        // One note lasting a quarter note at the default tempo (0.5 s).
        let midi =
            b"MThd\0\0\0\x06\0\0\0\x01\0\x60MTrk\0\0\0\x0c\0\x90\x3c\x64\x60\x80\x3c\0\0\xff\x2f\0";
        let mut player = AudioPlayer::new(48_000);
        let renderer = player.clone();
        let mut output = vec![[0.0; 2]; 72_000];

        for looped in [false, true] {
            player.play_midi(midi, looped);
            renderer.render(&mut output);
            assert!(output[..24_000].iter().flatten().any(|s| s.abs() > 0.01));
            let state = player.playback.lock().unwrap();
            let Some(Playback::Midi(sequencer)) = state.as_ref() else {
                panic!("expected active MIDI playback");
            };
            assert_eq!(sequencer.end_of_sequence(), !looped);
        }
        player.stop_bgm();
        renderer.render(&mut output);
        assert!(output.iter().flatten().all(|&s| s == 0.0));
    }

    #[test]
    fn resamples_wav_and_preserves_stereo_and_loop_position() {
        let mut player = AudioPlayer::new(8_000);
        let clip = PcmClip {
            sample_rate: 32_000,
            channels: 2,
            samples: vec![16384, -16384, 8192, -8192, 4096, -4096],
        };
        player.play_bgm(clip.clone(), true);
        let mut output = [[0.0; 2]; 4];
        player.render(&mut output);
        assert_eq!(
            output,
            [[0.5, -0.5], [0.25, -0.25], [0.125, -0.125], [0.5, -0.5]]
        );

        player.play_effect(clip);
        player.render(&mut output);
        assert_eq!(output, [[0.5, -0.5], [0.0; 2], [0.0; 2], [0.0; 2]]);
    }
}
