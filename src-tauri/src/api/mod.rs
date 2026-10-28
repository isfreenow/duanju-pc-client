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
//! * **Every hop of a redirect chain is re-checked**, and an untrusted host is
//!   looked up before it is connected to — that part lives in [`netguard`], which
//!   owns the address rules and the socket pools.

use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

pub mod app_search;
pub mod app_video;
pub mod hema;
pub mod netguard;
pub mod rank;
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
    /// 逐跳校验过了，但重定向链没有尽头。
    #[error("来源跳转次数过多，已停止请求")]
    TooManyRedirects,
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
            ApiError::TooManyRedirects => "REDIRECT_LOOP".to_string(),
            ApiError::Business { code, .. } => code.clone(),
        }
    }

    /// 「下一步做什么」。失败信息只有一句「播放出错」时，用户能做的只有重试；
    /// 这一字段把「换一条清晰度」「稍后再试」「到别的页签找」这类动作随错误一起
    /// 交出去。只在少数几类错误上有定制说法，其余走默认文案。
    pub fn recovery_hint(&self) -> Option<&'static str> {
        match self {
            ApiError::Timeout => Some("如果一直这样，检查一下网络，或稍后再试一次。"),
            ApiError::Network(_) => Some("网络不通时只影响取数据，已经看过的进度不会丢。"),
            ApiError::SecurityBlocked => {
                Some("这条片源指向了不被支持的地址，换一条清晰度或稍后再试。")
            }
            ApiError::InvalidResponse => Some("来源页面结构可能变了；稍后重试，仍不行请反馈。"),
            ApiError::TooManyRedirects => Some("来源跳转异常，稍后重试或换一条片源。"),
            ApiError::Business { code, .. } => match code.as_str() {
                "SOURCE" => Some("来源暂时不可用，稍后重试会自动换一个片源。"),
                "NO_PLAYABLE_SOURCE" => Some("换一集或稍后再试；本集没有可播的片源。"),
                "SEARCH_UPSTREAM" => Some("稍后重试；搜索服务暂时不可用，与本地设置无关。"),
                "SEARCH_FORMAT" => Some("搜索接口变了，需要新版本适配；可先到首页类目浏览。"),
                "APP_EMPTY" | "APP_BUSINESS" => Some("已自动改走官网数据；若结果不全，稍后重试。"),
                _ => None,
            },
            ApiError::Input(_) => None,
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

/// 一条请求走哪套地址规则。把「控制面」与「数据面」分开说，是因为它们的风险
/// 完全不同：API 主机名是本模块里写死的常量，已经过了域名白名单；站点与片源
/// 主机名来自响应内容，必须由 [`netguard`] 再看一眼实际地址。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchKind {
    /// 签名接口主机（域名白名单已把住），不做 IP 校验。
    Api,
    /// 站点页面与站点接口：逐跳验地址，官网那一跳优先可信 DNS（DoH 钉地址）。
    Site,
    /// 与 `Site` 同一逐跳地址规则，但**强制走普通系统解析**（不优先可信 DNS）。
    /// 分集页双路线竞速用它做「DoH 端点被墙时仍能出片单」的那条退路。
    SitePlain,
    /// 片源地址：逐跳验地址，普通 DNS。
    Media,
}

/// 传输层的中间错误，由调用方按自己的语义翻成 `ApiError`（同一个 HTTP 429 在
/// 签名接口上是业务错误，在站点页面上是「来源不可用」）。
enum TransportError {
    Status(u16),
    Failure(ApiError),
    TooManyRedirections,
}

impl From<TransportError> for ApiError {
    fn from(error: TransportError) -> Self {
        match error {
            TransportError::Failure(error) => error,
            TransportError::TooManyRedirections => ApiError::TooManyRedirects,
            TransportError::Status(_) => ApiError::InvalidResponse,
        }
    }
}

