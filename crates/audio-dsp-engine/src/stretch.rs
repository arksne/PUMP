//! Независимое изменение темпа WSOLA и высоты тона ресемплингом.
use crate::{Result, StereoFrame, range, sample_rate, validate_frames};
pub fn transform(
    input: &[StereoFrame],
    rate: u32,
    speed: f64,
    semitones: f64,
) -> Result<Vec<StereoFrame>> {
    sample_rate(rate)?;
    validate_frames(input)?;
    range(speed, 0.5, 2.0, "speed")?;
    range(semitones, -12.0, 12.0, "pitch")?;
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let pitch = 2f64.powf(semitones / 12.0);
    let count = (input.len() as f64 / pitch).round() as usize;
    use rubato::{FftFixedInOut, Resampler};
    let mut resampler = FftFixedInOut::<f32>::new(
        rate as usize,
        (rate as f64 / pitch).round() as usize,
        1024,
        2,
    )
    .map_err(|e| crate::Error::Backend(e.to_string()))?;
    let mut shifted = Vec::with_capacity(count + 4096);
    let chunk = resampler.input_frames_next();
    let delay = resampler.output_delay();
    for block in input.chunks(chunk) {
        let planar = [
            block.iter().map(|f| f[0]).collect::<Vec<_>>(),
            block.iter().map(|f| f[1]).collect::<Vec<_>>(),
        ];
        let output = resampler
            .process_partial(Some(&planar), None)
            .map_err(|e| crate::Error::Backend(e.to_string()))?;
        shifted.extend(output[0].iter().zip(&output[1]).map(|(l, r)| [*l, *r]));
    }
    let output = resampler
        .process_partial::<Vec<f32>>(None, None)
        .map_err(|e| crate::Error::Backend(e.to_string()))?;
    shifted.extend(output[0].iter().zip(&output[1]).map(|(l, r)| [*l, *r]));
    shifted.drain(..delay.min(shifted.len()));
    shifted.resize(count, [0.0; 2]);
    let ratio = pitch / speed;
    let target = (input.len() as f64 / speed).round() as usize;
    if (ratio - 1.0).abs() < 1e-6 {
        return Ok(shifted);
    }
    let window = (rate as usize / 50).max(32);
    let hop = window / 2;
    let search = window / 4;
    let mut output = vec![[0.0; 2]; target + window];
    let mut weights = vec![0f32; target + window];
    let mut previous = 0usize;
    let mut dest = 0usize;
    while dest < target {
        let expected = (dest as f64 / ratio) as usize;
        let mut start = expected.min(shifted.len().saturating_sub(window));
        if dest > 0 {
            let mut best = f64::NEG_INFINITY;
            for candidate in start.saturating_sub(search)
                ..=(start + search).min(shifted.len().saturating_sub(window))
            {
                let mut corr = 0f64;
                let mut energy = 1e-12;
                for j in (0..hop).step_by(4) {
                    let a = shifted[(previous + hop + j).min(shifted.len() - 1)][0] as f64;
                    let b = shifted[(candidate + j).min(shifted.len() - 1)][0] as f64;
                    corr += a * b;
                    energy += b * b;
                }
                let score = corr / energy.sqrt();
                if score > best {
                    best = score;
                    start = candidate;
                }
            }
        }
        for j in 0..window {
            if start + j >= shifted.len() {
                break;
            }
            let gain = (std::f32::consts::PI * j as f32 / window as f32)
                .sin()
                .powi(2);
            for c in 0..2 {
                output[dest + j][c] += shifted[start + j][c] * gain;
            }
            weights[dest + j] += gain;
        }
        previous = start;
        dest += hop;
    }
    output.truncate(target);
    for (frame, weight) in output.iter_mut().zip(weights) {
        if weight > 1e-6 {
            frame[0] /= weight;
            frame[1] /= weight;
        }
    }
    Ok(output)
}
