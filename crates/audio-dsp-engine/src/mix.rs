//! Переходы между треками, ограниченная нормализация и пятнадцатисекундный таймер сна.
use crate::{Result, StereoFrame, db_gain, range, sample_rate, validate_frames};
use std::f64::consts::FRAC_PI_2;

#[derive(Clone, Copy, Debug)]
pub enum CrossfadeCurve {
    Linear,
    EqualPower,
}
pub fn crossfade_gains(progress: f64, curve: CrossfadeCurve) -> Result<[f32; 2]> {
    range(progress, 0.0, 1.0, "crossfade progress")?;
    if progress == 0.0 {
        return Ok([1.0, 0.0]);
    }
    if progress == 1.0 {
        return Ok([0.0, 1.0]);
    }
    Ok(match curve {
        CrossfadeCurve::Linear => [(1.0 - progress) as f32, progress as f32],
        CrossfadeCurve::EqualPower => [
            (progress * FRAC_PI_2).cos() as f32,
            (progress * FRAC_PI_2).sin() as f32,
        ],
    })
}
/// Ограниченный gain для LUFS. None означает неизвестную громкость/тишину, gain=1.
pub fn normalization_gain(
    measured_lufs: Option<f64>,
    target_lufs: f64,
    max_gain_db: f64,
) -> Result<f32> {
    range(target_lufs, -36.0, -5.0, "target LUFS")?;
    range(max_gain_db, 0.0, 24.0, "maximum normalization gain")?;
    let Some(measured) = measured_lufs else {
        return Ok(1.0);
    };
    range(measured, -120.0, 30.0, "measured LUFS")?;
    Ok(db_gain((target_lufs - measured).clamp(-60.0, max_gain_db)) as f32)
}
pub fn automix_gains(
    progress: f64,
    outgoing_lufs: Option<f64>,
    incoming_lufs: Option<f64>,
    target_lufs: f64,
) -> Result<[f32; 2]> {
    let fades = crossfade_gains(progress, CrossfadeCurve::EqualPower)?;
    Ok([
        fades[0] * normalization_gain(outgoing_lufs, target_lufs, 12.0)?,
        fades[1] * normalization_gain(incoming_lufs, target_lufs, 12.0)?,
    ])
}

#[derive(Clone, Debug)]
pub struct Crossfade {
    length: usize,
    position: usize,
    curve: CrossfadeCurve,
}
impl Crossfade {
    pub fn new(rate: u32, seconds: f64, curve: CrossfadeCurve) -> Result<Self> {
        sample_rate(rate)?;
        range(
            seconds,
            1.0 / rate as f64,
            60.0,
            "crossfade duration (one frame..60 s)",
        )?;
        Ok(Self {
            length: (rate as f64 * seconds).round() as usize,
            position: 0,
            curve,
        })
    }
    /// После завершения перехода пропускает только incoming. Все буферы равной длины.
    pub fn process(
        &mut self,
        outgoing: &[StereoFrame],
        incoming: &[StereoFrame],
        output: &mut [StereoFrame],
    ) -> Result<()> {
        if outgoing.len() != incoming.len() || output.len() != incoming.len() {
            return Err(crate::Error::Invalid("crossfade buffer lengths"));
        }
        validate_frames(outgoing)?;
        validate_frames(incoming)?;
        for ((a, b), out) in outgoing.iter().zip(incoming).zip(output) {
            let progress = if self.length <= 1 {
                1.0
            } else {
                self.position.min(self.length - 1) as f64 / (self.length - 1) as f64
            };
            let gain = crossfade_gains(progress, self.curve)?;
            *out = [
                a[0] * gain[0] + b[0] * gain[1],
                a[1] * gain[0] + b[1] * gain[1],
            ];
            self.position = self.position.saturating_add(1).min(self.length);
        }
        Ok(())
    }
    pub fn is_finished(&self) -> bool {
        self.position >= self.length
    }
}

pub const SLEEP_FADE_SECONDS: u32 = 15;
#[derive(Clone, Debug)]
pub struct SleepFade {
    rate: u32,
    remaining: Option<u64>,
    fade_frames: u64,
}
impl SleepFade {
    pub fn new(rate: u32) -> Result<Self> {
        sample_rate(rate)?;
        Ok(Self {
            rate,
            remaining: None,
            fade_frames: rate as u64 * SLEEP_FADE_SECONDS as u64,
        })
    }
    /// Останавливает звук через seconds; последние min(15, seconds) секунд — линейный fade.
    /// Для короткого таймера fade начинается с единицы, без скачка громкости.
    pub fn arm(&mut self, seconds: f64) -> Result<()> {
        range(seconds, 0.0, 86400.0, "sleep duration (0..86400 s)")?;
        let n = (seconds * self.rate as f64).round() as u64;
        self.remaining = Some(n);
        self.fade_frames = n.min(self.rate as u64 * SLEEP_FADE_SECONDS as u64);
        Ok(())
    }
    pub fn cancel(&mut self) {
        self.remaining = None;
    }
    pub fn is_finished(&self) -> bool {
        self.remaining == Some(0)
    }
    pub fn next_gain(&mut self) -> f32 {
        let Some(remaining) = self.remaining else {
            return 1.0;
        };
        if remaining == 0 {
            return 0.0;
        }
        self.remaining = Some(remaining - 1);
        if remaining > self.fade_frames {
            1.0
        } else if self.fade_frames <= 1 {
            0.0
        } else {
            (remaining - 1) as f32 / (self.fade_frames - 1) as f32
        }
    }
    pub fn process(&mut self, frames: &mut [StereoFrame]) -> Result<()> {
        validate_frames(frames)?;
        for f in frames {
            let gain = self.next_gain();
            f[0] *= gain;
            f[1] *= gain;
        }
        Ok(())
    }
}

/// Анализирует обе записи, обрезает крайние паузы и смешивает их в общем PCM.
pub fn auto_mix(
    outgoing: &[StereoFrame],
    incoming: &[StereoFrame],
    rate: u32,
    seconds: f64,
) -> Result<Vec<StereoFrame>> {
    range(seconds, 0.0, 30.0, "automix overlap")?;
    let a = crate::analysis::analyze_track(outgoing, rate)?;
    let b = crate::analysis::analyze_track(incoming, rate)?;
    let outgoing = &outgoing[a.start_frame..a.end_frame];
    let incoming = &incoming[b.start_frame..b.end_frame];
    let overlap = ((rate as f64 * seconds).round() as usize)
        .min(outgoing.len())
        .min(incoming.len());
    let mut output = Vec::with_capacity(outgoing.len() + incoming.len() - overlap);
    let outgoing_gain = normalization_gain(a.integrated_lufs, -14.0, 12.0)?;
    let incoming_gain = normalization_gain(b.integrated_lufs, -14.0, 12.0)?;
    output.extend(
        outgoing[..outgoing.len() - overlap]
            .iter()
            .map(|f| [f[0] * outgoing_gain, f[1] * outgoing_gain]),
    );
    for i in 0..overlap {
        let progress = if overlap <= 1 {
            1.0
        } else {
            i as f64 / (overlap - 1) as f64
        };
        let gains = automix_gains(progress, a.integrated_lufs, b.integrated_lufs, -14.0)?;
        let left = outgoing[outgoing.len() - overlap + i];
        let right = incoming[i];
        output.push([
            left[0] * gains[0] + right[0] * gains[1],
            left[1] * gains[0] + right[1] * gains[1],
        ]);
    }
    output.extend(
        incoming[overlap..]
            .iter()
            .map(|f| [f[0] * incoming_gain, f[1] * incoming_gain]),
    );
    Ok(output)
}
