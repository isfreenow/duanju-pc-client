//! 网络层健壮性：SSRF 逐跳校验、IP 级校验与 socket 池隔离。
//!
//! 这一模块只管一件事——**请求实际连到哪个地址**。它不认业务、不认接口、不认
//! 片源策略：调用方给一个 URL，它回答「这个地址能不能连」，并在能连的时候决定
//! 用哪一个连接池去连。之所以单独立一个模块，是因为这几条规则一旦被业务代码
//! 各写一遍，就会有的地方写全、有的地方漏掉（改造前的实现正是这样：域名白名单
//! 齐了，IP 级校验只覆盖到字面 IP）。
//!
//! 三条规则：
//!
//! * **逐跳校验。** 重定向链上的每一跳都要重新过一遍协议、凭据与地址规则；任何
//!   一跳指向环回、私网或保留段，整条链就地失败。
//! * **解析结果校验。** 域名要给连接池之前先看它解析到什么：逐个验公网，再把
//!   验过的地址钉进连接（pin）。官网这一跳优先走可信 DNS（DoH）——本地代理的
//!   Fake-IP 会编出一个「看起来是公网」的 CDN 地址，普通解析器看不出它，可信
//!   解析器 + 保留段黑名单可以。
//! * **池隔离。** 调用方池、钉地址池、代理池、DoH 解析池各自独立。reqwest 按
//!   host:port 复用连接，共享池会把「经代理开的 socket」借给直连请求，反之亦
//!   然；原实现（Node 侧）用 agent key 加 `proxy`/`direct` 后缀避开这件事，这里
//!   用「不同池」达到同一个效果。
//!
//! 判定逻辑（公网段、Fake-IP 段、DoH 响应解析、逐跳校验）都是纯函数，配单元
//! 测试；真实 DoH 响应的样本放在 `src-tauri/tests/vectors/netguard.json`，由
//! `golden_netguard.rs` 读回来跑同一组断言。

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use url::Url;

use super::{ApiError, ApiResult, is_public_media_url};

/// 官网 origin 的主机名。这一跳的可信解析与 Fake-IP 兜底都以它为准。
pub const OFFICIAL_HOST: &str = "hongguoduanju.com";

/// 钉地址结果的存活上限。DoH 的 TTL 更短时按 TTL。
const PINNED_TTL: Duration = Duration::from_secs(60);
/// 可信解析整体预算：两路 DoH 竞速，谁先给出合法答案用谁。
const RESOLVE_BUDGET: Duration = Duration::from_secs(4);

/// DoH 端点。端点本身走系统解析——它是否可信由 TLS 证书与「答案必须全是公网」
/// 两道关卡兜住，而不是靠把解析器 IP 写死。
const DOH_ENDPOINTS: &[&str] = &[
    "https://cloudflare-dns.com/dns-query",
    "https://dns.alidns.com/resolve",
];

/// 一条地址的判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressVerdict {
    /// 公网可路由地址，允许连接。
    Public,
    /// 环回、私网、链路本地、保留段等——永远不许连。
    Blocked,
    /// 本地代理的 Fake-IP（198.18.0.0/15）。它形式上是「公网」，实际是代理编的：
    /// 既不许连，也不算致命错误，只表示「换可信解析重来」。
    Spoofed,
}

/// `classifyAddress` — 与 `is_public_media_url` 同一套保留段，额外把 Fake-IP 段
/// 单列出来（参考实现的 BlockList 把 198.18/15 直接列为 blocked，这里把它升级成
/// 一个可恢复的信号，因为「装了本地代理」不是错误）。
pub fn classify_address(address: IpAddr) -> AddressVerdict {
    if let IpAddr::V4(v4) = address {
        // 198.18.0.0/15：Benchmarking 段，被 Faster / Clash 之类当作 Fake-IP 池。
        if matches!(u32::from(v4) >> 16, 0xc612 | 0xc613) {
            return AddressVerdict::Spoofed;
        }
    }
    if is_public_ip(address) {
        AddressVerdict::Public
    } else {
        AddressVerdict::Blocked
    }
}

