//! 河马短剧（kuaikaw.cn）数据源：目录、搜索、分集与取源。
//!
//! 这一路完全没有签名：目录和详情都是 Next.js 服务端渲染，数据放在页面里那个
//! `id="__NEXT_DATA__"` 的 `<script>` JSON 中，搜索是一个公开 POST。取回的
//! `chapterVideoVo.mp4720p` 是明文 MP4，浏览器直接播放，不需要内容密钥。
//!
//! 沿用参考实现的三条约束，因为它们挡的是真实问题：
//!
//! * **编号自解释。** `series_id` 是 `hema:<bookId>`，`vid` 是 `hema:<bookId>:<chapterId>`；
//!   每段都要过 `^\d{1,30}$`，`resolve_play` 带的 `series_id` 必须和 `vid` 前缀一致，
//!   否则就是前端把两本书的分集拼错了。
//! * **每次取源重读详情。** 免费/付费的判定只存在于详情里，缓存它等于替用户
//!   保留一个过期的「能看」结论；锁定集直接返回错误，不猜地址。
//! * **只接受公网 HTTPS 地址。** 页面里的封面、播放地址都过 `checked_url` /
//!   `is_public_media_url`，带凭据、明文 http、指向内网的地址一律丢弃。

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;
use url::Url;

use super::router::{as_u64, read_json_between, trimmed};
use super::web::{CatalogPage, Episode, EpisodeList, Genre, PublicDetails, ReviewBlock};
use super::{
    ApiError, ApiResult, Deadline, PlainSource, PublicMetadata, checked_url, find_sub,
    is_public_media_url,
};

const SITE: &str = "https://www.kuaikaw.cn";
const SITE_HOSTS: &[&str] = &["kuaikaw.cn"];
const TIMEOUT_SECS: u64 = 10;
const MAX_PAGE_BYTES: usize = 8 * 1024 * 1024;
const GENRE_CACHE: Duration = Duration::from_secs(300);

/// 与参考实现同一个浏览器 UA：这两个来源都只按 UA 粗筛，不给就回验证券。
const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36";

fn is_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) && value.len() <= 30
}

/// `parseId` — `hema:<bookId>` 或 `hema:<bookId>:<chapterId>`。
fn parse_id(value: &str, episode: bool) -> ApiResult<(String, Option<String>)> {
    let parts: Vec<&str> = value.split(':').collect();
    let expected = if episode { 3 } else { 2 };
    if parts.len() != expected || parts[0] != "hema" || !parts[1..].iter().copied().all(is_digits) {
        return Err(ApiError::Input("河马剧集编号无效".to_string()));
    }
    let book_id = parts[1].to_string();
    let chapter_id = parts.get(2).map(|value| value.to_string());
    Ok((book_id, chapter_id))
}

/// `httpsUrl` — 只放行无凭据的公网 https 地址，其它一律当「没有」处理。
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

/// `plainText` — 去标签后 trim。
///
/// 这里刻意不用目录页那个 `visible_text`：它把每个标签换成一个空格（榜单标题
/// 需要），而河马的书名和分集标题里的高亮 `<em>` 必须整个删掉，否则
/// 「第一集」会变成「第 一 集」。
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

async fn request_html(client: &reqwest::Client, path: &str, deadline: Deadline) -> ApiResult<String> {
    let url = checked_url(&format!("{SITE}{path}"), SITE_HOSTS)?.to_string();
    super::fetch_text(
        client,
        &url,
        &[
            ("User-Agent", BROWSER_UA),
            ("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"),
            ("pname", "www.kuaikaw.cn"),
            ("Origin", SITE),
            ("Referer", &format!("{SITE}/")),
        ],
        deadline,
        MAX_PAGE_BYTES,
    )
    .await
}

