//! Краткоживущие адреса WASM-провайдеров доступны только через проверенный прокси.
use crate::{ApiError, App, plugins};
use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    response::Response,
};
use futures_util::{StreamExt, stream};
use plugin_spine::AudioCandidate;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::RwLock;
use url::{Host, Url};

const CACHE_TTL: Duration = Duration::from_secs(15 * 60);
const CACHE_CAPACITY: usize = 256;

#[derive(Clone)]
struct Resolved {
    created: Instant,
    track_id: String,
    provider_id: String,
    provider_track_id: String,
    candidates: Vec<AudioCandidate>,
}

struct Configuration {
    hosts: AllowedHosts,
    client: reqwest::Client,
}

#[derive(Clone)]
pub(crate) struct PluginMedia {
    configuration: Arc<Result<Configuration, &'static str>>,
    resolved: Arc<RwLock<HashMap<String, Resolved>>>,
}

impl Default for PluginMedia {
    fn default() -> Self {
        // Ошибка конфигурации видна при resolve; остальные источники продолжают работать.
        let configuration = (|| {
            let hosts = AllowedHosts::parse(
                &std::env::var("PUMP_PLUGIN_ALLOWED_HOSTS").unwrap_or_default(),
            )?;
            let client = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(4))
                .timeout(Duration::from_secs(120))
                .redirect(reqwest::redirect::Policy::none())
                .no_gzip()
                .no_brotli()
                .no_deflate()
                .no_zstd()
                .user_agent("PUMP/0.2 addon media")
                .build()
                .map_err(|_| "Не удалось создать HTTP-клиент провайдеров.")?;
            Ok(Configuration { hosts, client })
        })();
        Self {
            configuration: Arc::new(configuration),
            resolved: Arc::default(),
        }
    }
}

impl PluginMedia {
    fn configuration(&self) -> Result<&Configuration, ApiError> {
        self.configuration
            .as_ref()
            .as_ref()
            .map_err(|message| ApiError::unavailable(message))
    }

    pub(crate) fn ensure_configured(&self) -> Result<(), ApiError> {
        self.configuration().map(|_| ())
    }

    pub(crate) fn allowed_candidates(
        &self,
        candidates: Vec<AudioCandidate>,
    ) -> Result<Vec<AudioCandidate>, ApiError> {
        let configuration = self.configuration()?;
        let candidates: Vec<_> = candidates
            .into_iter()
            .filter(|candidate| configuration.hosts.allows(candidate.url.as_str()))
            .collect();
        if candidates.is_empty() {
            return Err(ApiError::unavailable(
                "У провайдера нет аудио на хостах из PUMP_PLUGIN_ALLOWED_HOSTS.",
            ));
        }
        Ok(candidates)
    }

    pub(crate) async fn remember(
        &self,
        id: &str,
        provider_id: &str,
        provider_track_id: &str,
        candidates: Vec<AudioCandidate>,
    ) -> String {
        let lease = uuid::Uuid::new_v4().simple().to_string();
        let mut cache = self.resolved.write().await;
        cache.retain(|_, entry| entry.created.elapsed() < CACHE_TTL);
        if cache.len() >= CACHE_CAPACITY {
            let oldest = cache
                .iter()
                .min_by_key(|(_, entry)| entry.created)
                .map(|(id, _)| id.clone());
            if let Some(oldest) = oldest {
                cache.remove(&oldest);
            }
        }
        cache.insert(
            lease.clone(),
            Resolved {
                track_id: id.to_owned(),
                created: Instant::now(),
                provider_id: provider_id.to_owned(),
                provider_track_id: provider_track_id.to_owned(),
                candidates,
            },
        );
        lease
    }

    async fn lookup(&self, id: &str) -> Option<Resolved> {
        let mut cache = self.resolved.write().await;
        cache.retain(|_, entry| entry.created.elapsed() < CACHE_TTL);
        cache.get(id).cloned()
    }
}

pub(crate) fn stream_url(id: &str, lease: &str, candidate: usize) -> String {
    format!("/addon/stream/{id}/{lease}/{candidate}")
}

#[derive(Debug)]
struct AllowedHosts(Vec<HostRule>);

#[derive(Debug)]
enum HostRule {
    Exact(String),
    Subdomains(String),
}

