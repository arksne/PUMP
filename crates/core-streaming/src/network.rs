//! Общая политика исходящей сети: необязательный прокси и выбор DNS.
//!
//! DNS не меняет страну сервера: он лишь выбирает адрес для уже разрешённого
//! имени. Региональные ограничения обходятся только прокси (`PUMP_HTTP_PROXY`),
//! через который идёт весь трафик настроенного клиента.
//! Секрет прокси никогда не попадает в сообщения об ошибках.
use anyhow::{Context, Result, bail};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

/// Построитель клиента с общей политикой PUMP: таймауты, лимиты пула,
/// запрет редиректов, необязательный прокси и выбор DNS.
pub fn client_builder() -> Result<reqwest::ClientBuilder> {
    let mut builder = base_builder()?;
    if let Ok(mode) = std::env::var("PUMP_DNS_MODE")
        && !mode.trim().is_empty()
        && mode != "system"
    {
        if mode == "cloudflare" {
            builder = builder.dns_resolver(Arc::new(CloudflareDns::new()?));
        } else {
            bail!("PUMP_DNS_MODE должен быть system или cloudflare");
        }
    }
    Ok(builder)
}

fn base_builder() -> Result<reqwest::ClientBuilder> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(4))
        .timeout(Duration::from_secs(9))
        .pool_max_idle_per_host(4)
        .pool_idle_timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("PUMP/0.1 music player");
    if let Ok(value) = std::env::var("PUMP_HTTP_PROXY")
        && !value.trim().is_empty()
    {
        validate_proxy(&value)?;
        let proxy = reqwest::Proxy::all(&value).context("Некорректный PUMP_HTTP_PROXY")?;
        builder = builder.proxy(proxy);
    }
    Ok(builder)
}

fn validate_proxy(value: &str) -> Result<()> {
    let valid = url::Url::parse(value).ok().is_some_and(|url| {
        matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h")
            && url.has_host()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && matches!(url.path(), "" | "/")
    });
    if !valid {
        bail!("PUMP_HTTP_PROXY должен быть HTTP(S) или SOCKS5 URL без пути и параметров");
    }
    Ok(())
}

#[derive(Clone)]
struct Entry {
    addresses: Vec<IpAddr>,
    expires: Instant,
}

/// DNS через Cloudflare DoH с ограниченным кэшем. Имена самого прокси
/// и localhost всегда резолвятся системно: сам прокси может иметь
/// приватное имя, недоступное публичному DNS.
#[derive(Clone)]
struct CloudflareDns {
    client: reqwest::Client,
    cache: Arc<Mutex<HashMap<String, Entry>>>,
    slots: Arc<Semaphore>,
    proxy_hosts: Arc<HashSet<String>>,
}

impl CloudflareDns {
    fn new() -> Result<Self> {
        // Bootstrap фиксированными IP: резолвер ещё не создан,
        // поэтому DNS для cloudflare-dns.com здесь не используется.
        let client = base_builder()?
            .resolve_to_addrs(
                "cloudflare-dns.com",
                &[
                    SocketAddr::from(([1, 1, 1, 1], 443)),
                    SocketAddr::from(([1, 0, 0, 1], 443)),
                ],
            )
            .timeout(Duration::from_secs(9))
            .build()?;
        let proxy_hosts = [
            "PUMP_HTTP_PROXY",
            "HTTPS_PROXY",
            "HTTP_PROXY",
            "ALL_PROXY",
            "https_proxy",
            "http_proxy",
            "all_proxy",
        ]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .filter_map(|value| url::Url::parse(&value).ok())
        .filter_map(|url| url.host_str().map(str::to_lowercase))
        .collect();
        Ok(Self {
            client,
            cache: Arc::default(),
            slots: Arc::new(Semaphore::new(8)),
            proxy_hosts: Arc::new(proxy_hosts),
        })
    }