/// 公网判定：复用 `is_public_media_url` 的地址段规则（它已覆盖参考实现 BlockList
/// 的等价范围），只是这里手上已经是地址而不是 URL。
fn is_public_ip(address: IpAddr) -> bool {
    let url = match address {
        IpAddr::V4(v4) => format!("https://{v4}/"),
        IpAddr::V6(v6) => format!("https://[{v6}]/"),
    };
    is_public_media_url(&url)
}

/// 一个主机名的已校验地址集合与它的过期时间。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Resolved {
    pub host: String,
    /// 按地址字典序去过重，好让「同一组地址」在池 key 上是同一个值。
    pub addresses: Vec<IpAddr>,
    #[serde(skip)]
    pub expires_at: Instant,
}

impl Resolved {
    pub fn fresh(&self) -> bool {
        Instant::now() < self.expires_at
    }

    /// 钉地址池的分组键：主机名 + 地址集合。地址轮换即换池，不会把旧连接借给新答案。
    fn key(&self) -> String {
        // 按地址字序归一，让「同一组地址」不论输入顺序都是同一个键。
        let mut joined = self
            .addresses
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        joined.sort();
        joined.dedup();
        format!("{}:{}", self.host, joined.join(","))
    }
}

/// 校验一组解析结果。`Err` 表示这组地址不能连；`Ok(None)` 表示「全是 Fake-IP，
/// 该换可信解析再试一次」。
pub fn validate_addresses(host: &str, addresses: &[IpAddr]) -> ApiResult<Option<Resolved>> {
    if addresses.is_empty() {
        return Err(ApiError::SecurityBlocked);
    }
    let mut public = Vec::new();
    for address in addresses {
        match classify_address(*address) {
            AddressVerdict::Public => public.push(*address),
            // 一条越界就整组作废：半可信的答案不能拿来钉地址。
            AddressVerdict::Blocked => return Err(ApiError::SecurityBlocked),
            AddressVerdict::Spoofed => {}
        }
    }
    if public.is_empty() {
        // 整组都是 Fake-IP：不是错误，是「换可信解析重来」的信号。
        return Ok(None);
    }
    normalize(&mut public);
    Ok(Some(Resolved {
        host: host.to_string(),
        addresses: public,
        expires_at: Instant::now() + PINNED_TTL,
    }))
}

fn normalize(addresses: &mut Vec<IpAddr>) {
    addresses.sort_by_key(ToString::to_string);
    addresses.dedup();
}

/// 一条 URL 的连接计划：拿哪个池去发。
#[derive(Debug, Clone)]
pub enum Route {
    /// 调用方原本的池：字面 IP、或可信解析也拿不到答案时的退路（逐跳的
    /// `check_hop` 仍然拦保留段，域名白名单仍由各调用方把住）。
    Direct,
    /// 钉到这些公网地址的池。
    Pinned(Resolved),
    /// 用户显式指定的代理池。
    Proxied,
}

