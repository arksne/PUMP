//! Сопоставление импортируемых записей с каталогом приложения.
use crate::{ImportItem, Result, TrackMetadata};
use async_trait::async_trait;

#[derive(Clone, Debug)]
pub struct Candidate<Id> {
    pub id: Id,
    pub metadata: TrackMetadata,
}

/// Реализация вызывающей стороны ищет кандидатов в собственном каталоге.
#[async_trait]
pub trait TrackResolver<Id>: Sync {
    async fn candidates(&self, track: &TrackMetadata) -> Result<Vec<Candidate<Id>>>;
}

#[derive(Clone, Debug)]
pub enum Resolution<Id> {
    Matched { id: Id, confidence: f32 },
    Ambiguous { candidates: Vec<(Id, f32)> },
    Unmatched,
    Unavailable(String),
    ResolverError(String),
}

#[derive(Clone, Debug)]
pub struct ImportedTrack<Id> {
    pub source: ImportItem,
    pub resolution: Resolution<Id>,
}

fn normalized(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn score(a: &TrackMetadata, b: &TrackMetadata) -> f32 {
    if let (Some(x), Some(y)) = (&a.isrc, &b.isrc)
        && !x.is_empty()
        && x.eq_ignore_ascii_case(y)
    {
        return 1.0;
    }
    // Разные ISRC могут обозначать разные издания той же записи.
    let title = normalized(&a.title);
    if title.is_empty() || title != normalized(&b.title) {
        return 0.0;
    }
    if !a.artists.iter().any(|x| {
        !normalized(x).is_empty() && b.artists.iter().any(|y| normalized(x) == normalized(y))
    }) {
        return 0.0;
    }
    match (a.duration_ms, b.duration_ms) {
        (Some(x), Some(y)) if x.abs_diff(y) > 5000 => 0.6,
        (Some(_), Some(_)) => 0.95,
        _ => 0.85,
    }
}

/// Последовательный импорт сохраняет порядок и дубликаты; ошибка одной записи не обрывает импорт.
pub async fn import_tracks<Id: Clone + Eq + Send, R: TrackResolver<Id> + ?Sized>(
    items: impl IntoIterator<Item = ImportItem>,
    resolver: &R,
) -> Vec<ImportedTrack<Id>> {
    let mut result = Vec::new();
    for source in items {
        let resolution = if let Some(metadata) = &source.metadata {
            match resolver.candidates(metadata).await {
                Err(e) => Resolution::ResolverError(e.to_string()),
                Ok(candidates) => {
                    let mut ranked: Vec<(Id, f32)> = Vec::new();
                    for c in candidates {
                        let confidence = score(metadata, &c.metadata);
                        if confidence < 0.8 {
                            continue;
                        }
                        if let Some(old) = ranked.iter_mut().find(|(id, _)| id == &c.id) {
                            old.1 = old.1.max(confidence);
                        } else {
                            ranked.push((c.id, confidence));
                        }
                    }
                    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
                    if ranked.is_empty() {
                        Resolution::Unmatched
                    } else if ranked.len() > 1 && ranked[0].1 - ranked[1].1 < 0.08 {
                        Resolution::Ambiguous { candidates: ranked }
                    } else {
                        Resolution::Matched {
                            id: ranked[0].0.clone(),
                            confidence: ranked[0].1,
                        }
                    }
                }
            }
        } else {
            Resolution::Unavailable(
                source
                    .unavailable_reason
                    .clone()
                    .unwrap_or_else(|| "no metadata".into()),
            )
        };
        result.push(ImportedTrack { source, resolution });
    }
    result
}
