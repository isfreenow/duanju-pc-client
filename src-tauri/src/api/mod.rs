//! HTTP transport and shared error type for the Hongguo service layer.
//!
//! Everything network-facing lives here so the rules the reference encodes are
//! enforced once:
//!
//! * **Domain allowlists.** Signed API hosts must stay under `fqnovel.com` /
//!   `snssdk.com`; resolved media hosts must stay under the three CDN domains.
//!   A response that points anywhere else is refused rather than fetched.
//! * **No signed body is ever replayed to a redirect target.** A POST that comes
//!   back as a redirect is an error, exactly like the reference.
//! * **One shared deadline per logical call**, so a slow first hop cannot make a
//!   later hop run without a budget.

use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

pub mod app_search;
pub mod app_video;
pub mod hema;
pub mod router;
pub mod web;
pub mod xifan;

/// 第三方来源给出的一条明文片源（无密钥、浏览器直接播）。
///
/// `codec` 允许为空：这些 CDN 不声明编码，前端按 `video/mp4` 探测即可，
/// 与其猜一个 `h264` 不如不写。
#[derive(Debug, Clone)]
pub struct PlainSource {
    pub url: String,
    pub quality: String,
    pub width: u32,
    pub height: u32,
    pub codec: String,
}

pub const API_DOMAINS: &[&str] = &["fqnovel.com", "snssdk.com"];
pub const MEDIA_DOMAINS: &[&str] = &["qznovelvod.com", "fqnovelvod.com", "douyinvod.com"];

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// Input the caller should fix (bad id, bad page, empty keyword).
    #[error("{0}")]
    Input(String),
    #[error("{0}")]
    Network(String),
    #[error("请求超时，请稍后重试")]
    Timeout,
    #[error("来源返回了不支持的服务地址")]
    SecurityBlocked,
    #[error("来源未返回有效数据")]
    InvalidResponse,
    #[error("{message}")]
    Business { message: String, code: String },
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    /// Stable machine-readable code for the UI; never leaks an upstream body.
    pub fn code(&self) -> String {
        match self {
            ApiError::Input(_) => "INPUT".to_string(),
            ApiError::Network(_) => "NETWORK".to_string(),
            ApiError::Timeout => "ETIMEDOUT".to_string(),
            ApiError::SecurityBlocked => "SECURITY_BLOCKED".to_string(),
            ApiError::InvalidResponse => "INVALID_RESPONSE".to_string(),
            ApiError::Business { code, .. } => code.clone(),
        }
    }
}

impl From<reqwest::Error> for ApiError {
    fn from(error: reqwest::Error) -> Self {
        if error.is_timeout() {
            ApiError::Timeout
        } else {
            ApiError::Network(error.to_string())
        }
    }
}

/// `checkedUrl(value, domains)` — https only, no credentials, default port, and
/// the host must be the domain itself or a subdomain of it.
pub fn checked_url(value: &str, domains: &[&str]) -> ApiResult<Url> {
    let parsed = Url::parse(value).map_err(|_| ApiError::Input("地址格式无效".to_string()))?;
    let host = parsed.host_str().unwrap_or("");
    let allowed = domains
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")));
    if parsed.scheme() != "https"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || (parsed.port().is_some() && parsed.port() != Some(443))
        || !allowed
    {
        return Err(ApiError::SecurityBlocked);
    }
    Ok(parsed)
}

/// A deadline shared by every hop of one logical call.
#[derive(Clone, Copy)]
pub struct Deadline(Instant);

impl Deadline {
    pub fn after(seconds: u64) -> Self {
        Deadline(Instant::now() + Duration::from_secs(seconds))
    }

    /// Remaining budget; `None` once it is exhausted (the reference throws
    /// instead of issuing a request with no time left).
    pub fn remaining(&self) -> Option<Duration> {
        let now = Instant::now();
        (now < self.0).then(|| self.0 - now)
    }
}

