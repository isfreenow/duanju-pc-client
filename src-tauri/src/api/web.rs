//! Website (unsigned) data source: catalogue pages, rankings, episode lists and
//! public ratings/reviews.
//!
//! `hongguoduanju.com` renders its browse and player pages on the server, so the
//! data lives in a JSON blob in the HTML and needs no signing. This module is the
//! Rust port of `catalog.js`, `public-details.js`, `fetchEpisodeListFromWeb` and
//! `fetchPlayUrlFromWeb`.
//!
//! The ranking page is plain server-rendered markup, and the shapes needed here
//! (`<script data-fn-name=… data-fn-args=…>`, `<article>` cards, `<nav>` pagination
//! and the `application/ld+json` VideoObject) are stable. They are scanned by hand
//! instead of pulling a regex engine into a crate that also carries the signing
//! code, with the reference's patterns written out beside each helper.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures_util::stream::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use super::router::{as_u64, decode_html, read_router_data, trimmed, visible_text};
use super::{
    ApiError, ApiResult, Deadline, PublicMetadata, find_sub, is_js_space, is_public_media_url,
};

const SITE: &str = "https://hongguoduanju.com";
/// The transport headers the playback handler must reuse for website media.
pub const WEB_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36";
pub const WEB_REFERER: &str = "https://hongguoduanju.com/";
const TIMEOUT_SECS: u64 = 10;
const PLAY_TIMEOUT_SECS: u64 = 4;
// 分集页竞速的两档预算：第一档短，健康的页早回来了就不再加压；第二档放宽，
// 并补一条普通解析的退路。
const EPISODE_FIRST_SECS: u64 = 2;
const EPISODE_RETRY_SECS: u64 = 4;
const MAX_PAGE_BYTES: usize = 8 * 1024 * 1024;
// 分类页与热播页共用同一份目录缓存；30 秒过短会让每次切页都直连来源。
const CATALOG_CACHE: Duration = Duration::from_secs(120);
const DETAILS_CACHE: Duration = Duration::from_secs(30);

const CATEGORIES: &[&str] = &["real-drama", "comic-drama", "ai-drama", "comic"];
const RANKINGS: &[&str] = &[
    "hot-drama",
    "hot-real-drama",
    "hot-comic-drama",
    "hot-ai-drama",
];

fn is_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// `/^\d{6,}$/` — the site never lists a series with a shorter id.
fn is_series_id(value: &str) -> bool {
    is_digits(value) && value.len() >= 6
}

