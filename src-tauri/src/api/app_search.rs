//! Signed App search (`hongguo-app-search.js`).
//!
//! The App's search tab returns the catalogue the website search does not, but it
//! is the request that needs the whole signing suite. Everything sensitive stays
//! in Rust: only the parsed cards and the server's opaque pagination cursor ever
//! reach the UI.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::device::parse_search_integer;
use crate::core::{signed_search_request, SearchCursor, SignRng};

use super::{ApiError, ApiResult, API_DOMAINS, Deadline, SeriesCard, now_millis, request_json};

const SEARCH_TAB: i64 = 11;
const TIMEOUT_SECS: u64 = 4;

/// The cursor the App echoes back for the next page. It is server-issued data,
/// not a signing secret, so it may round-trip through the UI.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchPageCursor {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub passback: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub results: Vec<SeriesCard>,
    pub page: i64,
    pub total: f64,
    pub total_pages: u32,
    pub has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_page: Option<i64>,
    pub channel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<SearchPageCursor>,
    /// Set when the App channel failed and the website answered instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// `cleanTitle` — tags and entities stripped, whitespace collapsed.
fn clean_title(value: Option<&Value>) -> String {
    let raw = match value {
        Some(Value::String(text)) => text.clone(),
        Some(other) if !other.is_null() => other.to_string(),
        _ => String::new(),
    };
    let mut out = String::with_capacity(raw.len());
    let mut inside_tag = false;
    for ch in raw.chars() {
        match ch {
            '<' => inside_tag = true,
            '>' => inside_tag = false,
            _ if !inside_tag => out.push(ch),
            _ => {}
        }
    }
    out = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `appCover` — the App returns HEIC variants Chromium may not decode, so those
/// become an empty string rather than a broken card.
fn app_cover(value: Option<&Value>) -> String {
    let url = value.and_then(Value::as_str).unwrap_or_default().trim();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return String::new();
    }
    let lowered = url.to_ascii_lowercase();
    let cut = lowered.find(['?', '#']).unwrap_or(lowered.len());
    if cut >= 5 && lowered[..cut].ends_with(".heic") {
        return String::new();
    }
    url.to_string()
}

fn to_cursor(cursor: Option<&SearchPageCursor>) -> Option<SearchCursor> {
    cursor.map(|value| SearchCursor {
        search_id: value.search_id.clone(),
        passback: value.passback.clone(),
        next_offset: value.next_offset.map(|number| number.to_string()),
    })
}

