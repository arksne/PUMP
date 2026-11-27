//! Bounded local-track music analysis for browser playback and transition planning.
use crate::{ApiError, App, stats};
use audio_dsp_engine::{
    decode::decode,
    music_analysis::{Mode, TransitionKind, analyze_music},
};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};
use tokio::sync::{Mutex, Semaphore};

const MAX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_FRAMES: usize = 14_400_000;
const CACHE_LIMIT: usize = 64;
static SLOTS: Semaphore = Semaphore::const_new(1);

#[derive(Clone, Serialize)]
pub(crate) struct Analysis {
    version: u32,
    duration_seconds: f64,
    bpm: Option<f64>,
    bpm_confidence: Option<f64>,
    beat_confidence: f64,
    beats: Vec<f64>,
    key: Option<String>,
    key_confidence: Option<f64>,
    integrated_lufs: Option<f64>,
    true_peak: f64,
    energy: f64,
    mix_in_seconds: Option<f64>,
    mix_out_seconds: Option<f64>,
}

struct Entry {
    size: u64,
    modified: Option<SystemTime>,
    inserted: Instant,
    analysis: Analysis,
}

#[derive(Clone, Default)]
pub(crate) struct AnalysisCache(Arc<Mutex<HashMap<String, Entry>>>);

pub(crate) async fn track(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Analysis>, ApiError> {
    let _ = stats::owner(&app, &headers).await?;
    let path = app
        .catalog
        .media(&id)
        .ok_or_else(|| ApiError::missing("Анализ доступен для локальной медиатеки."))?;
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|_| ApiError::missing("Файл не найден."))?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return Err(ApiError::invalid("Файл слишком большой для анализа."));
    }
    let size = metadata.len();
    let modified = metadata.modified().ok();
    {
        let cache = app.analysis.0.lock().await;
        if let Some(entry) = cache.get(&id)
            && entry.size == size
            && entry.modified == modified
            && entry.inserted.elapsed() < Duration::from_secs(3600)
        {
            return Ok(Json(entry.analysis.clone()));
        }
    }
    let permit = SLOTS.try_acquire().map_err(|_| {
        ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "Анализатор занят. Повтори позже.",
        )
    })?;
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = cancel.clone();
    let job = tokio::task::spawn_blocking(move || {
        // Keep the slot until the blocking decoder actually exits, including
        // when the HTTP request times out or is abandoned.
        let _permit = permit;
        let decoded = decode(path, MAX_FRAMES)?;
        let result = analyze_music(&decoded.frames, decoded.sample_rate, &worker_cancel)?;
        let transition = |kind| {
            result
                .transitions
                .iter()
                .filter(|item| item.kind == kind && item.confidence >= 0.35)
                .max_by(|a, b| a.confidence.total_cmp(&b.confidence))
                .map(|item| item.at_seconds)
        };
        Ok::<_, audio_dsp_engine::Error>(Analysis {
            version: result.version,
            duration_seconds: result.duration_seconds,
            bpm: result.bpm.map(|bpm| bpm.bpm),
            bpm_confidence: result.bpm.map(|bpm| bpm.confidence),
            beat_confidence: result.beat_confidence,
            beats: result.beats.iter().copied().take(1_500).collect(),
            key: result.key.map(|key| {
                format!(
                    "{} {}",
                    key.tonic.name(),
                    match key.mode {
                        Mode::Major => "major",
                        Mode::Minor => "minor",
                    }
                )
            }),
            key_confidence: result.key.map(|key| key.confidence),
            integrated_lufs: result.integrated_lufs,
            true_peak: result.true_peak,
            energy: ((result.rms[0] + result.rms[1]) * 0.5).clamp(0.0, 1.0),
            mix_in_seconds: transition(TransitionKind::MixIn),
            mix_out_seconds: transition(TransitionKind::MixOut),
        })
    });
    let analysis = match tokio::time::timeout(Duration::from_secs(45), job).await {
        Ok(Ok(Ok(analysis))) => analysis,
        Ok(_) => return Err(ApiError::invalid("Не удалось проанализировать этот файл.")),
        Err(_) => {
            cancel.store(true, Ordering::Relaxed);
            return Err(ApiError::unavailable("Превышено время анализа."));
        }
    };
    let mut cache = app.analysis.0.lock().await;
    cache.retain(|_, entry| entry.inserted.elapsed() < Duration::from_secs(3600));
    if cache.len() >= CACHE_LIMIT
        && let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, entry)| entry.inserted)
            .map(|(id, _)| id.clone())
    {
        cache.remove(&oldest);
    }
    cache.insert(
        id,
        Entry {
            size,
            modified,
            inserted: Instant::now(),
            analysis: analysis.clone(),
        },
    );
    Ok(Json(analysis))
}