/// 解析 + 校验 + 钉地址。`trusted` 为真时官网这一跳直接走 DoH，不给 Fake-IP
/// 机会；其余情况先系统解析，撞到「全是 Fake-IP」再升级到 DoH。
pub async fn route_for(url: &Url, trusted: bool) -> ApiResult<Route> {
    check_hop(url)?;
    if proxy_url().is_some() {
        return Ok(Route::Proxied);
    }
    let Some(host) = url.host_str() else {
        return Ok(Route::Direct);
    };
    // 字面 IP 没有 DNS 重绑面：按地址规则放行后直接用调用方池。
    if host.parse::<IpAddr>().is_ok() {
        return Ok(Route::Direct);
    }
    let port = url.port_or_known_default().unwrap_or(443);
    if trusted && host == OFFICIAL_HOST {
        if let Some(resolved) = cached_pin(host) {
            return Ok(Route::Pinned(resolved));
        }
        return match query_trusted_dns(host).await {
            Ok(resolved) => {
                remember_pin(host, &resolved);
                Ok(Route::Pinned(resolved))
            }
            // DoH 不通（被封 / 无网）时不该把官网整站拦死：退回调用方池，
            // 逐跳校验与域名白名单仍然生效。
            Err(_) => Ok(Route::Direct),
        };
    }
    match system_lookup(host, port).and_then(|addresses| validate_addresses(host, &addresses)) {
        Ok(Some(resolved)) => {
            // Fake-IP 混在真公网地址里时，真地址是代理转发的出口，钉它没意义也无害。
            remember_pin(host, &resolved);
            Ok(Route::Pinned(resolved))
        }
        Ok(None) => match query_trusted_dns(host).await {
            Ok(resolved) => {
                remember_pin(host, &resolved);
                Ok(Route::Pinned(resolved))
            }
            Err(_) => Ok(Route::Direct),
        },
        // 系统解析本身失败：交给调用方池，让它报出真正的连接错误。
        Err(ApiError::SecurityBlocked) => Err(ApiError::SecurityBlocked),
        Err(_) => Ok(Route::Direct),
    }
}

fn system_lookup(host: &str, port: u16) -> ApiResult<Vec<IpAddr>> {
    let port = if port == 0 { 443 } else { port };
    (host, port)
        .to_socket_addrs()
        .map(|sockets| sockets.map(|socket| socket.ip()).collect())
        .map_err(|_| ApiError::InvalidResponse)
}

/// 按路由取池。`Route::Direct` 用调用方传进来的客户端。
pub fn client_for(route: &Route, fallback: &reqwest::Client) -> reqwest::Client {
    match route {
        Route::Direct => fallback.clone(),
        Route::Proxied => proxied_client().unwrap_or_else(|| fallback.clone()),
        Route::Pinned(resolved) => pinned_client(resolved).unwrap_or_else(|_| fallback.clone()),
    }
}

fn cached_pin(host: &str) -> Option<Resolved> {
    let guard = pins().lock().ok()?;
    guard.get(host).filter(|resolved| resolved.fresh()).cloned()
}

fn remember_pin(host: &str, resolved: &Resolved) {
    if let Ok(mut guard) = pins().lock() {
        // 只留最近若干主机：长时间挂机不该把钉住的地址越攒越多。
        if guard.len() > 32 {
            let oldest = guard
                .iter()
                .min_by_key(|(_, value)| value.expires_at)
                .map(|(key, _)| key.clone());
            if let Some(key) = oldest {
                guard.remove(&key);
            }
        }
        guard.insert(host.to_string(), resolved.clone());
    }
}

fn pins() -> &'static Mutex<BTreeMap<String, Resolved>> {
    static PINS: OnceLock<Mutex<BTreeMap<String, Resolved>>> = OnceLock::new();
    PINS.get_or_init(Default::default)
}

/// 钉地址池：按「主机 + 地址集合」分组，地址轮换时才新建一个池。
fn pinned_client(resolved: &Resolved) -> ApiResult<reqwest::Client> {
    static POOL: OnceLock<Mutex<BTreeMap<String, reqwest::Client>>> = OnceLock::new();
    let pool = POOL.get_or_init(Default::default);
    let key = resolved.key();
    if let Ok(guard) = pool.lock() {
        if let Some(client) = guard.get(&key) {
            return Ok(client.clone());
        }
    }
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .gzip(true)
        .connect_timeout(Duration::from_secs(6))
        // 钉地址与系统代理互斥：经代理时真实解析在远端，钉了也没用。
        .no_proxy();
    for address in &resolved.addresses {
        builder = builder.resolve(&resolved.host, SocketAddr::new(*address, 443));
    }
    let client = builder.build().map_err(ApiError::from)?;
    if let Ok(mut guard) = pool.lock() {
        if guard.len() > 16 {
            if let Some(first) = guard.keys().next().cloned() {
                guard.remove(&first);
            }
        }
        guard.insert(key, client.clone());
    }
    Ok(client)
}