/// `/^[a-z0-9-]+$/`
fn is_slug(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// `count(value)` — a finite non-negative number, `''`/`null` excluded.
fn count(value: Option<&Value>) -> Option<f64> {
    let number = match value? {
        Value::Null => return None,
        Value::Bool(flag) => f64::from(*flag),
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => {
            if text.is_empty() {
                return None;
            }
            text.trim().parse::<f64>().ok()?
        }
        _ => return None,
    };
    (number.is_finite() && number >= 0.0).then_some(number)
}

// ── DTOs ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Genre {
    pub slug: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogPage {
    pub results: Vec<super::SeriesCard>,
    pub page: u32,
    pub total: f64,
    pub total_pages: u32,
    pub genres: Vec<Genre>,
    pub page_title: String,
    pub source_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsupported: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub empty_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Episode {
    pub vid: String,
    pub vid_index: u32,
    pub title: String,
    pub series_id: String,
    pub series_title: String,
    pub cover: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_available: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EpisodeList {
    pub series_id: String,
    pub series_title: String,
    pub cover: String,
    pub total: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_accessible_episodes: Option<u32>,
    pub episodes: Vec<Episode>,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Review {
    pub author: String,
    pub avatar: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub likes: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewBlock {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    pub items: Vec<Review>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicDetails {
    pub series_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode_id: Option<String>,
    #[serde(flatten)]
    pub metadata: PublicMetadata,
    /// 剧情简介：官网取 seriesDetail.series_intro，河马/西饭各取自己的简介字段。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub rating: Option<f64>,
    pub rating_count: Option<f64>,
    pub like_count: Option<f64>,
    pub favorite_count: Option<f64>,
    pub reviews: ReviewBlock,
    pub episode_reviews: Option<ReviewBlock>,
    pub episode_like_count: Option<f64>,
    pub source_url: String,
}

/// `fetchPlayUrlFromWeb` result, minus anything the webview must not see.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicPlaySource {
    pub url: String,
    pub source: String,
    pub vid: String,
}

// ── Cache ────────────────────────────────────────────────────────────────

/// Parsed pages, keyed by source URL, with the reference's 120 s / 30 s windows.
/// (The reference also coalesces concurrent requests per URL; a single desktop
/// user does not produce enough overlap to need the in-flight map.)
fn cache() -> &'static Mutex<HashMap<String, (std::time::Instant, Value)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (std::time::Instant, Value)>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn cached(key: &str, ttl: Duration) -> Option<Value> {
    let guard = cache().lock().ok()?;
    let (created, value) = guard.get(key)?;
    (created.elapsed() < ttl).then(|| value.clone())
}

fn store(key: &str, value: &Value) {
    if let Ok(mut guard) = cache().lock() {
        // Bound memory while retaining the latest pages.
        if guard.len() > 64 {
            let oldest = guard
                .iter()
                .min_by_key(|(_, (created, _))| *created)
                .map(|(key, _)| key.clone());
            if let Some(key) = oldest {
                guard.remove(&key);
            }
        }
        guard.insert(key.to_string(), (std::time::Instant::now(), value.clone()));
    }
}

// ── Catalogue and rankings ───────────────────────────────────────────────

/// `browseList` — category pages and ranking pages share one entry point.
pub async fn browse_list(
    client: &reqwest::Client,
    category: &str,
    genre: &str,
    page: u32,
    force_refresh: bool,
) -> ApiResult<CatalogPage> {
    let ranking = RANKINGS.contains(&category);
    if !CATEGORIES.contains(&category) && !ranking {
        return Err(ApiError::Input("无效的目录分类或题材".to_string()));
    }
    if !genre.is_empty() && (!is_slug(genre) || ranking) {
        return Err(ApiError::Input("无效的目录分类或题材".to_string()));
    }
    let page = page.max(1);
    let url = format!(
        "{SITE}/{}/{category}{}{}",
        if ranking { "rank" } else { "category" },
        if genre.is_empty() {
            String::new()
        } else {
            format!("/{genre}")
        },
        if page > 1 {
            format!("?page={page}")
        } else {
            String::new()
        }
    );

    if !force_refresh {
        if let Some(value) = cached(&url, CATALOG_CACHE) {
            if let Ok(page) = serde_json::from_value::<CatalogPage>(value) {
                return Ok(page);
            }
        }
    }

    let html = fetch_site_html(client, &url, TIMEOUT_SECS, super::FetchKind::Site).await?;
    let page_data = if ranking {
        parse_ranking(&html, category, page, &url)
    } else {
        parse_catalog(&html, category, page, &url)
    }?;
    if let Ok(value) = serde_json::to_value(&page_data) {
        store(&url, &value);
    }
    Ok(page_data)
}

async fn fetch_site_html(
    client: &reqwest::Client,
    url: &str,
    secs: u64,
    kind: super::FetchKind,
) -> ApiResult<String> {
    let deadline = Deadline::after(secs);
    super::fetch_text(
        client,
        url,
        &[
            ("User-Agent", WEB_UA),
            (
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            ),
            ("Accept-Language", "zh-CN,zh;q=0.9"),
            ("Referer", WEB_REFERER),
            ("Cache-Control", "no-cache"),
        ],
        kind,
        deadline,
        MAX_PAGE_BYTES,
    )
    .await
}

/// `parseCatalog` — `recommendList` plus the genre selector rows.
pub fn parse_catalog(
    html: &str,
    category: &str,
    page: u32,
    source_url: &str,
) -> ApiResult<CatalogPage> {
    let router = read_router_data(html).map_err(ApiError::Input)?;
    let data = router
        .get("loaderData")
        .and_then(Value::as_object)
        .and_then(|rows| {
            rows.values().find(|row| {
                row.get("categoryRoute")
                    .and_then(|route| route.get("contentType"))
                    .and_then(Value::as_str)
                    == Some(category)
            })
        });
    let invalid = || ApiError::Input("来源目录暂不可用，请重试或打开来源页面".to_string());
    let Some(data) = data else {
        return Err(invalid());
    };
    if data.get("isSuccess").and_then(Value::as_bool) == Some(false) {
        return Err(invalid());
    }
    let page_title = strip_brand(&trimmed(data.get("seo").map(|seo| &seo["title"])));

    // Comics are image reading, not video: report them as unsupported rather
    // than pretending a player can open them.
    if category == "comic" {
        return Ok(CatalogPage {
            results: Vec::new(),
            page: 1,
            total: 0.0,
            total_pages: 0,
            genres: Vec::new(),
            page_title,
            source_url: source_url.to_string(),
            unsupported: Some(true),
            empty_message: Some(
                "漫画属于图文阅读，暂不支持视频播放或下载；请切换「漫剧」或打开来源页面阅读。"
                    .to_string(),
            ),
        });
    }

    let Some(list) = data.get("recommendList").and_then(Value::as_array) else {
        return Err(invalid());
    };

    let mut seen = HashSet::new();
    let mut results = Vec::new();
    for item in list {
        let id = trimmed(item.get("series_id"));
        let title = item
            .get("series_name")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if !is_series_id(&id) || title.is_empty() || seen.contains(&id) {
            continue;
        }
        seen.insert(id.clone());
        results.push(super::SeriesCard {
            series_id: id.clone(),
            series_title: title.to_string(),
            cover: trimmed(item.get("series_cover")),
            episode_count: as_u64(item.get("episode_cnt")).unwrap_or(0) as u32,
            tags: string_list(item.get("tags"), 4),
            url: format!("{SITE}/detail?series_id={id}"),
            description: None,
            rank: None,
            heat: None,
            metadata: PublicMetadata::from_item(item),
        });
    }

    let genres = data
        .get("selectorList")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .flat_map(|row| {
                    row.get("items")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default()
                })
                .filter_map(|item| {
                    let slug = trimmed(item.get("selector_item_id"));
                    let label = item
                        .get("show_name")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    (is_slug(&slug) && !label.is_empty()).then_some(Genre {
                        slug,
                        label: label.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let number = |key: &str| -> f64 {
        data.get("pagination")
            .and_then(|pagination| count(pagination.get(key)))
            .unwrap_or(0.0)
    };
    Ok(CatalogPage {
        results,
        page: {
            let value = number("pageNum");
            if value > 0.0 { value as u32 } else { page }
        },
        total: number("total"),
        total_pages: number("totalPages") as u32,
        genres,
        page_title,
        source_url: source_url.to_string(),
        unsupported: None,
        empty_message: None,
    })
}

fn string_list(value: Option<&Value>, limit: usize) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item.as_str()
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                        .map(str::to_string)
                })
                .take(limit)
                .collect()
        })
        .unwrap_or_default()
}

/// `parseRanking` — the resolved loader payload when the page ships one, and the
/// rendered `<article>` cards as a fallback for streaming responses.
pub fn parse_ranking(
    html: &str,
    category: &str,
    page: u32,
    source_url: &str,
) -> ApiResult<CatalogPage> {
    let mut data: Option<Value> = None;
    // /<script\b([^>]*)>/gi with data-fn-name="mergeLoaderData"
    for head in script_heads(html) {
        if attribute(head, "data-fn-name").as_deref() != Some("mergeLoaderData") {
            continue;
        }
        // /data-fn-args=(?:"([^"]*)"|'([^']*)')/
        let Some(encoded) = attribute(head, "data-fn-args") else {
            continue;
        };
        // Some streamed responses expose only their rendered cards.
        let Ok(args) = serde_json::from_str::<Vec<Value>>(&decode_html(&encoded)) else {
            continue;
        };
        if args.first().and_then(Value::as_str) != Some(&format!("rank_{category}/page")) {
            continue;
        }
        let Some(list) = args.get(1).and_then(Value::as_array) else {
            continue;
        };
        let Some(raw) = list
            .iter()
            .find(|item| {
                item.get("key").and_then(Value::as_str) == Some("content")
                    && item.get("routerDataFnName").and_then(Value::as_str) == Some("p")
            })
            .and_then(|item| item.get("routerDataFnArgs"))
            .and_then(Value::as_array)
            .and_then(|rows| rows.first())
            .and_then(Value::as_str)
        else {
            continue;
        };
        if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
            data = Some(parsed);
        }
    }

    let mut seen = HashSet::new();
    let mut results: Vec<super::SeriesCard> = Vec::new();
    let usable = data
        .as_ref()
        .map(|value| value.get("isSuccess").and_then(Value::as_bool) != Some(false))
        .unwrap_or(false);
    if usable {
        if let Some(rows) = data
            .as_ref()
            .and_then(|value| value.get("rankList"))
            .and_then(Value::as_array)
        {
            for item in rows {
                if let Some(card) = normalize_rank_item(item, &mut seen) {
                    results.push(card);
                }
            }
        }
    }
    if results.is_empty() {
        // Some pages omit the resolved loader. Semantic headings and detail links
        // remain in the SSR HTML.
        for article in extract_articles(html) {
            if let Some(card) = normalize_article_card(&article, &mut seen) {
                results.push(card);
            }
        }
    }
    if results.is_empty() {
        return Err(ApiError::Input(
            "来源榜单暂不可用，请重试或打开来源页面".to_string(),
        ));
    }

    // /<nav\b[^>]*aria-label=["']榜单分页["'][^>]*>([\s\S]*?)<\/nav>/i
    let nav = find_element(html, "nav", 0, |head| {
        matches!(attribute(head, "aria-label").as_deref(), Some("榜单分页"))
    })
    .and_then(|(_, inner_start, _)| slice_to(html, inner_start, "</nav>"))
    .unwrap_or_default();

    let current = data
        .as_ref()
        .and_then(|value| value.get("pagination"))
        .and_then(|pagination| count(pagination.get("pageNum")))
        .filter(|value| *value > 0.0)
        .map(|value| value as u32)
        // /aria-current=["']page["'][^>]*>(\d+)/
        .or_else(|| aria_current_page(&nav))
        .unwrap_or(page);

    let pages: Vec<f64> = extract_nav_pages(&nav, &format!("/rank/{category}"));
    let max_page = pages.iter().cloned().fold(current as f64, f64::max);
    let total_pages = data
        .as_ref()
        .and_then(|value| value.get("pagination"))
        .and_then(|pagination| count(pagination.get("totalPages")))
        .filter(|value| *value > 0.0)
        .unwrap_or(max_page) as u32;

    let page_title = strip_brand(
        &find_element(html, "title", 0, |_| true)
            .and_then(|(_, inner_start, _)| slice_to(html, inner_start, "</title>"))
            .map(|raw| normalize_text(&raw))
            .unwrap_or_default(),
    );

    Ok(CatalogPage {
        results,
        page: current,
        total: 0.0,
        total_pages,
        genres: Vec::new(),
        page_title,
        source_url: source_url.to_string(),
        unsupported: None,
        empty_message: None,
    })
}

/// `text(value)` from the ranking parser: tags become a single space, entities
/// decode, then whitespace collapses.
fn normalize_text(html: &str) -> String {
    visible_text(html)
}

/// Product copy must not carry the upstream brand, and page titles should be
/// the plain section name. The site's seo/<title> is a marketing string like
/// "热播榜_热门短剧排行榜 - 官网" or "爱情短剧在线观看 -", so we drop the brand and
/// keep only the core name: the leading segment before the first SEO separator
/// ("_"), minus any trailing "在线观看" marketing and dangling separators.
fn strip_brand(text: &str) -> String {
    let out = text.replace("红果短剧", "").replace("红果", "");
    let head = out.split('_').next().unwrap_or("");
    let core = trim_seps(head).trim_end_matches("在线观看");
    trim_seps(core).to_string()
}

/// Trim surrounding whitespace and dangling SEO separators from a title edge.
fn trim_seps(s: &str) -> &str {
    let is_sep = |c: char| matches!(c, '-' | '–' | '—' | '|' | '·' | '｜');
    s.trim()
        .trim_start_matches(is_sep)
        .trim_end_matches(is_sep)
        .trim()
}

fn normalize_rank_item(item: &Value, seen: &mut HashSet<String>) -> Option<super::SeriesCard> {
    let id = trimmed(item.get("seriesId"));
    let title = item
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let rank = count(item.get("rank")).filter(|value| *value >= 1.0 && *value < 9e15)? as u32;
    if !is_series_id(&id) || title.is_empty() || seen.contains(&id) {
        return None;
    }
    seen.insert(id.clone());
    Some(super::SeriesCard {
        series_id: id.clone(),
        series_title: title,
        cover: item
            .get("cover")
            .and_then(Value::as_str)
            .filter(|value| value.starts_with("http://") || value.starts_with("https://"))
            .unwrap_or_default()
            .to_string(),
        episode_count: item
            .get("episodeVids")
            .and_then(Value::as_array)
            .map(|rows| rows.len() as u32)
            .unwrap_or(0),
        tags: string_list(item.get("tags"), 4),
        url: format!("{SITE}/detail?series_id={id}"),
        description: None,
        rank: Some(rank),
        heat: Some(
            item.get("heatText")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string(),
        ),
        metadata: PublicMetadata::from_item(item),
    })
}

/// The article fallback rebuilds the same fields the loader payload would carry:
/// `<h2 id="rank-title-<digits>">`, an `<img src>`, a `pc-categories-` paragraph,
/// a digits-only `<span>` for the rank and the heat phrase in the card's text.
fn normalize_article_card(article: &str, seen: &mut HashSet<String>) -> Option<super::SeriesCard> {
    // /<h2\b[^>]*\bid=["']rank-title-(\d+)["'][^>]*>([\s\S]*?)<\/h2>/i
    let (head, inner_start, _) = find_element(article, "h2", 0, |head| {
        attribute(head, "id")
            .map(|value| value.starts_with("rank-title-"))
            .unwrap_or(false)
    })?;
    let id = attribute(head, "id")?["rank-title-".len()..].to_string();
    if !is_digits(&id) || id.is_empty() {
        return None;
    }
    let inner = slice_to(article, inner_start, "</h2")?;
    let title = normalize_text(&inner);
    if !article.contains(&format!("/detail?series_id={id}")) {
        return None;
    }

    // /<img\b[^>]*\bsrc=(?:"([^"]*)"|'([^']*)')/i
    let cover = find_element(article, "img", 0, |head| attribute(head, "src").is_some())
        .map(|(head, _, _)| head)
        .and_then(|head| attribute(head, "src"))
        .map(|value| decode_html(&value))
        .unwrap_or_default();

    // /<p\b[^>]*class=["'][^"']*pc-categories-[^"']*["'][^>]*>([\s\S]*?)<\/p>/i
    let tags = find_element(article, "p", 0, |head| {
        attribute(head, "class")
            .map(|value| value.contains("pc-categories-"))
            .unwrap_or(false)
    })
    .map(|(_, inner_start, _)| slice_to(article, inner_start, "</p").unwrap_or_default())
    .map(|body| {
        let mut out = Vec::new();
        let mut from = 0usize;
        // /<span\b[^>]*>([\s\S]*?)<\/span>/gi
        while let Some((_, inner_start, next)) = find_element(&body, "span", from, |_| true) {
            let Some(end) = find_sub(&body, "</span", inner_start) else {
                break;
            };
            let text = normalize_text(&body[inner_start..end]);
            if !text.is_empty() {
                out.push(text);
            }
            from = next;
            if out.len() >= 4 {
                break;
            }
        }
        out
    })
    .unwrap_or_default();

    // /<span\b[^>]*>\s*(\d+)\s*<\/span>/i
    let mut rank: Option<u32> = None;
    let mut from = 0usize;
    while let Some((_, inner_start, next)) = find_element(article, "span", from, |_| true) {
        let Some(end) = find_sub(article, "</span", inner_start) else {
            break;
        };
        let body = article[inner_start..end].trim();
        if is_digits(body) {
            rank = body.parse().ok();
            break;
        }
        from = next;
    }

    let heat = extract_heat(&normalize_text(article));

    let item = serde_json::json!({
        "seriesId": id,
        "title": title,
        "rank": rank,
        "heatText": heat,
        "cover": cover,
        "tags": tags,
    });
    normalize_rank_item(&item, seen)
}

/// `/[\d.,]+\s*[万亿]?\s*热度/`
fn extract_heat(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let numeric = |ch: char| ch.is_ascii_digit() || ch == '.' || ch == ',';
    let mut index = 0usize;
    while index < chars.len() {
        if !numeric(chars[index]) {
            index += 1;
            continue;
        }
        let start = index;
        while index < chars.len() && numeric(chars[index]) {
            index += 1;
        }
        let mut cursor = index;
        while cursor < chars.len() && is_js_space(chars[cursor]) {
            cursor += 1;
        }
        if matches!(chars.get(cursor), Some('万') | Some('亿')) {
            cursor += 1;
            while cursor < chars.len() && is_js_space(chars[cursor]) {
                cursor += 1;
            }
        }
        if chars.get(cursor) == Some(&'热') && chars.get(cursor + 1) == Some(&'度') {
            return chars[start..cursor + 2].iter().collect();
        }
    }
    String::new()
}

// ── Small HTML scanners ──────────────────────────────────────────────────

/// Every `<script …>` tag head, i.e. the text between `<script` and its `>`.
fn script_heads(html: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(at) = find_sub(html, "<script", from) {
        let after = at + "<script".len();
        from = after;
        if !is_tag_delimiter(html.as_bytes().get(after).copied()) {
            continue;
        }
        let Some(end) = find_sub(html, ">", after) else {
            break;
        };
        out.push(&html[after..end]);
    }
    out
}

fn is_tag_delimiter(byte: Option<u8>) -> bool {
    matches!(
        byte,
        None | Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r') | Some(b'>')
    )
}

/// Locate the first `<name …>` whose head passes `accept`. Returns the head text,
/// the index just after `>` and the index where the search should continue.
fn find_element<'a>(
    html: &'a str,
    name: &str,
    from: usize,
    accept: fn(&str) -> bool,
) -> Option<(&'a str, usize, usize)> {
    let open = format!("<{name}");
    let mut cursor = from;
    while let Some(at) = find_sub(html, &open, cursor) {
        let after = at + open.len();
        cursor = after;
        if !is_tag_delimiter(html.as_bytes().get(after).copied()) {
            continue;
        }
        let Some(end) = find_sub(html, ">", after) else {
            return None;
        };
        let head = &html[after..end];
        if accept(head) {
            return Some((head, end + 1, end + 1));
        }
    }
    None
}

fn slice_to(haystack: &str, from: usize, close: &str) -> Option<String> {
    let end = find_sub(haystack, close, from)?;
    Some(haystack[from..end].to_string())
}

/// An attribute value inside a tag head: `name="value"` or `name='value'`. The
/// preceding character must not be part of an attribute name, so `data-src` cannot
/// satisfy a search for `src`.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let pattern = format!("{name}=");
    let mut from = 0usize;
    while let Some(at) = find_sub(tag, &pattern, from) {
        from = at + pattern.len();
        if at > 0 {
            let previous = tag.as_bytes()[at - 1];
            if previous.is_ascii_alphanumeric()
                || previous == b'-'
                || previous == b'_'
                || previous == b':'
            {
                continue;
            }
        }
        let rest = &tag[at + pattern.len()..];
        let quote = rest.as_bytes().first()?;
        if *quote != b'"' && *quote != b'\'' {
            continue;
        }
        let closing = find_sub(rest, &[*quote as char].iter().collect::<String>(), 1)?;
        return Some(rest[1..closing].to_string());
    }
    None
}

