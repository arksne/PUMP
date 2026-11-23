//! Регистрация музыкальных провайдеров и выбор резервных источников.
use std::{collections::BTreeMap, sync::Arc};

use crate::{
    AudioCandidate, MusicProviderPlugin, PluginError, ResolveRequest, Result, SearchRequest,
    SearchResponse, model::valid_id,
};

#[derive(Clone, Debug)]
pub struct ProviderFailure {
    pub provider_id: String,
    pub error: String,
}

#[derive(Clone, Debug)]
pub struct ProviderSearch {
    pub provider_id: String,
    pub response: SearchResponse,
    pub failures: Vec<ProviderFailure>,
}

#[derive(Clone, Debug)]
pub struct ResolvedAudio {
    pub provider_id: String,
    pub primary: AudioCandidate,
    pub fallbacks: Vec<AudioCandidate>,
}

struct Entry {
    priority: i32,
    plugin: Arc<dyn MusicProviderPlugin>,
}

/// Больший priority побеждает; при равенстве используется порядок ID по UTF-8.
#[derive(Default)]
pub struct PluginRegistry {
    entries: BTreeMap<String, Entry>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, plugin: Arc<dyn MusicProviderPlugin>, priority: i32) -> Result<()> {
        let id = plugin.id().to_owned();
        if !valid_id(&id) {
            return Err(PluginError::Configuration(
                "неверный идентификатор провайдера".into(),
            ));
        }
        if self.entries.contains_key(&id) {
            return Err(PluginError::DuplicateProvider(id));
        }
        self.entries.insert(id, Entry { priority, plugin });
        Ok(())
    }

    pub fn remove(&mut self, id: &str) -> Option<Arc<dyn MusicProviderPlugin>> {
        self.entries.remove(id).map(|entry| entry.plugin)
    }

    pub fn provider_ids(&self) -> Vec<&str> {
        let mut entries: Vec<_> = self.entries.iter().collect();
        entries.sort_by(|(a_id, a), (b_id, b)| b.priority.cmp(&a.priority).then(a_id.cmp(b_id)));
        entries.into_iter().map(|(id, _)| id.as_str()).collect()
    }

    /// Явный ID выбирает только этого провайдера; None выбирает первый по приоритету.
    pub fn select(&self, id: Option<&str>) -> Result<Arc<dyn MusicProviderPlugin>> {
        let id = match id {
            Some(id) => id,
            None => self
                .provider_ids()
                .first()
                .copied()
                .ok_or(PluginError::NoProviders)?,
        };
        self.entries
            .get(id)
            .map(|entry| Arc::clone(&entry.plugin))
            .ok_or_else(|| PluginError::ProviderNotFound(id.into()))
    }

    /// Без ID перебирает провайдеров до первого непустого ответа, сохраняя ошибки.
    pub fn search(
        &self,
        request: &SearchRequest,
        provider_id: Option<&str>,
    ) -> Result<ProviderSearch> {
        request.validate()?;
        let ids = match provider_id {
            Some(id) => {
                self.select(Some(id))?;
                vec![id]
            }
            None => self.provider_ids(),
        };
        if ids.is_empty() {
            return Err(PluginError::NoProviders);
        }
        let mut failures = Vec::new();
        let mut empty = None;
        for id in ids {
            let result = self.entries[id]
                .plugin
                .search(request)
                .and_then(|response| {
                    response.validate(request)?;
                    Ok(response)
                });
            match result {
                Ok(response) => {
                    let result = ProviderSearch {
                        provider_id: id.into(),
                        response,
                        failures: Vec::new(),
                    };
                    if !result.response.tracks.is_empty() || provider_id.is_some() {
                        return Ok(ProviderSearch { failures, ..result });
                    }
                    if empty.is_none() {
                        empty = Some(result);
                    }
                }
                Err(error) if provider_id.is_some() => return Err(error),
                Err(error) => failures.push(ProviderFailure {
                    provider_id: id.into(),
                    error: error.to_string(),
                }),
            }
        }
        if let Some(result) = empty {
            return Ok(ProviderSearch { failures, ..result });
        }
        Err(PluginError::ProvidersFailed(failures))
    }

    /// ID трека принадлежит провайдеру из поиска; между провайдерами он не переносится.
    pub fn resolve(&self, provider_id: &str, request: &ResolveRequest) -> Result<ResolvedAudio> {
        request.validate()?;
        let response = self.select(Some(provider_id))?.resolve(request)?;
        let mut candidates = response.ranked(request.quality)?.into_iter();
        let primary = candidates.next().ok_or(PluginError::NoCandidates)?;
        Ok(ResolvedAudio {
            provider_id: provider_id.into(),
            primary,
            fallbacks: candidates.collect(),
        })
    }
}