/// 用户显式代理（`DUANJU_PROXY`）。设了就走代理池，并且**不再**对目标做 IP 校验：
/// 解析发生在代理远端，本地看不到真实地址，把它当作信任边界才是诚实的做法
/// （参考实现对代理的远端 DNS 也是同样的取舍）。
pub fn proxy_url() -> Option<String> {
    static PROXY: OnceLock<Option<String>> = OnceLock::new();
    PROXY
        .get_or_init(|| {
            std::env::var("DUANJU_PROXY")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty() && is_usable_proxy(value))
        })
        .clone()
}

/// 代理地址必须是 http(s)/socks 且带主机；环回与私网代理是常态，不作公网要求。
fn is_usable_proxy(value: &str) -> bool {
    Url::parse(value)
        .map(|url| {
            matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h")
                && url.host_str().is_some()
        })
        .unwrap_or(false)
}

fn proxied_client() -> Option<reqwest::Client> {
    static CLIENT: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            let target = proxy_url().unwrap_or_default();
            let proxy = reqwest::Proxy::all(&target).ok()?;
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .gzip(true)
                .connect_timeout(Duration::from_secs(6))
                .proxy(proxy)
                .build()
                .ok()
        })
        .clone()
}

/// 逐跳校验：协议、凭据与地址规则。重定向链的每一跳都过它。
pub fn check_hop(url: &Url) -> ApiResult<()> {
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || !is_public_media_url(url.as_str())
    {
        return Err(ApiError::SecurityBlocked);
    }
    Ok(())
}

/// 最多几跳。与参考实现的 `maxRedirects: 5` 同量级。
pub const MAX_HOPS: usize = 5;

// ── 可信 DNS（DoH）───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct DohAnswer {
    #[serde(rename = "type")]
    kind: Option<u16>,
    data: Option<String>,
    #[serde(rename = "TTL")]
    ttl: Option<u32>,
}

#[derive(Deserialize)]
struct DohResponse {
    #[serde(rename = "Status")]
    status: Option<u8>,
    #[serde(rename = "Answer")]
    answer: Option<Vec<DohAnswer>>,
}

/// `parseDohResponse` — 纯函数：只取 A 记录，且每一条都必须是公网可路由地址；
/// 任何一条越界整份作废。
pub fn parse_doh_response(host: &str, status: u16, body: &str) -> ApiResult<Resolved> {
    if status != 200 {
        return Err(ApiError::InvalidResponse);
    }
    let parsed: DohResponse = serde_json::from_str(body).map_err(|_| ApiError::InvalidResponse)?;
    if parsed.status != Some(0) {
        return Err(ApiError::InvalidResponse);
    }
    let mut addresses = Vec::new();
    let mut min_ttl = u32::MAX;
    for row in parsed.answer.unwrap_or_default() {
        if row.kind != Some(1) {
            continue;
        }
        let Some(data) = row.data.as_deref() else {
            continue;
        };
        let Ok(address) = data.trim().parse::<IpAddr>() else {
            return Err(ApiError::SecurityBlocked);
        };
        if classify_address(address) != AddressVerdict::Public {
            return Err(ApiError::SecurityBlocked);
        }
        addresses.push(address);
        min_ttl = min_ttl.min(row.ttl.unwrap_or(0));
    }
    if addresses.is_empty() {
        return Err(ApiError::SecurityBlocked);
    }
    normalize(&mut addresses);
    let cap = PINNED_TTL.as_secs();
    let ttl = Duration::from_secs(u64::from(min_ttl.min(cap as u32)).min(cap));
    Ok(Resolved {
        host: host.to_string(),
        addresses,
        expires_at: Instant::now() + ttl,
    })
}

