//! Воспроизведение PCM на выходном устройстве операционной системы.
use crate::{Error, Result, StereoFrame};
use rodio::{
    OutputStream, OutputStreamHandle, Sink,
    cpal::{
        self,
        traits::{DeviceTrait, HostTrait},
    },
};
pub struct NativePlayer {
    _stream: OutputStream,
    _handle: OutputStreamHandle,
    pub sink: Sink,
}
pub fn devices() -> Result<Vec<String>> {
    cpal::default_host()
        .output_devices()
        .map_err(|e| Error::Backend(e.to_string()))?
        .map(|d| d.name().map_err(|e| Error::Backend(e.to_string())))
        .collect()
}
impl NativePlayer {
    pub fn open(name: Option<&str>) -> Result<Self> {
        let host = cpal::default_host();
        let device = if let Some(name) = name {
            host.output_devices()
                .map_err(|e| Error::Backend(e.to_string()))?
                .find(|d| d.name().ok().as_deref() == Some(name))
        } else {
            host.default_output_device()
        }
        .ok_or(Error::Invalid("output device unavailable"))?;
        let (stream, handle) =
            OutputStream::try_from_device(&device).map_err(|e| Error::Backend(e.to_string()))?;
        let sink = Sink::try_new(&handle).map_err(|e| Error::Backend(e.to_string()))?;
        Ok(Self {
            _stream: stream,
            _handle: handle,
            sink,
        })
    }
    pub fn append(&self, rate: u32, frames: Vec<StereoFrame>) -> Result<()> {
        crate::sample_rate(rate)?;
        crate::validate_frames(&frames)?;
        self.sink.append(rodio::buffer::SamplesBuffer::new(
            2,
            rate,
            frames.into_iter().flatten().collect::<Vec<_>>(),
        ));
        Ok(())
    }
}

/// Блокирующий захват до 30 секунд с выбранного микрофона. Вызывать в spawn_blocking.
pub fn record_microphone(
    seconds: u32,
    device_name: Option<&str>,
) -> Result<crate::decode::DecodedAudio> {
    use cpal::traits::StreamTrait;
    if !(1..=30).contains(&seconds) {
        return Err(Error::Invalid("microphone duration"));
    }
    let host = cpal::default_host();
    let device = if let Some(name) = device_name {
        host.input_devices()
            .map_err(|e| Error::Backend(e.to_string()))?
            .find(|d| d.name().ok().as_deref() == Some(name))
    } else {
        host.default_input_device()
    }
    .ok_or(Error::Invalid("input device unavailable"))?;
    let supported = device
        .default_input_config()
        .map_err(|e| Error::Backend(e.to_string()))?;
    let config: cpal::StreamConfig = supported.clone().into();
    let count = config.sample_rate.0 as usize * seconds as usize;
    let frames = std::sync::Arc::new(std::sync::Mutex::new(Vec::with_capacity(count)));
    let failure = std::sync::Arc::new(std::sync::Mutex::new(None));
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => {
            capture_stream::<f32>(&device, &config, frames.clone(), failure.clone(), count)
        }
        cpal::SampleFormat::I16 => {
            capture_stream::<i16>(&device, &config, frames.clone(), failure.clone(), count)
        }
        cpal::SampleFormat::U16 => {
            capture_stream::<u16>(&device, &config, frames.clone(), failure.clone(), count)
        }
        _ => return Err(Error::Invalid("unsupported microphone sample format")),
    }?;
    stream.play().map_err(|e| Error::Backend(e.to_string()))?;
    std::thread::sleep(std::time::Duration::from_secs(seconds as u64));
    drop(stream);
    if let Some(error) = failure
        .lock()
        .map_err(|_| Error::Invalid("microphone lock"))?
        .take()
    {
        return Err(Error::Backend(error));
    }
    let frames = frames
        .lock()
        .map_err(|_| Error::Invalid("microphone lock"))?
        .clone();
    Ok(crate::decode::DecodedAudio {
        sample_rate: config.sample_rate.0,
        frames,
    })
}
fn capture_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    frames: std::sync::Arc<std::sync::Mutex<Vec<StereoFrame>>>,
    failure: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    limit: usize,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    use cpal::Sample;
    let channels = config.channels as usize;
    device
        .build_input_stream(
            config,
            move |input: &[T], _| {
                if let Ok(mut frames) = frames.lock() {
                    for sample in input.chunks_exact(channels) {
                        if frames.len() >= limit {
                            break;
                        }
                        frames.push([
                            f32::from_sample(sample[0]),
                            f32::from_sample(sample[usize::from(channels > 1)]),
                        ]);
                    }
                }
            },
            move |error| {
                if let Ok(mut failure) = failure.lock() {
                    *failure = Some(error.to_string());
                }
            },
            None,
        )
        .map_err(|e| Error::Backend(e.to_string()))
}