/// `<article …>inner</article>` bodies.
fn extract_articles(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(at) = find_sub(html, "<article", from) {
        let after = at + "<article".len();
        if !is_tag_delimiter(html.as_bytes().get(after).copied()) {
            from = after;
            continue;
        }
        let Some(head_end) = find_sub(html, ">", after) else {
            break;
        };
        let Some(end) = find_sub(html, "</article>", head_end) else {
            break;
        };
        out.push(html[head_end + 1..end].to_string());
        from = end + "</article>".len();
    }
    out
}

/// `Number(nav.match(/aria-current=["']page["'][^>]*>(\d+)/)?.[1])`
fn aria_current_page(nav: &str) -> Option<u32> {
    let mut from = 0usize;
    while let Some(at) = find_sub(nav, "aria-current=", from) {
        from = at + "aria-current=".len();
        let Some(value) = attribute(&nav[at..], "aria-current") else {
            continue;
        };
        if value != "page" {
            continue;
        }
        let Some(gt) = find_sub(nav, ">", at) else {
            continue;
        };
        let digits: String = nav[gt + 1..]
            .chars()
            .take_while(|ch| ch.is_ascii_digit())
            .collect();
        if let Ok(number) = digits.parse::<u32>() {
            return Some(number);
        }
    }
    None
}

