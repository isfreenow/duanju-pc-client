//! The only JS ↔ Rust boundary.
//!
//! Commands return business-level DTOs. The signing algorithms, the spade key
//! derivation, the App endpoints and the request headers all stay in Rust; the
//! webview only ever sees what it needs to render (titles, covers, episode
//! numbers) plus, for the single episode the user asked to play, the resolved
//! stream address and its content key — the browser decryptor cannot exist
//! without them, so the boundary is drawn there on purpose.
//!
//! Three catalogues sit behind one command each. The routing key is the id
//! itself: `hema:<bookId>` and `xifan:<source>:<duanjuId>` are self-describing,
//! and a bare number is the website's. The webview never names a host.

use serde::Serialize;
use tauri::{AppHandle, Manager, State};

use crate::api::app_search::{SearchPageCursor, SearchResult};
use crate::api::web::{CatalogPage, EpisodeList, PublicDetails};
use crate::api::{self, ApiError};
use crate::favorites::{self, FavoriteEntry};
use crate::history::{self, HistoryEntry};

/// Shared, application-managed state.
pub struct App {
    pub client: reqwest::Client,
}

/// A stable error shape for the UI: machine-readable code plus a user message.
#[derive(Debug, Serialize)]
pub struct CommandError {
    pub code: String,
    pub message: String,
}

impl From<ApiError> for CommandError {
    fn from(error: ApiError) -> Self {
        CommandError {
            code: error.code(),
            message: error.to_string(),
        }
    }
}

