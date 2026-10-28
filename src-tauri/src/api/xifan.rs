//! 西饭短剧（youlishipin.com）数据源：分类、搜索、分集与取源。
//!
//! 这一路是一个公开 JSON 接口，没有签名、没有 Cookie：所有请求都打到同一个
//! API 域，靠 `version` / `androidVersionCode` 两个固定参数说明调用方形态，
//! 响应统一包在 `{ret, result}` 里。给出的 `playUrl` 是明文 MP4（302 到下载域），
//! 浏览器直接播，不需要内容密钥。
//!
//! 沿用参考实现的三条约束：
//!
//! * **编号自解释。** `series_id` 是 `xifan:<source>:<duanjuId>`，`vid` 再多一段
//!   `:<episodeId>`；每段过 `^[A-Za-z0-9_-]{1,200}$`，取源时带的 `series_id`
//!   必须与 `vid` 的前三段一致。
//! * **回显必须对上。** 详情接口会把 `source`/`duanjuId` 原样带回，对不上就当
//!   来源串数据，直接报错而不是渲染别人的分集。
//! * **每次取源重读详情。** `unlock` 是唯一的可看依据，缓存它等于给用户一个
//!   过期的「能看」结论；未解锁的集直接报错。

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;
use url::Url;

use super::router::{as_u64, trimmed};
use super::web::{CatalogPage, Episode, EpisodeList, Genre, PublicDetails, ReviewBlock};
use super::{
    ApiError, ApiResult, Deadline, PlainSource, PublicMetadata, checked_url, is_public_media_url,
};

const API: &str = "https://xifan-api-cn.youlishipin.com";
const API_HOSTS: &[&str] = &["youlishipin.com"];
const VERSION: &str = "2001001";
const ANDROID_VERSION_CODE: &str = "28";
const PAGE_SIZE: u32 = 30;
const TIMEOUT_SECS: u64 = 10;
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
const GENRE_CACHE: Duration = Duration::from_secs(300);

const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36";

/// `ID` — 接口里的 source/duanjuId/episodeId 都是这种不透明短串。
fn is_opaque_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// `parseId` — `xifan:<source>:<duanjuId>`（剧集）或再加 `:<episodeId>`（分集）。
fn parse_id(value: &str, episode: bool) -> ApiResult<(String, String, Option<String>)> {
    let parts: Vec<&str> = value.split(':').collect();
    let expected = if episode { 4 } else { 3 };
    if parts.len() != expected
        || parts[0] != "xifan"
        || !parts[1..].iter().copied().all(is_opaque_id)
    {
        return Err(ApiError::Input("西饭剧集编号无效".to_string()));
    }
    Ok((
        parts[1].to_string(),
        parts[2].to_string(),
        parts.get(3).map(|row| row.to_string()),
    ))
}

fn public_https_url(value: Option<&Value>) -> String {
    let Some(text) = value.and_then(Value::as_str) else {
        return String::new();
    };
    let Ok(url) = Url::parse(text) else {
        return String::new();
    };
    let clean = url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && is_public_media_url(text);
    if clean {
        url.to_string()
    } else {
        String::new()
    }
}

/// `plainText` — 去标签后 trim；标题里的高亮标签整段删掉，不留空格。
fn plain_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => strip_tags(text),
        Some(other) => trimmed(Some(other)),
        None => String::new(),
    }
}

fn strip_tags(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut inside = false;
    for ch in value.chars() {
        match ch {
            '<' => inside = true,
            '>' => inside = false,
            _ if !inside => out.push(ch),
            _ => {}
        }
    }
    out.trim().to_string()
}

/// `request` — 固定带两个版本参数，只认 `ret == 0` 且 `result` 是对象。
async fn request(
    client: &reqwest::Client,
    path: &str,
    params: &[(&str, String)],
    deadline: Deadline,
) -> ApiResult<Value> {
    let mut query: Vec<(&str, String)> = params.to_vec();
    query.push(("version", VERSION.to_string()));
    query.push(("androidVersionCode", ANDROID_VERSION_CODE.to_string()));

    let mut base = checked_url(&format!("{API}{path}"), API_HOSTS)?;
    base.query_pairs_mut()
        .extend_pairs(query.iter().map(|(k, v)| (*k, v.as_str())));
    let url = base.to_string();

    let remaining = deadline.remaining().ok_or(ApiError::Timeout)?;
    let response = client
        .get(&url)
        .timeout(remaining)
        .header("User-Agent", BROWSER_UA)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                ApiError::Timeout
            } else {
                ApiError::Network("西饭来源暂不可用，请稍后重试".to_string())
            }
        })?;
    if !response.status().is_success() {
        return Err(ApiError::Business {
            message: format!("西饭请求失败（HTTP {}）", response.status().as_u16()),
            code: "SOURCE".to_string(),
        });
    }
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_BODY_BYTES {
        return Err(ApiError::Input("西饭返回内容过大，请稍后重试".to_string()));
    }
    let payload: Value = serde_json::from_slice(&bytes).map_err(|_| ApiError::InvalidResponse)?;
    if payload.get("ret").and_then(Value::as_i64) != Some(0) {
        return Err(ApiError::InvalidResponse);
    }
    payload
        .get("result")
        .filter(|result| result.is_object())
        .cloned()
        .ok_or(ApiError::InvalidResponse)
}