/// `href=["']([^"']+)["']` in the pagination nav, resolved against the site and
/// kept only when it points at this ranking page.
fn extract_nav_pages(nav: &str, path: &str) -> Vec<f64> {
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(at) = find_sub(nav, "href=", from) {
        from = at + "href=".len();
        let Some(value) = attribute(&nav[at..], "href") else {
            continue;
        };
        let Ok(resolved) = Url::parse(SITE).and_then(|base| base.join(&decode_html(&value))) else {
            continue;
        };
        if resolved.origin_string() != SITE || resolved.path() != path {
            out.push(0.0);
            continue;
        }
        let page = resolved
            .query_pairs()
            .find(|(key, _)| key == "page")
            .and_then(|(_, value)| value.parse::<f64>().ok())
            .filter(|value| *value > 0.0)
            .unwrap_or(1.0);
        out.push(page);
    }
    out
}

/// `url.origin === SITE` without rebuilding the origin string each time.
trait OriginString {
    fn origin_string(&self) -> String;
}

impl OriginString for Url {
    fn origin_string(&self) -> String {
        let mut origin = format!(
            "{}://{}",
            self.scheme(),
            self.host_str().unwrap_or_default()
        );
        if let Some(port) = self.port() {
            origin.push_str(&format!(":{port}"));
        }
        origin
    }
}

// ── Episode list ─────────────────────────────────────────────────────────

/// 分集页「熔断」表：某一部在双路线竞速里彻底失败后，记下到某个时刻为止不再
/// 重走那条慢路。键是 `series_id`，值是冷却截止的 `Instant`。参考实现把 web 片源
/// 失败的 vid 熔断 5 分钟，这里熔断的对象是分集页（一部一个键），语义一致：坏
/// 掉的 CDN 边缘不该让每次切集都再撞一遍。
fn episode_breaker() -> &'static Mutex<HashMap<String, Instant>> {
    static BREAKER: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    BREAKER.get_or_init(Default::default)
}

fn episode_tripped(series_id: &str) -> bool {
    episode_breaker()
        .lock()
        .ok()
        .and_then(|map| map.get(series_id).map(|until| Instant::now() < *until))
        .unwrap_or(false)
}

/// 记一次失败：5 分钟内跳过竞速直接快速失败。表只留最近 128 部，先清已过期的。
fn trip_episode(series_id: &str) {
    if let Ok(mut map) = episode_breaker().lock() {
        let now = Instant::now();
        map.retain(|_, until| *until > now);
        if map.len() >= 128 {
            if let Some(oldest) = map
                .iter()
                .min_by_key(|(_, until)| *until)
                .map(|(key, _)| key.clone())
            {
                map.remove(&oldest);
            }
        }
        map.insert(series_id.to_string(), now + Duration::from_secs(5 * 60));
    }
}

fn clear_episode(series_id: &str) {
    if let Ok(mut map) = episode_breaker().lock() {
        map.remove(series_id);
    }
}

/// `parseEpisodeListHtml` — 从 player 或 detail 页的 router data 里取分集。两条
/// 路线的 loaderData 键不同（`player_(series_id)/page`、`player_(series_id)/(vid)/page`
/// 或 `detail_page`），都指向同一个带 `seriesDetail.vid_list` 的当前剧集对象。
fn parse_episode_page(html: &str, series_id: &str) -> ApiResult<EpisodeList> {
    let router = read_router_data(html).map_err(ApiError::Input)?;
    let mismatch = || ApiError::Input("官网返回的剧集与请求不一致或缺少剧集身份".to_string());
    let loader = router.get("loaderData").and_then(Value::as_object);
    // Read identity and episodes from the same current-player object, never
    // recommendations.
    let data = loader.and_then(|rows| {
        rows.get("player_(series_id)/page")
            .or_else(|| rows.get("player_(series_id)/(vid)/page"))
            .or_else(|| rows.get("detail_page"))
    });
    let Some(data) = data else {
        return Err(mismatch());
    };
    let detail = &data["seriesDetail"];
    let detail_id = trimmed(detail.get("series_id"));
    let page_id = trimmed(data.get("series_id"));
    let page_id_present = data.get("series_id").is_some_and(|value| !value.is_null());
    if data.get("isSuccess").and_then(Value::as_bool) == Some(false)
        || !is_digits(&detail_id)
        || detail_id.len() > 30
        || detail_id != series_id
        || (page_id_present && !page_id.is_empty() && page_id != series_id)
    {
        return Err(mismatch());
    }
    let invalid = || ApiError::Input("官网页面未找到分集数据（vid_list）".to_string());
    let Some(vids_value) = detail.get("vid_list").and_then(Value::as_array) else {
        return Err(invalid());
    };
    let vids: Vec<String> = vids_value
        .iter()
        .map(|value| trimmed(Some(value)))
        .collect();
    if vids.iter().any(|vid| !is_digits(vid) || vid.len() > 30) {
        return Err(ApiError::Input("官网分集数据异常".to_string()));
    }
    if vids.iter().collect::<HashSet<_>>().len() != vids.len() {
        return Err(ApiError::Input("官网分集数据异常".to_string()));
    }
    if vids.is_empty() {
        return Err(ApiError::Input("官网分集列表为空".to_string()));
    }
    let series_title = detail
        .get("series_name")
        .and_then(Value::as_str)
        .unwrap_or("未命名短剧")
        .to_string();
    let cover = detail
        .get("series_cover")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    // `/^\d+$/` on the accessible count, never a float or a negative.
    let web_accessible = detail
        .get("accessible_episode_cnt")
        .map(|value| trimmed(Some(value)))
        .filter(|value| is_digits(value))
        .and_then(|value| value.parse::<u64>().ok())
        .map(|value| (value as usize).min(vids.len()) as u32);
    Ok(EpisodeList {
        series_id: series_id.to_string(),
        series_title: series_title.clone(),
        cover: cover.clone(),
        total: vids.len() as u32,
        web_accessible_episodes: web_accessible,
        episodes: vids
            .iter()
            .enumerate()
            .map(|(index, vid)| Episode {
                vid: vid.clone(),
                vid_index: index as u32 + 1,
                title: String::new(),
                series_id: series_id.to_string(),
                series_title: series_title.clone(),
                cover: cover.clone(),
                web_available: web_accessible.map(|value| index < value as usize),
            })
            .collect(),
        source: "web".to_string(),
    })
}

/// 一条分集路线：取某一 URL 的页面 HTML，按 `parse_episode_page` 解析。解析失败
/// 与网络失败都算这一条路线失败，交给竞速层处理。
async fn episode_route(
    client: &reqwest::Client,
    url: String,
    series_id: String,
    kind: super::FetchKind,
    secs: u64,
) -> ApiResult<EpisodeList> {
    let html = fetch_site_html(client, &url, secs, kind).await?;
    parse_episode_page(&html, &series_id)
}