impl From<ApiError> for TransportError {
    fn from(error: ApiError) -> Self {
        TransportError::Failure(error)
    }
}

/// 单一传输入口：逐跳重读 URL、逐跳过地址规则、每一跳都重新查剩余预算。
///
/// 重定向一律手工跟（`Policy::none`），因为签名过的请求体不能被重放到另一个
/// 主机上；`domains` 非空时每一跳还要求主机落在白名单里。`kind` 决定是否需要
/// 在连接前先解析并校验地址（钉到公网地址）。
async fn transport(
    client: &reqwest::Client,
    url: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    domains: &[&str],
    deadline: Deadline,
    kind: FetchKind,
) -> Result<Vec<u8>, TransportError> {
    let method = if body.is_some() {
        reqwest::Method::POST
    } else {
        reqwest::Method::GET
    };
    let mut current = url.to_string();
    for _ in 0..netguard::MAX_HOPS {
        let target =
            Url::parse(&current).map_err(|_| ApiError::Input("地址格式无效".to_string()))?;
        if !domains.is_empty() {
            if checked_url(&current, domains).is_err() {
                return Err(TransportError::Failure(ApiError::SecurityBlocked));
            }
        } else if netguard::check_hop(&target).is_err() {
            return Err(TransportError::Failure(ApiError::SecurityBlocked));
        }
        let trusted = match kind {
            FetchKind::Api => true,
            FetchKind::Site => target.host_str() == Some(netguard::OFFICIAL_HOST),
            FetchKind::SitePlain | FetchKind::Media => false,
        };
        let hop = match kind {
            FetchKind::Api => netguard::Route::Direct,
            _ => netguard::route_for(&target, trusted)
                .await
                .map_err(TransportError::Failure)?,
        };
        let sender = netguard::client_for(&hop, client);
        let remaining = match deadline.remaining() {
            Some(remaining) => remaining,
            None => return Err(TransportError::Failure(ApiError::Timeout)),
        };
        let mut builder = sender
            .request(method.clone(), target.clone())
            .timeout(remaining)
            .headers(build_headers(headers)?);
        if let Some(payload) = body {
            builder = builder.body(payload.to_vec());
        }
        let response = builder.send().await.map_err(|error| {
            TransportError::Failure(if error.is_timeout() {
                ApiError::Timeout
            } else {
                ApiError::Network(error.to_string())
            })
        })?;
        let status = response.status();
        if status.is_redirection() {
            // 签名请求体绝不重放到重定向目标上，这一点与参考实现一致。
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok());
            if body.is_some() || location.is_none() {
                return Err(TransportError::Failure(ApiError::SecurityBlocked));
            }
            current = match target.join(location.unwrap_or_default()) {
                Ok(next) => next.to_string(),
                Err(_) => return Err(TransportError::Failure(ApiError::SecurityBlocked)),
            };
            continue;
        }
        if !status.is_success() {
            return Err(TransportError::Status(status.as_u16()));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|error| TransportError::Failure(error.into()))?;
        return Ok(bytes.to_vec());
    }
    Err(TransportError::TooManyRedirections)
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
    deadline: Deadline,
) -> ApiResult<serde_json::Value> {
    let bytes = match transport(
        client,
        url,
        headers,
        body,
        domains,
        deadline,
        FetchKind::Api,
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(TransportError::Status(code)) => Err(ApiError::Business {
            message: format!("请求失败（HTTP {code}）"),
            code: "HTTP".to_string(),
        })?,
        Err(other) => Err(ApiError::from(other))?,
    };
    if bytes.is_empty() {
        return Err(ApiError::Business {
            message: "接口返回空响应".to_string(),
            code: "APP_EMPTY".to_string(),
        });
    }
    let parsed: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| ApiError::InvalidResponse)?;
    if !parsed.is_object() {
        return Err(ApiError::InvalidResponse);
    }
    Ok(parsed)
}