/// One candidate stream for the requested episode.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayableStream {
    /// The resolved media address. Short-lived: the CDN signs it per request.
    pub url: String,
    /// Hex AES-128 key for the stream's CENC samples; absent for plaintext MP4.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_key: Option<String>,
    pub codec: String,
    pub quality: String,
    pub width: u32,
    pub height: u32,
    /// `web` for an unencrypted file off the site, `app` for signed sources,
    /// `hema` / `xifan` for the two catalogues that serve plain MP4.
    pub source: String,
    /// True when Rust holds a key, so the player knows it must decrypt.
    pub encrypted: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayResolution {
    pub vid: String,
    pub streams: Vec<PlayableStream>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Which catalogue an id or a tab belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Catalogue {
    Hongguo,
    Hema,
    Xifan,
}

/// `hema:…` / `xifan:…` 前缀优先；tab 名与 id 用同一套词表，避免两条路各认一半。
pub fn catalogue_of(value: &str) -> Catalogue {
    if value.starts_with("hema:") || value == "hema" {
        Catalogue::Hema
    } else if value.starts_with("xifan:") || value == "xifan" {
        Catalogue::Xifan
    } else {
        Catalogue::Hongguo
    }
}

#[tauri::command]
pub async fn browse_list(
    app: State<'_, App>,
    category: String,
    genre: Option<String>,
    page: Option<u32>,
    force_refresh: Option<bool>,
) -> Result<CatalogPage, CommandError> {
    let genre = genre.unwrap_or_default();
    let page = page.unwrap_or(1);
    let force = force_refresh.unwrap_or(false);
    let result = match catalogue_of(&category) {
        Catalogue::Hema => api::hema::browse_list(&app.client, &genre, page, force).await,
        Catalogue::Xifan => api::xifan::browse_list(&app.client, &genre, page, force).await,
        Catalogue::Hongguo => {
            api::web::browse_list(&app.client, &category, &genre, page, force).await
        }
    }?;
    Ok(result)
}

/// 搜索的分发：`hema` / `xifan` 类目走各自来源，其余走官网接口。
///
/// webview 命令与命令行的 `--search`（见 [`crate::launch`]）共用这一份，避免两处
/// 各认一遍路由规则。
pub async fn run_search(
    client: &reqwest::Client,
    keyword: &str,
    page: u32,
    cursor: Option<SearchPageCursor>,
    category: Option<&str>,
) -> Result<SearchResult, CommandError> {
    let catalogue = catalogue_of(category.unwrap_or_default());
    if catalogue != Catalogue::Hongguo {
        let page_data = match catalogue {
            Catalogue::Hema => api::hema::search(client, keyword, page).await,
            Catalogue::Xifan => api::xifan::search(client, keyword, page).await,
            Catalogue::Hongguo => unreachable!(),
        }?;
        return Ok(SearchResult {
            results: page_data.results,
            page: i64::from(page_data.page),
            total: page_data.total,
            total_pages: page_data.total_pages,
            has_more: page_data.total_pages > page_data.page,
            next_page: (page_data.total_pages > page_data.page)
                .then_some(i64::from(page_data.page) + 1),
            channel: if catalogue == Catalogue::Hema { "hema" } else { "xifan" }.to_string(),
            cursor: None,
            note: None,
        });
    }
    // Measured 2026-10-26: the signed App tab answers 200 with an empty body from
    // this network (the reference implementation fails the same way), while the
    // website endpoint works. So the website search is the default and the App
    // route only runs when the caller carries a cursor it wants continued.
    if cursor.is_some() {
        match api::app_search::search_app(client, keyword, page as i64, cursor.as_ref()).await {
            Ok(result) => return Ok(result),
            Err(error) if matches!(error, ApiError::Input(_)) => return Err(error.into()),
            Err(_) => {}
        }
    }
    let mut result = api::web::search_web(client, keyword, i64::from(page)).await?;
    result.channel = "web".to_string();
    Ok(result)
}

#[tauri::command]
pub async fn search_series(
    app: State<'_, App>,
    keyword: String,
    page: Option<u32>,
    cursor: Option<SearchPageCursor>,
    category: Option<String>,
) -> Result<SearchResult, CommandError> {
    run_search(&app.client, &keyword, page.unwrap_or(1), cursor, category.as_deref()).await
}

#[tauri::command]
pub async fn get_series_episodes(
    app: State<'_, App>,
    series_id: String,
) -> Result<EpisodeList, CommandError> {
    let result = match catalogue_of(&series_id) {
        Catalogue::Hema => api::hema::episode_list(&app.client, &series_id).await,
        Catalogue::Xifan => api::xifan::episode_list(&app.client, &series_id).await,
        Catalogue::Hongguo => api::web::fetch_episode_list(&app.client, &series_id).await,
    }?;
    Ok(result)
}

#[tauri::command]
pub async fn get_public_details(
    app: State<'_, App>,
    series_id: String,
    vid: Option<String>,
) -> Result<PublicDetails, CommandError> {
    let result = match catalogue_of(&series_id) {
        Catalogue::Hema => api::hema::public_details(&app.client, &series_id).await,
        Catalogue::Xifan => api::xifan::public_details(&app.client, &series_id).await,
        Catalogue::Hongguo => {
            api::web::fetch_public_details(&app.client, &series_id, vid.as_deref()).await
        }
    }?;
    Ok(result)
}

/// Resolve an episode into the streams the player can try, best first.
#[tauri::command]
pub async fn resolve_play(
    app: State<'_, App>,
    vid: String,
    series_id: Option<String>,
) -> Result<PlayResolution, CommandError> {
    let series = series_id.unwrap_or_default();
    let catalogue = match catalogue_of(&vid) {
        Catalogue::Hongguo => catalogue_of(&series),
        other => other,
    };
    if catalogue != Catalogue::Hongguo {
        let source = match catalogue {
            Catalogue::Hema => api::hema::play_source(&app.client, &vid, &series).await?,
            Catalogue::Xifan => api::xifan::play_source(&app.client, &vid, &series).await?,
            Catalogue::Hongguo => unreachable!(),
        };
        return Ok(PlayResolution {
            vid,
            streams: vec![PlayableStream {
                url: source.url,
                content_key: None,
                codec: source.codec,
                quality: source.quality,
                width: source.width,
                height: source.height,
                source: if catalogue == Catalogue::Hema { "hema" } else { "xifan" }.to_string(),
                encrypted: false,
            }],
            note: None,
        });
    }

    let mut streams: Vec<PlayableStream> = Vec::new();
    let mut note: Option<String> = None;

    // The website's public MP4 needs no key at all, so it is worth a try first —
    // measured 2026-10-26 it exists for 0 of 10 hot titles, but where the site
    // does expose it, playback is free of both signing and decryption.
    match api::web::fetch_public_play_url(&app.client, &series, &vid).await {
        Ok(Some(source)) => streams.push(PlayableStream {
            url: source.url,
            content_key: None,
            codec: "h264".to_string(),
            quality: String::new(),
            width: 0,
            height: 0,
            source: "web".to_string(),
            encrypted: false,
        }),
        Ok(None) => note = Some("官网只提供预览分集，改用 App 片源".to_string()),
        Err(error) => note = Some(error.to_string()),
    }

    if streams.is_empty() {
        for variant in api::app_video::fetch_app_play_url(&app.client, &vid).await? {
            streams.push(PlayableStream {
                url: variant.url,
                encrypted: variant.content_key.is_some(),
                content_key: variant.content_key,
                codec: variant.codec,
                quality: variant.quality,
                width: variant.width,
                height: variant.height,
                source: variant.source,
            });
        }
    }

    if streams.is_empty() {
        return Err(ApiError::Business {
            message: "本集暂无可用片源".to_string(),
            code: "NO_PLAYABLE_SOURCE".to_string(),
        }
        .into());
    }

    Ok(PlayResolution { vid, streams, note })
}

/// 记录「看到哪儿」。落盘的只有剧名、集号、秒数与来源页签。
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn record_progress(
    store: State<'_, history::Store>,
    series_id: String,
    series_title: String,
    cover: String,
    vid: String,
    vid_index: u32,
    seconds: u32,
    duration: u32,
    category: Option<String>,
) -> Result<bool, CommandError> {
    Ok(store.record(HistoryEntry {
        series_id,
        series_title,
        cover,
        vid,
        vid_index,
        seconds,
        duration,
        updated_at: api::now_millis(),
        category: category.unwrap_or_default(),
    }))
}

/// 历史条目 + Rust 侧算好的可续播判定。
///
/// 「看完没看完」「还值不值得续播」是同一套规则（95% 阈值、15 秒阈值），
/// 放在前端就等于让首页、详情页、历史页各写一遍；`#[serde(flatten)]` 把
/// 原始字段原样带过去，前端只读结果。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressRow {
    #[serde(flatten)]
    pub entry: HistoryEntry,
    pub finished: bool,
    pub resumable: bool,
}

