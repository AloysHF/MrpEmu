use std::io::Cursor;

use symphonia::core::errors::{Error, Result};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::TrackType;
use symphonia::core::io::MediaSourceStream;

use super::PcmClip;

pub(crate) fn decode_mp3(data: Vec<u8>) -> Result<PcmClip> {
    let stream = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("mp3");
    let mut format = symphonia::default::get_probe().probe(
        &hint,
        stream,
        Default::default(),
        Default::default(),
    )?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or(Error::DecodeError("missing MP3 audio track"))?;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|params| params.audio())
        .ok_or(Error::DecodeError("missing MP3 codec parameters"))?;
    let track_id = track.id;
    let mut decoder =
        symphonia::default::get_codecs().make_audio_decoder(params, &Default::default())?;
    let mut clip = None;

    while let Some(packet) = format.next_packet()? {
        if packet.track_id != track_id {
            continue;
        }
        let decoded = decoder.decode(&packet)?;
        let sample_rate = decoded.spec().rate();
        let channels = decoded.num_planes() as u16;
        let clip = clip.get_or_insert_with(|| PcmClip {
            sample_rate,
            channels,
            samples: Vec::new(),
        });
        if clip.sample_rate != sample_rate || clip.channels != channels {
            return Err(Error::Unsupported(
                "MP3 changed sample rate or channel count",
            ));
        }
        let start = clip.samples.len();
        clip.samples
            .resize(start + decoded.samples_interleaved(), 0);
        decoded.copy_to_slice_interleaved(&mut clip.samples[start..]);
    }

    clip.filter(|clip| !clip.samples.is_empty())
        .ok_or(Error::DecodeError("missing MP3 samples"))
}
