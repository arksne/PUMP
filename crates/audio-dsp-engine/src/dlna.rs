//! Поиск DLNA-рендереров по SSDP и команды AVTransport в домашней сети.
use crate::{Error, Result};
use std::{collections::BTreeSet, time::Duration};
use tokio::net::UdpSocket;
use url::Url;
#[derive(Clone, Debug)]
pub struct Renderer {
    pub name: String,
    pub control_url: Url,
    pub service_type: String,
}
pub async fn discover(timeout: Duration) -> Result<Vec<Renderer>> {
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| Error::Backend(e.to_string()))?;
    socket.send_to(b"M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n","239.255.255.250:1900").await.map_err(|e|Error::Backend(e.to_string()))?;
    let deadline = tokio::time::Instant::now() + timeout.min(Duration::from_secs(30));
    let mut locations = BTreeSet::new();
    let mut buffer = [0u8; 8192];
    while let Ok(Ok((count, _))) =
        tokio::time::timeout_at(deadline, socket.recv_from(&mut buffer)).await
    {
        for line in String::from_utf8_lossy(&buffer[..count]).lines() {
            if let Some((key, value)) = line.split_once(':')
                && key.eq_ignore_ascii_case("location")
            {
                locations.insert(value.trim().to_owned());
            }
        }
        if locations.len() >= 64 {
            break;
        }
    }
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(4))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| Error::Backend(e.to_string()))?;
    let mut renderers = Vec::new();
    for location in locations {
        let Ok(base) = Url::parse(&location) else {
            continue;
        };
        if !matches!(base.scheme(), "http" | "https") {
            continue;
        }
        let Ok(response) = http.get(base.clone()).send().await else {
            continue;
        };
        if response.content_length().is_some_and(|n| n > 1024 * 1024) {
            continue;
        }
        let Ok(xml) = response.error_for_status() else {
            continue;
        };
        let Ok(text) = xml.text().await else {
            continue;
        };
        if text.len() > 1024 * 1024 {
            continue;
        }
        let Ok(document) = roxmltree::Document::parse(&text) else {
            continue;
        };
        let name = document
            .descendants()
            .find(|n| n.has_tag_name("friendlyName"))
            .and_then(|n| n.text())
            .unwrap_or("DLNA");
        for service in document.descendants().filter(|n| n.has_tag_name("service")) {
            let field = |tag| {
                service
                    .children()
                    .find(|n| n.has_tag_name(tag))
                    .and_then(|n| n.text())
            };
            let Some(kind) = field("serviceType")
                .filter(|s| s.starts_with("urn:schemas-upnp-org:service:AVTransport:"))
            else {
                continue;
            };
            let Some(url) = field("controlURL").and_then(|u| base.join(u).ok()) else {
                continue;
            };
            if url.origin() == base.origin() {
                renderers.push(Renderer {
                    name: name.into(),
                    control_url: url,
                    service_type: kind.into(),
                });
            }
        }
    }
    Ok(renderers)
}
impl Renderer {
    async fn command(&self, action: &str, args: &str) -> Result<()> {
        let body = format!(
            "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action} xmlns:u=\"{}\"><InstanceID>0</InstanceID>{args}</u:{action}></s:Body></s:Envelope>",
            self.service_type
        );
        reqwest::Client::new()
            .post(self.control_url.clone())
            .timeout(Duration::from_secs(8))
            .header("Content-Type", "text/xml; charset=\"utf-8\"")
            .header("SOAPAction", format!("\"{}#{action}\"", self.service_type))
            .body(body)
            .send()
            .await
            .map_err(|e| Error::Backend(e.to_string()))?
            .error_for_status()
            .map_err(|e| Error::Backend(e.to_string()))?;
        Ok(())
    }
    pub async fn load(&self, url: &Url) -> Result<()> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(Error::Invalid("DLNA media URL"));
        }
        let safe = url
            .as_str()
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        self.command(
            "SetAVTransportURI",
            &format!("<CurrentURI>{safe}</CurrentURI><CurrentURIMetaData></CurrentURIMetaData>"),
        )
        .await
    }
    pub async fn play(&self) -> Result<()> {
        self.command("Play", "<Speed>1</Speed>").await
    }
    pub async fn pause(&self) -> Result<()> {
        self.command("Pause", "").await
    }
    pub async fn seek(&self, seconds: u32) -> Result<()> {
        self.command(
            "Seek",
            &format!(
                "<Unit>REL_TIME</Unit><Target>{:02}:{:02}:{:02}</Target>",
                seconds / 3600,
                seconds / 60 % 60,
                seconds % 60
            ),
        )
        .await
    }
}