/// `contents` — 门户页把行拆在 `elements[].contents[]` 里。
fn contents(result: &Value) -> Vec<Value> {
    result
        .get("elements")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .flat_map(|row| {
                    row.get("contents")
                        .and_then(Value::as_array)
                        .map(|items| items.clone())
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

fn hex_of(text: &str) -> String {
    text.bytes().map(|byte| format!("{byte:02x}")).collect()
}

fn genre_cache() -> &'static Mutex<Option<(Instant, Vec<(Genre, u64, u32)>)>> {
    static CACHE: OnceLock<Mutex<Option<(Instant, Vec<(Genre, u64, u32)>)>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// 题材除了 slug 还要带接口认的 `categoryId` 与 `version`。
async fn genres(
    client: &reqwest::Client,
    force_refresh: bool,
) -> ApiResult<Vec<(Genre, u64, u32)>> {
    if !force_refresh {
        if let Some(guard) = genre_cache().lock().ok() {
            if let Some((created, items)) = guard.as_ref() {
                if created.elapsed() < GENRE_CACHE {
                    return Ok(items.clone());
                }
            }
        }
    }
    let deadline = Deadline::after(TIMEOUT_SECS);
    let result = request(
        client,
        "/xifan/drama/portalPage",
        &[("reqType", "duanjuCategory".to_string())],
        deadline,
    )
    .await?;
    let mut seen = HashSet::new();
    let mut items: Vec<(Genre, u64, u32)> = Vec::new();
    for row in contents(&result) {
        let Some(category) = row.get("categoryItemVo") else {
            continue;
        };
        let list = category
            .get("subCategories")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_else(|| vec![category.clone()]);
        for sub in list {
            let Some(category_id) = sub.get("categoryId").and_then(Value::as_u64) else {
                continue;
            };
            let label = plain_text(sub.get("oppoCategory"));
            if category_id == 0 || label.is_empty() {
                continue;
            }
            let slug = format!("{category_id}-{}", hex_of(&label));
            if !seen.insert(slug.clone()) {
                continue;
            }
            let version = sub.get("version").and_then(Value::as_u64).unwrap_or(1) as u32;
            items.push((Genre { slug, label }, category_id, version));
        }
    }
    if items.is_empty() {
        return Err(ApiError::Input("西饭没有返回可用分类，请重试".to_string()));
    }
    if let Ok(mut guard) = genre_cache().lock() {
        *guard = Some((Instant::now(), items.clone()));
    }
    Ok(items)
}

fn normalize_cards(result: &Value, page: u32, items: &[(Genre, u64, u32)]) -> CatalogPage {
    let mut seen = HashSet::new();
    let results: Vec<super::SeriesCard> = contents(result)
        .into_iter()
        .filter_map(|row| row.get("duanjuVo").cloned())
        .filter_map(|item| {
            let source = trimmed(item.get("source"));
            let duanju_id = item
                .get("duanjuId")
                .map(|value| trimmed(Some(value)))
                .unwrap_or_default();
            let title = plain_text(item.get("title"));
            if !is_opaque_id(&source) || !is_opaque_id(&duanju_id) || title.is_empty() {
                return None;
            }
            let series_id = format!("xifan:{source}:{duanju_id}");
            if !seen.insert(series_id.clone()) {
                return None;
            }
            let tags = item
                .get("categories")
                .and_then(Value::as_array)
                .map(|rows| {
                    rows.iter()
                        .map(|row| plain_text(Some(row)))
                        .filter(|tag| !tag.is_empty())
                        .take(4)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Some(super::SeriesCard {
                series_id,
                series_title: title,
                cover: public_https_url(item.get("coverImageUrl")),
                episode_count: as_u64(item.get("total")).unwrap_or(0) as u32,
                tags,
                url: String::new(),
                description: plain_text(item.get("desc"))
                    .chars()
                    .take(200)
                    .collect::<String>()
                    .into(),
                rank: None,
                heat: None,
                metadata: metadata_from_info(&item),
            })
        })
        .collect();
    let has_more =
        result.get("hasMore").and_then(Value::as_bool) == Some(true) && !results.is_empty();
    CatalogPage {
        results,
        page,
        total: 0.0,
        total_pages: if has_more { page + 1 } else { page },
        genres: items.iter().map(|(genre, _, _)| genre.clone()).collect(),
        page_title: "西饭短剧".to_string(),
        source_url: API.to_string(),
        unsupported: None,
        empty_message: None,
    }
}

/// 西饭用 `updateStatus`（over/…）和 `total` 表达更新状态。
fn metadata_from_info(item: &Value) -> PublicMetadata {
    let total = as_u64(item.get("total")).unwrap_or(0);
    let status = plain_text(item.get("updateStatus"));
    let finished = status.eq_ignore_ascii_case("over") || status.contains("完结");
    let label = if total == 0 {
        status
    } else if finished {
        format!("全 {total} 集")
    } else {
        format!("共 {total} 集")
    };
    PublicMetadata {
        episode_label: label.clone(),
        completion_status: if label.starts_with('全') {
            "completed".to_string()
        } else {
            "unknown".to_string()
        },
        published_at: None,
    }
}

/// `browseList` — 没选题材时落在「都市」，和参考实现保持一致。
pub async fn browse_list(
    client: &reqwest::Client,
    genre: &str,
    page: u32,
    force_refresh: bool,
) -> ApiResult<CatalogPage> {
    let page = page.max(1);
    let wanted = genre.trim();
    let items = genres(client, force_refresh).await?;
    let selected = if wanted.is_empty() {
        items
            .iter()
            .find(|(genre, _, _)| genre.label == "都市")
            .or_else(|| items.first())
    } else {
        items.iter().find(|(genre, _, _)| genre.slug == wanted)
    }
    .ok_or_else(|| ApiError::Input("西饭题材无效，请重新选择".to_string()))?;
    let (genre_row, category_id, version) = selected;

    let offset = (page - 1) * PAGE_SIZE;
    let params: Vec<(&str, String)> = vec![
        ("reqType", "aggregationPage".to_string()),
        ("offset", offset.to_string()),
        ("categoryId", category_id.to_string()),
        ("categoryNames", genre_row.label.clone()),
        ("categoryVersion", version.to_string()),
    ];
    let deadline = Deadline::after(TIMEOUT_SECS);
    let result = request(client, "/xifan/drama/portalPage", &params, deadline).await?;
    let page_data = normalize_cards(&result, page, &items);
    if page_data.results.is_empty() {
        return Err(ApiError::InvalidResponse);
    }
    Ok(page_data)
}

/// `search` — `getSearchList`，翻页用 `pageIndex`。
pub async fn search(client: &reqwest::Client, keyword: &str, page: u32) -> ApiResult<CatalogPage> {
    let keyword = keyword.trim();
    if keyword.is_empty() {
        return Err(ApiError::Input("请输入搜索关键词".to_string()));
    }
    let page = page.max(1);
    let params: Vec<(&str, String)> = vec![
        ("keyword", keyword.to_string()),
        ("pageIndex", page.to_string()),
    ];
    let deadline = Deadline::after(TIMEOUT_SECS);
    let result = request(client, "/xifan/search/getSearchList", &params, deadline).await?;
    let page_data = normalize_cards(&result, page, &[]);
    if page_data.results.is_empty() {
        return Err(ApiError::InvalidResponse);
    }
    Ok(page_data)
}

/// `detail` — 分集、可看状态和播放地址都来自这一个接口。
async fn detail(client: &reqwest::Client, source: &str, duanju_id: &str) -> ApiResult<Value> {
    let params: Vec<(&str, String)> = vec![
        ("source", source.to_string()),
        ("duanjuId", duanju_id.to_string()),
    ];
    let deadline = Deadline::after(TIMEOUT_SECS);
    let result = request(client, "/xifan/drama/getDuanjuInfo", &params, deadline).await?;
    if trimmed(result.get("source")) != source
        || trimmed(result.get("duanjuId")) != duanju_id
        || result
            .get("episodeList")
            .and_then(Value::as_array)
            .is_none()
    {
        return Err(ApiError::Input("西饭返回的剧集与请求不一致".to_string()));
    }
    Ok(result)
}

fn episode_rows(result: &Value, series_id: &str, series_title: &str, cover: &str) -> Vec<Episode> {
    let rows = result
        .get("episodeList")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut seen = HashSet::new();
    let mut episodes: Vec<Episode> = rows
        .iter()
        .filter_map(|row| {
            let episode_id = trimmed(row.get("episodeId"));
            if !is_opaque_id(&episode_id) || !seen.insert(episode_id.clone()) {
                return None;
            }
            Some(Episode {
                vid: format!("{series_id}:{episode_id}"),
                vid_index: as_u64(row.get("index")).unwrap_or(0) as u32,
                title: plain_text(row.get("title")),
                series_id: series_id.to_string(),
                series_title: series_title.to_string(),
                cover: cover.to_string(),
                web_available: Some(row.get("unlock").and_then(Value::as_bool) == Some(true)),
            })
        })
        .collect();
    episodes.sort_by_key(|episode| episode.vid_index);
    episodes
}

pub async fn episode_list(client: &reqwest::Client, series_id: &str) -> ApiResult<EpisodeList> {
    let (source, duanju_id, _) = parse_id(series_id, false)?;
    let result = detail(client, &source, &duanju_id).await?;
    let title = {
        let value = plain_text(result.get("title"));
        if value.is_empty() {
            "未命名短剧".to_string()
        } else {
            value
        }
    };
    let cover = public_https_url(result.get("coverImageUrl"));
    let id = format!("xifan:{source}:{duanju_id}");
    let episodes = episode_rows(&result, &id, &title, &cover);
    if episodes.is_empty() {
        return Err(ApiError::Input("西饭分集列表为空".to_string()));
    }
    let free = episodes
        .iter()
        .filter(|episode| episode.web_available == Some(true))
        .count() as u32;
    let total = episodes.len() as u32;
    Ok(EpisodeList {
        series_id: id,
        series_title: title,
        cover,
        total,
        web_accessible_episodes: Some(free),
        episodes,
        source: "xifan".to_string(),
    })
}

pub async fn public_details(client: &reqwest::Client, series_id: &str) -> ApiResult<PublicDetails> {
    let (source, duanju_id, _) = parse_id(series_id, false)?;
    let result = detail(client, &source, &duanju_id).await?;
    let intro = plain_text(result.get("desc")).trim().to_string();
    Ok(PublicDetails {
        series_id: format!("xifan:{source}:{duanju_id}"),
        episode_id: None,
        metadata: metadata_from_info(&result),
        description: (!intro.is_empty()).then_some(intro),
        rating: None,
        rating_count: None,
        like_count: None,
        favorite_count: None,
        reviews: ReviewBlock::default(),
        episode_reviews: None,
        episode_like_count: None,
        source_url: API.to_string(),
    })
}

/// `fetchPlayUrlSingle` — 每次重读详情，只有 `unlock == true` 才给地址。
pub async fn play_source(
    client: &reqwest::Client,
    vid: &str,
    series_id: &str,
) -> ApiResult<PlainSource> {
    let (source, duanju_id, episode_id) = parse_id(vid, true)?;
    let episode_id = episode_id.unwrap_or_default();
    if !series_id.is_empty() {
        // `series_id` 是三段的那一种；多一段说明前端把分集编号当剧集传了。
        let (wanted_source, wanted_duanju, _) = parse_id(series_id, false)?;
        if wanted_source != source || wanted_duanju != duanju_id {
            return Err(ApiError::Input("西饭分集不属于当前剧集".to_string()));
        }
    }
    let result = detail(client, &source, &duanju_id).await?;
    let rows = result
        .get("episodeList")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let episode = rows
        .iter()
        .find(|row| trimmed(row.get("episodeId")) == episode_id)
        .ok_or_else(|| ApiError::Input("西饭未找到该分集".to_string()))?;
    if episode.get("unlock").and_then(Value::as_bool) != Some(true) {
        let index = as_u64(episode.get("index")).unwrap_or(0);
        return Err(ApiError::Business {
            message: format!("第 {index} 集当前锁定，请在西饭官方 App 确认可看范围"),
            code: "LOCKED".to_string(),
        });
    }
    let url = public_https_url(episode.get("playUrl"));
    if url.is_empty() {
        return Err(ApiError::Input("西饭未提供有效 HTTPS 播放地址".to_string()));
    }
    // 接口在分集行里就写了分辨率，直接拿来标质量。
    let width = as_u64(episode.get("videoWidth")).unwrap_or(0) as u32;
    let height = as_u64(episode.get("videoHeight")).unwrap_or(0) as u32;
    let quality = if height > 0 {
        format!("{height}p")
    } else {
        String::new()
    };
    Ok(PlainSource {
        url,
        quality,
        width,
        height,
        codec: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_carry_the_source_and_the_show() {
        let (source, duanju, episode) = parse_id("xifan:jiuzhou:23280:1339243", true).unwrap();
        assert_eq!(
            (source.as_str(), duanju.as_str(), episode.as_deref()),
            ("jiuzhou", "23280", Some("1339243"))
        );
        assert!(parse_id("xifan:jiuzhou:23280", true).is_err());
        assert!(parse_id("xifan:jiuzhou:23280:1:2", true).is_err());
        assert!(parse_id("hema:jiuzhou:23280", false).is_err());
        assert!(parse_id("xifan:jiu zhou:23280", false).is_err());
        assert!(parse_id(&format!("xifan:{}:1", "a".repeat(201)), false).is_err());
    }

    #[test]
    fn opaque_ids_allow_the_charset_the_api_uses() {
        assert!(is_opaque_id("abc-XY_9"));
        assert!(!is_opaque_id(""));
        assert!(!is_opaque_id("a.b"));
        assert!(!is_opaque_id("a/b"));
    }

    #[test]
    fn only_clean_https_urls_survive() {
        let value = |text: &str| Value::String(text.to_string());
        assert_eq!(
            public_https_url(Some(&value(
                "https://dj.youlishipin.com/duanju/video302?a=1"
            ))),
            "https://dj.youlishipin.com/duanju/video302?a=1"
        );
        assert_eq!(
            public_https_url(Some(&value("http://dj.youlishipin.com/a"))),
            ""
        );
        assert_eq!(public_https_url(Some(&value("https://10.0.0.9/a.mp4"))), "");
        assert_eq!(public_https_url(None), "");
    }

    #[test]
    fn elements_are_flattened_and_deduplicated() {
        let result = serde_json::json!({
            "hasMore": true,
            "elements": [{"contents": [
                {"duanjuVo": {"source": "jiuzhou", "duanjuId": "23280", "title": "阎罗令", "total": 85, "updateStatus": "over", "categories": ["商战", ""], "coverImageUrl": "https://xifan.heytapimage.com/a.jpeg"}},
                {"duanjuVo": {"source": "jiuzhou", "duanjuId": "23280", "title": "重复"}},
                {"duanjuVo": {"source": "", "duanjuId": "1", "title": "缺来源"}},
                {"notADuanju": true}
            ]}]
        });
        let page = normalize_cards(&result, 2, &[genre()]);
        assert_eq!(page.results.len(), 1);
        let card = &page.results[0];
        assert_eq!(card.series_id, "xifan:jiuzhou:23280");
        assert_eq!(card.series_title, "阎罗令");
        assert_eq!(card.episode_count, 85);
        assert_eq!(card.tags, vec!["商战".to_string()]);
        assert_eq!(card.metadata.episode_label, "全 85 集");
        assert_eq!(card.metadata.completion_status, "completed");
        assert_eq!(page.genres.len(), 1);
        assert_eq!(page.total_pages, 3);
        assert_eq!(page.page_title, "西饭短剧");
    }

    fn genre() -> (Genre, u64, u32) {
        (
            Genre {
                slug: "68-e983bde5b882".to_string(),
                label: "都市".to_string(),
            },
            68,
            1,
        )
    }

    #[test]
    fn slug_hexes_the_label_so_same_id_categories_stay_distinct() {
        assert_eq!(hex_of("都市"), "e983bde5b882");
        assert_ne!(
            format!("68-{}", hex_of("都市")),
            format!("68-{}", hex_of("青春"))
        );
    }

    #[test]
    fn episode_rows_skip_locked_ids_and_sort_by_index() {
        let result = serde_json::json!({
            "episodeList": [
                {"episodeId": "b", "index": 2, "title": "第二集", "unlock": true},
                {"episodeId": "a", "index": 1, "title": "第一集", "unlock": false},
                {"episodeId": "b", "index": 9, "title": "重复"},
                {"episodeId": "bad id", "index": 3}
            ]
        });
        let episodes = episode_rows(&result, "xifan:s:d", "剧", "https://c/a.jpeg");
        assert_eq!(
            episodes
                .iter()
                .map(|episode| (episode.vid_index, episode.web_available))
                .collect::<Vec<_>>(),
            vec![(1, Some(false)), (2, Some(true))]
        );
        assert_eq!(episodes[0].vid, "xifan:s:d:a");
    }
}