impl From<HistoryEntry> for ProgressRow {
    fn from(entry: HistoryEntry) -> Self {
        let finished = entry.finished();
        let resumable = entry.resumable();
        ProgressRow { entry, finished, resumable }
    }
}

/// 最近观看列表，最新在前。带 `category` 时只返回同一类目的记录（列表页的
/// 「继续观看」），不带就是全量（「我的」页的观看那一段）。
#[tauri::command]
pub async fn list_history(
    store: State<'_, history::Store>,
    category: Option<String>,
) -> Result<Vec<ProgressRow>, CommandError> {
    let category = category.unwrap_or_default();
    Ok(store
        .list()
        .into_iter()
        .filter(|entry| category.is_empty() || entry.category == category)
        .map(ProgressRow::from)
        .collect())
}

/// 某一部的最新位置；详情页用它决定「继续播放」提示的内容。
#[tauri::command]
pub async fn latest_progress(
    store: State<'_, history::Store>,
    series_id: String,
) -> Result<Option<ProgressRow>, CommandError> {
    Ok(store.latest_for(&series_id).map(ProgressRow::from))
}

#[tauri::command]
pub async fn clear_history(store: State<'_, history::Store>) -> Result<(), CommandError> {
    store.clear();
    Ok(())
}

/// 点亮或取消收藏，返回操作后是否为已收藏。
#[tauri::command]
pub async fn toggle_favorite(
    store: State<'_, favorites::Store>,
    series_id: String,
    series_title: String,
    cover: String,
    category: Option<String>,
) -> Result<bool, CommandError> {
    store
        .toggle(FavoriteEntry {
            series_id,
            series_title,
            cover,
            category: category.unwrap_or_default(),
            updated_at: api::now_millis(),
        })
        .ok_or_else(|| CommandError {
            code: "INVALID_SERIES".to_string(),
            message: "缺少剧集 ID，无法收藏".to_string(),
        })
}

/// 收藏清单，最新点亮在前。历史页把最近观看与收藏融合展示时用它。
#[tauri::command]
pub async fn list_favorites(store: State<'_, favorites::Store>) -> Result<Vec<FavoriteEntry>, CommandError> {
    Ok(store.list())
}

/// 这一部是否已收藏；详情页的收藏按钮用它初始化。
#[tauri::command]
pub async fn is_favorited(store: State<'_, favorites::Store>, series_id: String) -> Result<bool, CommandError> {
    Ok(store.contains(&series_id))
}

/// 窗口关闭前无条件落盘，绕过节流；只做这一件事。
#[tauri::command]
pub async fn flush_history(app: AppHandle) -> Result<(), CommandError> {
    if let Some(store) = app.try_state::<history::Store>() {
        store.flush();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::catalogue_of;
    use super::Catalogue;

    #[test]
    fn ids_route_by_their_own_prefix() {
        assert_eq!(catalogue_of("hema:41000448669"), Catalogue::Hema);
        assert_eq!(catalogue_of("hema:41000448669:618017066"), Catalogue::Hema);
        assert_eq!(catalogue_of("xifan:jiuzhou:23280"), Catalogue::Xifan);
        assert_eq!(catalogue_of("xifan:jiuzhou:23280:1339243"), Catalogue::Xifan);
        assert_eq!(catalogue_of("7458123456789"), Catalogue::Hongguo);
    }

    #[test]
    fn tab_names_use_the_same_words_as_ids() {
        assert_eq!(catalogue_of("hema"), Catalogue::Hema);
        assert_eq!(catalogue_of("xifan"), Catalogue::Xifan);
        assert_eq!(catalogue_of("hot-drama"), Catalogue::Hongguo);
        // 前缀必须是完整一段，不能让 `hema-x` 这种分类名误命中。
        assert_eq!(catalogue_of("hema-hot"), Catalogue::Hongguo);
    }
}
