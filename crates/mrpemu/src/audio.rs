//! cpal-backed audio output for the standalone frontend.
//!
//! The core hands decoded PCM clips to [`CpalSink`]; a cpal output stream
//! mixes the looping background clip into the device buffer, resampling by
//! nearest-neighbour when the device rate differs from the clip rate.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use mrpemu_core::audio::{AudioSink, PcmClip};

/// The device configuration shared with the callback.
#[derive(Clone, Copy, Default)]
struct DeviceConfig {
    rate: f64,
    channels: usize,
}

/// The playback state guarded by the device callback.
#[derive(Default)]
struct Shared {
    device: DeviceConfig,
    /// The looping background clip, if one is playing.
    bgm: Option<BgmState>,
}

struct BgmState {
    samples: Arc<Vec<i16>>,
    channels: usize,
    /// The clip's sample rate.
    rate: f64,
    /// The playback position in clip frames; fractional because the device
    /// rate usually differs.
    pos: f64,
    looped: bool,
}

impl BgmState {
    /// Mixes the bgm into the (already cleared) output buffer.
    fn mix(&mut self, out: &mut [f32], device: DeviceConfig) {
        let out_channels = device.channels.max(1);
        let out_frames = out.len() / out_channels;
        let step = self.rate / device.rate.max(1.0);
        let src_channels = self.channels.max(1);
        let total_frames = self.samples.len() / src_channels;
        for frame in 0..out_frames {
            if self.pos >= total_frames as f64 {
                if !self.looped || total_frames == 0 {
                    break;
                }
                self.pos -= total_frames as f64;
            }
            let index = self.pos as usize;
            for channel in 0..out_channels {
                let source = if src_channels == 2 {
                    index * 2 + channel.min(1)
                } else {
                    index * src_channels
                };
                let sample = self.samples.get(source).copied().unwrap_or(0);
                out[frame * out_channels + channel] += sample as f32 / 32768.0;
            }
            self.pos += step;
        }
    }
}

/// The cpal-backed sink installed into the emulated machine.
pub struct CpalSink {
    shared: Arc<Mutex<Shared>>,
    /// Kept alive so the device callback keeps running.
    _stream: cpal::Stream,
}

impl CpalSink {
    /// Opens the default output device and starts its stream.
    pub fn open() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .context("no audio output device found")?;
        let config = device
            .default_output_config()
            .context("audio device has no default output configuration")?;
        eprintln!(
            "audio: output device '{}' ({} Hz, {} channels)",
            device,
            config.sample_rate(),
            config.channels()
        );
        let device_config = DeviceConfig {
            rate: config.sample_rate() as f64,
            channels: config.channels() as usize,
        };
        let shared = Arc::new(Mutex::new(Shared {
            device: device_config,
            bgm: None,
        }));
        let callback_shared = Arc::clone(&shared);
        let stream = device
            .build_output_stream(
                config.config(),
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    mix_callback(data, &callback_shared);
                },
                move |error| log::warn!("audio stream error: {error}"),
                None,
            )
            .context("failed to build the audio output stream")?;
        stream.play().context("failed to start the audio stream")?;
        Ok(Self {
            shared,
            _stream: stream,
        })
    }
}

/// The device callback: clears the buffer, then mixes the looping bgm.
fn mix_callback(out: &mut [f32], shared: &Mutex<Shared>) {
    for sample in out.iter_mut() {
        *sample = 0.0;
    }
    let Ok(mut guard) = shared.lock() else {
        return;
    };
    let device = guard.device;
    if let Some(state) = guard.bgm.as_mut() {
        state.mix(out, device);
    }
}

impl AudioSink for CpalSink {
    fn play_bgm(&mut self, clip: PcmClip, looped: bool) {
        let Ok(mut guard) = self.shared.lock() else {
            return;
        };
        guard.bgm = Some(BgmState {
            samples: Arc::new(clip.samples),
            channels: clip.channels as usize,
            rate: clip.sample_rate as f64,
            pos: 0.0,
            looped,
        });
    }

    fn play_effect(&mut self, clip: PcmClip) {
        // One-shot effects share the bgm channel: the most recent one wins,
        // which matches the feature-phone single-channel audio behaviour.
        self.play_bgm(clip, false);
    }

    fn stop_bgm(&mut self) {
        if let Ok(mut guard) = self.shared.lock() {
            guard.bgm = None;
        }
    }
}
