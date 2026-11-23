//! Параметрический эквалайзер: формулы RBJ, независимое состояние каналов, строгий AutoEq.
use crate::{Error, Result, StereoFrame, db_gain, range, sample_rate, validate_frames};
use std::f64::consts::PI;

pub const BAND_COUNT: usize = 10;
pub const DEFAULT_FREQUENCIES: [f64; BAND_COUNT] = [
    31.0, 62.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterKind {
    Peak,
    LowShelf,
    HighShelf,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EqBand {
    pub kind: FilterKind,
    pub frequency_hz: f64,
    pub gain_db: f64,
    pub q: f64,
    pub enabled: bool,
}
impl EqBand {
    pub fn peak(frequency_hz: f64, gain_db: f64, q: f64) -> Self {
        Self {
            kind: FilterKind::Peak,
            frequency_hz,
            gain_db,
            q,
            enabled: true,
        }
    }
    pub fn validate(&self, rate: u32) -> Result<()> {
        sample_rate(rate)?;
        range(
            self.frequency_hz,
            10.0,
            rate as f64 * 0.499,
            "EQ frequency (10 Hz..0.499 * sample rate)",
        )?;
        range(self.gain_db, -24.0, 24.0, "EQ gain (-24..24 dB)")?;
        range(self.q, 0.1, 20.0, "EQ Q (0.1..20)")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct EqSettings {
    pub preamp_db: f64,
    pub bands: [EqBand; BAND_COUNT],
}
impl EqSettings {
    /// Частоты верхних полос адаптируются к частоте дискретизации.
    pub fn flat(rate: u32) -> Result<Self> {
        sample_rate(rate)?;
        Ok(Self {
            preamp_db: 0.0,
            bands: DEFAULT_FREQUENCIES.map(|f| EqBand::peak(f.min(rate as f64 * 0.45), 0.0, 1.0)),
        })
    }
    pub fn validate(&self, rate: u32) -> Result<()> {
        sample_rate(rate)?;
        range(self.preamp_db, -60.0, 24.0, "EQ preamp (-60..24 dB)")?;
        for band in &self.bands {
            band.validate(rate)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Biquad {
    pub(crate) b: [f64; 3],
    pub(crate) a: [f64; 2],
    z: [[f64; 2]; 2],
}
impl Biquad {
    pub(crate) fn coefficients(b: [f64; 3], a: [f64; 2]) -> Self {
        Self {
            b,
            a,
            z: [[0.0; 2]; 2],
        }
    }
    fn identity() -> Self {
        Self::coefficients([1.0, 0.0, 0.0], [0.0, 0.0])
    }
    fn from_band(band: EqBand, rate: u32) -> Self {
        if !band.enabled || band.gain_db == 0.0 {
            return Self::identity();
        }
        let a = 10.0_f64.powf(band.gain_db / 40.0);
        let omega = 2.0 * PI * band.frequency_hz / rate as f64;
        let c = omega.cos();
        let alpha = omega.sin() / (2.0 * band.q);
        let beta = 2.0 * a.sqrt() * alpha;
        let (b, d) = match band.kind {
            FilterKind::Peak => (
                [1.0 + alpha * a, -2.0 * c, 1.0 - alpha * a],
                [1.0 + alpha / a, -2.0 * c, 1.0 - alpha / a],
            ),
            FilterKind::LowShelf => (
                [
                    a * ((a + 1.0) - (a - 1.0) * c + beta),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * c),
                    a * ((a + 1.0) - (a - 1.0) * c - beta),
                ],
                [
                    (a + 1.0) + (a - 1.0) * c + beta,
                    -2.0 * ((a - 1.0) + (a + 1.0) * c),
                    (a + 1.0) + (a - 1.0) * c - beta,
                ],
            ),
            FilterKind::HighShelf => (
                [
                    a * ((a + 1.0) + (a - 1.0) * c + beta),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * c),
                    a * ((a + 1.0) + (a - 1.0) * c - beta),
                ],
                [
                    (a + 1.0) - (a - 1.0) * c + beta,
                    2.0 * ((a - 1.0) - (a + 1.0) * c),
                    (a + 1.0) - (a - 1.0) * c - beta,
                ],
            ),
        };
        Self::coefficients(b.map(|v| v / d[0]), [d[1] / d[0], d[2] / d[0]])
    }
    pub(crate) fn tick(&mut self, x: f64, ch: usize) -> f64 {
        let y = self.b[0] * x + self.z[ch][0];
        self.z[ch][0] = self.b[1] * x - self.a[0] * y + self.z[ch][1];
        self.z[ch][1] = self.b[2] * x - self.a[1] * y;
        y
    }
    pub(crate) fn reset(&mut self) {
        self.z = [[0.0; 2]; 2];
    }
}

#[derive(Clone, Debug)]
pub struct Equalizer {
    rate: u32,
    settings: EqSettings,
    filters: [Biquad; BAND_COUNT],
    gain: f64,
}
impl Equalizer {
    pub fn new(rate: u32, settings: EqSettings) -> Result<Self> {
        settings.validate(rate)?;
        Ok(Self {
            rate,
            filters: settings.bands.map(|b| Biquad::from_band(b, rate)),
            gain: db_gain(settings.preamp_db),
            settings,
        })
    }
    pub fn settings(&self) -> &EqSettings {
        &self.settings
    }
    /// Атомарная замена параметров; сбрасывает историю. Для плавной смены применяйте crossfade двух цепочек.
    pub fn configure(&mut self, settings: EqSettings) -> Result<()> {
        *self = Self::new(self.rate, settings)?;
        Ok(())
    }
    pub fn reset(&mut self) {
        for filter in &mut self.filters {
            filter.reset();
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
            let mut v = frame[ch] as f64 * self.gain;
            for filter in &mut self.filters {
                v = filter.tick(v, ch);
            }
            out[ch] = v as f32;
        }
        out
    }
}

/// Поддерживает только параметрический AutoEq: Preamp и Filter 1..10, ON/OFF, PK/LS/HS.
/// Каждая полоса обязательна; повторы, лишние поля, неизвестные строки и неверные числа — ошибка.
/// Пустые строки и отдельные строки комментариев `#` разрешены; порядок строк свободный.
pub fn parse_autoeq(text: &str, rate: u32) -> Result<EqSettings> {
    sample_rate(rate)?;
    let mut settings = EqSettings::flat(rate)?;
    let mut seen = [false; BAND_COUNT];
    let mut preamp = false;
    for (offset, line) in text.lines().enumerate() {
        let line_no = offset + 1;
        let err = |message: &str| Error::AutoEq {
            line: line_no,
            message: message.into(),
        };
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let words: Vec<_> = line.split_whitespace().collect();
        let number =
            |s: &str| -> Result<f64> { s.parse::<f64>().map_err(|_| err("expected number")) };
        if words[0] == "Preamp:" {
            if preamp || words.len() != 3 || words[2] != "dB" {
                return Err(err("expected one 'Preamp: <number> dB'"));
            }
            settings.preamp_db = number(words[1])?;
            range(settings.preamp_db, -60.0, 24.0, "preamp").map_err(|e| err(&e.to_string()))?;
            preamp = true;
        } else {
            if words.len() != 12
                || words[0] != "Filter"
                || words[4] != "Fc"
                || words[6] != "Hz"
                || words[7] != "Gain"
                || words[9] != "dB"
                || words[10] != "Q"
            {
                return Err(err(
                    "expected 'Filter N: ON|OFF PK|LS|HS Fc F Hz Gain G dB Q Q'",
                ));
            }
            let index = words[1]
                .strip_suffix(':')
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|i| (1..=10).contains(i))
                .ok_or_else(|| err("filter index must be 1..10"))?
                - 1;
            if seen[index] {
                return Err(err("duplicate filter"));
            }
            let enabled = match words[2] {
                "ON" => true,
                "OFF" => false,
                _ => return Err(err("expected ON or OFF")),
            };
            let kind = match words[3] {
                "PK" => FilterKind::Peak,
                "LS" => FilterKind::LowShelf,
                "HS" => FilterKind::HighShelf,
                _ => return Err(err("unsupported filter type")),
            };
            let band = EqBand {
                enabled,
                kind,
                frequency_hz: number(words[5])?,
                gain_db: number(words[8])?,
                q: number(words[11])?,
            };
            band.validate(rate).map_err(|e| err(&e.to_string()))?;
            settings.bands[index] = band;
            seen[index] = true;
        }
    }
    if !seen.iter().any(|value| *value) {
        return Err(Error::AutoEq {
            line: text.lines().count() + 1,
            message: "at least one filter is required".into(),
        });
    }
    Ok(settings)
}
