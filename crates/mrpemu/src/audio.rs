//! CPAL device adapter for the shared guest audio player.

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use mrpemu_core::audio::AudioPlayer;

pub fn open() -> Result<(AudioPlayer, cpal::Stream)> {
    let device = cpal::default_host()
        .default_output_device()
        .context("no audio output device found")?;
    let config = device
        .default_output_config()
        .context("audio device has no default output configuration")?;
    let player = AudioPlayer::new(config.sample_rate());
    let stream = match config.sample_format() {
        SampleFormat::F32 => build_stream::<f32>(&device, config.config(), player.clone()),
        SampleFormat::I16 => build_stream::<i16>(&device, config.config(), player.clone()),
        SampleFormat::U16 => build_stream::<u16>(&device, config.config(), player.clone()),
        format => anyhow::bail!("unsupported audio device sample format: {format}"),
    }
    .context("failed to build the audio output stream")?;
    stream.play().context("failed to start the audio stream")?;
    log::info!(
        "audio: output device '{}' ({} Hz, {} channels)",
        device,
        config.sample_rate(),
        config.channels()
    );
    Ok((player, stream))
}

fn build_stream<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    player: AudioPlayer,
) -> Result<cpal::Stream, cpal::Error> {
    let channels = usize::from(config.channels);
    let mut stereo = [[0.0; 2]; 128];
    device.build_output_stream(
        config,
        move |output: &mut [T], _| {
            for chunk in output.chunks_mut(stereo.len() * channels) {
                let frames = chunk.len() / channels;
                player.render(&mut stereo[..frames]);
                for (frame, &[left, right]) in chunk.chunks_exact_mut(channels).zip(&stereo) {
                    match frame {
                        [mono] => *mono = T::from_sample((left + right) * 0.5),
                        [front_left, front_right, rest @ ..] => {
                            *front_left = T::from_sample(left);
                            *front_right = T::from_sample(right);
                            rest.fill(T::EQUILIBRIUM);
                        }
                        [] => unreachable!(),
                    }
                }
            }
        },
        |error| log::warn!("audio stream error: {error}"),
        None,
    )
}
