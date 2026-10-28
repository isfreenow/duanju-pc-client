//! The two signed request builders: `signedVideoRequest` and `signedSearchRequest`.
//!
//! These are the only places where the signing primitives are assembled into an
//! actual HTTP request, so the ordering rules below are part of the contract
//! with the server and must match the reference exactly:
//!
//! * **Identity is minted per request and reused consistently.** `iid` and
//!   `device_id` are generated once, then feed both the query string and the
//!   Medusa payload. Generating them twice produces a signature over a query the
//!   server never sees.
//! * **`_rticket` jitter is not optional.** Branch 1 of `hashF13` produces a
//!   signature variant the server rejects, so the video path walks up to 32
//!   ticket offsets looking for a branch other than 1, and the search path walks
//!   up to 128 offsets looking specifically for branch 0.
//! * **Header evaluation order consumes the RNG in a fixed sequence** (gorgon's
//!   16-bit draw, then Helios, then Medusa's five draws). Changing the order
//!   changes the bytes.

use base64::Engine;

use super::f13::branch_of;
use super::gorgon::x_gorgon;
use super::helios::helios;
use super::medusa::{SignIdentity, build_medusa, md5_hex_upper};
use super::query::python_url_encode;
use super::rand::SignRng;
use super::register::RegisteredDevice;
use super::tables::{DEVICE_DEFAULT, PLAYBACK_UA, SEARCH_API, SEARCH_UA, VIDEO_API, VIDEO_UA};

/// A fully signed request, ready for the transport layer to send.
pub struct SignedRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

impl SignedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// Pagination state echoed back by the App search response.
#[derive(Default, Clone)]
pub struct SearchCursor {
    pub search_id: Option<String>,
    pub passback: Option<String>,
    pub next_offset: Option<String>,
}

