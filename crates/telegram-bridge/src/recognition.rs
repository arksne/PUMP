//! Распознавание аудио через настроенный сервис AudD.
//! Токен остаётся на сервере, отправка выполняется по запросу пользователя.
use crate::{ApiError, App, stats};
use axum::{Json, body::Bytes, extract::State, http::HeaderMap, response::IntoResponse};
pub(crate) async fn identify(bytes: Vec<u8>) -> Result<serde_json::Value, ApiError> {
    if bytes.is_empty() || bytes.len() > 20 * 1024 * 1024 {
        return Err(ApiError::invalid("Допустимый размер записи — до 20 МБ."));
    }
    let token = crate::setting("AUDD_TOKEN").ok_or_else(|| {
        ApiError::invalid("Для распознавания произвольной музыки настрой AUDD_TOKEN.")
    })?;
    let form = reqwest::multipart::Form::new()
        .text("api_token", token)
        .part(
            "file",
            reqwest::multipart::Part::bytes(bytes).file_name("audio.bin"),
        );
    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| ApiError::unavailable("Не удалось создать клиент распознавания."))?
        .post("https://api.audd.io/")
        .multipart(form)
        .send()
        .await
        .map_err(|_| ApiError::unavailable("Сервис распознавания недоступен."))?
        .error_for_status()
        .map_err(|_| ApiError::unavailable("Сервис распознавания отклонил запрос."))?;
    let result: serde_json::Value = response
        .json()
        .await
        .map_err(|_| ApiError::unavailable("Некорректный ответ распознавания."))?;
    if result["status"] != "success" {
        return Err(ApiError::unavailable(
            "Сервис распознавания отклонил запись или токен.",
        ));
    }
    Ok(result["result"].clone())
}
pub(crate) async fn upload(
    State(app): State<App>,
    headers: HeaderMap,
    bytes: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    stats::owner(&app, &headers).await?;
    Ok(Json(identify(bytes.to_vec()).await?))
}
