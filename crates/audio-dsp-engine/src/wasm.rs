//! Ограниченный потоковый адаптер для AudioWorklet и нативных PCM-источников.
use crate::{
    Error, Result, StereoFrame,
    eq::{BAND_COUNT, EqSettings, FilterKind},
    range,
    reverb::ReverbSettings,
    stream::StereoProcessor,
};

pub const MAX_BLOCK_FRAMES: usize = 2048;
pub const CONFIG_LEN: usize = 7 + BAND_COUNT * 5;

/// Две головки с треугольным перекрытием. Память и задержка ограничены 40 мс.
/// Это зерновой pitch-shift для живого сигнала, не автономный WSOLA-рендер.
pub struct StreamingPitch {
    ring: Vec<StereoFrame>,
    position: usize,
    window: f64,
    phase: f64,
    ratio: f64,
}

impl StreamingPitch {
    pub fn new(rate: u32) -> Result<Self> {
        crate::sample_rate(rate)?;
        let window = (rate as f64 * 0.04).round();
        Ok(Self {
            ring: vec![[0.0; 2]; window as usize + 8],
            position: 0,
            window,
            phase: 0.0,
            ratio: 1.0,
        })
    }

    pub fn configure(&mut self, semitones: f64) -> Result<()> {
        range(semitones, -12.0, 12.0, "pitch (-12..12 semitones)")?;
        self.ratio = 2.0_f64.powf(semitones / 12.0);
        Ok(())
    }

    pub fn reset(&mut self) {
        self.ring.fill([0.0; 2]);
        self.position = 0;
        self.phase = 0.0;
    }

    fn read(&self, delay: f64) -> StereoFrame {
        let position = (self.position as f64 - delay).rem_euclid(self.ring.len() as f64);
        let index = position.floor() as usize;
        let fraction = (position - index as f64) as f32;
        let a = self.ring[index];
        let b = self.ring[(index + 1) % self.ring.len()];
        std::array::from_fn(|ch| a[ch] + (b[ch] - a[ch]) * fraction)
    }

    fn tick(&mut self, input: StereoFrame) -> StereoFrame {
        self.ring[self.position] = input;
        let output = if (self.ratio - 1.0).abs() < 1e-12 {
            input
        } else {
            let other = (self.phase + 0.5).fract();
            let weight = (1.0 - (2.0 * self.phase - 1.0).abs()) as f32;
            let a = self.read(4.0 + self.phase * self.window);
            let b = self.read(4.0 + other * self.window);
            self.phase = (self.phase + (1.0 - self.ratio) / self.window).rem_euclid(1.0);
            std::array::from_fn(|ch| a[ch] * weight + b[ch] * (1.0 - weight))
        };
        self.position = (self.position + 1) % self.ring.len();
        output
    }
}

/// Буферы выделяются один раз; указатели действуют до уничтожения адаптера.
/// Конфигурация: enabled, preamp, concert, wet, decay, damping, pitch;
/// затем 10 × [kind (0=PK/1=LS/2=HS), frequency, gain, Q, enabled].
pub struct WorkletDsp {
    processor: StereoProcessor,
    pitch: StreamingPitch,
    pcm: Vec<f32>,
    frames: Vec<StereoFrame>,
    config: [f64; CONFIG_LEN],
    applied: [f64; CONFIG_LEN],
}

impl WorkletDsp {
    pub fn new(rate: u32) -> Result<Self> {
        let settings = EqSettings::flat(rate)?;
        let mut config = [0.0; CONFIG_LEN];
        config[3..7].copy_from_slice(&[0.15, 1.2, 0.4, 0.0]);
        for (index, band) in settings.bands.iter().enumerate() {
            config[7 + index * 5..12 + index * 5].copy_from_slice(&[
                0.0,
                band.frequency_hz,
                0.0,
                1.4,
                1.0,
            ]);
        }
        Ok(Self {
            processor: StereoProcessor::new(rate)?,
            pitch: StreamingPitch::new(rate)?,
            pcm: vec![0.0; MAX_BLOCK_FRAMES * 2],
            frames: vec![[0.0; 2]; MAX_BLOCK_FRAMES],
            config,
            applied: config,
        })
    }

    pub fn pcm_mut(&mut self) -> &mut [f32] {
        &mut self.pcm
    }

    pub fn config_mut(&mut self) -> &mut [f64; CONFIG_LEN] {
        &mut self.config
    }

    pub fn rms(&self) -> [f64; 2] {
        self.processor.rms()
    }

