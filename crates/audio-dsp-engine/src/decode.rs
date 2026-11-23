//! Декодирование файлов Symphonia в стерео PCM с пределом памяти.
use crate::{Error, Result, StereoFrame};
use std::{fs::File, path::Path};
use symphonia::core::{
    audio::SampleBuffer, codecs::DecoderOptions, formats::FormatOptions, io::MediaSourceStream,
    meta::MetadataOptions, probe::Hint,
};
pub struct DecodedAudio {
    pub sample_rate: u32,
    pub frames: Vec<StereoFrame>,
}
pub fn decode(path: impl AsRef<Path>, max_frames: usize) -> Result<DecodedAudio> {
    let path = path.as_ref();
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
        hint.with_extension(ext);
    }
    let file = File::open(path).map_err(|e| Error::Backend(e.to_string()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut format = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| Error::Backend(e.to_string()))?
        .format;
    let track = format
        .default_track()
        .ok_or(Error::Invalid("missing audio track"))?;
    let id = track.id;
    let rate = track
        .codec_params
        .sample_rate
        .ok_or(Error::Invalid("missing sample rate"))?;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| Error::Backend(e.to_string()))?;
    let mut frames = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(e) => return Err(Error::Backend(e.to_string())),
        };
        if packet.track_id() != id {
            continue;
        }
        let audio = decoder
            .decode(&packet)
            .map_err(|e| Error::Backend(e.to_string()))?;
        if audio.spec().rate != rate {
            return Err(Error::Invalid("sample rate changed"));
        }
        let channels = audio.spec().channels.count();
        if channels == 0 {
            return Err(Error::Invalid("missing channels"));
        }
        let mut buffer = SampleBuffer::<f32>::new(audio.capacity() as u64, *audio.spec());
        buffer.copy_interleaved_ref(audio);
        if frames.len() + buffer.samples().len() / channels > max_frames {
            return Err(Error::Invalid("decoded audio exceeds limit"));
        }
        for samples in buffer.samples().chunks_exact(channels) {
            frames.push([samples[0], samples[usize::from(channels > 1)]]);
        }
    }
    Ok(DecodedAudio {
        sample_rate: rate,
        frames,
    })
}