/// `base` device fields, with the search profile's version overrides applied in
/// place. Spreading an object in JS keeps the original key order, so overrides
/// replace the value without moving the key to the end.
fn device_values(
    overrides: &[(&'static str, &str)],
    iid: &str,
    device_id: &str,
) -> Vec<(&'static str, String)> {
    let mut values = Vec::with_capacity(DEVICE_DEFAULT.len() + 2);
    for (key, default) in DEVICE_DEFAULT {
        let value = overrides
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| *value)
            .unwrap_or(*default);
        values.push((*key, value.to_string()));
    }
    values.push(("iid", iid.to_string()));
    values.push(("device_id", device_id.to_string()));
    values
}

/// `x-ladon` / `x-argus`: the same khronos timestamp, big-endian and
/// little-endian respectively.
fn ladon(khronos: u32) -> String {
    base64::engine::general_purpose::STANDARD.encode(super::bits::be32(khronos as i64))
}

fn argus(khronos: u32) -> String {
    base64::engine::general_purpose::STANDARD.encode(super::bits::le32(khronos as i64))
}

/// `playbackRequest(payload, identity)` — the video-model endpoint.
///
/// Unlike search, this endpoint authenticates on the **registered device
/// identity alone** (`device_id` / `iid` / `cdid` in the query) and needs none of
/// the gorgon/medusa/helios headers; the reference sends only UA, Accept and
/// Content-Type. Minting the ids locally instead of registering makes the gateway
/// answer 200 with an empty body, so `device` must come from [`register_device`].
pub fn playback_request(payload: &[u8], device: &RegisteredDevice, now_ms: i64) -> SignedRequest {
    let mut values: Vec<(&'static str, String)> = DEVICE_DEFAULT
        .iter()
        .map(|(key, value)| (*key, value.to_string()))
        .collect();
    values.push(("device_id", device.device_id.clone()));
    values.push(("iid", device.iid.clone()));
    values.push(("cdid", device.cdid.clone()));
    values.push(("_rticket", now_ms.to_string()));
    let query = python_url_encode(&values);
    let url = format!("{VIDEO_API}?{query}");
    SignedRequest {
        url,
        headers: vec![
            ("User-Agent".to_string(), PLAYBACK_UA.to_string()),
            (
                "Accept".to_string(),
                "application/json; charset=utf-8,application/x-protobuf".to_string(),
            ),
            (
                "Content-Type".to_string(),
                "application/json; charset=UTF-8".to_string(),
            ),
        ],
        body: Some(payload.to_vec()),
    }
}

/// `signedVideoRequest(payload)`.
///
/// `now_ms` and `process_started` are the reference's `Date.now()` calls, and
/// `process_nonce` is its module-level `PROCESS_NONCE`; both are injected so the
/// golden vectors can replay a deterministic request.
pub fn signed_video_request(
    payload: &[u8],
    rng: &SignRng,
    now_ms: i64,
    process_nonce: &str,
    process_started: i64,
) -> SignedRequest {
    let iid = rng.local_id();
    let device_id = rng.local_id();
    let khronos = (now_ms / 1000) as u32;
    let body = payload.to_vec();

    let mut values = device_values(&[], &iid, &device_id);
    values.push(("ts", khronos.to_string()));

    // branch 1 的签名变体服务端不认：抖动 _rticket 找一个可用的 branch（最多试 32 秒偏移）
    let mut ticket = now_ms;
    let mut query = String::new();
    let mut url = String::new();
    for offset in 0..32i64 {
        ticket = now_ms + offset;
        let mut attempt = values.clone();
        attempt.push(("_rticket", ticket.to_string()));
        query = python_url_encode(&attempt);
        url = format!("{VIDEO_API}?{query}");
        if branch_of(&url, Some(&body), khronos) != 1 {
            break;
        }
    }

    let identity = SignIdentity {
        iid: iid.clone(),
        device_id: device_id.clone(),
        version_code: "71332".to_string(),
        version_name: version_name(&values),
    };

    let random = rng.next_u16();
    let headers = vec![
        ("User-Agent".to_string(), VIDEO_UA.to_string()),
        (
            "Accept".to_string(),
            "application/json; charset=utf-8,application/x-protobuf".to_string(),
        ),
        (
            "Content-Type".to_string(),
            "application/json; charset=UTF-8".to_string(),
        ),
        ("x-xs-from-web".to_string(), "0".to_string()),
        ("x-ss-req-ticket".to_string(), ticket.to_string()),
        ("x-tt-request-tag".to_string(), "t=0;n=0".to_string()),
        ("sdk-version".to_string(), "2".to_string()),
        ("passport-sdk-version".to_string(), "50561".to_string()),
        (
            "x-vc-bdturing-sdk-version".to_string(),
            "3.7.2.cn".to_string(),
        ),
        ("x-ss-stub".to_string(), md5_hex_upper(&body)),
        (
            "x-gorgon".to_string(),
            x_gorgon(&query, Some(&body), khronos, random),
        ),
        ("x-khronos".to_string(), khronos.to_string()),
        ("x-ladon".to_string(), ladon(khronos)),
        ("x-argus".to_string(), argus(khronos)),
        ("x-helios".to_string(), helios(khronos, rng)),
        (
            "x-medusa".to_string(),
            build_medusa(
                &url,
                Some(&body),
                khronos,
                &identity,
                process_nonce,
                process_started,
                now_ms,
                rng,
            ),
        ),
        ("x-tt-dt".to_string(), String::new()),
    ];

    SignedRequest {
        url,
        headers,
        body: Some(body),
    }
}

/// `signedSearchRequest({ keyword, page, cursor })`.
pub fn signed_search_request(
    keyword: &str,
    page: i64,
    cursor: Option<&SearchCursor>,
    rng: &SignRng,
    now_ms: i64,
    process_nonce: &str,
    process_started: i64,
) -> Result<SignedRequest, String> {
    if !(1..=50).contains(&page) {
        return Err("App 搜索页码需为 1 至 50 的整数".to_string());
    }
    // Search uses the hongguoTV 7.2.2.32 profile and only hash branch 0.
    let overrides: [(&'static str, &str); 5] = [
        ("version_code", "72232"),
        ("version_name", "7.2.2.32"),
        ("manifest_version_code", "72232"),
        ("update_version_code", "72232"),
        ("pv_player", "72232"),
    ];
    let iid = rng.local_id();
    let device_id = rng.local_id();
    let khronos = (now_ms / 1000) as u32;

    let mut base = device_values(&overrides, &iid, &device_id);
    // `String(keyword).slice(0, 80)` counts UTF-16 code units; for CJK the
    // character count matches, which is the case that matters here.
    let clipped: String = keyword.chars().take(80).collect();
    base.push(("query", clipped));
    base.push(("count", "20".to_string()));
    base.push(("tab_type", "11".to_string()));
    base.push(("bookshelf_search_plan", "4".to_string()));
    base.push(("use_correct", "true".to_string()));
    base.push(("user_is_login", "0".to_string()));
    if let Some(cursor) = cursor {
        if let Some(search_id) = cursor
            .search_id
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            base.push(("search_id", search_id.to_string()));
        }
        if let Some(passback) = cursor.passback.as_deref().filter(|value| !value.is_empty()) {
            base.push(("passback", passback.to_string()));
        }
    }
    let offset = cursor
        .and_then(|value| parse_search_integer(value.next_offset.as_deref()))
        .unwrap_or((page - 1) * 20);
    base.push(("offset", offset.to_string()));

    let mut ticket = now_ms;
    let mut query = String::new();
    let mut url = String::new();
    for n in 0..128i64 {
        ticket = now_ms + n;
        let mut attempt = base.clone();
        attempt.push(("ts", khronos.to_string()));
        attempt.push(("_rticket", ticket.to_string()));
        query = python_url_encode(&attempt);
        url = format!("{SEARCH_API}?{query}");
        if branch_of(&url, None, khronos) == 0 {
            break;
        }
        if n == 127 {
            return Err("无法生成搜索请求签名".to_string());
        }
    }

    let identity = SignIdentity {
        iid: iid.clone(),
        device_id: device_id.clone(),
        version_code: "72232".to_string(),
        version_name: version_name(&base),
    };

    let random = rng.next_u16();
    let headers = vec![
        ("User-Agent".to_string(), SEARCH_UA.to_string()),
        (
            "Accept".to_string(),
            "application/json; charset=utf-8,application/x-protobuf".to_string(),
        ),
        ("x-ss-req-ticket".to_string(), ticket.to_string()),
        ("x-tt-request-tag".to_string(), "t=0;n=0".to_string()),
        ("sdk-version".to_string(), "2".to_string()),
        ("passport-sdk-version".to_string(), "50561".to_string()),
        ("x-xs-from-web".to_string(), "0".to_string()),
        ("x-khronos".to_string(), khronos.to_string()),
        ("x-ladon".to_string(), ladon(khronos)),
        ("x-argus".to_string(), argus(khronos)),
        (
            "x-gorgon".to_string(),
            x_gorgon(&query, None, khronos, random),
        ),
        ("x-helios".to_string(), helios(khronos, rng)),
        (
            "x-medusa".to_string(),
            build_medusa(
                &url,
                None,
                khronos,
                &identity,
                process_nonce,
                process_started,
                now_ms,
                rng,
            ),
        ),
        ("x-tt-dt".to_string(), String::new()),
    ];

    Ok(SignedRequest {
        url,
        headers,
        body: None,
    })
}

/// Read `version_name` back out of an assembled value list.
fn version_name(values: &[(&'static str, String)]) -> String {
    values
        .iter()
        .find(|(key, _)| *key == "version_name")
        .map(|(_, value)| value.clone())
        .unwrap_or_default()
}

/// `parseSearchInteger` — a non-negative safe integer, or `None`.
pub fn parse_search_integer(value: Option<&str>) -> Option<i64> {
    let text = value?.trim();
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let number: i64 = text.parse().ok()?;
    (number >= 0 && number < (1i64 << 53)).then_some(number)
}
