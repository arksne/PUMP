//! Спектральные отпечатки для поиска среди предварительно индексированных записей.
use crate::{Error, Result};
use rustfft::{FftPlanner, num_complex::Complex};
use serde::{Deserialize, Serialize};
#[derive(Clone, Serialize, Deserialize)]
pub struct Fingerprint {
    pub frames: Vec<u32>,
}
#[derive(Clone, Serialize)]
pub struct Match {
    pub id: String,
    pub confidence: f32,
    pub offset_ms: u64,
}
pub fn fingerprint(samples: &[f32], sample_rate: u32) -> Result<Fingerprint> {
    if sample_rate < 8000 || samples.iter().any(|s| !s.is_finite()) {
        return Err(Error::Invalid("PCM или sample rate".into()));
    }
    let rate = 11025usize;
    let count = samples.len() * rate / sample_rate as usize;
    let mono: Vec<_> = (0..count)
        .map(|n| samples[(n * sample_rate as usize / rate).min(samples.len() - 1)])
        .collect();
    let size = 2048;
    let hop = 512;
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(size);
    let mut frames = Vec::new();
    let mut previous = [0f32; 16];
    for start in (0..mono.len().saturating_sub(size)).step_by(hop) {
        let mut spectrum: Vec<_> = (0..size)
            .map(|n| {
                Complex::new(
                    mono[start + n]
                        * (0.5 - 0.5 * (2.0 * std::f32::consts::PI * n as f32 / size as f32).cos()),
                    0.0,
                )
            })
            .collect();
        fft.process(&mut spectrum);
        let mut bands = [0f32; 16];
        for (i, band) in bands.iter_mut().enumerate() {
            let a = (6.0 * 1.4f32.powi(i as i32)) as usize;
            let b = (6.0 * 1.4f32.powi(i as i32 + 1)) as usize;
            *band = spectrum[a.min(size / 2)..b.min(size / 2)]
                .iter()
                .map(|v| v.norm_sqr())
                .sum::<f32>()
                .max(1e-12)
                .ln();
        }
        let mut bits = 0u32;
        for i in 0..15 {
            if bands[i] > bands[i + 1] {
                bits |= 1 << i;
            }
            if bands[i] - previous[i] > bands[i + 1] - previous[i + 1] {
                bits |= 1 << (i + 16);
            }
        }
        previous = bands;
        frames.push(bits);
    }
    Ok(Fingerprint { frames })
}
#[derive(Default)]
pub struct FingerprintIndex {
    tracks: Vec<(String, Fingerprint)>,
}
impl FingerprintIndex {
    pub fn insert(&mut self, id: String, fp: Fingerprint) {
        self.tracks.retain(|(old, _)| old != &id);
        self.tracks.push((id, fp));
    }
    pub fn identify(&self, query: &Fingerprint) -> Option<Match> {
        if query.frames.len() < 20 {
            return None;
        }
        let mut matches = Vec::new();
        for (id, track) in &self.tracks {
            if track.frames.len() < query.frames.len() {
                continue;
            }
            let mut best = (0f32, 0);
            for offset in 0..=track.frames.len() - query.frames.len() {
                let difference: u32 = query
                    .frames
                    .iter()
                    .zip(&track.frames[offset..])
                    .map(|(a, b)| (a ^ b).count_ones())
                    .sum();
                let score = 1.0 - difference as f32 / (30 * query.frames.len()) as f32;
                if score > best.0 {
                    best = (score, offset);
                }
            }
            matches.push(Match {
                id: id.clone(),
                confidence: best.0,
                offset_ms: best.1 as u64 * 512000 / 11025,
            });
        }
        matches.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
        let first = matches.first()?;
        if first.confidence < 0.82
            || matches
                .get(1)
                .is_some_and(|other| first.confidence - other.confidence < 0.04)
        {
            return None;
        }
        Some(first.clone())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finds_excerpt_and_rejects_short_query() {
        let fp = Fingerprint {
            frames: (0..150).map(|i: u32| i.wrapping_mul(19873277)).collect(),
        };
        let query = Fingerprint {
            frames: fp.frames[35..90].to_vec(),
        };
        let mut index = FingerprintIndex::default();
        index.insert("track".into(), fp);
        assert_eq!(index.identify(&query).unwrap().id, "track");
        assert!(index.identify(&Fingerprint { frames: vec![0] }).is_none());
    }
}