async fn request_json(
    client: &reqwest::Client,
    path: &str,
    body: &Value,
    deadline: Deadline,
) -> ApiResult<Value> {
    let url = checked_url(&format!("{SITE}{path}"), SITE_HOSTS)?.to_string();
    let remaining = deadline.remaining().ok_or(ApiError::Timeout)?;
    let response = client
        .post(&url)
        .timeout(remaining)
        .header("User-Agent", BROWSER_UA)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .header("pname", "www.kuaikaw.cn")
        .header("Origin", SITE)
        .header("Referer", format!("{SITE}/"))
        .body(body.to_string())
        .send()
        .await
        .map_err(|error| {
            if error.is_timeout() {
                ApiError::Timeout
            } else {
                ApiError::Network("河马来源暂不可用，请稍后重试".to_string())
            }
        })?;
    if !response.status().is_success() {
        return Err(ApiError::Business {
            message: format!("河马请求失败（HTTP {}）", response.status().as_u16()),
            code: "SOURCE".to_string(),
        });
    }
    let bytes = response.bytes().await?;
    if bytes.len() > MAX_PAGE_BYTES {
        return Err(ApiError::Input("河马返回内容过大，请稍后重试".to_string()));
    }
    serde_json::from_slice(&bytes).map_err(|_| ApiError::InvalidResponse)
}

/// `pageData` — 取出 `__NEXT_DATA__` 里的 `props.pageProps`。
fn page_props(html: &str) -> ApiResult<Value> {
    let marker = match find_sub(html, "__NEXT_DATA__", 0) {
        Some(at) => at,
        None => return Err(ApiError::InvalidResponse),
    };
    // 标签的结束 `>` 一定在 marker 之后，JSON 体在 `>` 与 `</script>` 之间。
    let body = read_json_between(html, marker).ok_or(ApiError::InvalidResponse)?;
    let parsed: Value =
        serde_json::from_str(body).map_err(|_| ApiError::InvalidResponse)?;
    parsed
        .get("props")
        .and_then(|props| props.get("pageProps"))
        .cloned()
        .filter(|data| data.is_object())
        .ok_or(ApiError::InvalidResponse)
}

