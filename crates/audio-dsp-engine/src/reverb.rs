//! Комнатная реверберация Шрёдера: четыре демпфированных гребёнки и два all-pass на канал.
use crate::{Result, StereoFrame, range, sample_rate, validate_frames};

#[derive(Clone, Copy, Debug)]
pub struct ReverbSettings {
    pub wet: f64,
    pub decay_seconds: f64,
    /// 0 — яркий хвост, 0.95 — сильное поглощение высоких частот.
    pub damping: f64,
}
impl Default for ReverbSettings {
    fn default() -> Self {
        Self {
            wet: 0.15,
            decay_seconds: 1.2,
            damping: 0.4,
        }
    }
}
impl ReverbSettings {
    pub fn validate(&self) -> Result<()> {
        range(self.wet, 0.0, 1.0, "reverb wet")?;
        range(self.decay_seconds, 0.1, 10.0, "reverb decay (0.1..10 s)")?;
        range(self.damping, 0.0, 0.95, "reverb damping")
    }
}
#[derive(Clone, Debug)]
struct Delay {
    data: Vec<f64>,
    pos: usize,
    low: f64,
}
impl Delay {
    fn new(size: usize) -> Self {
        Self {
            data: vec![0.0; size.max(1)],
            pos: 0,
            low: 0.0,
        }
    }
    fn comb(&mut self, input: f64, feedback: f64, damping: f64) -> f64 {
        let out = self.data[self.pos];
        self.low = out * (1.0 - damping) + self.low * damping;
        self.data[self.pos] = input + feedback * self.low;
        self.pos = (self.pos + 1) % self.data.len();
        out
    }
    fn allpass(&mut self, input: f64) -> f64 {
        let delayed = self.data[self.pos];
        let out = delayed - 0.5 * input;
        self.data[self.pos] = input + 0.5 * out;
        self.pos = (self.pos + 1) % self.data.len();
        out
    }
    fn reset(&mut self) {
        self.data.fill(0.0);
        self.pos = 0;
        self.low = 0.0;
    }
}
#[derive(Clone, Debug)]
pub struct RoomReverb {
    rate: u32,
    settings: ReverbSettings,
    combs: [Vec<Delay>; 2],
    allpasses: [Vec<Delay>; 2],
    feedback: [[f64; 4]; 2],
}
impl RoomReverb {
    pub fn new(rate: u32, settings: ReverbSettings) -> Result<Self> {
        sample_rate(rate)?;
        settings.validate()?;
        let combs = std::array::from_fn(|ch| {
            [0.0297, 0.0371, 0.0411, 0.0437]
                .iter()
                .map(|&s| Delay::new(((s + ch as f64 * 0.0011) * rate as f64).round() as usize))
                .collect()
        });
        let allpasses = std::array::from_fn(|ch| {
            [0.005, 0.0017]
                .iter()
                .map(|&s| Delay::new(((s + ch as f64 * 0.0003) * rate as f64).round() as usize))
                .collect()
        });
        let mut reverb = Self {
            rate,
            settings,
            combs,
            allpasses,
            feedback: [[0.0; 4]; 2],
        };
        reverb.configure(settings)?;
        Ok(reverb)
    }
    /// Изменяет коэффициенты без выделения памяти и без уничтожения хвоста.
    pub fn configure(&mut self, settings: ReverbSettings) -> Result<()> {
        settings.validate()?;
        for ch in 0..2 {
            for i in 0..4 {
                self.feedback[ch][i] = 10.0_f64.powf(
                    -3.0 * self.combs[ch][i].data.len() as f64
                        / (self.rate as f64 * settings.decay_seconds),
                );
            }
        }
        self.settings = settings;
        Ok(())
    }
    pub fn reset(&mut self) {
        for delays in self.combs.iter_mut().chain(self.allpasses.iter_mut()) {
            for d in delays {
                d.reset();
            }
        }
    }
    pub fn process(&mut self, frames: &mut [StereoFrame]) -> Result<()> {
        validate_frames(frames)?;
        for frame in frames {
            *frame = self.tick(*frame);
        }
        Ok(())
    }
    pub(crate) fn tick(&mut self, frame: StereoFrame) -> StereoFrame {
        let mut out = [0.0; 2];
        for ch in 0..2 {
            let input = 0.8 * frame[ch] as f64 + 0.2 * frame[1 - ch] as f64;
            let mut wet = 0.0;
            for i in 0..4 {
                wet += self.combs[ch][i].comb(input, self.feedback[ch][i], self.settings.damping)
                    * 0.25;
            }
            for d in &mut self.allpasses[ch] {
                wet = d.allpass(wet);
            }
            out[ch] =
                ((1.0 - self.settings.wet) * frame[ch] as f64 + self.settings.wet * wet) as f32;
        }
        out
    }
}
