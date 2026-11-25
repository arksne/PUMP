//! Потоковый WebDAV-клиент с ограничением путей настроенным корнем сервера.
use crate::{Error, NetworkMode, Result};
use std::time::Duration;
use url::Url;
#[derive(Clone)]
pub struct WebDav {
    client: reqwest::Client,
    base: Url,
    auth: Option<(String, String)>,
}
impl WebDav {
    pub fn new(base: Url, auth: Option<(String, String)>) -> Result<Self> {
        crate::disk::validate_url(&base)?;
        if !base.path().ends_with('/') || base.query().is_some() {
            return Err(Error::Invalid(
                "WebDAV root должен заканчиваться / и не иметь query".into(),
            ));
        }
        Ok(Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()?,
            base,
            auth,
        })
    }
    pub async fn stream(
        &self,
        path: &str,
        range: Option<&str>,
        mode: NetworkMode,
    ) -> Result<reqwest::Response> {
        mode.require_online()?;
        if path.split('/').any(|p| p == "..")
            || path.contains(['\\', '%', '?', '#'])
            || path.starts_with('/')
        {
            return Err(Error::Invalid("неверный относительный путь NAS".into()));
        }
        let url = self
            .base
            .join(path)
            .map_err(|e| Error::Invalid(e.to_string()))?;
        if url.origin() != self.base.origin() || !url.path().starts_with(self.base.path()) {
            return Err(Error::Invalid("путь вне NAS".into()));
        }
        let mut req = self.client.get(url);
        if let Some((user, password)) = &self.auth {
            req = req.basic_auth(user, Some(password));
        }
        if let Some(range) = range {
            req = req.header("Range", range);
        }
        // Ответные заголовки должны прийти за 30 с; тело идёт потоком дальше.
        let sent = tokio::time::timeout(Duration::from_secs(30), req.send())
            .await
            .map_err(|_| Error::Limit("время ожидания NAS".into()))?;
        Ok(sent?.error_for_status()?)
    }
}
