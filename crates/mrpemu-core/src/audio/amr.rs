use amr_nb::{rfc4867, Decoder};

use super::PcmClip;

const AMR_LICENSE: &str = include_str!("../../assets/AmrNb_License.txt");
const FRAME_SAMPLES: usize = 160;
// A no-data frame is one byte on disk but expands to 320 bytes of PCM.
const MAX_FRAMES: usize = 8 * 1024 * 1024 / (FRAME_SAMPLES * size_of::<i16>());

#[derive(Debug, thiserror::Error)]
pub(crate) enum AmrError {
    #[error(transparent)]
    Format(#[from] rfc4867::Error),
    #[error("AMR-NB clip has no frames")]
    Empty,
    #[error("AMR-NB clip exceeds the decoded size limit")]
    TooLong,
}

pub(crate) fn decode_amr(data: &[u8]) -> Result<PcmClip, AmrError> {
    let frames = rfc4867::read_storage(data)?;
    log::trace!("AMR-NB decoder license:\n{AMR_LICENSE}");
    let mut decoder = Decoder::new();
    let mut samples = Vec::new();
    let mut output = [0; FRAME_SAMPLES];
    for (index, frame) in frames.enumerate() {
        if index >= MAX_FRAMES {
            return Err(AmrError::TooLong);
        }
        let frame = frame?;
        decoder.decode(frame.frame_type, frame.mode, &frame.bits, &mut output);
        samples.extend_from_slice(&output);
    }
    if samples.is_empty() {
        return Err(AmrError::Empty);
    }
    Ok(PcmClip {
        sample_rate: 8_000,
        channels: 1,
        samples,
    })
}