/// `requestJson` for GET/POST against the signed API hosts.
///
/// `body` is only ever sent to the original URL; a redirect on a POST is refused.
pub async fn request_json(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    domains: &[&str],
    deadline: Deadline
) -> ApiResult<serde_json::Value> {
    let mut current = checked_url(url, domains)?.as_str().to_string();
    for _ in 0..4 {
        let target = checked_url(&current, domains)?;
        let remaining = deadline.remaining().ok_or(ApiError::Timeout)?;
        let method = if body.is_some() { reqwest::Method::POST } else { reqwest::Method::GET };
        let mut builder = client
            .request(method, target.clone())
            .timeout(remaining)
            .headers(build_headers(headers)?);
        if let Some(payload) = body {
            builder = builder.body(payload.to_vec());
        }
        let response = builder.send().await.map_err(|error| {
            if error.is_timeout() {
                ApiError::Timeout
            } else {
                ApiError::Network("网络请求失败，请稍后重试".to_string())
            }
        })?;
        let status = response.status();
        if status.is_redirection() {
            // Do not resend a signed body to a redirect destination.
            let location = response.headers().get("location").and_then(|v| v.to_str().ok());
            if body.is_some() || location.is_none() {
                return Err(ApiError::SecurityBlocked);
            }
            current = target
                .join(location.unwrap_or_default())
                .map_err(|_| ApiError::SecurityBlocked)?
                .to_string();
            continue;
        }
        if status.as_u16() != 200 {
            return Err(ApiError::Business {
                message: format!("请求失败（HTTP {}）", status.as_u16()),
                code: "HTTP".to_string(),
            });
        }
        let bytes = response.bytes().await?;
        if bytes.is_empty() {
            return Err(ApiError::Business {
                message: "接口返回空响应".to_string(),
                code: "APP_EMPTY".to_string(),
            });
        }
        let parsed: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|_| ApiError::InvalidResponse)?;
        if !parsed.is_object() {
            return Err(ApiError::InvalidResponse);
        }
        return Ok(parsed);
    }
    Err(ApiError::SecurityBlocked)
}

/// Plain text fetch for the website pages (no domain allowlist: the site origin
/// is pinned by the caller).
pub async fn fetch_text(
    client: &reqwest::Client,
    url: &str,
    headers: &[(&str, &str)],
    deadline: Deadline,
    max_bytes: usize,
) -> ApiResult<String> {
    let remaining = deadline.remaining().ok_or(ApiError::Timeout)?;
    let mut builder = client.get(url).timeout(remaining);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = builder.send().await.map_err(|error| {
        if error.is_timeout() {
            ApiError::Timeout
        } else {
            ApiError::Network("来源连接失败，请稍后重试".to_string())
        }
    })?;
    let status = response.status();
    if status.as_u16() == 429 || status.is_server_error() {
        return Err(ApiError::Business {
            message: format!("来源暂不可用（HTTP {}）", status.as_u16()),
            code: "SOURCE".to_string(),
        });
    }
    if !status.is_success() {
        return Err(ApiError::Business {
            message: format!("来源请求失败（HTTP {}）", status.as_u16()),
            code: "SOURCE".to_string(),
        });
    }
    let bytes = response.bytes().await?;
    if bytes.len() > max_bytes {
        return Err(ApiError::Input("来源页面过大，请稍后重试".to_string()));
    }
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

fn build_headers(headers: &[(String, String)]) -> ApiResult<HeaderMap> {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let key = HeaderName::from_bytes(name.as_bytes()).map_err(|_| ApiError::Input("请求头名称无效".to_string()))?;
        map.insert(key, HeaderValue::from_str(value).map_err(|_| ApiError::Input("请求头内容无效".to_string()))?);
    }
    Ok(map)
}

/// Build the shared client: redirects handled manually so a signature is never
/// replayed. The `cookies` feature is deliberately not enabled, which is what
/// keeps the reference's "no site cookies" rule by construction.
pub fn build_client() -> ApiResult<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .gzip(true)
        .connect_timeout(Duration::from_secs(6))
        .build()
        .map_err(ApiError::from)
}

/// Business-code envelope helper: `Number(payload.Code ?? payload.code ?? 0) !== 0`.
pub fn business_error(payload: &serde_json::Value) -> Option<String> {
    for key in ["Code", "code"] {
        match payload.get(key) {
            Some(serde_json::Value::Number(value)) => {
                if value.as_i64().unwrap_or(0) != 0 {
                    return Some(value.to_string());
                }
                return None;
            }
            Some(serde_json::Value::String(value)) => {
                if !value.is_empty() && value.parse::<i64>().unwrap_or(0) != 0 {
                    return Some(value.clone());
                }
                return None;
            }
            _ => {}
        }
    }
    None
}

/// Anything the UI may see must pass through one of these DTOs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesCard {
    pub series_id: String,
    pub series_title: String,
    pub cover: String,
    pub episode_count: u32,
    pub tags: Vec<String>,
    pub url: String,
    /// Only the website search exposes a synopsis; the App tab does not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rank: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heat: Option<String>,
    /// The catalogue and ranking cards spread `publicMetadata(item)`; App search
    /// results have no such field, so they carry the `unknown` default.
    #[serde(flatten)]
    pub metadata: PublicMetadata,
}

/// `publicMetadata(item)` — the only two labels the site exposes are the episode
/// counter and the release timestamp. Numeric `series_status` has no verified
/// mapping, so it is deliberately not guessed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicMetadata {
    pub episode_label: String,
    pub completion_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_at: Option<i64>,
}