fn normalize_books(books: Option<&Value>) -> Vec<super::SeriesCard> {
    let mut seen = HashSet::new();
    let items = match books {
        Some(Value::Array(items)) => items,
        _ => return Vec::new(),
    };
    items
        .iter()
        .filter_map(|item| {
            let book_id = item.get("bookId").map(|value| trimmed(Some(value))).unwrap_or_default();
            if !is_digits(&book_id) {
                return None;
            }
            let title = plain_text(item.get("bookName"));
            if title.is_empty() {
                return None;
            }
            let tags = item
                .get("bookTypeThree")
                .and_then(Value::as_array)
                .map(|rows| {
                    rows.iter()
                        .filter_map(|row| {
                            let tag = plain_text(row.get("name"));
                            (!tag.is_empty()).then_some(tag)
                        })
                        .take(4)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let series_id = format!("hema:{book_id}");
            if !seen.insert(series_id.clone()) {
                return None;
            }
            Some(super::SeriesCard {
                series_id,
                series_title: title,
                cover: public_https_url(item.get("coverWap")),
                episode_count: as_u64(item.get("totalChapterNum")).unwrap_or(0) as u32,
                tags,
                url: format!("{SITE}/drama/{book_id}"),
                description: plain_text(item.get("introduction"))
                    .chars()
                    .take(200)
                    .collect::<String>()
                    .into(),
                rank: None,
                heat: plain_text(item.get("videoStarsNum")).into(),
                metadata: metadata_from_book(item),
            })
        })
        .collect()
}

/// 河马用 `statusDesc`（完本/连载）+ 总集数表达更新状态，没有官网那两个字段。
fn metadata_from_book(item: &Value) -> PublicMetadata {
    let total = as_u64(item.get("totalChapterNum")).unwrap_or(0);
    let status = plain_text(item.get("statusDesc"));
    let label = if status.contains("完本") || status.contains("完结") {
        format!("全 {total} 集")
    } else if total > 0 {
        format!("共 {total} 集")
    } else {
        status
    };
    PublicMetadata {
        completion_status: if label.starts_with('全') {
            "completed".to_string()
        } else {
            "unknown".to_string()
        },
        episode_label: label,
        published_at: None,
    }
}

/// `watchDirectory` 题材缓存；目录页第一屏会带全量题材，没必要每页都重解析。
fn genre_cache() -> &'static Mutex<Option<(Instant, Vec<Genre>)>> {
    static CACHE: OnceLock<Mutex<Option<(Instant, Vec<Genre>)>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn cached_genres() -> Option<Vec<Genre>> {
    let guard = genre_cache().lock().ok()?;
    let (created, items) = guard.as_ref()?;
    (created.elapsed() < GENRE_CACHE).then(|| items.clone())
}

fn store_genres(items: &[Genre]) {
    if let Ok(mut guard) = genre_cache().lock() {
        *guard = Some((Instant::now(), items.to_vec()));
    }
}

/// `parseGenres` — `typeThreeList[].typeThreeIds` 拼成 slug，「全部」（id 0）不进列表。
fn parse_genres(directory: &Value) -> ApiResult<Vec<Genre>> {
    let rows = directory
        .get("typeThreeList")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::Input("河马没有返回可用题材，请重试".to_string()))?;
    let mut seen = HashSet::new();
    let mut items = Vec::new();
    for row in rows {
        let ids = row.get("typeThreeIds").and_then(Value::as_array);
        let label = plain_text(row.get("name"));
        let Some(ids) = ids else { continue };
        if ids.is_empty() || ids.len() > 16 || label.is_empty() {
            continue;
        }
        let parts = ids
            .iter()
            .map(|id| trimmed(Some(id)))
            .collect::<Vec<_>>();
        if !parts.iter().all(|id| is_digits(id)) {
            continue;
        }
        let slug = parts.join("-");
        if slug == "0" || !seen.insert(slug.clone()) {
            continue;
        }
        items.push(Genre { slug, label });
    }
    if items.is_empty() {
        return Err(ApiError::Input("河马没有返回可用题材，请重试".to_string()));
    }
    Ok(items)
}

/// `browseList` — 题材页 `/browse/<slug>/<page>`，第一屏同时给出题材表。
pub async fn browse_list(
    client: &reqwest::Client,
    genre: &str,
    page: u32,
    force_refresh: bool,
) -> ApiResult<CatalogPage> {
    let page = page.max(1);
    let slug = if genre.is_empty() { "0" } else { genre };
    if !is_hema_slug(slug) {
        return Err(ApiError::Input("河马题材或页码无效，请重新选择".to_string()));
    }

    let deadline = Deadline::after(TIMEOUT_SECS);
    // 第一屏（slug 0 + page 1）既是目录又是题材表，别拉两次。
    let needs_directory = force_refresh || cached_genres().is_none() || (slug == "0" && page == 1);
    let first_html = if needs_directory {
        Some(request_html(client, "/browse", deadline).await?)
    } else {
        None
    };
    let genres = if slug == "0" && page == 1 {
        let directory = page_props(first_html.as_deref().unwrap_or_default())
            .map_err(|_| ApiError::Input("河马目录数据异常，请稍后重试".to_string()))?;
        let items = parse_genres(&directory)?;
        store_genres(&items);
        items
    } else {
        cached_genres().ok_or_else(|| ApiError::Input("河马题材表已过期，请重试".to_string()))?
    };
    if slug != "0" && !genres.iter().any(|item| item.slug == slug) {
        return Err(ApiError::Input("河马题材无效，请重新选择".to_string()));
    }

    let path = if slug == "0" && page == 1 {
        "/browse".to_string()
    } else if page > 1 {
        format!("/browse/{slug}/{page}")
    } else {
        format!("/browse/{slug}")
    };
    let html = if path == "/browse" {
        first_html.unwrap_or_default()
    } else {
        request_html(client, &path, deadline).await?
    };
    let data = page_props(&html)
        .map_err(|_| ApiError::Input("河马目录数据异常，请稍后重试".to_string()))?;

    // 参考实现会核对页面回显的页码与题材，避免拿上一页的结果当本页。
    let echoed_page = as_u64(data.get("page")).unwrap_or(0) as u32;
    let echoed_slug = trimmed(data.get("typeThreeId"));
    let echoed_slug = if echoed_slug.is_empty() { "0" } else { &echoed_slug[..] };
    data.get("bookList")
        .and_then(Value::as_array)
        .ok_or(ApiError::InvalidResponse)?;
    if echoed_page != page || echoed_slug != slug {
        return Err(ApiError::Input(
            "河马返回的目录与请求不一致，请重试".to_string(),
        ));
    }
    let results = normalize_books(data.get("bookList"));
    if results.is_empty() {
        return Err(ApiError::InvalidResponse);
    }
    // 河马不给总数，只给下一页链接；沿用参考实现「跟着 next 走」的做法。
    let has_more = has_next_link(&html, slug, page + 1);
    Ok(CatalogPage {
        results,
        page,
        total: 0.0,
        total_pages: if has_more { page + 1 } else { page },
        genres,
        page_title: "河马短剧".to_string(),
        source_url: format!("{SITE}{path}"),
        unsupported: None,
        empty_message: None,
    })
}

fn is_hema_slug(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    !parts.is_empty() && parts.len() <= 16 && parts.iter().all(|part| is_digits(part))
}

/// `/href="\/browse\/<slug>\/<next>"/` 的等价检查：只要下一页链接出现在页面里。
fn has_next_link(html: &str, slug: &str, next: u32) -> bool {
    let needle = format!("href=\"/browse/{slug}/{next}\"");
    find_sub(html, &needle, 0).is_some()
}

/// `search` — `POST /seo/video/6007`。
pub async fn search(client: &reqwest::Client, keyword: &str, page: u32) -> ApiResult<CatalogPage> {
    let keyword = keyword.trim();
    if keyword.is_empty() {
        return Err(ApiError::Input("请输入搜索关键词".to_string()));
    }
    let page = page.max(1);
    let deadline = Deadline::after(TIMEOUT_SECS);
    let body = serde_json::json!({ "sourceType": 1, "keyword": keyword, "index": page });
    let payload = request_json(client, "/seo/video/6007", &body, deadline).await?;
    if payload.get("retCode").and_then(Value::as_i64).unwrap_or(-1) != 0 {
        return Err(ApiError::InvalidResponse);
    }
    let data = payload.get("data").ok_or(ApiError::InvalidResponse)?;
    let results = normalize_books(data.get("bookList"));
    if results.is_empty() {
        return Err(ApiError::InvalidResponse);
    }
    let has_more = data.get("isMore").and_then(Value::as_i64) == Some(1);
    Ok(CatalogPage {
        results,
        page,
        total: as_u64(data.get("totalSize")).unwrap_or(0) as f64,
        total_pages: if has_more { page + 1 } else { page },
        genres: Vec::new(),
        page_title: "河马短剧".to_string(),
        source_url: SITE.to_string(),
        unsupported: None,
        empty_message: None,
    })
}

/// `detail` — 详情读一次，分集与取源各自复用同一份校验。
async fn detail(client: &reqwest::Client, book_id: &str) -> ApiResult<Value> {
    if !is_digits(book_id) {
        return Err(ApiError::Input("河马剧集编号无效".to_string()));
    }
    let deadline = Deadline::after(TIMEOUT_SECS);
    let html = request_html(client, &format!("/drama/{book_id}"), deadline).await?;
    let data = page_props(&html)
        .map_err(|_| ApiError::Input("河马详情数据异常，请稍后重试".to_string()))?;
    let echoed = trimmed(data.get("bookInfoVo").and_then(|info| info.get("bookId")));
    if echoed != book_id {
        return Err(ApiError::Input("河马返回的剧集与请求不一致".to_string()));
    }
    if data.get("chapterList").and_then(Value::as_array).is_none() {
        return Err(ApiError::Input("河马分集数据异常，请稍后重试".to_string()));
    }
    Ok(data)
}

pub async fn episode_list(client: &reqwest::Client, series_id: &str) -> ApiResult<EpisodeList> {
    let (book_id, _) = parse_id(series_id, false)?;
    let data = detail(client, &book_id).await?;
    let info = data.get("bookInfoVo").cloned().unwrap_or(Value::Null);
    let chapters = data.get("chapterList").and_then(Value::as_array).cloned().unwrap_or_default();
    let series_title = {
        let title = plain_text(info.get("bookName"));
        if title.is_empty() {
            "未命名短剧".to_string()
        } else {
            title
        }
    };
    let cover = public_https_url(info.get("coverWap"));
    let id = format!("hema:{book_id}");

    let mut seen: HashSet<String> = HashSet::new();
    let mut episodes: Vec<Episode> = Vec::with_capacity(chapters.len());
    for (position, chapter) in chapters.iter().enumerate() {
        let chapter_id = trimmed(chapter.get("chapterId"));
        if !is_digits(&chapter_id) || !seen.insert(chapter_id.clone()) {
            return Err(ApiError::Input("河马分集数据异常，请稍后重试".to_string()));
        }
        if let Some(owner) = chapter.get("bookId").filter(|value| !value.is_null()) {
            if trimmed(Some(owner)) != book_id {
                return Err(ApiError::Input("河马分集数据异常，请稍后重试".to_string()));
            }
        }
        let index = as_u64(chapter.get("chapterIndex")).unwrap_or((position + 1) as u64) as u32;
        episodes.push(Episode {
            vid: format!("{id}:{chapter_id}"),
            vid_index: index,
            title: plain_text(chapter.get("chapterName")),
            series_id: id.clone(),
            series_title: series_title.clone(),
            cover: cover.clone(),
            web_available: Some(chapter.get("isCharge").and_then(Value::as_str) == Some("0")),
        });
    }
    if episodes.is_empty() {
        return Err(ApiError::Input("河马分集列表为空".to_string()));
    }
    episodes.sort_by_key(|episode| episode.vid_index);
    let total = episodes.len() as u32;
    let free = episodes
        .iter()
        .filter(|episode| episode.web_available == Some(true))
        .count() as u32;
    Ok(EpisodeList {
        series_id: id,
        series_title,
        cover,
        total,
        web_accessible_episodes: Some(free),
        episodes,
        source: "hema".to_string(),
    })
}

pub async fn public_details(client: &reqwest::Client, series_id: &str) -> ApiResult<PublicDetails> {
    let (book_id, _) = parse_id(series_id, false)?;
    let data = detail(client, &book_id).await?;
    let info = data.get("bookInfoVo").cloned().unwrap_or(Value::Null);
    Ok(PublicDetails {
        series_id: format!("hema:{book_id}"),
        episode_id: None,
        metadata: metadata_from_book(&info),
        rating: None,
        rating_count: None,
        like_count: None,
        favorite_count: as_u64(info.get("followCount")).map(|value| value as f64),
        reviews: ReviewBlock::default(),
        episode_reviews: None,
        episode_like_count: None,
        source_url: format!("{SITE}/drama/{book_id}"),
    })
}

/// `fetchPlayUrlSingle` — 每次现读详情，只在这集确实免费时给出明文地址。
pub async fn play_source(client: &reqwest::Client, vid: &str, series_id: &str) -> ApiResult<PlainSource> {
    let (book_id, chapter_id) = parse_id(vid, true)?;
    let chapter_id = chapter_id.unwrap_or_default();
    if !series_id.is_empty() {
        // `series_id` 是两段的那一种；能解析成三段就是前端把分集编号当剧集传了。
        let (wanted_book, _) = parse_id(series_id, false)?;
        if wanted_book != book_id {
            return Err(ApiError::Input("河马分集不属于当前剧集".to_string()));
        }
    }
    let data = detail(client, &book_id).await?;
    let chapters = data.get("chapterList").and_then(Value::as_array).cloned().unwrap_or_default();
    let chapter = chapters
        .iter()
        .find(|row| trimmed(row.get("chapterId")) == chapter_id)
        .ok_or_else(|| ApiError::Input("河马未找到该分集".to_string()))?;
    if chapter.get("isCharge").and_then(Value::as_str) != Some("0") {
        let index = as_u64(chapter.get("chapterIndex")).unwrap_or(0);
        return Err(ApiError::Business {
            message: format!("第 {index} 集当前锁定，请在河马官方平台确认可看范围"),
            code: "LOCKED".to_string(),
        });
    }
    let video = chapter.get("chapterVideoVo");
    // 720p 优先，回落到通用 `mp4`；质量标签跟着实际选中的字段走。
    for (key, quality) in [("mp4720p", "720p"), ("mp4", "默认")] {
        let url = public_https_url(video.and_then(|vo| vo.get(key)));
        if !url.is_empty() {
            return Ok(PlainSource {
                url,
                quality: quality.to_string(),
                width: 0,
                height: 0,
                codec: String::new(),
            });
        }
    }
    Err(ApiError::Input("河马未提供有效 HTTPS 播放地址".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_self_describing() {
        assert!(parse_id("hema:41000448669", false).is_ok());
        assert_eq!(
            parse_id("hema:41000448669:618017066", true)
                .unwrap()
                .1
                .as_deref(),
            Some("618017066")
        );
        // 段数、前缀、纯数字、长度上限任一不过都不接受。
        assert!(parse_id("hema:12", true).is_err());
        assert!(parse_id("hema:12:34", false).is_err());
        assert!(parse_id("hema:12:34:56", true).is_err());
        assert!(parse_id("other:12:34", true).is_err());
        assert!(parse_id("hema:abc:34", true).is_err());
        assert!(parse_id("hema:12:", true).is_err());
        assert!(parse_id(&format!("hema:{}", "1".repeat(31)), false).is_err());
    }

    #[test]
    fn slug_accepts_the_piped_genre_ids() {
        assert!(is_hema_slug("0"));
        assert!(is_hema_slug("1170-1165"));
        assert!(!is_hema_slug(""));
        assert!(!is_hema_slug("abc"));
        assert!(!is_hema_slug("1-2-3-4-5-6-7-8-9-10-11-12-13-14-15-16-17"));
    }

    #[test]
    fn only_clean_https_urls_survive() {
        let value = |text: &str| Value::String(text.to_string());
        assert_eq!(
            public_https_url(Some(&value("https://dzzt-video.cbread.cn/a.mp4"))),
            "https://dzzt-video.cbread.cn/a.mp4"
        );
        assert_eq!(public_https_url(Some(&value("http://a.example.com/v.mp4"))), "");
        assert_eq!(public_https_url(Some(&value("https://127.0.0.1/a.mp4"))), "");
        assert_eq!(
            public_https_url(Some(&value("https://user:pw@kuaikaw.cn/a.mp4"))),
            ""
        );
        assert_eq!(public_https_url(None), "");
    }

    #[test]
    fn next_data_needs_a_json_object_at_page_props() {
        let html = r#"<html><script id="__NEXT_DATA__" type="application/json">{"props":{"pageProps":{"bookList":[{"bookId":"7","bookName":"<em>测试</em>剧","totalChapterNum":"12","statusDesc":"完本","bookTypeThree":[{"name":"逆袭"},{"name":""}]}],"page":1,"typeThreeId":""}}}</script></html>"#;
        let data = page_props(html).unwrap();
        let cards = normalize_books(data.get("bookList"));
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].series_id, "hema:7");
        assert_eq!(cards[0].series_title, "测试剧");
        assert_eq!(cards[0].tags, vec!["逆袭".to_string()]);
        assert_eq!(cards[0].metadata.episode_label, "全 12 集");
        assert_eq!(cards[0].metadata.completion_status, "completed");
        assert!(page_props("<html>no payload</html>").is_err());
        assert!(page_props(r#"<script id="__NEXT_DATA__">{"props":{}}</script>"#).is_err());
    }

    #[test]
    fn genres_drop_the_all_row_and_keep_order() {
        let directory = serde_json::json!({
            "typeThreeList": [
                {"name": "全部", "typeThreeIds": [0]},
                {"name": "青春", "typeThreeIds": [1170]},
                {"name": "女帝", "typeThreeIds": [1165, 1170]},
                {"name": "坏", "typeThreeIds": ["x"]},
                {"name": "青春", "typeThreeIds": [1170]}
            ]
        });
        let items = parse_genres(&directory).unwrap();
        assert_eq!(
            items.iter().map(|genre| genre.slug.as_str()).collect::<Vec<_>>(),
            vec!["1170", "1165-1170"]
        );
        assert!(parse_genres(&serde_json::json!({})).is_err());
        assert!(parse_genres(&serde_json::json!({"typeThreeList": []})).is_err());
    }
}