/// Plain text fetch for the website pages (no domain allowlist: the site origin
/// is pinned by the caller).
pub async fn fetch_text(
    client: &reqwest::Client,
    url: &str,
    headers: &[(&str, &str)],
    kind: FetchKind,
    deadline: Deadline,
    max_bytes: usize,
) -> ApiResult<String> {
    let headers = headers
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect::<Vec<_>>();
    let bytes = match transport(client, url, &headers, None, &[], deadline, kind).await {
        Ok(bytes) => bytes,
        Err(TransportError::Status(code)) => Err(ApiError::Business {
            message: if code == 429 || code >= 500 {
                format!("来源暂不可用（HTTP {code}）")
            } else {
                format!("来源请求失败（HTTP {code}）")
            },
            code: "SOURCE".to_string(),
        })?,
        Err(other) => Err(ApiError::from(other))?,
    };
    if bytes.len() > max_bytes {
        return Err(ApiError::Input("来源页面过大，请稍后重试".to_string()));
    }
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

/// 同一个传输入口的二进制版本：片源下载与按需拉取的转码工具都走它，因此同样
/// 逐跳过 SSRF 规则，并且只用普通系统解析（数据面的主机名来自响应，不给它
/// 优先走可信 DNS 的理由）。
///
/// 与 [`fetch_text`] 的区别只在出口：这里落盘而不是转字符串，并且把每一块的进
/// 度回报给调用方（命令层把它变成前端事件）。`max_bytes` 是硬上限，宁可失败也
/// 不把无上限的响应写满磁盘。
pub async fn fetch_binary(
    client: &reqwest::Client,
    url: &str,
    headers: &[(&str, &str)],
    deadline: Deadline,
    max_bytes: u64,
    path: &std::path::Path,
    mut on_progress: impl FnMut(u64, Option<u64>),
) -> ApiResult<u64> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;

    let headers = headers
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect::<Vec<_>>();
    // 手工重定向那一套与 `transport` 相同，但数据面不整块读进内存。
    let mut current = url.to_string();
    for _ in 0..netguard::MAX_HOPS {
        let target =
            Url::parse(&current).map_err(|_| ApiError::Input("地址格式无效".to_string()))?;
        if netguard::check_hop(&target).is_err() {
            return Err(ApiError::SecurityBlocked);
        }
        let hop = netguard::route_for(&target, false).await?;
        let sender = netguard::client_for(&hop, client);
        let remaining = deadline.remaining().ok_or(ApiError::Timeout)?;
        let response = sender
            .request(reqwest::Method::GET, target.clone())
            .timeout(remaining)
            .headers(build_headers(&headers)?)
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    ApiError::Timeout
                } else {
                    ApiError::from(error)
                }
            })?;
        let status = response.status();
        if status.is_redirection() {
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok());
            current = match location.and_then(|value| target.join(value).ok()) {
                Some(next) => next.to_string(),
                None => return Err(ApiError::TooManyRedirects),
            };
            continue;
        }
        if !status.is_success() {
            return Err(ApiError::Business {
                message: format!("下载失败（HTTP {}）", status.as_u16()),
                code: "DOWNLOAD".to_string(),
            });
        }
        let total = response.content_length().filter(|value| *value > 0);
        if total.is_some_and(|value| value > max_bytes) {
            return Err(ApiError::Business {
                message: "文件超出允许的大小".to_string(),
                code: "DOWNLOAD".to_string(),
            });
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| ApiError::Network(error.to_string()))?;
        }
        let mut file = tokio::fs::File::create(path)
            .await
            .map_err(|error| ApiError::Network(error.to_string()))?;
        let mut stream = response.bytes_stream();
        let mut done: u64 = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(ApiError::from)?;
            done += chunk.len() as u64;
            if done > max_bytes {
                let _ = file.shutdown().await;
                let _ = tokio::fs::remove_file(path).await;
                return Err(ApiError::Business {
                    message: "文件超出允许的大小".to_string(),
                    code: "DOWNLOAD".to_string(),
                });
            }
            file.write_all(&chunk)
                .await
                .map_err(|error| ApiError::Network(error.to_string()))?;
            on_progress(done, total);
        }
        file.flush()
            .await
            .map_err(|error| ApiError::Network(error.to_string()))?;
        drop(file);
        return Ok(done);
    }
    Err(ApiError::TooManyRedirects)
}