/// 并发跑同一 `kind` 下的两条路线，返回第一个解析成功的片单；都失败返回 `None`。
async fn race_episode_routes(
    client: &reqwest::Client,
    series_id: &str,
    player: &str,
    detail: &str,
    kind: super::FetchKind,
    secs: u64,
) -> Option<EpisodeList> {
    let mut set = FuturesUnordered::new();
    set.push(episode_route(
        client,
        player.to_string(),
        series_id.to_string(),
        kind,
        secs,
    ));
    set.push(episode_route(
        client,
        detail.to_string(),
        series_id.to_string(),
        kind,
        secs,
    ));
    while let Some(outcome) = set.next().await {
        if let Ok(list) = outcome {
            return Some(list);
        }
    }
    None
}

/// 成功片单落进按 player URL 归键的目录缓存，便于后续切页命中。
fn remember_episode(url: &str, list: EpisodeList) -> EpisodeList {
    if let Ok(value) = serde_json::to_value(&list) {
        store(url, &value);
    }
    list
}

/// `fetchEpisodeListFromWeb` — 双路线竞速 + 熔断。
///
/// 有些 `/player` 页会从一台坏掉的 CDN 边缘取到（挂了再断），而同剧的 `/detail`
/// 路线是健康的；官网那一跳的可信 DNS（DoH）也可能整体被墙。于是：先让 player 与
/// detail 两条路线、可信 DNS 并发竞速（短预算），都不成就放宽预算重试，再补一条
/// 普通系统解析的退路。任一路线解析出合法片单即返回。两轮都落则熔断该部，
/// 切集时直接快速失败而不是反复撞坏边缘。
pub async fn fetch_episode_list(
    client: &reqwest::Client,
    series_id: &str,
) -> ApiResult<EpisodeList> {
    if !is_digits(series_id) || series_id.len() > 30 {
        return Err(ApiError::Input("剧集编号无效".to_string()));
    }
    let player = format!("{SITE}/player/{}", encode_uri_component(series_id));
    if let Some(value) = cached(&player, CATALOG_CACHE) {
        if let Ok(list) = serde_json::from_value::<EpisodeList>(value) {
            return Ok(list);
        }
    }
    if episode_tripped(series_id) {
        return Err(ApiError::Business {
            message: "官网分集暂时不可用，稍后会自动重试".to_string(),
            code: "SOURCE".to_string(),
        });
    }
    let detail = format!(
        "{SITE}/detail?series_id={}",
        encode_uri_component(series_id)
    );
    // 第一轮：两条路线并发，可信 DNS。谁先给出合法片单用谁。
    if let Some(list) = race_episode_routes(
        client,
        series_id,
        &player,
        &detail,
        super::FetchKind::Site,
        EPISODE_FIRST_SECS,
    )
    .await
    {
        clear_episode(series_id);
        return Ok(remember_episode(&player, list));
    }
    // 第二轮：放宽预算再可信一次；仍不成则换普通系统解析，DoH 端点被墙时也能出片单。
    let mut raced = race_episode_routes(
        client,
        series_id,
        &player,
        &detail,
        super::FetchKind::Site,
        EPISODE_RETRY_SECS,
    )
    .await;
    if raced.is_none() {
        raced = race_episode_routes(
            client,
            series_id,
            &player,
            &detail,
            super::FetchKind::SitePlain,
            EPISODE_RETRY_SECS,
        )
        .await;
    }
    match raced {
        Some(list) => {
            clear_episode(series_id);
            Ok(remember_episode(&player, list))
        }
        None => {
            trip_episode(series_id);
            Err(ApiError::Business {
                message: "官网分集暂不可用（双路线与两种解析都未取到）".to_string(),
                code: "SOURCE".to_string(),
            })
        }
    }
}

/// `encodeURIComponent` — everything outside the unreserved set is percent-encoded
/// by UTF-8 byte. Ids are digits, so this only guards against a crafted input.
fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '!' | '~' | '*' | '\'' => {
                out.push(ch)
            }
            other => {
                for byte in other.encode_utf8(&mut [0u8; 4]).as_bytes() {
                    out.push_str(&format!("%{byte:02X}"));
                }
            }
        }
    }
    out
}

// ── Public details and reviews ───────────────────────────────────────────

