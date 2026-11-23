//! Звуковое ядро PUMP: стереокадры, потоковые эффекты и автономный анализ.
//! Значения PCM конечны и лежат в [-16, 16]; запас нужен для обработки выше 0 dBFS.
//! Ядро не ограничивает выход клиппером: запас громкости обеспечивает вызывающий код.

pub mod analysis;
#[cfg(feature = "decode")]
pub mod decode;
pub mod eq;
pub mod mix;
#[cfg(all(feature = "native-output", not(target_arch = "wasm32")))]
pub mod native;
pub mod queue;
pub mod reverb;
pub mod stream;
#[cfg(feature = "time-stretch")]
pub mod stretch;

pub type StereoFrame = [f32; 2];
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid {0}")]
    Invalid(&'static str),
    #[error("AutoEq line {line}: {message}")]
    AutoEq { line: usize, message: String },
    #[error("audio backend: {0}")]
    Backend(String),
}

pub(crate) fn range(value: f64, min: f64, max: f64, name: &'static str) -> Result<()> {
    if !value.is_finite() || !(min..=max).contains(&value) {
        return Err(Error::Invalid(name));
    }
    Ok(())
}

pub(crate) fn sample_rate(rate: u32) -> Result<()> {
    if !(8_000..=192_000).contains(&rate) {
        return Err(Error::Invalid("sample rate (8000..192000 Hz)"));
    }
    Ok(())
}

pub(crate) fn validate_frames(frames: &[StereoFrame]) -> Result<()> {
    for frame in frames {
        for &sample in frame {
            range(sample as f64, -16.0, 16.0, "PCM sample (-16..16, finite)")?;
        }
    }
    Ok(())
}

pub(crate) fn db_gain(db: f64) -> f64 {
    10.0_f64.powf(db / 20.0)
}

#[cfg(feature = "dlna")]
pub mod dlna;

pub mod wasm;
