//! RMS, K-взвешенная громкость с абсолютным/относительным гейтом и задержанный детектор тишины.
use crate::{Result, StereoFrame, eq::Biquad, range, sample_rate, validate_frames};
use std::{collections::VecDeque, f64::consts::PI};

pub fn rms(frames: &[StereoFrame]) -> Result<[f64; 2]> {
    validate_frames(frames)?;
    let mut sum = [0.0; 2];
    if frames.is_empty() {
        return Ok(sum);
    }
    for f in frames {
        for ch in 0..2 {
            sum[ch] += (f[ch] as f64).powi(2);
        }
    }
    Ok(sum.map(|s| (s / frames.len() as f64).sqrt()))
}
/// Нулевой сигнал возвращает -∞ dBFS; это корректный результат, но не допустимый вход настройки.
pub fn amplitude_dbfs(amplitude: f64) -> Result<f64> {
    range(amplitude, 0.0, f64::MAX, "amplitude")?;
    Ok(20.0 * amplitude.log10())
}

#[derive(Clone, Debug)]
pub struct RollingRms {
    values: Vec<[f64; 2]>,
    pos: usize,
    count: usize,
    sum: [f64; 2],
}
impl RollingRms {
    pub fn new(rate: u32, window_seconds: f64) -> Result<Self> {
        sample_rate(rate)?;
        range(window_seconds, 0.001, 10.0, "RMS window (0.001..10 s)")?;
        Ok(Self {
            values: vec![[0.0; 2]; (rate as f64 * window_seconds).round() as usize],
            pos: 0,
            count: 0,
            sum: [0.0; 2],
        })
    }
    pub fn process(&mut self, frames: &[StereoFrame]) -> Result<[f64; 2]> {
        validate_frames(frames)?;
        for &f in frames {
            self.tick(f);
        }
        Ok(self.value())
    }
    pub(crate) fn tick(&mut self, f: StereoFrame) {
        for (ch, sample) in f.into_iter().enumerate() {
            let energy = (sample as f64).powi(2);
            self.sum[ch] += energy - self.values[self.pos][ch];
            self.values[self.pos][ch] = energy;
        }
        self.pos = (self.pos + 1) % self.values.len();
        self.count = (self.count + 1).min(self.values.len());
    }
    pub fn value(&self) -> [f64; 2] {
        if self.count == 0 {
            [0.0; 2]
        } else {
            self.sum.map(|s| (s.max(0.0) / self.count as f64).sqrt())
        }
    }
    pub fn reset(&mut self) {
        self.values.fill([0.0; 2]);
        self.pos = 0;
        self.count = 0;
        self.sum = [0.0; 2];
    }
}