impl Default for PublicMetadata {
    fn default() -> Self {
        PublicMetadata {
            episode_label: String::new(),
            completion_status: "unknown".to_string(),
            published_at: None,
        }
    }
}

impl PublicMetadata {
    pub fn from_item(item: &serde_json::Value) -> Self {
        // `typeof item?.episode_right_text === 'string' ? ….trim().slice(0, 80) : ''`
        let episode_label: String = item
            .get("episode_right_text")
            .and_then(Value::as_str)
            .map(|text| text.trim().chars().take(80).collect::<String>())
            .unwrap_or_default();
        let completion_status = if is_completed_label(&episode_label) {
            "completed"
        } else if is_ongoing_label(&episode_label) {
            "ongoing"
        } else {
            "unknown"
        };
        let published_at = match item.get("first_visible_time") {
            Some(Value::Number(number)) => number.as_f64(),
            Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
            _ => None,
        }
        .filter(|raw| raw.is_finite())
        .map(|raw| if raw < 1e12 { raw * 1000.0 } else { raw })
        .filter(|timestamp| *timestamp > 0.0 && *timestamp <= now_millis() as f64)
        .map(|timestamp| timestamp as i64);
        PublicMetadata {
            episode_label,
            completion_status: completion_status.to_string(),
            published_at,
        }
    }
}

/// `/^(?:全\s*\d+\s*集|已完结|完结)$/` — anchored, so the whole label must match.
fn is_completed_label(label: &str) -> bool {
    if label == "已完结" || label == "完结" {
        return true;
    }
    let Some(rest) = label.strip_prefix('全') else {
        return false;
    };
    let rest = trim_js_space(rest);
    let (digits, rest) = take_digits(rest);
    !digits.is_empty() && trim_js_space(rest) == "集"
}

/// `/连载|更新至\s*\d+\s*集/` — an unanchored search.
fn is_ongoing_label(label: &str) -> bool {
    if label.contains("连载") {
        return true;
    }
    let mut from = 0usize;
    while let Some(at) = find_sub(label, "更新至", from) {
        from = at + "更新至".len();
        let rest = trim_js_space(&label[from..]);
        let (digits, rest) = take_digits(rest);
        if !digits.is_empty() && trim_js_space(rest).starts_with("集") {
            return true;
        }
    }
    false
}

/// JavaScript's `\s`: Unicode whitespace plus the BOM, which `char::is_whitespace`
/// excludes.
pub fn is_js_space(ch: char) -> bool {
    ch.is_whitespace() || ch == '\u{feff}'
}

fn trim_js_space(value: &str) -> &str {
    value.trim_start_matches(is_js_space)
}

/// JavaScript's `\d`, which is ASCII-only even in a Unicode string.
fn take_digits(value: &str) -> (&str, &str) {
    let end = value
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(value.len());
    (&value[..end], &value[end..])
}

/// `String.prototype.indexOf(needle, from)` over byte offsets. Every scanner in
/// this crate slices by byte index, so the offsets stay consistent; the reference's
/// UTF-16 indices would not be usable here.
pub fn find_sub(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let haystack = haystack.as_bytes();
    let needle = needle.as_bytes();
    if needle.is_empty() || haystack.len() < needle.len() + from {
        return None;
    }
    (from..=haystack.len() - needle.len())
        .find(|&index| &haystack[index..index + needle.len()] == needle)
}

/// `isPublicMediaUrl` — the website's media links may name any host, so literal
/// addresses are checked against the same reserved ranges as the reference, and
/// hostnames must be dotted and outside the private suffixes.
///
/// A hostname is validated, not resolved: reqwest performs the lookup itself, so
/// rebinding is prevented at delivery time by fetching media through the
/// `hongguo://` protocol, where the resolved address is checked before connect.
pub fn is_public_media_url(value: &str) -> bool {
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_lowercase();
    if let Ok(address) = host.parse::<std::net::IpAddr>() {
        return is_public_address(&address);
    }
    host.contains('.') && !host.ends_with(".localhost")
        && !host.ends_with(".local")
        && !host.ends_with(".internal")
        && !host.ends_with(".lan")
        && !host.ends_with(".home")
        && !host.eq_ignore_ascii_case("localhost")
}