    async fn query(&self, name: &str, kind: &str) -> Result<Answer> {
        let mut response = self
            .client
            .get("https://cloudflare-dns.com/dns-query")
            .header("Accept", "application/dns-json")
            .query(&[("name", name), ("type", kind)])
            .send()
            .await?
            .error_for_status()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if bytes.len() + chunk.len() > 65536 {
                bail!("Слишком большой ответ DNS");
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(serde_json::from_slice(&bytes)?)
    }

    async fn addresses(&self, name: String) -> Result<Vec<IpAddr>> {
        if let Some(entry) = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&name)
            .filter(|entry| entry.expires > Instant::now())
            .cloned()
        {
            return Ok(entry.addresses);
        }
        let _slot = tokio::time::timeout(Duration::from_secs(4), self.slots.acquire())
            .await
            .context("DNS перегружен")?;
        let (a, aaaa) = tokio::join!(self.query(&name, "A"), self.query(&name, "AAAA"));
        let mut addresses = Vec::new();
        let mut ttl = 300;
        for answer in [a, aaaa].into_iter().flatten() {
            let (ips, seconds) = answer.addresses();
            addresses.extend(ips);
            ttl = ttl.min(seconds);
        }
        addresses.sort_unstable();
        addresses.dedup();
        if addresses.is_empty() {
            bail!("Cloudflare DNS не вернул адрес");
        }
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.retain(|_, entry| entry.expires > Instant::now());
        if cache.len() >= 256
            && let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, e)| e.expires)
                .map(|(key, _)| key.clone())
        {
            cache.remove(&oldest);
        }
        cache.insert(
            name,
            Entry {
                addresses: addresses.clone(),
                expires: Instant::now() + Duration::from_secs(ttl.clamp(1, 300)),
            },
        );
        Ok(addresses)
    }
}

impl Resolve for CloudflareDns {
    fn resolve(&self, name: Name) -> Resolving {
        let resolver = self.clone();
        Box::pin(async move {
            let hostname = name.as_str().to_lowercase();
            if resolver.proxy_hosts.contains(&hostname)
                || hostname == "localhost"
                || hostname.ends_with(".localhost")
            {
                let addresses: Vec<SocketAddr> = tokio::net::lookup_host((hostname.as_str(), 0))
                    .await?
                    .collect();
                return Ok(Box::new(addresses.into_iter()) as Addrs);
            }
            let addresses = resolver
                .addresses(name.as_str().to_owned())
                .await
                .map_err(|_| std::io::Error::other("Cloudflare DNS недоступен"))?;
            Ok(Box::new(addresses.into_iter().map(|ip| SocketAddr::new(ip, 0))) as Addrs)
        })
    }
}

#[derive(Deserialize)]
struct Answer {
    #[serde(rename = "Status")]
    status: u32,
    #[serde(rename = "Answer", default)]
    answers: Vec<Record>,
}

#[derive(Deserialize)]
struct Record {
    #[serde(rename = "type")]
    kind: u32,
    #[serde(rename = "TTL")]
    ttl: u64,
    data: String,
}

impl Answer {
    fn addresses(self) -> (Vec<IpAddr>, u64) {
        if self.status != 0 {
            return (Vec::new(), 300);
        }
        let mut result = Vec::new();
        let mut ttl = 300;
        for record in self.answers {
            if matches!(record.kind, 1 | 28)
                && let Ok(ip) = record.data.parse::<IpAddr>()
                && ((record.kind == 1 && ip.is_ipv4()) || (record.kind == 28 && ip.is_ipv6()))
            {
                result.push(ip);
                ttl = ttl.min(record.ttl);
            }
        }
        (result, ttl)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn proxy_validation_rejects_paths_and_hides_credentials() {
        assert!(validate_proxy("socks5h://127.0.0.1:1080").is_ok());
        assert!(validate_proxy("http://proxy.test:8080/").is_ok());
        let error = validate_proxy("http://user:secret@proxy.test/path")
            .unwrap_err()
            .to_string();
        assert!(!error.contains("secret"));
        assert!(validate_proxy("file:///tmp/x").is_err());
        assert!(validate_proxy("https://proxy.test:443/path").is_err());
    }
    #[test]
    fn dns_answer_ignores_cnames_and_obeys_ttl() {
        let answer: Answer = serde_json::from_str(
            r#"{"Status":0,"Answer":[{"type":5,"TTL":999,"data":"other.test"},{"type":1,"TTL":42,"data":"1.2.3.4"},{"type":28,"TTL":60,"data":"::1"},{"type":1,"TTL":1,"data":"invalid"}]}"#,
        )
        .unwrap();
        let (addresses, ttl) = answer.addresses();
        assert_eq!(addresses.len(), 2);
        assert_eq!(ttl, 42);
    }
    #[test]
    fn builder_honors_proxy_and_rejects_bad_mode() {
        unsafe { std::env::set_var("PUMP_HTTP_PROXY", "http://127.0.0.1:8080") };
        assert!(client_builder().is_ok());
        unsafe { std::env::set_var("PUMP_DNS_MODE", "yandex") };
        assert!(client_builder().is_err());
        unsafe {
            std::env::remove_var("PUMP_DNS_MODE");
            std::env::remove_var("PUMP_HTTP_PROXY");
        }
    }
}