/// 两路 DoH 竞速，先给出合法答案的胜出；两路都不成时把最后一个错误交回去，
/// 让调用方知道是「DoH 不通」而不是「官网挂了」。
async fn query_trusted_dns(host: &str) -> ApiResult<Resolved> {
    let client = resolver_client()?;
    let mut pending = Vec::new();
    for endpoint in DOH_ENDPOINTS {
        let Some(query) = doh_query(endpoint, host) else {
            continue;
        };
        pending.push({
            let client = client.clone();
            Box::pin(async move {
                let response = client.get(query).send().await?;
                let status = response.status().as_u16();
                let body = response.text().await?;
                Ok::<_, reqwest::Error>((status, body))
            })
        });
    }
    if pending.is_empty() {
        return Err(ApiError::InvalidResponse);
    }
    let budget = tokio::time::sleep(RESOLVE_BUDGET);
    tokio::pin!(budget);
    let mut last_error = ApiError::Timeout;
    for index in 0..pending.len() {
        tokio::select! {
            biased;
            outcome = &mut pending[index] => match outcome {
                Ok((status, body)) => match parse_doh_response(host, status, &body) {
                    Ok(resolved) => return Ok(resolved),
                    Err(error) => last_error = error,
                },
                Err(error) => last_error = ApiError::from(error),
            },
            _ = &mut budget => break,
        }
    }
    Err(last_error)
}

/// 查询串只允许常规主机名字符：不替未知输入做编码，宁可拒绝。
fn doh_query(endpoint: &str, host: &str) -> Option<String> {
    let clean = !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'));
    if !clean || check_hop(&Url::parse(endpoint).ok()?).is_err() {
        return None;
    }
    Some(format!("{endpoint}?name={host}&type=A"))
}

