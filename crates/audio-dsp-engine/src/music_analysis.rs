//! Независимый музыкальный анализ: STFT, onset, BPM/beatgrid, chroma/key,
//! LUFS/true peak, энергия, тишина, секции, точки микширования.
//!
//! Только `std` + `realfft`. Без Tokio/Axum/DOM/Telegram: работает и на native,
//! и на wasm32. Детерминирован: фиксированные окно/шаг, без RNG, порядок обхода
//! фиксирован, накопления — в f64 там, где важна точность.
//!
//! Ориентиры (формулы и структура — свои, чужой код не копировался):
//! Mixxx — автокорреляция темпа по onset-огибающей и фазовый beatgrid;
//! Essentia — HPCP-chroma и RhythmExtractor;
//! aubio — spectral flux с логарифмическим сжатием;
//! librosa — chroma и структурная самоподобность;
//! realfft — прямое FFT; libebur128/BS.1770 — двухступенчатые гейты громкости;
//! Signalsmith Stretch — только как референс фазовой обработки (не используется).
//!
//! Память ограничена: вход берётся срезом (без копирования PCM целиком —
//! моно-микс идёт в один `Vec<f32>`), FFT-план и рабочие буферы переиспользуются
//! внутри [`MusicAnalyzer`], хранятся только chroma/энергия на STFT-кадр
//! (13 f32), а не спектры. Лимит входа — [`MAX_ANALYSIS_FRAMES`].

use crate::{Error, Result, StereoFrame, sample_rate, validate_frames};
use realfft::{RealFftPlanner, RealToComplex, num_complex::Complex};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Версия результата: кэш анализа инвалидируется при несовпадении.
pub const ANALYSIS_VERSION: u32 = 1;
/// Лимит входа (~300 с стерео 48 кГц). Длиннее — ошибка, а не OOM.
pub const MAX_ANALYSIS_FRAMES: usize = 14_400_000;

const FFT_LEN: usize = 4096;
const HOP: usize = 1024;
const MIN_BPM: f64 = 50.0;
const MAX_BPM: f64 = 220.0;
const CANCEL_MASK: usize = 31;
const SILENCE_DBFS: f64 = -60.0;
const SILENCE_MIN_SECONDS: f64 = 0.2;

