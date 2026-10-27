//! Signed App play-address resolution (`fetchAppPlayUrl` in `app-source.js`).
//!
//! The App returns one or more candidate streams per episode. Each candidate may
//! carry a `spade_a` blob (the AES key material for the media) and its URL may
//! itself be an encrypted spade URL. **Deriving the key and recovering the URL
//! stay in Rust** — the spade tables, the popcount recurrence and the endpoints
//! never reach the webview. What crosses the boundary is only the resolved stream
//! for the episode the user just asked to play: its address and its key, which a
//! browser-side decryptor needs by design. Measurements on a real episode: a
//! progressive MP4 (`moov` first) whose samples are CENC-encrypted whole, with a
//! per-sample 16-byte IV listed in `senc`, so WebCrypto AES-CTR undoes it.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::core;
use crate::core::spade::{base64_lenient, clean_url, decrypt_spade_url, derive_content_key};
use crate::core::tables::VIDEO_UA;

use super::{
    ApiError, ApiResult, API_DOMAINS, Deadline, MEDIA_DOMAINS, now_millis, request_json,
};

const TIMEOUT_SECS: u64 = 12;

/// One resolved stream: the play URL plus the hex AES-128 key for its samples.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaVariant {
    pub url: String,
    pub content_key: Option<String>,
    pub codec: String,
    pub quality: String,
    pub width: u32,
    pub height: u32,
    pub source: String,
    pub vid: String,
    pub user_agent: String,
    pub referer: Option<String>,
}

/// `str(value)` in the reference.
fn text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.trim().to_string(),
        Some(other) => other.to_string().trim().to_string(),
    }
}

fn number(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::Number(number)) => number.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(text)) => text.trim().parse().unwrap_or(f64::NAN),
        _ => f64::NAN,
    }
}

/// `jsonBody(value)`
fn json_body(value: &str) -> Option<Value> {
    serde_json::from_str(value).ok()
}

/// `parseFallback(value)`
fn parse_fallback(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => {
            let trimmed = text.trim();
            match json_body(trimmed) {
                Some(parsed) => text_from(&parsed["fallback_api"]).unwrap_or_else(|| trimmed.to_string()),
                None => trimmed.to_string(),
            }
        }
        Some(Value::Array(rows)) => rows
            .first()
            .map(|item| text_from(item).unwrap_or_default())
            .unwrap_or_default(),
        Some(other) => text_from(&other["fallback_api"]).unwrap_or_default(),
        None => String::new(),
    }
}

fn text_from(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.trim().to_string()),
        Value::Null => None,
        other => Some(other.to_string().trim().to_string()),
    }
}

/// `Number(value?.Code || value?.code || 0) !== 0`
fn rejected(payload: &Value) -> bool {
    for key in ["Code", "code"] {
        let value = number(payload.get(key));
        if value.is_finite() && value != 0.0 {
            return true;
        }
    }
    false
}

fn codec_rank(codec: &str) -> u8 {
    if ["h264", "avc", "avc1"].contains(&codec) {
        0
    } else if ["hevc", "h265", "hvc1", "hev1", "bytevc1"].contains(&codec) {
        1
    } else {
        2
    }
}