/// `parseSearchPayload`
pub fn parse_search_payload(payload: &Value, page: i64) -> ApiResult<SearchResult> {
    if !(1..=50).contains(&page) {
        return Err(ApiError::Input("App 搜索页码需为 1 至 50 的整数".to_string()));
    }
    if !payload.is_object() {
        return Err(ApiError::Business {
            message: "App 搜索返回空响应".to_string(),
            code: "APP_EMPTY".to_string(),
        });
    }
    let code = payload
        .get("code")
        .map(|value| match value {
            Value::Number(number) => number.as_i64().unwrap_or(0),
            Value::String(text) => text.trim().parse().unwrap_or(0),
            _ => 0,
        })
        .unwrap_or(0);
    if code != 0 {
        return Err(ApiError::Business {
            message: "App 搜索返回业务错误".to_string(),
            code: "APP_BUSINESS".to_string(),
        });
    }
    let tabs = payload
        .get("search_tabs")
        .or_else(|| payload.get("data").and_then(|data| data.get("search_tabs")));
    let Some(tabs) = tabs.and_then(Value::as_array) else {
        return Err(ApiError::Business {
            message: "App 搜索数据结构异常".to_string(),
            code: "APP_SCHEMA".to_string(),
        });
    };
    let Some(tab) = tabs
        .iter()
        .find(|item| item.get("tab_type").and_then(Value::as_i64) == Some(SEARCH_TAB))
    else {
        return Err(ApiError::Business {
            message: "App 搜索暂未返回短剧结果".to_string(),
            code: "APP_TAB_MISSING".to_string(),
        });
    };

    let mut seen: Vec<String> = Vec::new();
    let mut results = Vec::new();
    for cell in tab.get("data").and_then(Value::as_array).unwrap_or(&Vec::new()) {
        // Header/ad/search-suggestion cells have no video_data; their book_id is
        // not a drama.
        let video = cell
            .get("video_data")
            .and_then(Value::as_array)
            .and_then(|rows| {
                rows.iter().find(|item| {
                    matches!(
                        item.get("series_id"),
                        Some(Value::String(id)) if is_search_id(id)
                    ) || matches!(
                        item.get("series_id"),
                        Some(Value::Number(id)) if is_search_id(&id.to_string())
                    )
                })
            });
        let Some(video) = video else { continue };
        let series_id = video
            .get("series_id")
            .map(|value| match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default();
        let title = clean_title(
            video
                .get("search_high_light")
                .and_then(|highlight| highlight.get("title"))
                .and_then(|title| title.get("text"))
                .or_else(|| video.get("title"))
                .or_else(|| cell.get("cell_name")),
        );
        if title.is_empty() || seen.contains(&series_id) {
            continue;
        }
        seen.push(series_id.clone());
        let episode_count = video
            .get("episode_cnt")
            .or_else(|| video.get("episode_count"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        results.push(SeriesCard {
            series_id: series_id.clone(),
            series_title: title,
            cover: app_cover(video.get("cover")),
            episode_count,
            tags: Vec::new(),
            url: format!("https://hongguoduanju.com/detail?series_id={series_id}"),
            description: None,
            rank: None,
            heat: None,
            metadata: Default::default(),
        });
    }

    let has_more = tab.get("has_more").and_then(Value::as_bool) == Some(true) && page < 50;
    Ok(SearchResult {
        results,
        page,
        total: 0.0,
        total_pages: 0,
        has_more,
        next_page: has_more.then_some(page + 1),
        channel: "app".to_string(),
        note: None,
        cursor: Some(SearchPageCursor {
            search_id: tab
                .get("search_id")
                .map(|value| clean_title(Some(value)))
                .filter(|value| !value.is_empty()),
            passback: tab
                .get("passback")
                .map(|value| clean_title(Some(value)))
                .filter(|value| !value.is_empty()),
            next_offset: tab
                .get("next_offset")
                .map(as_text)
                .and_then(|text| parse_search_integer(Some(text.as_str()))),
        }),
    })
}

/// `String(value)` for a JSON scalar, without the quotes `Value::to_string` adds.
fn as_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// `/^\d{1,30}$/`
fn is_search_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 30
        && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// `searchApp` — one signed request against the App search tab.
pub async fn search_app(
    client: &reqwest::Client,
    keyword: &str,
    page: i64,
    cursor: Option<&SearchPageCursor>,
) -> ApiResult<SearchResult> {
    let keyword = clean_title(Some(&Value::String(keyword.to_string())));
    if keyword.is_empty() || keyword.chars().count() > 80 {
        return Err(ApiError::Input("App 搜索词无效".to_string()));
    }
    if !(1..=50).contains(&page) {
        return Err(ApiError::Input("App 搜索页码需为 1 至 50 的整数".to_string()));
    }
    let has_cursor = cursor
        .map(|value| {
            value.search_id.as_deref().is_some_and(|v| !v.is_empty())
                || value.passback.as_deref().is_some_and(|v| !v.is_empty())
                || value.next_offset.is_some()
        })
        .unwrap_or(false);
    if page > 1 && !has_cursor {
        return Err(ApiError::Business {
            message: "App 搜索游标已失效，请重新搜索".to_string(),
            code: "APP_CURSOR".to_string(),
        });
    }

    let deadline = Deadline::after(TIMEOUT_SECS);
    if deadline.remaining().is_none() {
        return Err(ApiError::Business {
            message: "App 搜索超时，已回退官网".to_string(),
            code: "APP_TIMEOUT".to_string(),
        });
    }

    let identity = crate::state::ProcessIdentity::current();
    let rng = SignRng::system();
    let signed = signed_search_request(
        &keyword,
        page,
        to_cursor(cursor).as_ref(),
        &rng,
        now_millis(),
        &identity.nonce,
        identity.started_ms,
    )
    .map_err(|_| ApiError::Input("App 搜索词无效".to_string()))?;

    let payload = request_json(
        client,
        &signed.url,
        &signed.headers,
        signed.body.as_deref(),
        API_DOMAINS,
        deadline,
    )
    .await
    .map_err(|error| match error {
        ApiError::Timeout => ApiError::Business {
            message: "App 搜索超时，已回退官网".to_string(),
            code: "APP_TIMEOUT".to_string(),
        },
        ApiError::Network(_) | ApiError::InvalidResponse => ApiError::Business {
            message: "App 搜索暂不可用，已回退官网".to_string(),
            code: "APP_NETWORK".to_string(),
        },
        other => other,
    })?;

    parse_search_payload(&payload, page)
}