impl AllowedHosts {
    fn parse(value: &str) -> Result<Self, &'static str> {
        let mut rules = Vec::new();
        for value in value
            .split(|ch: char| ch == ',' || ch.is_ascii_whitespace())
            .filter(|value| !value.is_empty())
        {
            let (wildcard, value) = match value.strip_prefix("*.") {
                Some(value) => (true, value),
                None => (false, value),
            };
            let host = normalized_host(value).ok_or(
                "PUMP_PLUGIN_ALLOWED_HOSTS: ожидаются хосты или шаблоны *.domain без URL и портов.",
            )?;
            if wildcard {
                if !matches!(Host::parse(&host), Ok(Host::Domain(_))) || !host.contains('.') {
                    return Err("PUMP_PLUGIN_ALLOWED_HOSTS: неверный домен шаблона.");
                }
                rules.push(HostRule::Subdomains(host));
            } else {
                rules.push(HostRule::Exact(host));
            }
        }
        if rules.is_empty() {
            return Err("Настройте PUMP_PLUGIN_ALLOWED_HOSTS для воспроизведения провайдеров.");
        }
        Ok(Self(rules))
    }

    fn allows(&self, value: &str) -> bool {
        if !(value.starts_with("https://") || value.starts_with("http://"))
            || value
                .chars()
                .any(|ch| ch.is_whitespace() || ch.is_control())
            || value.contains('\\')
        {
            return false;
        }
        let Ok(url) = Url::parse(value) else {
            return false;
        };
        if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
            return false;
        }
        let Some(host) = url.host_str().and_then(normalized_host) else {
            return false;
        };
        self.0.iter().any(|rule| match rule {
            HostRule::Exact(allowed) => &host == allowed,
            HostRule::Subdomains(suffix) => host
                .strip_suffix(suffix)
                .is_some_and(|prefix| prefix.len() > 1 && prefix.ends_with('.')),
        })
    }
}

fn normalized_host(value: &str) -> Option<String> {
    // Host::parse канонизирует IP и IDN; запрещаем синтаксис URL и пустые DNS-метки.
    if value.is_empty()
        || value.chars().any(|ch| {
            ch.is_whitespace()
                || ch.is_control()
                || matches!(ch, '/' | '\\' | '@' | '?' | '#' | '%' | '*')
        })
    {
        return None;
    }
    let host = Host::parse(value).ok()?;
    if let Host::Domain(domain) = &host
        && (domain.len() > 253
            || domain.split('.').any(|label| {
                label.is_empty()
                    || label.len() > 63
                    || label.starts_with('-')
                    || label.ends_with('-')
                    || !label
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            }))
    {
        return None;
    }
    Some(host.to_string())
}

fn audio_content_type(value: &str) -> bool {
    let mime = value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    matches!(
        mime.as_str(),
        "application/octet-stream" | "application/ogg" | "application/x-ogg"
    ) || mime.strip_prefix("audio/").is_some_and(|subtype| {
        !subtype.is_empty()
            && subtype
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&byte))
    })
}

pub(crate) async fn stream(
    State(app): State<App>,
    Path((id, lease, index)): Path<(String, String, usize)>,
    request: Request,
) -> Result<Response, ApiError> {
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return Err(ApiError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "Нужен GET или HEAD.",
        ));
    }
    let configuration = app.plugin_media.configuration()?;
    let resolved = app
        .plugin_media
        .lookup(&lease)
        .await
        .filter(|entry| entry.track_id == id && index < entry.candidates.len())
        .ok_or_else(|| ApiError::missing("Адрес потока истёк. Повторите разрешение трека."))?;
    if plugins::track_id(&resolved.provider_id, &resolved.provider_track_id) != id
        || app.plugins.select(Some(&resolved.provider_id)).is_err()
        || !app
            .catalog
            .track(&id)
            .await
            .is_some_and(|track| track.source == plugins::SOURCE)
    {
        return Err(ApiError::missing("Трек провайдера не найден в каталоге."));
    }

    let mut unsatisfied_range = None;
    // Один URL всегда соответствует одному формату, включая последующие Range-запросы.
    // Клиент выбирает следующий URL при ошибке и заново загружает заголовок формата.
    for candidate in &resolved.candidates[index..=index] {
        if !configuration.hosts.allows(candidate.url.as_str()) {
            continue;
        }
        let mut upstream = configuration
            .client
            .request(request.method().clone(), candidate.url.as_str())
            .header(header::ACCEPT_ENCODING, "identity");
        // Cookies, Authorization, Host и остальные заголовки браузера не пересылаются.
        for name in [header::RANGE, header::IF_RANGE] {
            if let Some(value) = request.headers().get(&name) {
                upstream = upstream.header(name, value);
            }
        }
        let Ok(upstream) = upstream.send().await else {
            continue;
        };
        if upstream.status() == StatusCode::RANGE_NOT_SATISFIABLE {
            unsatisfied_range = upstream.headers().get(header::CONTENT_RANGE).cloned();
            continue;
        }
        if upstream.status() != StatusCode::OK && upstream.status() != StatusCode::PARTIAL_CONTENT {
            continue;
        }
        let content_type = upstream
            .headers()
            .get(header::CONTENT_TYPE)
            .cloned()
            .or_else(|| HeaderValue::from_str(&candidate.mime_type).ok());
        let Some(content_type) =
            content_type.filter(|value| value.to_str().is_ok_and(audio_content_type))
        else {
            continue;
        };
        let mut response = Response::builder()
            .status(upstream.status())
            .header(header::CONTENT_TYPE, content_type)
            .header(header::CACHE_CONTROL, "private, no-store")
            .header("x-content-type-options", "nosniff");
        for name in [
            header::CONTENT_LENGTH,
            header::CONTENT_RANGE,
            header::ACCEPT_RANGES,
            header::CONTENT_ENCODING,
            header::ETAG,
            header::LAST_MODIFIED,
        ] {
            if let Some(value) = upstream.headers().get(&name) {
                response = response.header(name, value);
            }
        }
        if request.method() == Method::HEAD {
            return response
                .body(Body::empty())
                .map_err(|_| ApiError::unavailable("Некорректный ответ источника."));
        }
        let mut body = upstream.bytes_stream().boxed();
        // Проверяем первый байт до отправки заголовков клиенту, пока ещё возможен fallback.
        let first = loop {
            match body.next().await {
                Some(Ok(bytes)) if bytes.is_empty() => continue,
                first => break first,
            }
        };
        let Some(Ok(first)) = first else {
            continue;
        };
        let body = stream::once(async move { Ok::<_, reqwest::Error>(first) }).chain(body);
        return response
            .body(Body::from_stream(body))
            .map_err(|_| ApiError::unavailable("Некорректный ответ источника."));
    }
    if let Some(content_range) = unsatisfied_range {
        return Response::builder()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, content_range)
            .body(Body::empty())
            .map_err(|_| ApiError::unavailable("Некорректный ответ источника."));
    }
    Err(ApiError::unavailable(
        "Все разрешённые аудиокандидаты недоступны.",
    ))
}