/// 块间流控：`download_stream` 每写完一块就回调调用方，由它决定下一条怎么办。
/// 暂停与取消只能在块与块之间生效（一次长连接中途停不下来），这也是把
/// 下载拆成多 `Range` 块的意义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadFlow {
    Continue,
    Pause,
    Cancel,
}

/// 断点续传下载：从磁盘已有的字节数起，用 `Range: bytes=<off>-` 接着往 `path`
/// 追加，逐块回报绝对进度并让调用方在块间选择继续/暂停/取消。
///
/// 与 [`fetch_binary`] 的区别：① 可续——起点以磁盘文件实际大小为准，调用方
/// 不需要传；② 能解析总量（从 `Content-Range` 或 `Content-Length`）；③ 块间可停。
/// 返回 `(已落盘绝对字节, 总量, 是否下完)`；暂停时第三个为 `false`，且已写入的
/// 部分保留在 `path` 里供下次续。超时/网络错时不删文件（下次自动续），
/// 只有取消与超限才清。
///
/// 地址是 CDN 签发的短期凭据：长下载中途过期会在这里报错，由调用方重新
/// 解一条新鲜地址、再带同一个文件重进即可（函数本身不关心地址从哪来）。
pub async fn download_stream(
    client: &reqwest::Client,
    url: &str,
    headers: &[(&str, &str)],
    deadline: Deadline,
    max_bytes: u64,
    path: &std::path::Path,
    mut on_progress: impl FnMut(u64, Option<u64>) -> DownloadFlow,
) -> ApiResult<(u64, Option<u64>, bool)> {
    use futures_util::StreamExt;
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};

    let headers = headers
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect::<Vec<_>>();
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| ApiError::Network(error.to_string()))?;
    }
    // 起点以磁盘实际大小为准，不信任调用方传的值：避免文件与记录的字节数不一致。
    let mut offset = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    let mut current = url.to_string();
    for _ in 0..netguard::MAX_HOPS {
        let target =
            Url::parse(&current).map_err(|_| ApiError::Input("地址格式无效".to_string()))?;
        if netguard::check_hop(&target).is_err() {
            return Err(ApiError::SecurityBlocked);
        }
        let hop = netguard::route_for(&target, false).await?;
        let sender = netguard::client_for(&hop, client);
        let remaining = deadline.remaining().ok_or(ApiError::Timeout)?;
        let mut builder = sender
            .request(reqwest::Method::GET, target.clone())
            .timeout(remaining)
            .headers(build_headers(&headers)?);
        if offset > 0 {
            builder = builder.header("Range", format!("bytes={offset}-"));
        }
        let response = builder.send().await.map_err(|error| {
            if error.is_timeout() {
                ApiError::Timeout
            } else {
                ApiError::from(error)
            }
        })?;
        let status = response.status();
        if status.is_redirection() {
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok());
            current = match location.and_then(|value| target.join(value).ok()) {
                Some(next) => next.to_string(),
                None => return Err(ApiError::TooManyRedirects),
            };
            continue;
        }
        let partial = status.as_u16() == 206;
        if !(status.is_success() || partial) {
            return Err(ApiError::Business {
                message: format!("下载失败（HTTP {}）", status.as_u16()),
                code: "DOWNLOAD".to_string(),
            });
        }
        // 从 Content-Range（`bytes <start>-<end>/<total>`）拿真实起点与总量；不支持
        // Range 的服务器会返回 200 + 整段，此时 `partial=false` → 必须从头重写。
        let content_range = response
            .headers()
            .get("content-range")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let (range_total, range_start) = parse_content_range(content_range.as_deref());
        let write_start = if partial {
            range_start.unwrap_or(offset)
        } else {
            0
        };
        offset = write_start;
        let total = range_total.or_else(|| {
            response
                .content_length()
                .filter(|len| *len > 0)
                .map(|len| if partial { len + offset } else { len })
        });
        if total.is_some_and(|value| value > max_bytes) {
            return Err(ApiError::Business {
                message: "文件超出允许的大小".to_string(),
                code: "DOWNLOAD".to_string(),
            });
        }
        // 打开：从头（write_start=0）截新建，续传时写权限打开再 seek 到起点。
        let mut file = if offset == 0 {
            tokio::fs::File::create(path)
                .await
                .map_err(|error| ApiError::Network(error.to_string()))?
        } else {
            let mut opened = tokio::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .await
                .map_err(|error| ApiError::Network(error.to_string()))?;
            opened
                .seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(|error| ApiError::Network(error.to_string()))?;
            opened
        };
        let mut stream = response.bytes_stream();
        let mut done = offset;
        while let Some(chunk) = stream.next().await {
            // 块本身拉失败（含中途超时）：不删文件，已写部分保留，直接上报错误供上层重试续传。
            let chunk = chunk.map_err(ApiError::from)?;
            done += chunk.len() as u64;
            if done > max_bytes {
                let _ = tokio::fs::remove_file(path).await;
                return Err(ApiError::Business {
                    message: "文件超出允许的大小".to_string(),
                    code: "DOWNLOAD".to_string(),
                });
            }
            file.write_all(&chunk)
                .await
                .map_err(|error| ApiError::Network(error.to_string()))?;
            match on_progress(done, total) {
                DownloadFlow::Continue => {}
                DownloadFlow::Pause => {
                    file.flush()
                        .await
                        .map_err(|error| ApiError::Network(error.to_string()))?;
                    drop(file);
                    return Ok((done, total, false));
                }
                DownloadFlow::Cancel => {
                    drop(file);
                    let _ = tokio::fs::remove_file(path).await;
                    return Err(ApiError::Business {
                        code: "CANCEL".to_string(),
                        message: "已取消下载".to_string(),
                    });
                }
            }
        }
        file.flush()
            .await
            .map_err(|error| ApiError::Network(error.to_string()))?;
        drop(file);
        return Ok((done, total, true));
    }
    Err(ApiError::TooManyRedirects)
}