    /// Проверяет ВСЕ поля до изменения состояния. Ошибка сохраняет прежние настройки.
    pub fn configure(&mut self) -> Result<()> {
        let c = &self.config;
        let boolean = |value: f64| -> Result<bool> {
            if value == 0.0 || value == 1.0 {
                Ok(value == 1.0)
            } else {
                Err(Error::Invalid("boolean (0 or 1)"))
            }
        };
        let enabled = boolean(c[0])?;
        let concert = boolean(c[2])?;
        let mut eq = EqSettings::flat(self.processor.sample_rate())?;
        eq.preamp_db = c[1];
        for (index, band) in eq.bands.iter_mut().enumerate() {
            let offset = 7 + index * 5;
            band.kind = match c[offset] {
                0.0 => FilterKind::Peak,
                1.0 => FilterKind::LowShelf,
                2.0 => FilterKind::HighShelf,
                _ => return Err(Error::Invalid("EQ kind (0..2)")),
            };
            band.frequency_hz = c[offset + 1];
            band.gain_db = c[offset + 2];
            band.q = c[offset + 3];
            band.enabled = boolean(c[offset + 4])?;
        }
        eq.validate(self.processor.sample_rate())?;
        let reverb = ReverbSettings {
            wet: c[3],
            decay_seconds: c[4],
            damping: c[5],
        };
        reverb.validate()?;
        range(c[6], -12.0, 12.0, "pitch (-12..12 semitones)")?;

        // Изменение реверберации не сбрасывает историю эквалайзера и наоборот.
        if c[0..2] != self.applied[0..2] || c[7..] != self.applied[7..] {
            if !enabled {
                eq = EqSettings::flat(self.processor.sample_rate())?;
            }
            self.processor.set_eq(eq)?;
        }
        if c[2..6] != self.applied[2..6] {
            if concert {
                self.processor.set_reverb(reverb)?;
            } else {
                self.processor.bypass_reverb();
            }
        }
        self.pitch.configure(c[6])?;
        self.applied = self.config;
        Ok(())
    }

    pub fn reset(&mut self) {
        self.processor.reset();
        self.pitch.reset();
        self.pcm.fill(0.0);
    }

    /// PCM L,R,L,R; проверка входа предшествует любому изменению истории.
    pub fn process(&mut self, count: usize) -> Result<()> {
        if count > MAX_BLOCK_FRAMES {
            return Err(Error::Invalid("worklet block (0..2048 frames)"));
        }
        for &sample in &self.pcm[..count * 2] {
            range(sample as f64, -16.0, 16.0, "finite PCM (-16..16)")?;
        }
        for index in 0..count {
            self.frames[index] = self
                .pitch
                .tick([self.pcm[index * 2], self.pcm[index * 2 + 1]]);
        }
        self.processor.process(&mut self.frames[..count])?;
        for (index, frame) in self.frames[..count].iter().enumerate() {
            self.pcm[index * 2..index * 2 + 2].copy_from_slice(frame);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_is_exact_stereo_and_buffers_stay_fixed() {
        let mut dsp = WorkletDsp::new(48000).unwrap();
        let pointer = dsp.pcm_mut().as_ptr();
        let config = dsp.config_mut().as_ptr();
        for _ in 0..1000 {
            dsp.pcm_mut()[..4].copy_from_slice(&[0.2, -0.3, -0.5, 0.8]);
            dsp.process(2).unwrap();
            assert_eq!(&dsp.pcm_mut()[..4], &[0.2, -0.3, -0.5, 0.8]);
        }
        assert_eq!(pointer, dsp.pcm_mut().as_ptr());
        assert_eq!(config, dsp.config_mut().as_ptr());
        assert!(dsp.rms()[0] > 0.1);
    }

    #[test]
    fn preamp_and_invalid_config_are_atomic() {
        let mut dsp = WorkletDsp::new(48000).unwrap();
        dsp.config_mut()[0] = 1.0;
        dsp.config_mut()[1] = -6.0;
        dsp.configure().unwrap();
        dsp.config_mut()[1] = 6.0;
        dsp.config_mut()[6] = f64::NAN;
        assert!(dsp.configure().is_err());
        dsp.pcm_mut()[..2].copy_from_slice(&[0.5, -0.5]);
        dsp.process(1).unwrap();
        assert!((dsp.pcm_mut()[0] - 0.5 * 10.0_f32.powf(-6.0 / 20.0)).abs() < 1e-6);
        assert!(dsp.process(MAX_BLOCK_FRAMES + 1).is_err());
        dsp.pcm_mut()[0] = f32::INFINITY;
        assert!(dsp.process(1).is_err());
    }

    #[test]
    fn concert_has_tail_and_reset_removes_it() {
        let mut dsp = WorkletDsp::new(48000).unwrap();
        dsp.config_mut()[2] = 1.0;
        dsp.config_mut()[3] = 1.0;
        dsp.configure().unwrap();
        dsp.pcm_mut()[0] = 1.0;
        dsp.process(1).unwrap();
        let mut energy = 0.0;
        for _ in 0..30 {
            dsp.pcm_mut().fill(0.0);
            dsp.process(128).unwrap();
            energy += dsp.pcm_mut()[..256].iter().map(|x| x * x).sum::<f32>();
        }
        assert!(energy > 0.01);
        dsp.reset();
        dsp.process(2048).unwrap();
        assert!(dsp.pcm_mut().iter().all(|&x| x == 0.0));
    }

    #[test]
    fn pitch_changes_frequency_without_changing_frame_count() {
        let rate = 48000;
        for (semitones, expected) in [(-12.0, 200.0), (0.0, 400.0), (12.0, 800.0)] {
            let mut pitch = StreamingPitch::new(rate).unwrap();
            pitch.configure(semitones).unwrap();
            let mut crossings = 0;
            let mut previous = 0.0;
            for index in 0..rate {
                let sample =
                    (std::f64::consts::TAU * 400.0 * index as f64 / rate as f64).sin() as f32;
                let out = pitch.tick([sample, -sample]);
                assert!((out[0] + out[1]).abs() < 1e-6);
                assert!(out[0].is_finite() && out[0].abs() <= 1.0);
                if index >= rate / 5 && previous <= 0.0 && out[0] > 0.0 {
                    crossings += 1;
                }
                previous = out[0];
            }
            let measured = crossings as f64 / 0.8;
            assert!((measured - expected).abs() < 3.0, "{semitones}: {measured}");
        }
    }
}