/// Профили Krumhansl–Schmuckler (12 pitch-классов от C). Эмпирические константы
/// из литературы (Krumhansl-Kessler), используются для корреляции, а не код.
const KS_MAJOR: [f64; 12] = [
    6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88,
];
const KS_MINOR: [f64; 12] = [
    6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PitchClass {
    C,
    CSharp,
    D,
    DSharp,
    E,
    F,
    FSharp,
    G,
    GSharp,
    A,
    ASharp,
    B,
}

impl PitchClass {
    fn from_semitone(s: usize) -> Self {
        match s % 12 {
            0 => Self::C,
            1 => Self::CSharp,
            2 => Self::D,
            3 => Self::DSharp,
            4 => Self::E,
            5 => Self::F,
            6 => Self::FSharp,
            7 => Self::G,
            8 => Self::GSharp,
            9 => Self::A,
            10 => Self::ASharp,
            _ => Self::B,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::C => "C",
            Self::CSharp => "C#",
            Self::D => "D",
            Self::DSharp => "D#",
            Self::E => "E",
            Self::F => "F",
            Self::FSharp => "F#",
            Self::G => "G",
            Self::GSharp => "G#",
            Self::A => "A",
            Self::ASharp => "A#",
            Self::B => "B",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Major,
    Minor,
}

#[derive(Clone, Copy, Debug)]
pub struct BpmEstimate {
    pub bpm: f64,
    /// 0..1: высота автокорреляционного пика над фоном.
    pub confidence: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct KeyEstimate {
    pub tonic: PitchClass,
    pub mode: Mode,
    /// 0..1: отрыв лучшего профиля от второго.
    pub confidence: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransitionKind {
    /// Первый сильный даунбит после интро — кандидат входа микширования.
    MixIn,
    /// Последний бит перед аутро — кандидат выхода.
    MixOut,
    /// Граница повторяющейся секции.
    SectionBoundary,
    /// Энергетический провал рядом с битом.
    EnergyDip,
}

#[derive(Clone, Copy, Debug)]
pub struct Segment {
    pub start_seconds: f64,
    pub end_seconds: f64,
}

#[derive(Clone, Debug)]
pub struct Section {
    pub label: char,
    pub start_seconds: f64,
    pub end_seconds: f64,
    /// 0..1: средняя косинусная близость окон к прототипу.
    pub confidence: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct Transition {
    pub at_seconds: f64,
    pub kind: TransitionKind,
    /// 0..1.
    pub confidence: f64,
}

#[derive(Clone, Debug)]
pub struct MusicAnalysis {
    pub version: u32,
    pub sample_rate: u32,
    pub frames: usize,
    pub duration_seconds: f64,
    pub bpm: Option<BpmEstimate>,
    /// Сетка в секундах от фазового поиска. Пуста без уверенного BPM.
    pub beats: Vec<f64>,
    /// 0..1: доля энергии onset-огибающей, лёгшей на сетку.
    pub beat_confidence: f64,
    pub key: Option<KeyEstimate>,
    pub rms: [f64; 2],
    pub sample_peak: f64,
    /// Оценка 4x линейной интерполяцией (консервативная, не полный полифазник).
    pub true_peak: f64,
    pub integrated_lufs: Option<f64>,
    /// RMS в дБ, шаг 100 мс.
    pub energy_curve_db: Vec<f32>,
    pub silence: Vec<Segment>,
    /// Секции из самоподобности chroma+энергии, не равномерная нарезка.
    pub sections: Vec<Section>,
    pub transitions: Vec<Transition>,
}

/// Переиспользуемое ядро: FFT-план и рабочие буферы живут между вызовами.
pub struct MusicAnalyzer {
    rate: u32,
    fft: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    input: Vec<f32>,
    spectrum: Vec<Complex<f32>>,
}

impl MusicAnalyzer {
    pub fn new(rate: u32) -> Result<Self> {
        sample_rate(rate)?;
        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_LEN);
        let window: Vec<f32> = (0..FFT_LEN)
            .map(|i| {
                (0.5 * (1.0 - (2.0 * std::f64::consts::PI * i as f64 / FFT_LEN as f64).cos()))
                    as f32
            })
            .collect();
        let input = fft.make_input_vec();
        let spectrum = fft.make_output_vec();
        Ok(Self {
            rate,
            fft,
            window,
            input,
            spectrum,
        })
    }

    pub fn reset(&mut self) {
        self.input.fill(0.0);
        self.spectrum.fill(Complex::new(0.0, 0.0));
    }

    pub fn analyze(
        &mut self,
        frames: &[StereoFrame],
        cancel: &AtomicBool,
    ) -> Result<MusicAnalysis> {
        sample_rate(self.rate)?;
        validate_frames(frames)?;
        if frames.len() > MAX_ANALYSIS_FRAMES {
            return Err(Error::Invalid("analysis input too long"));
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let duration_seconds = frames.len() as f64 / self.rate as f64;

        // Пики: sample peak точно, true peak 4x линейной оценкой.
        let mut sample_peak = 0.0f64;
        let mut true_peak = 0.0f64;
        let mut prev = [0.0f32; 2];
        for (i, f) in frames.iter().enumerate() {
            for ch in 0..2 {
                let s = f[ch] as f64;
                sample_peak = sample_peak.max(s.abs());
                if i > 0 {
                    for k in 1..4 {
                        let t = k as f64 / 4.0;
                        let v = (prev[ch] as f64) * (1.0 - t) + s * t;
                        true_peak = true_peak.max(v.abs());
                    }
                }
            }
            prev = *f;
        }
        true_peak = true_peak.max(sample_peak);

        // Моно-микс в один буфер (единственная полная копия входа).
        let mono: Vec<f32> = frames
            .iter()
            .map(|f| (f[0] as f64 * 0.5 + f[1] as f64 * 0.5) as f32)
            .collect();

        let empty = MusicAnalysis {
            version: ANALYSIS_VERSION,
            sample_rate: self.rate,
            frames: frames.len(),
            duration_seconds,
            bpm: None,
            beats: Vec::new(),
            beat_confidence: 0.0,
            key: None,
            rms: crate::analysis::rms(frames).unwrap_or([0.0; 2]),
            sample_peak,
            true_peak,
            integrated_lufs: None,
            energy_curve_db: Vec::new(),
            silence: Vec::new(),
            sections: Vec::new(),
            transitions: Vec::new(),
        };
        if mono.len() < FFT_LEN + HOP * 3 {
            // Слишком коротко для STFT: громкость/тишину всё равно считаем.
            let mut full = empty;
            let mut loud = crate::analysis::LoudnessMeter::new(self.rate)?;
            loud.process(frames)?;
            full.integrated_lufs = loud.reading().integrated_lufs;
            full.energy_curve_db = energy_curve(&mono, self.rate);
            full.silence = silence_segments(&mono, self.rate);
            return Ok(full);
        }

        // STFT: flux (full + kick-band) и chroma на кадр.
        let bin_hz = self.rate as f64 / FFT_LEN as f64;
        // Кик-полоса: фундаментал бочки 50–100 Гц; при 280 Гц туда пролезает
        // энергия хэта/шума и IOI строится по subdivision. 160 Гц отделяет кик.
        let low_bins = ((160.0 / bin_hz).floor() as usize).clamp(2, FFT_LEN / 2);
        let mut prev_mag: Option<Vec<f32>> = None;
        let mut novelty_low: Vec<f32> = Vec::new();
        // ВЧ-флюкс (>2 кГц) отличает перкуссию от тонального гула: у сустейна
        // там только численный шум, у любого транзиента — значимая энергия.
        let hf_start = ((2000.0 / bin_hz).ceil() as usize).min(FFT_LEN / 2);
        let mut hf_max = 0.0f32;
        let mut full_max = 0.0f32;
        let mut chromas: Vec<[f32; 12]> = Vec::new();
        let mut frame_energy: Vec<f32> = Vec::new();
        let mut stft_index = 0usize;
        let mut pos = 0usize;
        while pos + FFT_LEN <= mono.len() {
            if stft_index & CANCEL_MASK == 0 && cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            for (i, slot) in self.input.iter_mut().enumerate() {
                *slot = mono[pos + i] * self.window[i];
            }
            self.fft
                .process(&mut self.input, &mut self.spectrum)
                .map_err(|_| Error::Invalid("fft failed"))?;
            let half = FFT_LEN / 2;
            let mut mag = vec![0.0f32; half + 1];
            for (i, c) in self.spectrum.iter().enumerate().take(half + 1) {
                mag[i] = (c.re * c.re + c.im * c.im).sqrt();
            }
            // Логарифмический flux с ФИКСИРОВАННОЙ компрессией (aubio-стиль):
            // нормировка на пик кадра делала бы тихий хэт равным кику и
            // раздувала бы биения утечки сустейна — уровни должны различаться.
            let logmag: Vec<f32> = mag.iter().map(|&m| (1.0 + 10.0 * m).ln()).collect();
            if let Some(prev) = prev_mag.take() {
                let mut ff = 0.0f32;
                let mut lf = 0.0f32;
                let mut hf = 0.0f32;
                for i in 1..logmag.len() {
                    let d = (logmag[i] - prev[i]).max(0.0);
                    ff += d;
                    if i <= low_bins {
                        lf += d;
                    }
                    if i >= hf_start {
                        hf += d;
                    }
                }
                novelty_low.push(lf);
                hf_max = hf_max.max(hf);
                full_max = full_max.max(ff);
            }
            prev_mag = Some(logmag);
            chromas.push(chroma_of(&mag, bin_hz));
            frame_energy.push(mag.iter().skip(1).map(|&m| m * m).sum());
            stft_index += 1;
            pos += HOP;
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }

        let fps = self.rate as f64 / HOP as f64;
        let hf_ratio = if full_max > 1e-9 {
            hf_max / full_max
        } else {
            0.0
        };
        let bpm = estimate_bpm(&novelty_low, fps, cancel, hf_ratio)?;
        let (beats, beat_confidence) = beat_grid(
            &novelty_low,
            fps,
            bpm.map(|b| b.bpm),
            duration_seconds,
            FFT_LEN as f64 / self.rate as f64,
        );
        let key = estimate_key(&chromas, &frame_energy);
        let mut loud = crate::analysis::LoudnessMeter::new(self.rate)?;
        loud.process(frames)?;
        let integrated_lufs = loud.reading().integrated_lufs;
        let energy_curve_db = energy_curve(&mono, self.rate);
        let silence = silence_segments(&mono, self.rate);
        let sections = detect_sections(&chromas, &frame_energy, fps, duration_seconds);
        let transitions = detect_transitions(
            &beats,
            beat_confidence,
            &sections,
            &energy_curve_db,
            &silence,
            duration_seconds,
        );

        Ok(MusicAnalysis {
            version: ANALYSIS_VERSION,
            sample_rate: self.rate,
            frames: frames.len(),
            duration_seconds,
            bpm,
            beats,
            beat_confidence,
            key,
            rms: crate::analysis::rms(frames).unwrap_or([0.0; 2]),
            sample_peak,
            true_peak,
            integrated_lufs,
            energy_curve_db,
            silence,
            sections,
            transitions,
        })
    }
}

/// Разовый анализ без хранения ядра между вызовами.
pub fn analyze_music(
    frames: &[StereoFrame],
    rate: u32,
    cancel: &AtomicBool,
) -> Result<MusicAnalysis> {
    MusicAnalyzer::new(rate)?.analyze(frames, cancel)
}

fn chroma_of(mag: &[f32], bin_hz: f64) -> [f32; 12] {
    // HPCP-стиль: голосуют только спектральные пики (Essentia отсекает лепестки
    // тем же приёмом), частота — с параболической интерполяцией, вес — энергией.
    // Октава (2f) безопасна всегда (тот же pitch-класс); 3f не добавляем:
    // квинта через октаву путает мажор/минор на триадах.
    let mut chroma = [0.0f32; 12];
    let top_bin = ((5000.0 / bin_hz).floor() as usize).min(mag.len().saturating_sub(2));
    let peak = mag.iter().take(top_bin + 1).copied().fold(0.0f32, f32::max);
    let floor = peak * 0.02;
    for i in 1..=top_bin {
        let m = mag[i];
        if m <= floor || m < mag[i - 1] || m < mag[i + 1] {
            continue;
        }
        let (a, b, c) = (mag[i - 1] as f64, m as f64, mag[i + 1] as f64);
        let denom = a - 2.0 * b + c;
        let shift = if denom.abs() > 1e-12 {
            (0.5 * (a - c) / denom).clamp(-0.5, 0.5)
        } else {
            0.0
        };
        let freq = (i as f64 + shift) * bin_hz;
        if freq < 55.0 {
            continue;
        }
        let w = m * m;
        vote(&mut chroma, freq, w, 1.0);
        vote(&mut chroma, freq * 2.0, w, 0.3);
    }
    chroma
}

fn vote(chroma: &mut [f32; 12], freq: f64, weight: f32, gain: f32) {
    if freq <= 0.0 {
        return;
    }
    let semitone = 12.0 * (freq / 440.0).log2() + 69.0; // MIDI, A4=69
    let rounded = semitone.round();
    let dist = (semitone - rounded).abs();
    if dist > 0.5 {
        return;
    }
    // MIDI 60 = C4 → C-базированный pitch-класс.
    let midi_c = ((rounded as i64 - 60).rem_euclid(12)) as usize;
    chroma[midi_c] += weight * gain * (1.0 - dist as f32);
}

fn median(mut v: Vec<f32>) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[v.len() / 2]
}

/// Пики low-band novelty выше median + 2*MAD, не ближе 80 мс.
/// Возвращает (время, высота) — высота нужна, чтобы IOI строился по сильным
/// долям (кик), а не по слабым subdivision (хэт).
fn onset_times(novelty: &[f32], fps: f64) -> Vec<(f64, f32)> {
    if novelty.len() < 3 {
        return Vec::new();
    }
    let med = median(novelty.to_vec());
    let mad = median(novelty.iter().map(|&x| (x - med).abs()).collect());
    let max = novelty.iter().copied().fold(0.0f32, f32::max);
    // При разреженных пиках MAD = 0 — страховаться долей максимума, иначе
    // тишина (max = 0) даёт thr = 0 и строгий `>` всё равно не срабатывает.
    let thr = (med + 2.0 * mad).max(0.15 * max);
    if thr <= 1e-9 {
        return Vec::new();
    }
    let min_gap = (0.08 * fps).ceil() as usize;
    let mut out = Vec::new();
    let mut last = 0usize.wrapping_sub(min_gap + 1);
    for i in 1..novelty.len() - 1 {
        if novelty[i] > thr
            && novelty[i] >= novelty[i - 1]
            && novelty[i] > novelty[i + 1]
            && i.wrapping_sub(last) > min_gap
        {
            out.push((i as f64 / fps, novelty[i]));
            last = i;
        }
    }
    out
}

/// Пик гистограммы IOI (бин 5 мс, диапазон под 50–240 BPM) → темп пульса.
/// Квантование onset-кадрами сглаживаем средним интервалов в окне пика ±1 бин.
fn ioi_tempo(onsets: &[f64]) -> Option<f64> {
    if onsets.len() < 4 {
        return None;
    }
    let mut dts: Vec<f64> = Vec::new();
    for w in onsets.windows(2) {
        let dt = w[1] - w[0];
        if (0.25..=1.31).contains(&dt) {
            dts.push(dt);
        }
    }
    if dts.len() < 3 {
        return None;
    }
    let mut hist = vec![0u32; 212]; // 0.25..1.31 с шагом 5 мс
    for &dt in &dts {
        hist[((dt - 0.25) / 0.005) as usize] += 1;
    }
    let (best, _) = hist
        .iter()
        .enumerate()
        .max_by_key(|(_, c)| **c)
        .map(|(i, &c)| (i, c))?;
    let lo = 0.25 + best.saturating_sub(1) as f64 * 0.005;
    let hi = 0.25 + (best + 2) as f64 * 0.005;
    let near: Vec<f64> = dts
        .into_iter()
        .filter(|&dt| (lo..hi).contains(&dt))
        .collect();
    if near.is_empty() {
        return None;
    }
    Some(60.0 / (near.iter().sum::<f64>() / near.len() as f64))
}

fn autocorr_best(x: &[f32], fps: f64) -> Option<(f64, f64)> {
    // Возвращает (уточнённый лаг в кадрах, лучший счёт).
    if x.len() < 8 {
        return None;
    }
    let mean = x.iter().map(|&v| v as f64).sum::<f64>() / x.len() as f64;
    let y: Vec<f64> = x.iter().map(|&v| v as f64 - mean).collect();
    let r0 = y.iter().map(|v| v * v).sum::<f64>() / y.len() as f64;
    if r0 <= 1e-18 {
        return None;
    }
    let min_lag = ((60.0 / MAX_BPM) * fps).round().max(2.0) as usize;
    let max_lag = ((60.0 / MIN_BPM) * fps).round() as usize;
    let mut scored: Vec<(usize, f64)> = Vec::new();
    for lag in min_lag..=max_lag.min(y.len() - 2) {
        let mut s = 0.0;
        for i in 0..y.len() - lag {
            s += y[i] * y[i + lag];
        }
        scored.push((lag, s / (y.len() - lag) as f64 / r0));
    }
    if scored.is_empty() {
        return None;
    }
    let (best_i, best_lag, best_s) = scored
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, &v)| (i, v.0, v.1))?;
    // Параболическая интерполяция для точности <1%.
    let refined = if best_i > 0 && best_i + 1 < scored.len() {
        let (ym, y0, yp) = (scored[best_i - 1].1, best_s, scored[best_i + 1].1);
        let denom = ym - 2.0 * y0 + yp;
        if denom.abs() > 1e-12 {
            best_lag as f64 + 0.5 * (ym - yp) / denom
        } else {
            best_lag as f64
        }
    } else {
        best_lag as f64
    };
    Some((refined, best_s))
}

fn estimate_bpm(
    novelty: &[f32],
    fps: f64,
    cancel: &AtomicBool,
    hf_ratio: f32,
) -> Result<Option<BpmEstimate>> {
    if novelty.len() < (fps * 2.0) as usize {
        return Ok(None);
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    // Без ВЧ-транзиентов ритма нет: тональный гул (чистые синусы) даёт
    // периодические биения утечки с высоким крестом, но нулевой ВЧ-флюкс.
    if hf_ratio < 0.02 {
        return Ok(None);
    }
    // Крест-фактор: перкуссивная огибающая разрежена (пики ≫ фон), а гул
    // сустейна и шум — нет. Без этого sustained-аккорд с биениями утечки
    // даёт ложный уверенный BPM. Порог 3.0: синкопа 3.9 проходит, шум 2.8
    // и сустейн 2.0 — нет (синтетика; для реальной музыки калибруется отдельно).
    let rms = (novelty.iter().map(|&v| (v as f64).powi(2)).sum::<f64>()
        / novelty.len().max(1) as f64)
        .sqrt();
    let cmax = novelty.iter().copied().fold(0.0f32, f32::max) as f64;
    if cmax <= 1e-9 || cmax / rms.max(1e-12) < 3.0 {
        return Ok(None);
    }
    let Some((lag, _)) = autocorr_best(novelty, fps) else {
        return Ok(None);
    };
    let mut tempo = 60.0 * fps / lag;
    // Правило октавы: IOI задаёт только октаву, значение — уточнённый пик
    // автокорреляции (у IOI квантование ±2%, у параболы <0.5%).
    // Чистый пульс даёт равные пики на 1x/2x: raw≈ioi/2 или raw≈ioi*2 означает,
    // что автокорреляция села на октаву — снапаем raw. Неоктавные расхождения
    // (синкопа с ghost-интервалами) не трогаем.
    // Кик+хэт даёт победу кику уже в low-band (хэт выше 280 Гц почти не входит).
    let onsets = onset_times(novelty, fps);
    // IOI — только по сильным долям (≥ половины максимума): слабые subdivision
    // дают октаву вверх и неверный снап.
    // Возвращает (время, высота) — высота нужна, чтобы IOI строился по сильным
    // долям (кик), а не по слабым subdivision (хэт).
    let peak_h = onsets.iter().map(|&(_, h)| h).fold(0.0f32, f32::max);
    let strong: Vec<f64> = onsets
        .iter()
        .filter(|&&(_, h)| h >= 0.5 * peak_h)
        .map(|&(t, _)| t)
        .collect();
    if let Some(ioi) = ioi_tempo(&strong) {
        if (MIN_BPM..=MAX_BPM).contains(&ioi) {
            let ratio = tempo / ioi;
            if (ratio - 0.5).abs() < 0.04 {
                tempo *= 2.0;
            } else if (ratio - 2.0).abs() < 0.08 {
                tempo /= 2.0;
            }
        }
    }
    if !(MIN_BPM..=MAX_BPM).contains(&tempo) {
        return Ok(None);
    }
    // Уверенность: высота пика над медианой сетки.
    let conf = confidence_of(novelty, fps, 60.0 * fps / tempo);
    if conf < 0.35 {
        return Ok(None);
    }
    Ok(Some(BpmEstimate {
        bpm: tempo,
        confidence: conf.clamp(0.0, 1.0),
    }))
}

fn autocorr_at(x: &[f32], lag: usize) -> f64 {
    if lag == 0 || lag + 2 > x.len() {
        return f64::NEG_INFINITY;
    }
    let mean = x.iter().map(|&v| v as f64).sum::<f64>() / x.len() as f64;
    let mut s = 0.0;
    let mut r0 = 0.0;
    for (i, &v) in x.iter().enumerate() {
        let d = v as f64 - mean;
        r0 += d * d;
        if i + lag < x.len() {
            s += d * (x[i + lag] as f64 - mean);
        }
    }
    s / r0.max(1e-18) * (x.len() as f64 / (x.len() - lag) as f64)
}

fn confidence_of(x: &[f32], fps: f64, lag: f64) -> f64 {
    let min_lag = ((60.0 / MAX_BPM) * fps).round().max(2.0) as usize;
    let max_lag = ((60.0 / MIN_BPM) * fps).round() as usize;
    let mut scores = Vec::new();
    for lag_i in min_lag..=max_lag.min(x.len().saturating_sub(2)) {
        scores.push(autocorr_at(x, lag_i) as f32);
    }
    if scores.is_empty() {
        return 0.0;
    }
    let med = median(scores);
    let best = autocorr_at(x, lag.round().clamp(1.0, usize::MAX as f64) as usize);
    ((best - med as f64) / (1.0 - med as f64 + 1e-9)).clamp(0.0, 1.0)
}

fn beat_grid(
    novelty: &[f32],
    fps: f64,
    bpm: Option<f64>,
    duration: f64,
    frame_end_offset_s: f64,
) -> (Vec<f64>, f64) {
    let Some(bpm) = bpm else {
        return (Vec::new(), 0.0);
    };
    let period = 60.0 / bpm;
    if period <= 0.0 || !period.is_finite() {
        return (Vec::new(), 0.0);
    }
    // Фазовый поиск шагом 5 мс по интерполированной огибающей.
    // Flux кадра i относится к моменту, когда кадр прибыл целиком
    // (конец окна: i*HOP + FFT_LEN), — конвенция realtime onset detection
    // (aubio): иначе сетка систематически спешит на длину окна (~85 мс).
    let at = |t: f64| -> f32 {
        let pos = (t - frame_end_offset_s) * fps;
        if pos < 0.0 || pos as usize >= novelty.len() {
            return 0.0;
        }
        let i = pos.floor() as usize;
        let frac = (pos - i as f64) as f32;
        let a = novelty[i];
        let b = novelty.get(i + 1).copied().unwrap_or(a);
        a + (b - a) * frac
    };
    let mean = novelty.iter().map(|&v| v as f64).sum::<f64>() / novelty.len().max(1) as f64;
    let steps = (period / 0.005).ceil() as usize;
    let (mut best_off, mut best_score) = (0.0, f64::NEG_INFINITY);
    for s in 0..steps.max(1) {
        let off = s as f64 * 0.005;
        let mut sum = 0.0;
        let mut n = 0u32;
        let mut t = off;
        while t < duration {
            sum += at(t) as f64;
            n += 1;
            t += period;
        }
        let score = if n > 0 { sum / n as f64 } else { 0.0 };
        if score > best_score {
            best_score = score;
            best_off = off;
        }
    }
    let mut beats = Vec::new();
    let mut t = best_off;
    // Продолжить сетку назад, чтобы первый бит не терялся при сдвиге фазы.
    while t - period >= 0.0 {
        t -= period;
    }
    while t < duration {
        beats.push(t);
        t += period;
    }
    let conf = if mean > 1e-12 {
        ((best_score / mean - 1.0) / 3.0).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (beats, conf)
}

fn estimate_key(chromas: &[[f32; 12]], energy: &[f32]) -> Option<KeyEstimate> {
    if chromas.is_empty() || chromas.len() != energy.len() {
        return None;
    }
    let mut avg = [0.0f64; 12];
    let mut wsum = 0.0f64;
    for (c, &e) in chromas.iter().zip(energy.iter()) {
        let w = e as f64;
        wsum += w;
        for i in 0..12 {
            avg[i] += c[i] as f64 * w;
        }
    }
    if wsum <= 1e-12 {
        return None;
    }
    for v in avg.iter_mut() {
        *v /= wsum;
    }
    let peak = avg.iter().copied().fold(0.0f64, f64::max);
    // Перцептуальная компрессия (стандартный приём chroma-пайплайнов):
    // ровняет сильные доли, иначе энергия пика решает за тональность
    // случайным везением попадания в бин.
    let mut comp = [0.0f64; 12];
    for i in 0..12 {
        comp[i] = (1.0 + 100.0 * avg[i] / peak.max(1e-18)).ln();
    }
    let mean = comp.iter().sum::<f64>() / 12.0;
    let norm = comp.iter().map(|v| (v - mean).powi(2)).sum::<f64>().sqrt();
    if norm <= 1e-12 {
        return None; // плоская chroma (шум/тишина) — тональности нет.
    }
    let mut scored: Vec<(usize, bool, f64)> = Vec::new();
    for shift in 0..12 {
        let r_major = correlate(&comp, &KS_MAJOR, shift);
        let r_minor = correlate(&comp, &KS_MINOR, shift);
        scored.push((shift, true, r_major));
        scored.push((shift, false, r_minor));
    }
    scored.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    let (shift, major, r1) = scored[0];
    let r2 = scored[1].2;
    if r1 < 0.55 {
        return None;
    }
    Some(KeyEstimate {
        tonic: PitchClass::from_semitone(shift),
        mode: if major { Mode::Major } else { Mode::Minor },
        confidence: ((r1 - r2) / (1.0 - r2 + 1e-9)).clamp(0.0, 1.0),
    })
}

fn correlate(avg: &[f64; 12], profile: &[f64; 12], shift: usize) -> f64 {
    let rotated: Vec<f64> = (0..12).map(|i| profile[(i + 12 - shift) % 12]).collect();
    let m_a = avg.iter().sum::<f64>() / 12.0;
    let m_r = rotated.iter().sum::<f64>() / 12.0;
    let mut num = 0.0;
    let mut da = 0.0;
    let mut dr = 0.0;
    for i in 0..12 {
        num += (avg[i] - m_a) * (rotated[i] - m_r);
        da += (avg[i] - m_a).powi(2);
        dr += (rotated[i] - m_r).powi(2);
    }
    num / (da * dr).sqrt().max(1e-18)
}

fn energy_curve(mono: &[f32], rate: u32) -> Vec<f32> {
    let step = (rate as f64 * 0.1).round().max(1.0) as usize;
    let mut out = Vec::new();
    for block in mono.chunks(step) {
        let rms = (block.iter().map(|&s| (s as f64).powi(2)).sum::<f64>()
            / block.len().max(1) as f64)
            .sqrt();
        out.push((20.0 * (rms.max(1e-9)).log10()) as f32);
    }
    out
}

fn silence_segments(mono: &[f32], rate: u32) -> Vec<Segment> {
    let threshold = 10.0f64.powf(SILENCE_DBFS / 20.0) as f32;
    let step = (rate as f64 * 0.01).round().max(1.0) as usize;
    let min_len = (SILENCE_MIN_SECONDS * rate as f64 / step as f64).ceil() as usize;
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    let mut idx = 0usize;
    for block in mono.chunks(step) {
        let peak = block.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        if peak < threshold {
            if start.is_none() {
                start = Some(idx);
            }
        } else if let Some(s) = start.take() {
            if idx - s >= min_len {
                out.push(Segment {
                    start_seconds: s as f64 * step as f64 / rate as f64,
                    end_seconds: idx as f64 * step as f64 / rate as f64,
                });
            }
        }
        idx += 1;
    }
    if let Some(s) = start {
        if idx - s >= min_len {
            out.push(Segment {
                start_seconds: s as f64 * step as f64 / rate as f64,
                end_seconds: (mono.len() as f64) / rate as f64,
            });
        }
    }
    out
}

fn detect_sections(chromas: &[[f32; 12]], energy: &[f32], fps: f64, duration: f64) -> Vec<Section> {
    // Признак окна 0.5 с: нормированная chroma + log-энергия. Кластеризация
    // жадным сопоставлением с прототипами (косинус ≥ 0.93), без равномерной нарезки.
    let win = (0.5 * fps).round().max(1.0) as usize;
    if chromas.len() < win * 2 {
        return Vec::new();
    }
    let mut feats: Vec<(f64, Vec<f32>)> = Vec::new();
    let mut t = 0usize;
    while t + win <= chromas.len() {
        let mut c = [0.0f64; 12];
        let mut e = 0.0f64;
        for i in t..t + win {
            for k in 0..12 {
                c[k] += chromas[i][k] as f64;
            }
            e += energy[i] as f64;
        }
        let n = c.iter().sum::<f64>();
        let mut v: Vec<f32> = c.iter().map(|&x| (x / n.max(1e-9)) as f32).collect();
        v.push(((e / win as f64 + 1e-9).ln() / 10.0) as f32);
        feats.push((t as f64 / fps, v));
        t += win;
    }
    if feats.len() < 2 {
        return Vec::new();
    }
    let mut protos: Vec<Vec<f32>> = Vec::new();
    let mut labels: Vec<usize> = Vec::new();
    let mut sims: Vec<f64> = Vec::new();
    for (_, f) in &feats {
        let mut best = (usize::MAX, -1.0f64);
        for (p, proto) in protos.iter().enumerate() {
            let s = cosine(f, proto);
            if s > best.1 {
                best = (p, s);
            }
        }
        if best.1 >= 0.93 {
            labels.push(best.0);
            sims.push(best.1);
        } else if protos.len() < 8 {
            protos.push(f.clone());
            labels.push(protos.len() - 1);
            sims.push(1.0);
        } else {
            labels.push(best.0);
            sims.push(best.1.max(0.0));
        }
    }
    // Слияние соседних окон с одинаковой меткой.
    let mut sections = Vec::new();
    let mut i = 0usize;
    while i < labels.len() {
        let mut j = i;
        while j + 1 < labels.len() && labels[j + 1] == labels[i] {
            j += 1;
        }
        let mean_sim = sims[i..=j].iter().sum::<f64>() / (j - i + 1) as f64;
        sections.push(Section {
            label: (b'A' + (labels[i] % 26) as u8) as char,
            start_seconds: feats[i].0,
            end_seconds: if j + 1 < feats.len() {
                feats[j + 1].0
            } else {
                duration
            },
            confidence: mean_sim.clamp(0.0, 1.0),
        });
        i = j + 1;
    }
    sections
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let mut num = 0.0f64;
    let mut da = 0.0f64;
    let mut db = 0.0f64;
    for (x, y) in a.iter().zip(b.iter()) {
        num += *x as f64 * *y as f64;
        da += (*x as f64).powi(2);
        db += (*y as f64).powi(2);
    }
    num / (da * db).sqrt().max(1e-18)
}

fn detect_transitions(
    beats: &[f64],
    beat_confidence: f64,
    sections: &[Section],
    energy_db: &[f32],
    silence: &[Segment],
    duration: f64,
) -> Vec<Transition> {
    let mut out = Vec::new();
    if beats.len() >= 4 && beat_confidence > 0.3 {
        let intro_end = silence
            .first()
            .filter(|s| s.start_seconds <= 0.05)
            .map(|s| s.end_seconds)
            .unwrap_or(0.0);
        let mix_in = beats
            .iter()
            .copied()
            .find(|&b| b >= intro_end)
            .unwrap_or(beats[0]);
        out.push(Transition {
            at_seconds: mix_in,
            kind: TransitionKind::MixIn,
            confidence: beat_confidence,
        });
        let outro_start = silence
            .last()
            .filter(|s| (s.end_seconds - duration).abs() <= 0.05)
            .map(|s| s.start_seconds)
            .unwrap_or(duration);
        if let Some(&last) = beats.iter().rfind(|&&b| b <= outro_start) {
            if last > mix_in {
                out.push(Transition {
                    at_seconds: last,
                    kind: TransitionKind::MixOut,
                    confidence: beat_confidence,
                });
            }
        }
    }
    for s in sections.iter().skip(1) {
        out.push(Transition {
            at_seconds: s.start_seconds,
            kind: TransitionKind::SectionBoundary,
            confidence: s.confidence,
        });
    }
    // Энергетические провалы: локальные минимумы ниже 0.3 медианы, снап к биту ±100 мс.
    if !energy_db.is_empty() && !beats.is_empty() {
        let mut sorted = energy_db.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let med = sorted[sorted.len() / 2];
        let mut dips = 0;
        for i in 1..energy_db.len() - 1 {
            if dips >= 4 {
                break;
            }
            if energy_db[i] < energy_db[i - 1]
                && energy_db[i] <= energy_db[i + 1]
                && energy_db[i] < med - 5.0
            {
                let t = i as f64 * 0.1;
                if let Some(&b) = beats.iter().min_by(|a, b| {
                    ((**a - t).abs())
                        .partial_cmp(&((**b - t).abs()))
                        .unwrap_or(std::cmp::Ordering::Equal)
                }) {
                    if (b - t).abs() <= 0.1 && !out.iter().any(|tr| (tr.at_seconds - b).abs() < 0.2)
                    {
                        out.push(Transition {
                            at_seconds: b,
                            kind: TransitionKind::EnergyDip,
                            confidence: 0.5,
                        });
                        dips += 1;
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| {
        a.at_seconds
            .partial_cmp(&b.at_seconds)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(8);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    fn sine(freq: f64, seconds: f64, rate: u32, peak: f32) -> Vec<StereoFrame> {
        let n = (seconds * rate as f64) as usize;
        (0..n)
            .map(|i| {
                let s = (2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin() as f32
                    * peak;
                [s, s]
            })
            .collect()
    }

    /// Клик-трек: импульс с экспоненциальным спадом на каждую долю.
    fn click_train(bpm: f64, seconds: f64, rate: u32, weak: bool) -> Vec<StereoFrame> {
        let n = (seconds * rate as f64) as usize;
        let mut out = vec![[0.0; 2]; n];
        let period = 60.0 / bpm * rate as f64;
        let amp = if weak { 0.15 } else { 0.9 };
        let mut k = 0usize;
        loop {
            let pos = (k as f64 * period).round() as usize;
            if pos + 64 >= n {
                break;
            }
            for i in 0..64 {
                let s = amp * (-(i as f32) / 8.0).exp() * (i as f32 * 40.0).sin().abs();
                out[pos + i][0] += s;
                out[pos + i][1] += s;
            }
            k += 1;
        }
        out
    }

    fn kick_hat(bpm: f64, seconds: f64, rate: u32) -> Vec<StereoFrame> {
        // Кик (низкий синус-пинг) на доли, хэт (короткий шум) на восьмые слабее.
        let n = (seconds * rate as f64) as usize;
        let mut out = vec![[0.0; 2]; n];
        let beat = 60.0 / bpm * rate as f64;
        let mut k = 0usize;
        loop {
            let pos = (k as f64 * beat).round() as usize;
            if pos + 512 >= n {
                break;
            }
            for i in 0..512 {
                let env = (-(i as f32) / 120.0).exp();
                let s = 0.8
                    * env
                    * (2.0 * std::f64::consts::PI * 60.0 * i as f64 / rate as f64).sin() as f32;
                out[pos + i][0] += s;
                out[pos + i][1] += s;
            }
            // Восьмая: слабый высокочастотный тик.
            let off = (pos as f64 + beat / 2.0).round() as usize;
            if off + 32 < n {
                let mut x: u64 = 0x12345678 + k as u64;
                for i in 0..32 {
                    x = x.wrapping_mul(6364136223846793005).wrapping_add(21);
                    let nz = ((x >> 33) as f32 / u32::MAX as f32 - 0.5) * 0.2;
                    out[off + i][0] += nz;
                    out[off + i][1] += nz;
                }
            }
            k += 1;
        }
        out
    }

    fn chord_tone(freqs: &[f64], seconds: f64, rate: u32) -> Vec<StereoFrame> {
        let n = (seconds * rate as f64) as usize;
        (0..n)
            .map(|i| {
                let mut s = 0.0f32;
                for &f in freqs {
                    s += (2.0 * std::f64::consts::PI * f * i as f64 / rate as f64).sin() as f32;
                }
                s *= 0.2;
                [s, s]
            })
            .collect()
    }

    fn xorshift_noise(seconds: f64, rate: u32) -> Vec<StereoFrame> {
        let n = (seconds * rate as f64) as usize;
        let mut x: u64 = 0x9E3779B97F4A7C15;
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(21);
                let s = (x >> 33) as f32 / u32::MAX as f32 - 0.5;
                [s, s]
            })
            .collect()
    }

    fn check_bpm(frames: &[StereoFrame], rate: u32, expected: f64) {
        let a = analyze_music(frames, rate, &no_cancel()).unwrap();
        let got = a.bpm.expect("bpm must be detected").bpm;
        let err = ((got - expected) / expected).abs();
        assert!(err <= 0.01, "expected {expected}, got {got} (err {err})");
        assert!(!a.beats.is_empty());
    }

    #[test]
    fn bpm_grid_60_to_174() {
        for bpm in [60.0, 85.0, 90.0, 120.0, 128.0, 140.0, 160.0, 174.0] {
            let frames = click_train(bpm, 10.0, 48_000, false);
            check_bpm(&frames, 48_000, bpm);
        }
    }

    #[test]
    fn bpm_weak_and_kick_hat_layers() {
        check_bpm(&click_train(120.0, 10.0, 48_000, true), 48_000, 120.0);
        check_bpm(&kick_hat(100.0, 10.0, 48_000), 48_000, 100.0);
        check_bpm(&kick_hat(128.0, 10.0, 44_100), 44_100, 128.0);
    }

    #[test]
    fn bpm_syncopated_and_drift() {
        // Синкопа: доли + сдвиг на 3/4 периода слабее.
        let rate = 48_000;
        let bpm = 96.0;
        let n = (10.0 * rate as f64) as usize;
        let mut frames = vec![[0.0; 2]; n];
        let period = 60.0 / bpm * rate as f64;
        let mut k = 0usize;
        loop {
            let pos = (k as f64 * period).round() as usize;
            if pos + 64 >= n {
                break;
            }
            for i in 0..64 {
                let s = 0.8 * (-(i as f32) / 8.0).exp();
                frames[pos + i][0] += s;
                frames[pos + i][1] += s;
            }
            let off = (pos as f64 + period * 0.75).round() as usize;
            if off + 32 < n {
                for i in 0..32 {
                    let s = 0.4 * (-(i as f32) / 8.0).exp();
                    frames[off + i][0] += s;
                    frames[off + i][1] += s;
                }
            }
            k += 1;
        }
        check_bpm(&frames, rate, bpm);
        // Дрейф 120→140: оценка внутри диапазона, без panic.
        let n2 = (12.0 * rate as f64) as usize;
        let mut drift = vec![[0.0; 2]; n2];
        let mut phase = 0.0;
        for (i, frame) in drift.iter_mut().enumerate() {
            let t = i as f64 / rate as f64;
            let inst = 120.0 + 20.0 * t / 12.0;
            phase += inst / 60.0 / rate as f64;
            let frac = (phase % 1.0 + 1.0) % 1.0;
            if frac < 64.0 / rate as f64 * inst / 60.0 {
                let s = 0.8 * (-(frac * rate as f64 * 60.0 / inst) as f32 / 8.0).exp();
                frame[0] += s;
                frame[1] += s;
            }
        }
        let a = analyze_music(&drift, rate, &no_cancel()).unwrap();
        let got = a.bpm.expect("drift bpm").bpm;
        assert!((115.0..=145.0).contains(&got), "drift {got}");
    }

    #[test]
    fn bpm_sample_rates_and_stereo() {
        for rate in [44_100, 48_000, 96_000] {
            let frames = click_train(120.0, 8.0, rate, false);
            check_bpm(&frames, rate, 120.0);
        }
        // Моно (L==R) и стерео-вариант дают тот же BPM.
        let mut stereo = click_train(124.0, 8.0, 48_000, false);
        for f in stereo.iter_mut().step_by(2) {
            f[1] *= 0.5;
        }
        check_bpm(&stereo, 48_000, 124.0);
    }

    fn median_beat_error(beats: &[f64], refs: &[f64]) -> f64 {
        let mut errs: Vec<f64> = beats
            .iter()
            .map(|b| {
                refs.iter()
                    .map(|r| (b - r).abs())
                    .fold(f64::INFINITY, f64::min)
            })
            .collect();
        errs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        errs[errs.len() / 2]
    }

    #[test]
    fn beatgrid_median_error_under_20ms() {
        for bpm in [90.0, 120.0, 140.0] {
            let rate = 48_000;
            let frames = click_train(bpm, 10.0, rate, false);
            let a = analyze_music(&frames, rate, &no_cancel()).unwrap();
            let period = 60.0 / bpm;
            let refs: Vec<f64> = (0..(10.0 / period) as usize)
                .map(|k| k as f64 * period)
                .collect();
            let err = median_beat_error(&a.beats, &refs);
            assert!(err <= 0.020, "bpm {bpm}: median err {err}");
        }
    }

    #[test]
    fn key_unambiguous_chords() {
        // C–E–G → C major; A–C–E → A minor (A3/C4/E4).
        let cases = [
            (vec![261.63, 329.63, 392.00], PitchClass::C, Mode::Major),
            (vec![220.00, 261.63, 329.63], PitchClass::A, Mode::Minor),
            (vec![392.00, 493.88, 587.33], PitchClass::G, Mode::Major),
            (vec![329.63, 392.00, 493.88], PitchClass::E, Mode::Minor),
        ];
        for (freqs, tonic, mode) in cases {
            let frames = chord_tone(&freqs, 5.0, 48_000);
            let a = analyze_music(&frames, 48_000, &no_cancel()).unwrap();
            let key = a.key.expect("key must be detected");
            assert_eq!(key.tonic, tonic, "freqs {freqs:?}");
            assert_eq!(key.mode, mode);
        }
    }

    #[test]
    fn silence_noise_and_rhythm_free() {
        let rate = 48_000;
        let silence = vec![[0.0; 2]; rate as usize * 4];
        let a = analyze_music(&silence, rate, &no_cancel()).unwrap();
        assert!(a.bpm.is_none());
        assert!(a.beats.is_empty());
        assert!(a.key.is_none());
        assert!(a.integrated_lufs.is_none());
        assert_eq!(a.silence.len(), 1);
        // Шум: без паники, BPM либо нет, либо с низкой уверенностью.
        let noise = xorshift_noise(6.0, rate);
        let a = analyze_music(&noise, rate, &no_cancel()).unwrap();
        assert!(a.bpm.is_none_or(|b| b.confidence < 0.9));
        // Пауза без выраженного ритма: аккорд-пауза — тональность есть, ритма нет.
        let pad = chord_tone(&[261.63, 329.63, 392.00], 6.0, rate);
        let a = analyze_music(&pad, rate, &no_cancel()).unwrap();
        assert!(a.key.is_some());
        assert!(a.bpm.is_none_or(|b| b.confidence < 0.6));
    }

    #[test]
    fn degenerate_inputs_do_not_panic() {
        let rate = 48_000;
        let a = analyze_music(&[], rate, &no_cancel()).unwrap();
        assert_eq!(a.version, ANALYSIS_VERSION);
        assert!(a.bpm.is_none());
        let single = vec![[0.5, -0.5]];
        let a = analyze_music(&single, rate, &no_cancel()).unwrap();
        assert!(a.bpm.is_none());
        assert!(validate_frames(&[[f32::NAN, 0.0]]).is_err());
        assert!(validate_frames(&[[17.0, 0.0]]).is_err());
        assert!(analyze_music(&[[0.0; 2]; 8], 7000, &no_cancel()).is_err());
        let cancelled = AtomicBool::new(true);
        assert!(analyze_music(&click_train(120.0, 4.0, rate, false), rate, &cancelled).is_err());
    }

    #[test]
    fn lufs_streaming_matches_offline_reference() {
        // Независимый эталон BS.1770 в тесте: те же K-фильтры, но цельный расчёт
        // (f64, блочная средняя мощность, гейты -70/-10). Расхождение ≤ 0.5 LU.
        let rate = 48_000;
        let frames = sine(440.0, 6.0, rate, 0.5);
        let a = analyze_music(&frames, rate, &no_cancel()).unwrap();
        let got = a.integrated_lufs.expect("lufs");
        let expected = reference_lufs(&frames, rate);
        assert!(
            (got - expected).abs() <= 0.5,
            "got {got}, reference {expected}"
        );
        assert!((a.sample_peak - 0.5).abs() < 0.01);
        assert!(a.true_peak + 1e-6 >= a.sample_peak);
    }

    /// Независимый эталон BS.1770 в тесте: те же K-фильтры по формулам стандарта,
    /// но цельный расчёт своим кодом (f64, блочная средняя мощность, гейты -70/-10).
    /// Проверяет streaming-логику `LoudnessMeter`, а не дублирует её.
    fn reference_lufs(frames: &[StereoFrame], rate: u32) -> f64 {
        #[derive(Clone, Copy)]
        struct Bq {
            b: [f64; 3],
            a: [f64; 2],
            x1: f64,
            x2: f64,
            y1: f64,
            y2: f64,
        }
        impl Bq {
            fn tick(&mut self, x: f64) -> f64 {
                let y = self.b[0] * x + self.b[1] * self.x1 + self.b[2] * self.x2
                    - self.a[0] * self.y1
                    - self.a[1] * self.y2;
                self.x2 = self.x1;
                self.x1 = x;
                self.y2 = self.y1;
                self.y1 = y;
                y
            }
        }
        let (sb, sa) = k_shelf(rate);
        let (hb, ha) = k_highpass(rate);
        // Канал: shelf → highpass, своё состояние на фильтр.
        let mk = || Bq {
            b: [0.0; 3],
            a: [0.0; 2],
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        };
        let mut ch: [[Bq; 2]; 2] = std::array::from_fn(|_| std::array::from_fn(|_| mk()));
        for pair in ch.iter_mut() {
            pair[0].b = sb;
            pair[0].a = sa;
            pair[1].b = hb;
            pair[1].a = ha;
        }
        let win = (rate as f64 * 0.4).round() as usize;
        let hop = (rate as f64 * 0.1).round() as usize;
        let mut ring = vec![0.0f64; win];
        let mut pos = 0usize;
        let mut sum = 0.0f64;
        let mut blocks: Vec<f64> = Vec::new();
        let mut until = win;
        for f in frames {
            let mut e = 0.0;
            for (c, pair) in ch.iter_mut().enumerate() {
                let y1 = pair[0].tick(f[c] as f64);
                let y2 = pair[1].tick(y1);
                e += y2 * y2;
            }
            sum += e - ring[pos];
            ring[pos] = e;
            pos = (pos + 1) % win;
            until -= 1;
            if until == 0 {
                blocks.push(sum / win as f64);
                until = hop;
            }
        }
        let abs_gate = 10.0f64.powf((-70.0 + 0.691) / 10.0);
        let gated: Vec<f64> = blocks.into_iter().filter(|&v| v >= abs_gate).collect();
        if gated.is_empty() {
            return f64::NEG_INFINITY;
        }
        let rel = gated.iter().sum::<f64>() / gated.len() as f64 * 0.1;
        let gated2: Vec<f64> = gated
            .into_iter()
            .filter(|&v| v >= abs_gate.max(rel))
            .collect();
        if gated2.is_empty() {
            return f64::NEG_INFINITY;
        }
        -0.691 + 10.0 * (gated2.iter().sum::<f64>() / gated2.len() as f64).log10()
    }

    fn k_shelf(rate: u32) -> ([f64; 3], [f64; 2]) {
        let k = (std::f64::consts::PI * 1681.974450955533 / rate as f64).tan();
        let q = 0.7071752369554196;
        let vh = 10.0f64.powf(3.999843853973347 / 20.0);
        let vb = vh.powf(0.4996667741545416);
        let a0 = 1.0 + k / q + k * k;
        (
            [
                (vh + vb * k / q + k * k) / a0,
                2.0 * (k * k - vh) / a0,
                (vh - vb * k / q + k * k) / a0,
            ],
            [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
        )
    }

    fn k_highpass(rate: u32) -> ([f64; 3], [f64; 2]) {
        let k = (std::f64::consts::PI * 38.13547087602444 / rate as f64).tan();
        let q = 0.5003270373238773;
        let a0 = 1.0 + k / q + k * k;
        (
            [1.0, -2.0, 1.0],
            [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
        )
    }

    #[test]
    fn sections_and_transitions_follow_structure() {
        // Две половины: клики 120, затем аккорд-пад. Секций ≥ 2, переходы есть.
        let rate = 48_000;
        let mut frames = click_train(120.0, 6.0, rate, false);
        frames.extend(chord_tone(&[261.63, 329.63, 392.00], 6.0, rate));
        let a = analyze_music(&frames, rate, &no_cancel()).unwrap();
        assert!(a.sections.len() >= 2, "sections: {:?}", a.sections);
        assert!(
            a.transitions
                .iter()
                .any(|t| matches!(t.kind, TransitionKind::SectionBoundary)),
            "transitions: {:?}",
            a.transitions
        );
        assert!(
            a.transitions
                .iter()
                .any(|t| matches!(t.kind, TransitionKind::MixIn)),
            "no MixIn"
        );
        // Однородный клик: одна секция, без нарезки на равные куски.
        let flat = click_train(120.0, 8.0, rate, false);
        let a = analyze_music(&flat, rate, &no_cancel()).unwrap();
        assert_eq!(a.sections.len(), 1);
    }

    #[test]
    fn analyzer_reuse_and_version() {
        let rate = 48_000;
        let mut core = MusicAnalyzer::new(rate).unwrap();
        let cancel = no_cancel();
        let a1 = core
            .analyze(&click_train(120.0, 6.0, rate, false), &cancel)
            .unwrap();
        core.reset();
        let a2 = core
            .analyze(&click_train(120.0, 6.0, rate, false), &cancel)
            .unwrap();
        assert_eq!(a1.bpm.unwrap().bpm, a2.bpm.unwrap().bpm);
        assert_eq!(a1.version, ANALYSIS_VERSION);
        assert_eq!(a1.beats.len(), a2.beats.len());
    }
}