/// `reviews(value)` — public review cells from the same SSR payload. The caller
/// passes the `reviews` sub-object, exactly like the reference.
fn review_block(value: Option<&Value>) -> ReviewBlock {
    let Some(value) = value else {
        return ReviewBlock::default();
    };
    ReviewBlock {
        total: count(value.get("total_count")),
        items: value
            .get("reviews")
            .and_then(Value::as_array)
            .map(|reviews| {
                reviews
                    .iter()
                    .filter(|item| {
                        item.get("content")
                            .and_then(Value::as_str)
                            .is_some_and(|text| !text.trim().is_empty())
                    })
                    .take(50)
                    .map(|item| {
                        let created = count(item.get("create_time"));
                        Review {
                            author: item
                                .get("user_name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .chars()
                                .take(200)
                                .collect(),
                            avatar: item
                                .get("user_avatar")
                                .and_then(Value::as_str)
                                .filter(|text| {
                                    text.starts_with("http://") || text.starts_with("https://")
                                })
                                .unwrap_or_default()
                                .chars()
                                .take(4096)
                                .collect(),
                            content: item
                                .get("content")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .chars()
                                .take(20_000)
                                .collect(),
                            created_at: created
                                .map(|value| value * if value < 1e12 { 1000.0 } else { 1.0 }),
                            likes: count(item.get("digg_count")),
                            rating: count(item.get("rating")),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// `fetchPublicDetails` — ratings, like/favourite counts and public reviews.
pub async fn fetch_public_details(
    client: &reqwest::Client,
    series_id: &str,
    vid: Option<&str>,
) -> ApiResult<PublicDetails> {
    let vid = vid.unwrap_or_default();
    if !is_series_id(series_id)
        || series_id.len() > 30
        || (!vid.is_empty() && (!is_series_id(vid) || vid.len() > 30))
    {
        return Err(ApiError::Input("仅支持有效的剧集和分集编号".to_string()));
    }
    let url = if vid.is_empty() {
        format!("{SITE}/detail?series_id={series_id}")
    } else {
        format!("{SITE}/player/{series_id}/{vid}")
    };
    if let Some(value) = cached(&url, DETAILS_CACHE) {
        if let Ok(details) = serde_json::from_value::<PublicDetails>(value) {
            return Ok(details);
        }
    }
    let html = fetch_site_html(client, &url, TIMEOUT_SECS, super::FetchKind::Site).await?;
    let details = parse_public_details(&html, series_id, vid, &url)?;
    if let Ok(value) = serde_json::to_value(&details) {
        store(&url, &value);
    }
    Ok(details)
}

/// `parsePublicDetails`
pub fn parse_public_details(
    html: &str,
    series_id: &str,
    vid: &str,
    source_url: &str,
) -> ApiResult<PublicDetails> {
    let router = read_router_data(html).map_err(ApiError::Input)?;
    let data = router
        .get("loaderData")
        .and_then(Value::as_object)
        .and_then(|rows| {
            rows.values().find(|row| {
                row.get("seriesDetail").is_some()
                    && row.get("isSuccess").and_then(Value::as_bool) != Some(false)
            })
        })
        .ok_or_else(|| ApiError::Input("来源页面未返回公开剧集信息".to_string()))?;
    if trimmed(data.get("seriesDetail").map(|value| &value["series_id"])) != series_id {
        return Err(ApiError::Input("来源页面剧集不匹配".to_string()));
    }
    if !vid.is_empty() && trimmed(data.get("vid")) != vid {
        return Err(ApiError::Input("来源页面分集不匹配".to_string()));
    }
    let social = data.get("seriesSocialInfo").cloned().unwrap_or(Value::Null);
    let intro = clean_text(data["seriesDetail"].get("series_intro"));
    Ok(PublicDetails {
        series_id: series_id.to_string(),
        episode_id: (!vid.is_empty()).then(|| vid.to_string()),
        metadata: PublicMetadata::from_item(&data["seriesDetail"]),
        description: (!intro.is_empty()).then_some(intro),
        rating: count(social.get("rating")),
        rating_count: count(social.get("rating_count")),
        like_count: count(social.get("series_like_count")),
        favorite_count: count(social.get("series_favorite_count")),
        reviews: review_block(social.get("reviews")),
        episode_reviews: (!vid.is_empty()).then(|| {
            review_block(
                data.get("episodeSocialInfo")
                    .and_then(|info| info.get("reviews")),
            )
        }),
        episode_like_count: (!vid.is_empty())
            .then(|| {
                count(
                    data.get("episodeSocialInfo")
                        .and_then(|info| info.get("episode_like_count")),
                )
            })
            .flatten(),
        source_url: source_url.to_string(),
    })
}

// ── Website MP4 (no signing, no encryption) ──────────────────────────────

/// `fetchPlayUrlFromWeb` — the player page's `VideoObject.contentUrl` is a plain
/// MP4 bound to that exact episode, so it needs neither the App signature nor
/// spade decryption. Returns `None` when the page is protected or the link does
/// not match the current player.
pub async fn fetch_public_play_url(
    client: &reqwest::Client,
    series_id: &str,
    vid: &str,
) -> ApiResult<Option<PublicPlaySource>> {
    if !is_digits(vid) || vid.len() > 30 {
        return Err(ApiError::Input("分集编号无效".to_string()));
    }
    let mut candidates = Vec::new();
    if is_digits(series_id) && !series_id.is_empty() {
        candidates.push(format!(
            "{SITE}/player/{}/{}",
            encode_uri_component(series_id),
            encode_uri_component(vid)
        ));
    }
    candidates.push(format!("{SITE}/player/_/{}", encode_uri_component(vid)));

    let mut last_error: Option<ApiError> = None;
    for url in candidates {
        let html =
            match fetch_site_html(client, &url, PLAY_TIMEOUT_SECS, super::FetchKind::Site).await {
                Ok(html) => html,
                Err(error) => {
                    last_error = Some(error);
                    continue;
                }
            };
        let router = match read_router_data(&html) {
            Ok(router) => router,
            Err(error) => {
                last_error = Some(ApiError::Input(error));
                continue;
            }
        };
        let data = router
            .get("loaderData")
            .and_then(Value::as_object)
            .and_then(|rows| rows.get("player_(series_id)/(vid)/page"));
        let Some(data) = data else {
            last_error = Some(ApiError::InvalidResponse);
            continue;
        };
        let actual_sid = trimmed(data.get("series_id"));
        let bound = data
            .get("seriesDetail")
            .map(|detail| trimmed(detail.get("series_id")) == actual_sid)
            .unwrap_or(false);
        if !is_digits(&actual_sid)
            || data.get("isSuccess").and_then(Value::as_bool) == Some(false)
            || trimmed(data.get("vid")) != vid
            || (!series_id.is_empty() && actual_sid != series_id)
            || !bound
            || data
                .get("publicVideoSeo")
                .map(|seo| seo["available"].as_bool() == Some(true))
                != Some(true)
            || trimmed(data.get("publicVideoSeo").map(|seo| &seo["itemId"])) != vid
            || trimmed(data.get("publicVideoSeo").map(|seo| &seo["seriesId"])) != actual_sid
        {
            last_error = Some(ApiError::InvalidResponse);
            continue;
        }
        // Protected pages expose a short SEO preview even when metadata claims
        // the full duration.
        if data.get("protectedPlayback").and_then(Value::as_bool) == Some(true) {
            return Ok(None);
        }

        for inner_start in ld_json_blocks(&html) {
            let Some(end) = find_sub(&html, "</script", inner_start) else {
                break;
            };
            let Ok(video) = serde_json::from_str::<Value>(&html[inner_start..end]) else {
                continue;
            };
            if video.get("@type").and_then(Value::as_str) != Some("VideoObject") {
                continue;
            }
            let raw_url = match video.get("contentUrl").and_then(Value::as_str) {
                Some(value) => value,
                None => continue,
            };
            let Ok(media) = Url::parse(&normalize_video_url(raw_url)) else {
                continue;
            };
            let Ok(declared) = Url::parse(&normalize_video_url(
                video.get("url").and_then(Value::as_str).unwrap_or_default(),
            )) else {
                continue;
            };
            // Bind media to the current player, never a recommended video's URL.
            if !matches_player(&declared, &media, &actual_sid, vid) {
                continue;
            }
            if let Some(declared_id) = video.get("@id").and_then(Value::as_str) {
                let Ok(identity) = Url::parse(&normalize_video_url(declared_id)) else {
                    continue;
                };
                if !matches_player(&identity, &media, &actual_sid, vid) {
                    continue;
                }
            }
            if !is_public_media_url(media.as_str()) {
                last_error = Some(ApiError::SecurityBlocked);
                continue;
            }
            return Ok(Some(PublicPlaySource {
                url: media.into(),
                source: "web".to_string(),
                vid: vid.to_string(),
            }));
        }
    }
    match last_error {
        Some(error) => Err(error),
        None => Ok(None),
    }
}

/// The index just past `>` for every `application/ld+json` script block.
fn ld_json_blocks(html: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut from = 0usize;
    while let Some(at) = find_sub(html, "<script", from) {
        let after = at + "<script".len();
        from = after;
        if !is_tag_delimiter(html.as_bytes().get(after).copied()) {
            continue;
        }
        let Some(end) = find_sub(html, ">", after) else {
            break;
        };
        from = end + 1;
        // /\btype\s*=\s*["']application\/ld\+json["']/i
        if attribute(&html[after..end], "type")
            .map(|value| value.eq_ignore_ascii_case("application/ld+json"))
            .unwrap_or(false)
        {
            out.push(end + 1);
        }
    }
    out
}

/// `/player/{sid}/{vid}` or the canonical `/player/{sid}` paired with the
/// `/video/public/{vid}/` media path.
fn matches_player(declared: &Url, media: &Url, series_id: &str, vid: &str) -> bool {
    if declared.origin_string() != SITE
        || !declared.username().is_empty()
        || declared.password().is_some()
    {
        return false;
    }
    let exact = declared.path() == format!("/player/{series_id}/{vid}");
    let canonical = declared.path() == format!("/player/{series_id}")
        && media.origin_string() == SITE
        && media.path().starts_with(&format!("/video/public/{vid}/"));
    exact || canonical
}

/// `normalizeVideoUrl`
fn normalize_video_url(raw: &str) -> String {
    raw.replace("\\u002F", "/")
        .replace("\\u0026", "&")
        .replace("&amp;", "&")
}

// ── Website search (unsigned) ────────────────────────────────────────────
//
// The search page is a client-side shell that calls `/api/search/result`; hitting
// it directly is faster than sniffing the rendered DOM. The endpoint is not
// public, so its shape is treated as untrusted data and any surprise becomes a
// parse error the caller can fall back from.

const SEARCH_ENDPOINT: &str = "https://hongguoduanju.com/api/search/result";
const SEARCH_CACHE: Duration = Duration::from_secs(60);

/// `searchHongguo` — the website search, used when the signed App search is
/// unavailable. (`normalizeKeyword` applies NFKC in the reference; that is skipped
/// here rather than pulling in a normalisation crate — the trimmed keyword is sent
/// verbatim.)
pub async fn search_web(
    client: &reqwest::Client,
    keyword: &str,
    page: i64,
) -> ApiResult<super::app_search::SearchResult> {
    let keyword = keyword.trim().to_string();
    if keyword.is_empty() || keyword.chars().count() > 200 {
        return Err(ApiError::Input("请输入 1 至 200 字的搜索词".to_string()));
    }
    let page = page.clamp(1, 50);
    let url = format!(
        "{SEARCH_ENDPOINT}?aid=8662&query={}&page={page}",
        encode_uri_component(&keyword)
    );
    if let Some(value) = cached(&url, SEARCH_CACHE) {
        if let Ok(result) = serde_json::from_value::<super::app_search::SearchResult>(value) {
            return Ok(result);
        }
    }

    let headers = [
        ("User-Agent", WEB_UA),
        ("Accept", "application/json, text/plain, */*"),
        ("Accept-Language", "zh-CN,zh;q=0.9"),
        ("Referer", WEB_REFERER),
    ];
    let mut attempt = 0;
    let payload = loop {
        let deadline = Deadline::after(TIMEOUT_SECS);
        match super::fetch_text(
            client,
            &url,
            &headers,
            super::FetchKind::Site,
            deadline,
            MAX_PAGE_BYTES,
        )
        .await
        {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(payload) => {
                    if payload.get("code").and_then(Value::as_str).is_some() {
                        let message = payload
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let code = payload
                            .get("code")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        return Err(ApiError::Business {
                            message: format!(
                                "搜索服务暂不可用（{}{}）",
                                if message.is_empty() { "" } else { message },
                                if message.is_empty() { code } else { "" }
                            ),
                            code: "SEARCH_UPSTREAM".to_string(),
                        });
                    }
                    break payload;
                }
                // The endpoint answered with HTML, which means it changed or is
                // serving an interstitial.
                Err(_) => {
                    return Err(ApiError::Business {
                        message: "搜索返回了网页内容".to_string(),
                        code: "SEARCH_FORMAT".to_string(),
                    });
                }
            },
            Err(error)
                if attempt < 1 && matches!(error, ApiError::Timeout | ApiError::Network(_)) =>
            {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
            Err(ApiError::Business { message, code }) if code == "SOURCE" => {
                let status = message
                    .chars()
                    .filter(|ch| ch.is_ascii_digit())
                    .collect::<String>();
                // 4xx means the interface moved; anything else is upstream.
                return Err(if status.starts_with('4') {
                    ApiError::Business {
                        message: format!("搜索接口已变化（HTTP {status}）"),
                        code: "SEARCH_FORMAT".to_string(),
                    }
                } else {
                    ApiError::Business { message, code }
                });
            }
            Err(error) => return Err(error),
        }
    };

    let result = parse_search_response(&payload, page)?;
    if let Ok(value) = serde_json::to_value(&result) {
        store(&url, &value);
    }
    Ok(result)
}

/// `parseSearchResponse`
pub fn parse_search_response(
    payload: &Value,
    page: i64,
) -> ApiResult<super::app_search::SearchResult> {
    let format_error = |message: &str| ApiError::Business {
        message: message.to_string(),
        code: "SEARCH_FORMAT".to_string(),
    };
    if payload.is_null() {
        return Err(ApiError::Business {
            message: "搜索响应为空".to_string(),
            code: "SEARCH_UPSTREAM".to_string(),
        });
    }
    if !payload.is_object() {
        return Err(format_error("搜索返回异常数据"));
    }
    let list = payload
        .get("searchList")
        .and_then(Value::as_array)
        .ok_or_else(|| format_error("搜索返回异常数据"))?;

    let mut seen: Vec<String> = Vec::new();
    let mut results = Vec::new();
    for item in list {
        // Some results keep the series id in `item.keyword` with an empty
        // `video_data`; that is a match, not a parse failure.
        let video_data = item.get("video_data").filter(|value| {
            value
                .as_object()
                .map(|rows| !rows.is_empty())
                .unwrap_or(false)
        });
        let video = video_data.unwrap_or(item);
        let series_id = [
            video.get("series_id"),
            item.get("series_id"),
            item.get("keyword"),
        ]
        .into_iter()
        .flatten()
        .map(|value| trimmed(Some(value)))
        .find(|value| !value.is_empty())
        .unwrap_or_default();
        let title = [
            video.get("series_name"),
            video.get("series_title"),
            item.get("name"),
            video.get("title"),
        ]
        .into_iter()
        .flatten()
        .map(|value| clean_text(Some(value)))
        .find(|value| !value.is_empty())
        .unwrap_or_default();
        if !is_series_id(&series_id)
            || series_id.len() > 30
            || title.is_empty()
            || seen.contains(&series_id)
        {
            continue;
        }
        seen.push(series_id.clone());
        results.push(super::SeriesCard {
            series_id: series_id.clone(),
            series_title: title,
            cover: safe_cover(video.get("series_cover").or_else(|| video.get("cover"))),
            episode_count: episode_count_of(video),
            tags: tags_of(video),
            url: format!("{SITE}/detail?series_id={series_id}"),
            description: Some(clean_text(video.get("series_intro"))),
            rank: None,
            heat: None,
            metadata: Default::default(),
        });
    }
    // Rows arrived but none were recognised: the fields moved, which is a parse
    // failure rather than "no results".
    if results.is_empty() && !list.is_empty() {
        return Err(format_error("搜索返回的条目无法识别"));
    }

    let number = |key: &str| payload.get(key).and_then(|value| count(Some(value)));
    let rows = results.len() as f64;
    Ok(super::app_search::SearchResult {
        results,
        page: number("pageNum")
            .filter(|value| *value >= 1.0)
            .map(|value| value as i64)
            .unwrap_or(page),
        total: number("total").unwrap_or(rows),
        total_pages: number("totalPages").map(|value| value as u32).unwrap_or(0),
        has_more: payload.get("hasMore").and_then(Value::as_bool) == Some(true),
        next_page: None,
        channel: "web".to_string(),
        note: None,
        cursor: None,
    })
}

/// `cleanText` — the reference only swaps a handful of literal entities and trims.
/// This is a superset: entities go through `decode_html` (so numeric ones decode
/// too) and internal whitespace runs collapse to one space, which keeps titles
/// that arrive with markup-shaped padding readable in the UI.
fn clean_text(value: Option<&Value>) -> String {
    let raw = match value {
        Some(Value::String(text)) => text.clone(),
        Some(other) if !other.is_null() => other.to_string(),
        _ => String::new(),
    };
    decode_html(&raw)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// `cleanUrl` + `safeCover`. The reference's escapes are matched case-insensitively
/// (`/\\u002F/gi`), so `\U002F` and `\u002f` unescape the same way here.
fn safe_cover(value: Option<&Value>) -> String {
    let raw = match value {
        Some(Value::String(text)) => text.clone(),
        Some(other) if !other.is_null() => other.to_string(),
        _ => String::new(),
    };
    let cleaned = replace_ignore_case(&raw, "\\u002f", "/").replace("\\/", "/");
    let cleaned = replace_ignore_case(&cleaned, "\\u0026", "&").replace("&amp;", "&");
    let cleaned = cleaned.trim().to_string();
    let lower = cleaned.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        cleaned
    } else {
        String::new()
    }
}

/// Global, ASCII-case-insensitive replace — `/\\u002F/gi` in the reference.
fn replace_ignore_case(haystack: &str, needle: &str, replacement: &str) -> String {
    let source = haystack.as_bytes();
    let pattern = needle.as_bytes();
    let mut out = String::with_capacity(haystack.len());
    let mut index = 0usize;
    while index < source.len() {
        if source[index..].len() >= pattern.len()
            && source[index..index + pattern.len()].eq_ignore_ascii_case(pattern)
        {
            out.push_str(replacement);
            index += pattern.len();
        } else {
            let length = super::router::char_len(source[index]);
            let end = (index + length).min(source.len());
            out.push_str(&haystack[index..end]);
            index = end;
        }
    }
    out
}

/// `episodeCountOf` — the counter field, or the number inside `全 88 集`.
fn episode_count_of(video: &Value) -> u32 {
    // `Number(video.episode_cnt)` accepts a digit string too, so parse like the
    // reference and only accept a positive integer.
    if let Some(value) = count(video.get("episode_cnt")) {
        if value.fract() == 0.0 && value > 0.0 && value <= u32::MAX as f64 {
            return value as u32;
        }
    }
    let label = video
        .get("episode_right_text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let chars: Vec<char> = label.chars().collect();
    let numeric = |ch: char| ch.is_ascii_digit();
    let mut index = 0usize;
    while index < chars.len() {
        if !numeric(chars[index]) {
            index += 1;
            continue;
        }
        let start = index;
        while index < chars.len() && numeric(chars[index]) {
            index += 1;
        }
        let mut cursor = index;
        while cursor < chars.len() && is_js_space(chars[cursor]) {
            cursor += 1;
        }
        // `/(\d+)\s*集/` — the first run followed by 集 wins.
        if chars.get(cursor) == Some(&'集') {
            return chars[start..cursor]
                .iter()
                .collect::<String>()
                .parse()
                .unwrap_or(0);
        }
    }
    0
}

/// `tagsOf` — strings or `{name|tag_name|title}` rows, de-duplicated.
fn tags_of(video: &Value) -> Vec<String> {
    let rows = video
        .get("tags")
        .and_then(Value::as_array)
        .or_else(|| video.get("category_list").and_then(Value::as_array))
        .cloned()
        .unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    for row in rows {
        let label = match &row {
            Value::String(text) => text.clone(),
            object => [
                object.get("name"),
                object.get("tag_name"),
                object.get("title"),
            ]
            .into_iter()
            .flatten()
            .filter(|value| value.is_string())
            .map(|value| value.as_str().unwrap_or_default().to_string())
            .next()
            .unwrap_or_default(),
        };
        let text = label.trim().to_string();
        if !text.is_empty() && !out.contains(&text) {
            out.push(text);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn page(router: serde_json::Value) -> String {
        format!("<html><script>window._ROUTER_DATA = {router}</script></html>")
    }

    #[test]
    fn catalog_reads_only_the_matching_category_row() {
        let html = page(json!({
            "loaderData": {
                "other/page": { "recommendList": [{ "series_id": "1", "series_name": "错频道" }] },
                "category/page": {
                    "isSuccess": true,
                    "categoryRoute": { "contentType": "video" },
                    "seo": { "title": "  短剧  专区 " },
                    "recommendList": [
                        { "series_id": "7300000000000000000", "series_name": " 心动信号 ", "series_cover": "https://p3.fqnovel.com/a.jpg", "episode_cnt": "62", "tags": ["都市", "", "爱情"], "episode_right_text": "全62集" },
                        { "series_id": "7300000000000000000", "series_name": "重复项" },
                        { "series_id": "abc", "series_name": "非法编号" }
                    ],
                    "selectorList": [ { "items": [ { "selector_item_id": "hot", "show_name": "热播" }, { "selector_item_id": "a b", "show_name": "非法" } ] } ],
                    "pagination": { "pageNum": 3, "total": "128", "totalPages": 9 }
                }
            }
        }));
        let catalog =
            parse_catalog(&html, "video", 1, "https://hongguoduanju.com/category").unwrap();
        assert_eq!(catalog.results.len(), 1);
        let card = &catalog.results[0];
        assert_eq!(card.series_title, "心动信号");
        assert_eq!(card.episode_count, 62);
        assert_eq!(card.tags, vec!["都市".to_string(), "爱情".to_string()]);
        assert_eq!(
            card.url,
            "https://hongguoduanju.com/detail?series_id=7300000000000000000"
        );
        assert_eq!(card.metadata.episode_label, "全62集");
        assert_eq!(card.metadata.completion_status, "completed");
        assert_eq!(catalog.page, 3);
        assert_eq!(catalog.total, 128.0);
        assert_eq!(catalog.total_pages, 9);
        assert_eq!(catalog.genres.len(), 1);
        assert_eq!(catalog.page_title, "短剧  专区");
    }

    #[test]
    fn brand_never_reaches_page_titles() {
        // The site's real seo titles carry the brand; the UI must never see it.
        assert_eq!(strip_brand("红果短剧 - 免费短剧官网"), "免费短剧官网");
        assert_eq!(strip_brand("红果短剧热播榜"), "热播榜");
        assert_eq!(strip_brand("红果·推荐榜"), "推荐榜");
        assert_eq!(strip_brand("正常标题"), "正常标题");
        assert_eq!(strip_brand("热播榜_热门短剧排行榜 - 官网"), "热播榜");
        assert_eq!(strip_brand("爱情短剧在线观看 -"), "爱情短剧");
    }

    #[test]
    fn comics_are_reported_as_unsupported_not_empty() {
        let html = page(
            json!({ "loaderData": { "c/page": { "categoryRoute": { "contentType": "comic" }, "seo": { "title": "漫画" } } } }),
        );
        let catalog = parse_catalog(&html, "comic", 4, "https://hongguoduanju.com/comic").unwrap();
        assert!(catalog.results.is_empty());
        assert_eq!(catalog.page, 1);
        assert_eq!(catalog.unsupported, Some(true));
        assert!(catalog.empty_message.unwrap().contains("图文阅读"));
    }

    #[test]
    fn search_accepts_both_row_shapes_and_keeps_the_requested_page() {
        let payload = json!({
            "searchList": [
                { "video_data": { "series_id": "7300000000000000001", "series_name": "热&#x64ad;榜一", "series_cover": r"https:\u002F\u002Fp3.fqnovel.com\u002Fb.jpg", "episode_cnt": "80", "tags": [{ "name": "逆袭" }] } },
                { "keyword": "7300000000000000002", "name": "只有关键词", "video_data": {} },
                { "video_data": { "series_id": "bad", "series_name": "非法" } }
            ],
            "hasMore": true,
            "totalPages": 4
        });
        let result = parse_search_response(&payload, 7).unwrap();
        assert_eq!(result.results.len(), 2);
        assert_eq!(result.results[0].series_title, "热播榜一");
        assert_eq!(result.results[0].cover, "https://p3.fqnovel.com/b.jpg");
        assert_eq!(result.results[0].episode_count, 80);
        assert_eq!(result.results[1].series_id, "7300000000000000002");
        assert_eq!(result.page, 7);
        assert_eq!(result.total, 2.0);
        assert_eq!(result.total_pages, 4);
        assert!(result.has_more);
        assert_eq!(result.channel, "web");
        assert!(
            parse_search_response(
                &json!({ "searchList": [{ "video_data": { "series_id": "x" } }] }),
                1
            )
            .is_err()
        );
        assert!(parse_search_response(&Value::Null, 1).is_err());
    }

    #[test]
    fn public_details_read_social_counts_from_the_nested_blocks() {
        let html = page(json!({
            "loaderData": {
                "player_(series_id)/(vid)/page": {
                    "vid": "7400000000000000001",
                    "seriesDetail": { "series_id": "7300000000000000001", "series_name": "心动信号", "episode_right_text": "更新至 12 集", "first_visible_time": 1_700_000_000 },
                    "seriesSocialInfo": {
                        "rating": "9.2", "rating_count": 310, "series_like_count": 1000, "series_favorite_count": 200,
                        "reviews": { "total_count": 2, "reviews": [ { "user_name": "甲", "user_avatar": "https://p3.fqnovel.com/u.jpg", "content": "好看", "create_time": 1_700_000_000, "digg_count": 5, "rating": 10 }, { "content": "   " } ] }
                    },
                    "episodeSocialInfo": { "episode_like_count": 42, "reviews": { "total_count": 1, "reviews": [{ "user_name": "乙", "content": "这集上头", "create_time": 1700000001 }] } }
                }
            }
        }));
        let details = parse_public_details(
            &html,
            "7300000000000000001",
            "7400000000000000001",
            "https://hongguoduanju.com/player/7300000000000000001/7400000000000000001",
        )
        .unwrap();
        assert_eq!(details.rating, Some(9.2));
        assert_eq!(details.reviews.total, Some(2.0));
        assert_eq!(details.reviews.items.len(), 1);
        assert_eq!(
            details.reviews.items[0].created_at,
            Some(1_700_000_000_000.0)
        );
        assert_eq!(details.metadata.completion_status, "ongoing");
        assert_eq!(details.metadata.published_at, Some(1_700_000_000_000));
        let episode = details.episode_reviews.unwrap();
        assert_eq!(episode.items[0].content, "这集上头");
        assert_eq!(details.episode_like_count, Some(42.0));

        // A page for a different episode is refused rather than replayed.
        let other = html.replace("7400000000000000001", "7400000000000000009");
        assert!(
            parse_public_details(&other, "7300000000000000001", "7400000000000000001", "x")
                .unwrap_err()
                .to_string()
                .contains("分集不匹配")
        );
    }
}
