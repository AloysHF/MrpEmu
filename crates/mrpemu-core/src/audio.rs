//! Guest audio decoding and a shared stereo renderer for frontend sinks.

mod amr;
mod mp3;
mod player;

pub(crate) use amr::decode_amr;
pub(crate) use mp3::decode_mp3;
pub use player::AudioPlayer;

/// A decoded PCM clip (16-bit samples, interleaved channels).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcmClip {
    pub sample_rate: u32,
    pub channels: u16,
    pub samples: Vec<i16>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AudioError {
    /// Not a RIFF/WAVE container.
    NotWav,
    /// The format chunk is missing or not PCM.
    UnsupportedFormat,
    /// The container is truncated.
    Truncated,
}

/// Parses a RIFF/WAVE file into a 16-bit PCM clip. Accepts 8-bit unsigned
/// and 16-bit signed PCM inputs with one or two channels.
pub fn decode_wav(data: &[u8]) -> Result<PcmClip, AudioError> {
    if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return Err(AudioError::NotWav);
    }
    let mut pos = 12usize;
    let mut format = 0u16;
    let mut channels = 0u16;
    let mut sample_rate = 0u32;
    let mut bits = 0u16;
    let mut samples: Vec<i16> = Vec::new();
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let size = u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
            as usize;
        let body = pos + 8;
        let end = body.saturating_add(size).min(data.len());
        match id {
            b"fmt " => {
                if end < body + 16 {
                    return Err(AudioError::Truncated);
                }
                format = u16::from_le_bytes([data[body], data[body + 1]]);
                channels = u16::from_le_bytes([data[body + 2], data[body + 3]]);
                sample_rate = u32::from_le_bytes([
                    data[body + 4],
                    data[body + 5],
                    data[body + 6],
                    data[body + 7],
                ]);
                bits = u16::from_le_bytes([data[body + 14], data[body + 15]]);
            }
            b"data" => {
                if format != 1 || channels == 0 || channels > 2 {
                    return Err(AudioError::UnsupportedFormat);
                }
                match bits {
                    16 => {
                        for pair in data[body..end].as_chunks::<2>().0 {
                            samples.push(i16::from_le_bytes(*pair));
                        }
                    }
                    8 => {
                        // 8-bit WAV is unsigned; center it into 16-bit.
                        for &byte in &data[body..end] {
                            samples.push(((byte as i16) - 128) * 256);
                        }
                    }
                    _ => return Err(AudioError::UnsupportedFormat),
                }
            }
            _ => {}
        }
        pos = body + size + (size & 1);
    }
    if samples.is_empty() || sample_rate == 0 {
        return Err(AudioError::Truncated);
    }
    Ok(PcmClip {
        sample_rate,
        channels,
        samples,
    })
}

/// The guest's audio command receiver. Frontends can install an [`AudioPlayer`]
/// and pull stereo samples from a clone without sharing the guest itself.
pub trait AudioSink {
    /// Starts (or replaces) the looping background clip.
    fn play_bgm(&mut self, clip: PcmClip, looped: bool);
    /// Starts a Standard MIDI File if the sink supports MIDI.
    fn play_midi(&mut self, _data: &[u8], _looped: bool) {}
    /// Plays a one-shot effect.
    fn play_effect(&mut self, clip: PcmClip);
    /// Stops the active background audio.
    fn stop_bgm(&mut self);
    /// Sets the playback volume in percent (0-100). Sinks with shared state
    /// implement this through interior mutability.
    fn set_volume(&self, _volume: u32) {}
}

/// The default sink: discards audio (headless runs and tests).
#[derive(Default)]
pub struct NullAudioSink;

impl AudioSink for NullAudioSink {
    fn play_bgm(&mut self, _clip: PcmClip, _looped: bool) {}
    fn play_effect(&mut self, _clip: PcmClip) {}
    fn stop_bgm(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal 16-bit mono WAV: RIFF header + fmt + data.
    fn wav_bytes(samples: &[i16], sample_rate: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        let data_len = samples.len() * 2;
        out.extend_from_slice(&36u32.to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // mono
        out.extend_from_slice(&sample_rate.to_le_bytes());
        out.extend_from_slice(&(sample_rate * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data_len as u32).to_le_bytes());
        for sample in samples {
            out.extend_from_slice(&sample.to_le_bytes());
        }
        out
    }

    #[test]
    fn decodes_16bit_mono() {
        let wav = wav_bytes(&[100, -200, 300], 22050);
        let clip = decode_wav(&wav).expect("decode");
        assert_eq!(clip.sample_rate, 22050);
        assert_eq!(clip.channels, 1);
        assert_eq!(clip.samples, vec![100, -200, 300]);
    }

    #[test]
    fn converts_8bit() {
        let mut wav = wav_bytes(&[0], 8000);
        // fmt chunk body starts at 20: format(2) channels(2) rate(4)
        // byte_rate(4) align(2) bits(2). Patch to 8-bit mono.
        wav[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
        wav[22..24].copy_from_slice(&1u16.to_le_bytes()); // mono
        wav[24..28].copy_from_slice(&8000u32.to_le_bytes());
        wav[28..32].copy_from_slice(&8000u32.to_le_bytes()); // byte rate
        wav[32..34].copy_from_slice(&1u16.to_le_bytes()); // align
        wav[34..36].copy_from_slice(&8u16.to_le_bytes()); // bits
                                                          // data chunk: id at 36, size at 40, body at 44.
        assert_eq!(&wav[36..40], b"data");
        wav[40..44].copy_from_slice(&1u32.to_le_bytes());
        wav.truncate(45);
        let clip = decode_wav(&wav).expect("decode");
        assert_eq!(clip.sample_rate, 8000);
        assert_eq!(clip.samples, vec![((0u16) as i16 - 128) * 256]);
    }

    #[test]
    fn rejects_non_wav() {
        assert_eq!(
            decode_wav(b"not a wav file at all....."),
            Err(AudioError::NotWav)
        );
    }
}