#[cfg(test)]
mod tests {
    use super::AllowedHosts;

    #[test]
    fn exact_hosts_do_not_authorize_suffixes_or_userinfo() {
        let hosts = AllowedHosts::parse("CDN.example.com, media.example.org").unwrap();
        for url in [
            "https://cdn.example.com/a?token=x",
            "http://CDN.EXAMPLE.COM:8080/a",
        ] {
            assert!(hosts.allows(url), "{url}");
        }
        for url in [
            "https://cdn.example.com.evil.org/a",
            "https://evilcdn.example.com/a",
            "https://child.cdn.example.com/a",
            "https://cdn.example.com@evil.org/a",
            "https://user@cdn.example.com/a",
            "https://cdn.example.com./a",
            "https://cdn.example.com/a#fragment",
            "https://cdn.example.com\\@evil.org/a",
            "https://cdn.example.com/a\n",
            "file://cdn.example.com/a",
            "/relative",
        ] {
            assert!(!hosts.allows(url), "{url}");
        }
    }

    #[test]
    fn wildcard_requires_a_real_subdomain_boundary() {
        let hosts = AllowedHosts::parse("*.example.com").unwrap();
        assert!(hosts.allows("https://cdn.example.com/audio"));
        assert!(hosts.allows("https://a.cdn.example.com/audio"));
        for url in [
            "https://example.com/audio",
            "https://notexample.com/audio",
            "https://example.com.evil.org/audio",
            "https://cdn..example.com/audio",
        ] {
            assert!(!hosts.allows(url), "{url}");
        }
    }

    #[test]
    fn invalid_or_empty_allowlists_fail_closed() {
        for value in [
            "",
            " , \n",
            "*",
            "*.com",
            "*.127.0.0.1",
            "https://example.com",
            "example.com:443",
            "example.com/path",
            "user@example.com",
            "foo.*.example.com",
            "example.com?x=y",
            "example.com#fragment",
            "example.com.",
            "foo..example.com",
            "example.com,https://evil.org",
            "-bad.example.com",
            "exa%mple.com",
        ] {
            assert!(AllowedHosts::parse(value).is_err(), "{value}");
        }
    }

    #[test]
    fn literal_ips_need_explicit_administrator_permission() {
        let hosts = AllowedHosts::parse("127.0.0.1, [::1]").unwrap();
        assert!(hosts.allows("http://127.0.0.1/audio"));
        assert!(hosts.allows("http://[::1]/audio"));
        assert!(!hosts.allows("http://127.0.0.2/audio"));
        let domain = AllowedHosts::parse("*.example.com").unwrap();
        assert!(!domain.allows("http://127.0.0.1/audio"));
        assert!(!domain.allows("http://[::1]/audio"));
    }
}