/// `fetchAppPlayUrl(vid)` in auto-quality mode: prefer H.264, then the largest
/// picture. Download-oriented quality floors are out of scope for v1.
pub async fn fetch_app_play_url(client: &reqwest::Client, vid: &str) -> ApiResult<Vec<MediaVariant>> {
    if vid.is_empty() || vid.len() > 30 || !vid.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ApiError::Input("分集编号无效".to_string()));
    }
    let deadline = Deadline::after(TIMEOUT_SECS);

    let payload = json!({
        "biz_param": {
            "detail_page_version": 0,
            "device_level": 3,
            "disable_digg_stat": false,
            "need_all_video_definition": true,
            "need_mp4_align": false,
            "use_os_player": false,
            "use_server_dns": false,
            "video_platform": 1024
        },
        "mixed_video_id_map": { "1004": [vid] }
    })
    .to_string();

    let device = core::playback_device(client).await?;
    let signed = core::playback_request(payload.as_bytes(), device, now_millis());

    let response = request_json(
        client,
        &signed.url,
        &signed.headers,
        Some(signed.body.as_deref().unwrap_or_default()),
        API_DOMAINS,
        deadline,
    )
    .await?;
    if rejected(&response) {
        return Err(ApiError::Business {
            message: "App 接口暂未提供本集播放信息".to_string(),
            code: "BUSINESS_REJECTED".to_string(),
        });
    }

    let entry = response.get("data").and_then(|data| data.get(vid));
    let model = match entry.and_then(|item| item.get("video_model")) {
        Some(Value::String(raw)) => json_body(raw),
        Some(Value::Object(_)) => entry.and_then(|item| item.get("video_model")).cloned(),
        _ => None,
    };
    let Some(model) = model else {
        let had_model = entry
            .and_then(|item| item.get("video_model"))
            .is_some_and(|value| !value.is_null());
        return Err(ApiError::Business {
            message: "App 接口未返回所选分集，已停止取址".to_string(),
            code: if had_model { "INVALID_RESPONSE" } else { "MISSING_MODEL" }.to_string(),
        });
    };

    let fallback = parse_fallback(model.get("fallback_api"));
    if fallback.is_empty() {
        return Err(ApiError::Business {
            message: "App 接口未返回本集片源地址".to_string(),
            code: "MISSING_MODEL".to_string(),
        });
    }
    let outer = request_json(
        client,
        &fallback,
        &[("User-Agent".to_string(), VIDEO_UA.to_string())],
        None,
        API_DOMAINS,
        deadline,
    )
    .await?;
    if rejected(&outer)
        || outer
            .get("video_info")
            .map(|info| rejected(info))
            .unwrap_or(false)
    {
        return Err(ApiError::Business {
            message: "App 接口暂未提供本集播放信息".to_string(),
            code: "BUSINESS_REJECTED".to_string(),
        });
    }
    let Some(info) = outer.get("video_info").and_then(|value| value.get("data")) else {
        return Err(ApiError::Business {
            message: "App 接口未返回本集片源模型".to_string(),
            code: "MISSING_MODEL".to_string(),
        });
    };

    // Candidate rows, keeping only codecs the reference considers playable.
    let mut rows: Vec<Row> = info
        .get("video_list")
        .and_then(Value::as_object)
        .map(|rows| rows.values().cloned().collect::<Vec<Value>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|item| {
            item.is_object()
                && (!text(item.get("main_url")).is_empty()
                    || !text(item.get("backup_url")).is_empty()
                    || item.get("backup_url_list").and_then(Value::as_array).is_some())
        })
        .map(Row::from_item)
        .filter(|row| row.codec_rank < 2)
        .collect();
    if rows.is_empty() {
        return Err(ApiError::Business {
            message: "本集暂未提供兼容的 H.264/HEVC 片源".to_string(),
            code: "NO_PLAYABLE_SOURCE".to_string(),
        });
    }
    // Auto: lowest codec rank, then the largest picture.
    rows.sort_by(|a, b| {
        a.codec_rank
            .cmp(&b.codec_rank)
            .then(b.pixels.cmp(&a.pixels))
            .then_with(|| a.quality.cmp(&b.quality))
    });

    let key_seed = info
        .get("key_seed")
        .and_then(Value::as_str)
        .map(|value| base64_lenient(value));
    let mut variants: Vec<MediaVariant> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut last_error: Option<ApiError> = None;

    for selected in &rows {
        let spade = selected
            .item
            .get("spade_a")
            .or_else(|| selected.item.get("encrypt_info").and_then(|info| info.get("spade_a")));
        let content_key = match spade.map(|value| text(Some(value))) {
            Some(value) if !value.is_empty() => Some(
                derive_content_key(&value)
                    .ok()
                    .map(|bytes| hex::encode(bytes))
                    .unwrap_or_default(),
            ),
            _ => None,
        };
        let content_key = content_key.filter(|value| !value.is_empty());

        let candidates = selected.candidates();
        for candidate in candidates {
            // `info.key_seed ? decryptSpadeUrl(candidate, seed) : cleanUrl(candidate)`
            let media = match &key_seed {
                Some(seed) => match decrypt_spade_url(&candidate, seed) {
                    Ok(value) => value,
                    Err(_) => continue,
                },
                None => clean_url(&candidate),
            };
            let media = media.trim().to_string();
            match super::checked_url(&media, MEDIA_DOMAINS) {
                Ok(url) => {
                    let identity_key = format!("{}:{}", url.as_str(), content_key.clone().unwrap_or_default());
                    if seen.contains(&identity_key) {
                        continue;
                    }
                    seen.push(identity_key);
                    variants.push(MediaVariant {
                        url: url.into(),
                        content_key: content_key.clone(),
                        codec: if selected.codec == "bytevc1" { "hevc".to_string() } else { selected.codec.clone() },
                        quality: selected.quality.clone(),
                        width: selected.width,
                        height: selected.height,
                        source: "app".to_string(),
                        vid: vid.to_string(),
                        user_agent: VIDEO_UA.to_string(),
                        referer: None,
                    });
                }
                Err(error) => last_error = Some(error),
            }
        }
    }

    if variants.is_empty() {
        return Err(last_error.unwrap_or_else(|| ApiError::Business {
            message: "本集片源地址或密钥无法解析".to_string(),
            code: "INVALID_RESPONSE".to_string(),
        }));
    }
    Ok(variants)
}