fn is_public_address(address: &std::net::IpAddr) -> bool {
    match address {
        std::net::IpAddr::V4(address) => {
            let value: u32 = (*address).into();
            !v4_blocked(value)
        }
        std::net::IpAddr::V6(address) => {
            let value: u128 = (*address).into();
            // `globalV6`: 2000::/3, or an IPv4-mapped address (`::ffff:0:0/96`).
            let mapped = v6_prefix(value, 0xffff_u128 << 32, 96);
            let global = v6_prefix(value, 0x2000_u128 << 112, 3) || mapped;
            // `blocked` for IPv6: 2001:db8::/32, 2001::/32, 2002::/16.
            let reserved = v6_prefix(value, 0x2001_0db8_u128 << 96, 32)
                || v6_prefix(value, 0x2001_0000_u128 << 96, 32)
                || v6_prefix(value, 0x0002_u128 << 112, 16);
            // A mapped address is checked again through its embedded IPv4 half: the
            // reference would let `::ffff:127.0.0.1` through, and a desktop client
            // should not.
            let embedded_is_public = address
                .to_ipv4_mapped()
                .map(|v4| !v4_blocked(u32::from(v4)))
                .unwrap_or(true);
            global && !reserved && embedded_is_public
        }
    }
}

/// `blocked` ranges for IPv4, from the reference's list.
fn v4_blocked(value: u32) -> bool {
    const RANGES: &[(u32, u8)] = &[
        (0x0000_0000, 8),
        (0x0a00_0000, 8),
        (0x6440_0000, 10),
        (0x7f00_0000, 8),
        (0xa9fe_0000, 16),
        (0xac10_0000, 12),
        (0xc000_0000, 24),
        (0xc000_0200, 24),
        (0xc0a8_0000, 16),
        (0xc612_0000, 15),
        (0xc633_6400, 24),
        (0xcb00_7100, 24),
        (0xe000_0000, 3),
    ];
    RANGES.iter().any(|(base, prefix)| v4_prefix(value, *base, *prefix))
}

fn v4_prefix(value: u32, base: u32, prefix: u8) -> bool {
    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix as u32) };
    value & mask == base & mask
}

fn v6_prefix(value: u128, base: u128, prefix: u8) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - prefix as u128)
    };
    value & mask == base & mask
}

pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_labels_match_the_reference_regexes() {
        let item = serde_json::json!({ "episode_right_text": "  全 62 集  ", "first_visible_time": 1_700_000_000 });
        let meta = PublicMetadata::from_item(&item);
        assert_eq!(meta.episode_label, "全 62 集");
        assert_eq!(meta.completion_status, "completed");
        assert_eq!(meta.published_at, Some(1_700_000_000_000));

        for (label, status) in [
            ("更新至 12 集", "ongoing"),
            ("连载中的剧", "ongoing"),
            ("已完结", "completed"),
            ("完结", "completed"),
            ("热播中", "unknown"),
        ] {
            let item = serde_json::json!({ "episode_right_text": label });
            assert_eq!(
                PublicMetadata::from_item(&item).completion_status,
                status,
                "label {label}"
            );
        }

        // A future or absent timestamp is never presented as a release date, and
        // the label is capped at 80 characters.
        let item = serde_json::json!({ "episode_right_text": "x".repeat(120), "first_visible_time": now_millis() + 86_400_000 });
        let meta = PublicMetadata::from_item(&item);
        assert_eq!(meta.episode_label.chars().count(), 80);
        assert_eq!(meta.published_at, None);
    }

    #[test]
    fn only_public_media_hosts_pass_the_guard() {
        for url in [
            "https://hongguoduanju.com/detail",
            "http://v.douyinvod.com/a.mp4",
            "https://8.8.8.8/a.mp4",
            "https://[2001:4860:4860::8888]/a.mp4",
            "https://[::ffff:8.8.8.8]/a.mp4",
        ] {
            assert!(is_public_media_url(url), "{url} should be allowed");
        }
        for url in [
            "",
            "not a url",
            "file:///C:/Windows/system32",
            "ftp://1.1.1.1/a",
            "https://user:pass@hongguoduanju.com/",
            "http://127.0.0.1:8080/a.mp4",
            "http://localhost/a.mp4",
            "http://0.0.0.0/a",
            "http://10.1.2.3/a",
            "http://100.64.0.1/a",
            "http://169.254.169.254/latest/meta-data",
            "http://172.16.0.1/a",
            "http://192.168.1.1/a",
            "http://198.18.0.1/a",
            "http://224.0.0.1/a",
            "http://[::1]/a",
            "http://[fe80::1]/a",
            "http://[2001:db8::1]/a",
            "http://[64:ff9b::1]/a",
            "http://[::ffff:127.0.0.1]/a",
            "http://[::ffff:192.168.1.1]/a",
            "http://intranet/a",
            "http://host.local/a",
            "http://a.localhost/a",
        ] {
            assert!(!is_public_media_url(url), "{url} should be blocked");
        }
    }

    #[test]
    fn resolved_media_urls_carry_no_credentials_or_private_hosts() {
        // The App payload can hand back anything; the domain allowlist is enforced
        // separately, this guard is the SSRF boundary.
        assert!(!is_public_media_url("https://hongguoduanju.com@127.0.0.1/"));
        assert!(!is_public_media_url("https://127.0.0.1#/hongguoduanju.com"));
    }
}