/// `bytes 500-1000/1234` → `(Some(1234), Some(500))`；拿不到就回 `None`，不报错。
fn parse_content_range(value: Option<&str>) -> (Option<u64>, Option<u64>) {
    let Some(text) = value else {
        return (None, None);
    };
    let (span, total) = match text.split_once('/') {
        Some(parts) => parts,
        None => return (None, None),
    };
    let total = total.trim().parse::<u64>().ok();
    let start = span
        .trim()
        .strip_prefix("bytes")
        .unwrap_or(span)
        .trim()
        .split('-')
        .next()
        .and_then(|head| head.trim().parse::<u64>().ok());
    (total, start)
}

fn build_headers(headers: &[(String, String)]) -> Result<HeaderMap, TransportError> {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let key = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| TransportError::Failure(ApiError::Input("请求头名称无效".to_string())))?;
        map.insert(
            key,
            HeaderValue::from_str(value).map_err(|_| {
                TransportError::Failure(ApiError::Input("请求头内容无效".to_string()))
            })?,
        );
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
    host.contains('.')
        && !host.ends_with(".localhost")
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
    RANGES
        .iter()
        .any(|(base, prefix)| v4_prefix(value, *base, *prefix))
}

fn v4_prefix(value: u32, base: u32, prefix: u8) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix as u32)
    };
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