/// DoH 解析请求专用的池：与业务池彻底分开，也明确不经代理——Fake-IP 正是代理
/// 造成的，让同一套代理去解析解析器等于没绕开它。
fn resolver_client() -> ApiResult<reqwest::Client> {
    static CLIENT: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(4))
                .no_proxy()
                .build()
                .ok()
        })
        .clone()
        .ok_or_else(|| ApiError::Input("无法创建解析连接池".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(text: &str) -> IpAddr {
        text.parse().expect("valid address literal")
    }

    #[test]
    fn fake_ip_is_a_retry_signal_not_a_verdict() {
        assert_eq!(
            classify_address(address("198.18.0.1")),
            AddressVerdict::Spoofed
        );
        assert_eq!(
            classify_address(address("198.19.255.255")),
            AddressVerdict::Spoofed
        );
        // 相邻段各归各位：198.18/15 之外的公网照常放行，私网照常拦。
        assert_eq!(
            classify_address(address("198.20.0.1")),
            AddressVerdict::Public
        );
        assert_eq!(
            classify_address(address("127.0.0.1")),
            AddressVerdict::Blocked
        );
        assert_eq!(
            classify_address(address("10.1.2.3")),
            AddressVerdict::Blocked
        );
        assert_eq!(
            classify_address(address("169.254.169.254")),
            AddressVerdict::Blocked
        );
        assert_eq!(classify_address(address("::1")), AddressVerdict::Blocked);
        assert_eq!(
            classify_address(address("2001:4860:4860::8888")),
            AddressVerdict::Public
        );
    }

    #[test]
    fn one_private_answer_poisons_the_whole_set() {
        let mixed = vec![address("8.8.8.8"), address("127.0.0.1")];
        assert!(matches!(
            validate_addresses("a.com", &mixed),
            Err(ApiError::SecurityBlocked)
        ));
        let clean = vec![address("8.8.8.8"), address("8.8.8.8"), address("9.9.9.9")];
        let resolved = validate_addresses("a.com", &clean)
            .unwrap()
            .expect("public set");
        assert_eq!(
            resolved.addresses,
            vec![address("8.8.8.8"), address("9.9.9.9")]
        );
        assert_eq!(resolved.host, "a.com");
        assert!(
            validate_addresses("a.com", &[]).is_err(),
            "空答案不能当成可连"
        );
    }

    #[test]
    fn an_all_fake_ip_answer_asks_for_trusted_resolution() {
        let spoofed = vec![address("198.18.1.1"), address("198.19.9.9")];
        assert!(
            validate_addresses(OFFICIAL_HOST, &spoofed)
                .unwrap()
                .is_none()
        );
        // 混合场景：只要有一个真公网地址就用它，不必整组作废。
        let partly = vec![address("198.18.1.1"), address("8.8.8.8")];
        assert!(
            validate_addresses(OFFICIAL_HOST, &partly)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn doh_answers_are_accepted_only_when_every_row_is_public() {
        let ok = r#"{"Status":0,"Answer":[{"type":1,"data":"8.8.8.8","TTL":30},
            {"type":28,"data":"2001:4860:4860::8888","TTL":30}]}"#;
        let resolved = parse_doh_response("a.com", 200, ok).unwrap();
        assert_eq!(resolved.addresses.len(), 1, "只取 A 记录");
        assert!(resolved.fresh());

        for body in [
            r#"{"Status":2}"#,
            r#"{"Status":0}"#,
            r#"{"Status":0,"Answer":[]}"#,
            r#"{"Status":0,"Answer":[{"type":1,"data":"127.0.0.1","TTL":30}]}"#,
            r#"{"Status":0,"Answer":[{"type":1,"data":"10.0.0.1","TTL":30}]}"#,
            r#"{"Status":0,"Answer":[{"type":1,"data":"198.18.0.1","TTL":30}]}"#,
            r#"{"Status":0,"Answer":[{"type":1,"data":"not-an-ip","TTL":30}]}"#,
            "not json",
        ] {
            assert!(
                parse_doh_response("a.com", 200, body).is_err(),
                "must reject: {body}"
            );
        }
        assert!(
            parse_doh_response(
                "a.com",
                502,
                r#"{"Status":0,"Answer":[{"type":1,"data":"8.8.8.8"}]}"#
            )
            .is_err()
        );
    }

    #[test]
    fn hop_checks_reject_credentials_and_private_hosts() {
        for good in ["https://hongguoduanju.com/player/1", "http://8.8.8.8/a.mp4"] {
            assert!(check_hop(&Url::parse(good).unwrap()).is_ok(), "{good}");
        }
        for bad in [
            "http://127.0.0.1/a",
            "https://[::ffff:169.254.169.254]/a",
            "https://user:pw@hongguoduanju.com/",
            "https://a.localhost/a",
        ] {
            assert!(
                check_hop(&Url::parse(bad).unwrap()).is_err(),
                "{bad} should be blocked"
            );
        }
    }

    #[test]
    fn doh_query_only_carries_a_plain_hostname() {
        let endpoint = "https://dns.alidns.com/resolve";
        assert_eq!(
            doh_query(endpoint, OFFICIAL_HOST).as_deref(),
            Some("https://dns.alidns.com/resolve?name=hongguoduanju.com&type=A")
        );
        assert!(doh_query(endpoint, "a b.com").is_none());
        assert!(doh_query("http://127.0.0.1/dns-query", "a.com").is_none());
    }

    #[test]
    fn proxy_setting_only_accepts_a_usable_url() {
        assert!(is_usable_proxy("http://127.0.0.1:10808"));
        assert!(is_usable_proxy("socks5h://127.0.0.1:7890"));
        assert!(!is_usable_proxy("127.0.0.1:10808"));
        assert!(!is_usable_proxy("ftp://127.0.0.1/"));
    }

    #[test]
    fn pinned_pool_key_follows_the_address_set() {
        let make = |rows: &[&str]| Resolved {
            host: OFFICIAL_HOST.to_string(),
            addresses: rows.iter().map(|text| address(text)).collect(),
            expires_at: Instant::now() + PINNED_TTL,
        };
        assert_eq!(
            make(&["8.8.8.8", "9.9.9.9"]).key(),
            make(&["9.9.9.9", "8.8.8.8"]).key()
        );
        assert_ne!(make(&["8.8.8.8"]).key(), make(&["9.9.9.9"]).key());
    }
}
