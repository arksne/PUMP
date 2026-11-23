//! Потоковый тракт без выделения памяти в process: EQ → комната → громкость → сон → RMS.
use crate::{
    Result, StereoFrame,
    analysis::RollingRms,
    eq::{EqSettings, Equalizer},
    mix::SleepFade,
    range,
    reverb::{ReverbSettings, RoomReverb},
    validate_frames,
};

#[derive(Clone, Debug)]
pub struct StereoProcessor {
    rate: u32,
    eq: Equalizer,
    reverb: RoomReverb,
    reverb_enabled: bool,
    volume: f32,
    sleep: SleepFade,
    rms: RollingRms,
}
impl StereoProcessor {
    pub fn new(rate: u32) -> Result<Self> {
        Ok(Self {
            rate,
            eq: Equalizer::new(rate, EqSettings::flat(rate)?)?,
            reverb: RoomReverb::new(rate, ReverbSettings::default())?,
            reverb_enabled: false,
            volume: 1.0,
            sleep: SleepFade::new(rate)?,
            rms: RollingRms::new(rate, 0.4)?,
        })
    }
    pub fn sample_rate(&self) -> u32 {
        self.rate
    }
    pub fn set_eq(&mut self, settings: EqSettings) -> Result<()> {
        self.eq.configure(settings)
    }
    pub fn set_reverb(&mut self, settings: ReverbSettings) -> Result<()> {
        self.reverb.configure(settings)?;
        self.reverb_enabled = true;
        Ok(())
    }
    pub fn bypass_reverb(&mut self) {
        self.reverb_enabled = false;
        self.reverb.reset();
    }
    pub fn set_volume(&mut self, volume: f32) -> Result<()> {
        range(volume as f64, 0.0, 1.0, "volume")?;
        self.volume = volume;
        Ok(())
    }
    pub fn sleep_mut(&mut self) -> &mut SleepFade {
        &mut self.sleep
    }
    pub fn rms(&self) -> [f64; 2] {
        self.rms.value()
    }
    /// Сбрасывает хвосты и таймер после seek/смены источника; настройки эффектов сохраняются.
    pub fn reset(&mut self) {
        self.eq.reset();
        self.reverb.reset();
        self.rms.reset();
        self.sleep.cancel();
    }
    pub fn process(&mut self, frames: &mut [StereoFrame]) -> Result<()> {
        validate_frames(frames)?;
        for f in frames {
            *f = self.tick(*f);
        }
        Ok(())
    }
    /// Для PCM L,R,L,R…; нечётная длина и неверный PCM отклоняются до обработки.
    pub fn process_interleaved(&mut self, samples: &mut [f32]) -> Result<()> {
        if samples.len() % 2 != 0 {
            return Err(crate::Error::Invalid("interleaved stereo length"));
        }
        for &x in samples.iter() {
            range(x as f64, -16.0, 16.0, "PCM sample (-16..16, finite)")?;
        }
        for f in samples.chunks_exact_mut(2) {
            let out = self.tick([f[0], f[1]]);
            f.copy_from_slice(&out);
        }
        Ok(())
    }
    fn tick(&mut self, frame: StereoFrame) -> StereoFrame {
        let mut out = self.eq.tick(frame);
        if self.reverb_enabled {
            out = self.reverb.tick(out);
        }
        let gain = self.volume * self.sleep.next_gain();
        out[0] *= gain;
        out[1] *= gain;
        self.rms.tick(out);
        out
    }
}