struct Row {
    item: Value,
    codec: String,
    codec_rank: u8,
    quality: String,
    pixels: u64,
    width: u32,
    height: u32,
}

impl Row {
    fn from_item(item: Value) -> Self {
        let meta = item.get("video_meta").cloned().unwrap_or(Value::Null);
        let codec = text(
            item.get("codec_type")
                .or_else(|| meta.get("codec_type")),
        )
        .to_lowercase();
        let codec_rank = codec_rank(&codec);
        let raw_quality = [
            item.get("quality_desc"),
            item.get("definition"),
            item.get("height"),
            item.get("vheight"),
            meta.get("definition"),
        ]
        .into_iter()
        .flatten()
        .map(|value| text(Some(value)))
        .find(|value| !value.is_empty())
        .unwrap_or_default();
        // The first pair of finite, positive dimensions wins, exactly as in the
        // reference's four candidate pairs.
        let pairs = [
            [number(item.get("vwidth")), number(item.get("vheight"))],
            [number(item.get("width")), number(item.get("height"))],
            [number(meta.get("vwidth")), number(meta.get("vheight"))],
            [number(meta.get("width")), number(meta.get("height"))],
        ];
        let (width, height) = pairs
            .iter()
            .find(|pair| pair.iter().all(|value| value.is_finite() && *value > 0.0))
            .map(|pair| (pair[0] as u32, pair[1] as u32))
            .unwrap_or((0, 0));
        let digits: String = raw_quality
            .chars()
            .filter(|ch| ch.is_ascii_digit())
            .collect();
        let fallback_pixels = digits.parse::<u64>().unwrap_or(0);
        let pixels = if width != 0 && height != 0 {
            width.min(height) as u64
        } else {
            fallback_pixels
        };
        Row {
            item,
            codec,
            codec_rank,
            quality: raw_quality,
            pixels,
            width,
            height,
        }
    }

    /// `main_url`, `backup_url`, then every entry of `backup_url_list`.
    fn candidates(&self) -> Vec<String> {
        let mut out = Vec::new();
        for key in ["main_url", "backup_url"] {
            let value = text(self.item.get(key));
            if !value.is_empty() {
                out.push(value);
            }
        }
        if let Some(rows) = self.item.get("backup_url_list").and_then(Value::as_array) {
            for row in rows {
                let value = text(Some(row));
                if !value.is_empty() {
                    out.push(value);
                }
            }
        }
        out
    }
}