fn k_weighting(rate: u32) -> [Biquad; 2] {
    let k = (PI * 1681.974450955533 / rate as f64).tan();
    let q = 0.7071752369554196;
    let vh = 10.0_f64.powf(3.999843853973347 / 20.0);
    let vb = vh.powf(0.4996667741545416);
    let a0 = 1.0 + k / q + k * k;
    let shelf = Biquad::coefficients(
        [
            (vh + vb * k / q + k * k) / a0,
            2.0 * (k * k - vh) / a0,
            (vh - vb * k / q + k * k) / a0,
        ],
        [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
    );
    let k = (PI * 38.13547087602444 / rate as f64).tan();
    let q = 0.5003270373238773;
    let a0 = 1.0 + k / q + k * k;
    let highpass = Biquad::coefficients(
        [1.0, -2.0, 1.0],
        [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
    );
    [shelf, highpass]
}
fn energy_lufs(energy: f64) -> f64 {
    -0.691 + 10.0 * energy.log10()
}

/// Стерео BS.1770-style: K-weighting, блоки 400 мс с шагом 100 мс, гейты -70 LUFS и -10 LU.
/// История блоков растёт на 10 f64/сек; анализ вне аудиоколбэка. True peak не измеряется.
#[derive(Clone, Debug)]
pub struct LoudnessMeter {
    filters: [Biquad; 2],
    energies: Vec<f64>,
    pos: usize,
    count: usize,
    sum: f64,
    hop: usize,
    until_block: usize,
    blocks: Vec<f64>,
    peak: f64,
}
#[derive(Clone, Copy, Debug)]
pub struct LoudnessReading {
    pub momentary_lufs: Option<f64>,
    pub integrated_lufs: Option<f64>,
    pub sample_peak: f64,
    pub gated_blocks: usize,
}
impl LoudnessMeter {
    pub fn new(rate: u32) -> Result<Self> {
        sample_rate(rate)?;
        let window = (rate as f64 * 0.4).round() as usize;
        Ok(Self {
            filters: k_weighting(rate),
            energies: vec![0.0; window],
            pos: 0,
            count: 0,
            sum: 0.0,
            hop: (rate as f64 * 0.1).round() as usize,
            until_block: window,
            blocks: Vec::new(),
            peak: 0.0,
        })
    }
    pub fn process(&mut self, frames: &[StereoFrame]) -> Result<()> {
        validate_frames(frames)?;
        for f in frames {
            let mut energy = 0.0;
            for (ch, &sample) in f.iter().enumerate() {
                self.peak = self.peak.max((sample as f64).abs());
                let mut x = sample as f64;
                for filter in &mut self.filters {
                    x = filter.tick(x, ch);
                }
                energy += x * x;
            }
            self.sum += energy - self.energies[self.pos];
            self.energies[self.pos] = energy;
            self.pos = (self.pos + 1) % self.energies.len();
            self.count = (self.count + 1).min(self.energies.len());
            self.until_block -= 1;
            if self.until_block == 0 {
                self.blocks
                    .push((self.sum / self.energies.len() as f64).max(0.0));
                self.until_block = self.hop;
            }
        }
        Ok(())
    }
    pub fn reading(&self) -> LoudnessReading {
        let absolute = 10.0_f64.powf((-70.0 + 0.691) / 10.0);
        let (sum, count) = self
            .blocks
            .iter()
            .filter(|&&e| e >= absolute)
            .fold((0.0, 0), |(s, n), &e| (s + e, n + 1));
        let relative = if count > 0 {
            (sum / count as f64) * 0.1
        } else {
            absolute
        };
        let (sum, count) = self
            .blocks
            .iter()
            .filter(|&&e| e >= absolute.max(relative))
            .fold((0.0, 0), |(s, n), &e| (s + e, n + 1));
        LoudnessReading {
            momentary_lufs: if self.count == self.energies.len() && self.sum > 0.0 {
                Some(energy_lufs(self.sum / self.count as f64))
            } else {
                None
            },
            integrated_lufs: if count > 0 {
                Some(energy_lufs(sum / count as f64))
            } else {
                None
            },
            gated_blocks: count,
            sample_peak: self.peak,
        }
    }
    pub fn reset(&mut self) {
        for f in &mut self.filters {
            f.reset();
        }
        self.energies.fill(0.0);
        self.pos = 0;
        self.count = 0;
        self.sum = 0.0;
        self.until_block = self.energies.len();
        self.blocks.clear();
        self.peak = 0.0;
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LookaheadFrame {
    pub frame: StereoFrame,
    pub silent: bool,
}
/// Возвращает исходный кадр с задержкой N кадров; silent=true, если весь интервал [кадр, кадр+N]
/// ниже порога пикового уровня обоих каналов. Ничего не вырезает. drain завершает неполное окно.
#[derive(Clone, Debug)]
pub struct SilenceLookahead {
    delay: usize,
    threshold: f32,
    frames: VecDeque<StereoFrame>,
    peaks: VecDeque<(u64, f32)>,
    head: u64,
    next: u64,
}
impl SilenceLookahead {
    pub fn new(rate: u32, lookahead_seconds: f64, threshold_dbfs: f64) -> Result<Self> {
        sample_rate(rate)?;
        range(lookahead_seconds, 0.0, 2.0, "silence lookahead (0..2 s)")?;
        range(
            threshold_dbfs,
            -120.0,
            0.0,
            "silence threshold (-120..0 dBFS)",
        )?;
        let delay = (rate as f64 * lookahead_seconds).round() as usize;
        Ok(Self {
            delay,
            threshold: crate::db_gain(threshold_dbfs) as f32,
            frames: VecDeque::with_capacity(delay + 1),
            peaks: VecDeque::with_capacity(delay + 1),
            head: 0,
            next: 0,
        })
    }
    pub fn latency_frames(&self) -> usize {
        self.delay
    }
    pub fn push(&mut self, frame: StereoFrame) -> Result<Option<LookaheadFrame>> {
        validate_frames(&[frame])?;
        let peak = frame[0].abs().max(frame[1].abs());
        while self.peaks.back().is_some_and(|&(_, p)| p <= peak) {
            self.peaks.pop_back();
        }
        self.peaks.push_back((self.next, peak));
        self.next += 1;
        self.frames.push_back(frame);
        Ok(if self.frames.len() > self.delay {
            self.drain()
        } else {
            None
        })
    }
    pub fn drain(&mut self) -> Option<LookaheadFrame> {
        let frame = self.frames.pop_front()?;
        let silent = self.peaks.front().is_none_or(|&(_, p)| p < self.threshold);
        if self.peaks.front().is_some_and(|&(i, _)| i == self.head) {
            self.peaks.pop_front();
        }
        self.head += 1;
        Some(LookaheadFrame { frame, silent })
    }
    pub fn reset(&mut self) {
        self.frames.clear();
        self.peaks.clear();
        self.head = 0;
        self.next = 0;
    }
}

/// Признаки трека для обрезки пауз, нормализации и сортировки DJ-сета.
#[derive(Clone, Debug)]
pub struct TrackAnalysis {
    pub bpm: Option<f64>,
    pub energy: f64,
    pub start_frame: usize,
    pub end_frame: usize,
    pub integrated_lufs: Option<f64>,
}
pub fn analyze_track(frames: &[StereoFrame], rate: u32) -> Result<TrackAnalysis> {
    sample_rate(rate)?;
    validate_frames(frames)?;
    let window = (rate as usize / 100).max(1);
    let mut envelope = Vec::new();
    for block in frames.chunks(window) {
        let power = block
            .iter()
            .map(|f| (f[0] as f64).powi(2) + (f[1] as f64).powi(2))
            .sum::<f64>()
            / (2 * block.len()) as f64;
        envelope.push(power.sqrt());
    }
    let active: Vec<_> = envelope
        .iter()
        .enumerate()
        .filter(|(_, value)| **value > 0.00316227766)
        .map(|(i, _)| i)
        .collect();
    let start_frame = active.first().map(|n| n * window).unwrap_or(0);
    let end_frame = active
        .last()
        .map(|n| ((n + 1) * window).min(frames.len()))
        .unwrap_or(frames.len());
    let energy = envelope.iter().map(|e| e * e).sum::<f64>() / envelope.len().max(1) as f64;
    let novelty: Vec<_> = envelope
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).max(0.0))
        .take(12000)
        .collect();
    let envelope_rate = rate as f64 / window as f64;
    let mut best = (0.0, None);
    for bpm in 60..=200 {
        let lag = (60.0 * envelope_rate / bpm as f64).round() as usize;
        if lag >= novelty.len() {
            continue;
        }
        let mut sum = 0.0;
        let mut norm_a = 0.0;
        let mut norm_b = 0.0;
        for (a, b) in novelty.iter().zip(&novelty[lag..]) {
            sum += a * b;
            norm_a += a * a;
            norm_b += b * b;
        }
        let prior = 1.0 - ((bpm as f64 - 120.0) / 300.0).abs();
        let score = sum / (norm_a * norm_b).sqrt().max(1e-12) * prior;
        if score > best.0 {
            best = (score, Some(bpm as f64));
        }
    }
    let mut loudness = LoudnessMeter::new(rate)?;
    loudness.process(frames)?;
    Ok(TrackAnalysis {
        bpm: if best.0 >= 0.2 { best.1 } else { None },
        energy: energy.sqrt().clamp(0.0, 1.0),
        start_frame,
        end_frame,
        integrated_lufs: loudness.reading().integrated_lufs,
    })
}
