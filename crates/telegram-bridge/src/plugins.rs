//! Администратор добавляет WASM-файлы в PUMP_PLUGIN_DIR до запуска сервера.
use crate::{ApiError, App};
use axum::{
    Json,
    extract::{Path, Query, State},
    response::IntoResponse,
};
use plugin_spine::{PluginRegistry, ResolveRequest, RuntimeLimits, SearchRequest, WasmRuntime};
use std::{path::Path as FsPath, sync::Arc};
pub(crate) fn load(directory: Option<std::path::PathBuf>) -> anyhow::Result<PluginRegistry> {
    let mut registry = PluginRegistry::new();
    let Some(directory) = directory else {
        return Ok(registry);
    };
    let runtime = WasmRuntime::new(RuntimeLimits::default())?;
    let mut paths = std::fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
    paths.sort_by_key(|entry| entry.file_name());
    for entry in paths {
        if entry.file_type()?.is_file()
            && entry.path().extension().and_then(|s| s.to_str()) == Some("wasm")
        {
            let id = FsPath::new(&entry.file_name())
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_owned();
            let provider = runtime.load_file(id, entry.path())?;
            registry.register(Arc::new(provider), 0)?;
        }
    }
    Ok(registry)
}
pub(crate) async fn list(State(app): State<App>) -> impl IntoResponse {
    Json(
        app.plugins
            .provider_ids()
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>(),
    )
}
#[derive(serde::Deserialize)]
pub(crate) struct Search {
    q: String,
}
pub(crate) async fn search(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(query): Query<Search>,
) -> Result<impl IntoResponse, ApiError> {
    let registry = app.plugins.clone();
    let response = tokio::task::spawn_blocking(move || {
        registry.search(&SearchRequest::new(query.q, 20), Some(&id))
    })
    .await
    .map_err(|_| ApiError::unavailable("Провайдер завершился с ошибкой."))?
    .map_err(|_| ApiError::unavailable("Поиск провайдера недоступен."))?;
    Ok(Json(response.response))
}
pub(crate) async fn resolve(
    State(app): State<App>,
    Path(id): Path<String>,
    Json(request): Json<ResolveRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let registry = app.plugins.clone();
    let response = tokio::task::spawn_blocking(move || registry.resolve(&id, &request))
        .await
        .map_err(|_| ApiError::unavailable("Провайдер завершился с ошибкой."))?
        .map_err(|_| ApiError::unavailable("Провайдер не нашёл аудиопоток."))?;
    Ok(Json(
        serde_json::json!({"primary":response.primary,"fallbacks":response.fallbacks}),
    ))
}
