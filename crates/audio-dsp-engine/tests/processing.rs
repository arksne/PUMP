//! Проверяет граничные значения сведения, громкости и преобразования темпа.
use audio_dsp_engine::{
    eq::{EqSettings, Equalizer},
    mix::{CrossfadeCurve, crossfade_gains},
};
#[test]
fn flat_eq_preserves_samples() {
    let mut eq = Equalizer::new(48000, EqSettings::flat(48000).unwrap()).unwrap();
    let mut pcm = vec![[0.2, -0.3]; 100];
    eq.process(&mut pcm).unwrap();
    assert!((pcm[99][0] - 0.2).abs() < 1e-5);
    assert!((pcm[99][1] + 0.3).abs() < 1e-5);
}
#[test]
fn crossfade_endpoints_and_power() {
    assert_eq!(
        crossfade_gains(0.0, CrossfadeCurve::EqualPower).unwrap(),
        [1.0, 0.0]
    );
    let mid = crossfade_gains(0.5, CrossfadeCurve::EqualPower).unwrap();
    assert!((mid[0] * mid[0] + mid[1] * mid[1] - 1.0).abs() < 1e-5);
    assert!(crossfade_gains(f64::NAN, CrossfadeCurve::Linear).is_err());
}
#[cfg(feature = "time-stretch")]
#[test]
fn independent_speed_preserves_target_duration() {
    let pcm = vec![[0.1, 0.1]; 4800];
    let out = audio_dsp_engine::stretch::transform(&pcm, 48000, 2.0, 0.0).unwrap();
    assert_eq!(out.len(), 2400);
    assert!(out.iter().flatten().all(|x| x.is_finite()));
}

#[test]
fn detects_click_tempo_and_edge_silence() {
    let rate = 48000;
    let mut pcm = vec![[0.0; 2]; rate * 10];
    for beat in 2..18 {
        for frame in pcm.iter_mut().skip(beat * rate / 2).take(rate / 50) {
            *frame = [0.5; 2];
        }
    }
    let analysis = audio_dsp_engine::analysis::analyze_track(&pcm, rate as u32).unwrap();
    assert!(analysis.start_frame >= rate / 2);
    assert!(analysis.end_frame < pcm.len());
    assert!((analysis.bpm.unwrap() - 120.0).abs() <= 3.0);
}
