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

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;

use crate::api::app_search::{SearchPageCursor, SearchResult};
use crate::api::app_video::MediaVariant;
use crate::api::web::{CatalogPage, EpisodeList, PublicDetails};
use crate::api::{self, ApiError};
use crate::download::{DownloadManager, DownloadTask};
use crate::favorites::{self, FavoriteEntry};
use crate::history::{self, HistoryEntry};
use crate::play_cache::PlayCache;
use crate::transcode::{self, Layout, MediaInput, ToolStatus, TranscodeOutput};

/// Shared, application-managed state.
pub struct App {
    pub client: reqwest::Client,
}

/// 前端检查到更新结果时同步托盘角标：后台巡检之外的手动「检查更新」也能点上/熄灭红点。
#[tauri::command]
pub fn set_tray_badge(app: AppHandle, on: bool, version: Option<String>) {
    #[cfg(desktop)]
    crate::set_tray_badge(&app, on, version.as_deref());
    #[cfg(not(desktop))]
    {
        let _ = (app, on, version);
    }
}

/// 预取命令一次最多处几集。前端只需要「后面两集」，留一点余量给手动跳集；
/// 不设上限就等于给一次刷新发几十个签名请求，那会把来源打病。
const MAX_PREFETCH: usize = 4;

/// A stable error shape for the UI: machine-readable code, a user message, and
/// the「下一步动作」that makes the failure actionable instead of a bare
/// 「播放出错」.
#[derive(Debug, Clone, Serialize)]
pub struct CommandError {
    pub code: String,
    pub message: String,
    /// 只在少数几类错误上有定制说法；其余为 `None`，前端回落到通用文案。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl From<ApiError> for CommandError {
    fn from(error: ApiError) -> Self {
        let code = error.code();
        let message = error.to_string();
        // 转码失败里有一类是「FFmpeg 解不了这种 CENC 写法」（逐样本 senc），
        // 它不是下载出问题也不是工具坏了，退路是装系统解码组件——把这句话附在
        // 错误里，而不是让用户对着一个看不出所以然的退出码。
        let hint = match error {
            ApiError::Business { ref message, .. }
                if message.contains("decryption")
                    || message.contains("No usable ciphertext")
                    || message.contains("AES-CTR") =>
            {
                Some(
                    "这条片源的加密写法需要系统解码：请先安装 HEVC 视频扩展，或换一集再试。"
                        .to_string(),
                )
            }
            _ => error.recovery_hint().map(str::to_string),
        };
        CommandError {
            code,
            message,
            hint,
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

/// 把搜索页返回的卡片按「标题归一 + 季数感知」重排。
///
/// 上游只给热度序，同一关键词下的本传/续作、全名/别名会散落在各页。排序逻辑是
/// [`crate::api::rank`] 里的纯函数，页面前端与命令行的 `--search` 共用同一份，
/// 只在**当前页内**重排，不跨页（分页语义由来源决定）。
fn rank_cards(cards: Vec<crate::api::SeriesCard>, keyword: &str) -> Vec<crate::api::SeriesCard> {
    let order = crate::api::rank::rank_order(&cards, keyword, |card| {
        (card.series_title.clone(), card.description.clone())
    });
    let mut slots: Vec<Option<crate::api::SeriesCard>> = cards.into_iter().map(Some).collect();
    order
        .into_iter()
        .filter_map(|index| slots[index].take())
        .collect()
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
            results: rank_cards(page_data.results, keyword),
            page: i64::from(page_data.page),
            total: page_data.total,
            total_pages: page_data.total_pages,
            has_more: page_data.total_pages > page_data.page,
            next_page: (page_data.total_pages > page_data.page)
                .then_some(i64::from(page_data.page) + 1),
            channel: if catalogue == Catalogue::Hema {
                "hema"
            } else {
                "xifan"
            }
            .to_string(),
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
            Ok(mut result) => {
                result.results = rank_cards(result.results, keyword);
                return Ok(result);
            }
            Err(error) if matches!(error, ApiError::Input(_)) => return Err(error.into()),
            Err(_) => {}
        }
    }
    let mut result = api::web::search_web(client, keyword, i64::from(page)).await?;
    result.channel = "web".to_string();
    result.results = rank_cards(result.results, keyword);
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
    run_search(
        &app.client,
        &keyword,
        page.unwrap_or(1),
        cursor,
        category.as_deref(),
    )
    .await
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

/// 把一集解析成一组「能播的流」（Rust 侧内部形状，带传输头）。
///
/// 与 [`resolve_play`] 的区别是这里多一个 `prefetch_rank`：它是**会话缓存在内存里的
/// 那一份**。前端拿到的仍是只含 url + 密钥的 DTO，边界没动；但转码要拉本片字节时，
/// 需要知道该带哪一套请求头，那套头不应该下给前端。下载引擎（`download`）续传/重试
/// 时也要重新解一条新鲜地址，故对 crate 内可见。
pub(crate) async fn assemble_streams(
    client: &reqwest::Client,
    vid: &str,
    series: &str,
) -> Result<(Vec<MediaVariant>, Option<String>), CommandError> {
    let catalogue = match catalogue_of(vid) {
        Catalogue::Hongguo => catalogue_of(series),
        other => other,
    };
    if catalogue != Catalogue::Hongguo {
        let source = match catalogue {
            Catalogue::Hema => api::hema::play_source(client, vid, series).await?,
            Catalogue::Xifan => api::xifan::play_source(client, vid, series).await?,
            Catalogue::Hongguo => unreachable!(),
        };
        return Ok((
            vec![MediaVariant {
                url: source.url,
                content_key: None,
                codec: source.codec,
                quality: source.quality,
                width: source.width,
                height: source.height,
                source: if catalogue == Catalogue::Hema {
                    "hema"
                } else {
                    "xifan"
                }
                .to_string(),
                vid: vid.to_string(),
                user_agent: api::web::WEB_UA.to_string(),
                referer: None,
            }],
            None,
        ));
    }

    let mut streams: Vec<MediaVariant> = Vec::new();
    let mut note: Option<String> = None;

    // The website's public MP4 needs no key at all, so it is worth a try first —
    // measured 2026-10-26 it exists for 0 of 10 hot titles, but where the site
    // does expose it, playback is free of both signing and decryption.
    match api::web::fetch_public_play_url(client, series, vid).await {
        Ok(Some(source)) => streams.push(MediaVariant {
            url: source.url,
            content_key: None,
            codec: "h264".to_string(),
            quality: String::new(),
            width: 0,
            height: 0,
            source: "web".to_string(),
            vid: vid.to_string(),
            user_agent: api::web::WEB_UA.to_string(),
            referer: Some(api::web::WEB_REFERER.to_string()),
        }),
        Ok(None) => note = Some("官网只提供预览分集，改用 App 片源".to_string()),
        Err(error) => note = Some(error.to_string()),
    }

    if streams.is_empty() {
        for variant in api::app_video::fetch_app_play_url(client, vid).await? {
            streams.push(variant);
        }
    }

    if streams.is_empty() {
        return Err(ApiError::Business {
            message: "本集暂无可用片源".to_string(),
            code: "NO_PLAYABLE_SOURCE".to_string(),
        }
        .into());
    }
    Ok((streams, note))
}

/// Resolve an episode into the streams the player can try, best first.
///
/// 会话级片单缓存就在这一层命中：预取过的集切过来不必重新签一次接口（那一段
/// 实测约一秒，与网络无关）。`force_refresh` 是「换一条清晰度试试」那条路——
/// 缓存里的地址可能已经被 CDN 判死，绕开它重新解析。
#[tauri::command]
pub async fn resolve_play(
    app: State<'_, App>,
    cache: State<'_, PlayCache>,
    vid: String,
    series_id: Option<String>,
    force_refresh: Option<bool>,
) -> Result<PlayResolution, CommandError> {
    let series = series_id.unwrap_or_default();
    let force = force_refresh.unwrap_or(false);
    if !force && let Some((variants, note)) = cache.get(&series, &vid) {
        return Ok(PlayResolution {
            vid,
            streams: variants.into_iter().map(stream_dto).collect(),
            note,
        });
    }
    let (variants, note) = assemble_streams(&app.client, &vid, &series).await?;
    cache.put(&series, &vid, variants.clone(), note.clone());
    Ok(PlayResolution {
        vid,
        streams: variants.into_iter().map(stream_dto).collect(),
        note,
    })
}

fn stream_dto(variant: MediaVariant) -> PlayableStream {
    PlayableStream {
        encrypted: variant.content_key.is_some(),
        content_key: variant.content_key,
        url: variant.url,
        codec: variant.codec,
        quality: variant.quality,
        width: variant.width,
        height: variant.height,
        source: variant.source,
    }
}

/// 后续几集的预取：提前把「签接口 + 取回源地址 + 解 spade」跑完，结果只进
/// Rust 的会话缓存。
///
/// 返回值故意只是一个条数：地址与密钥不因为「预取」就多出一条下发通道，前端
/// 仍然只能靠 `resolve_play` 拿到当前那一份。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrefetchReport {
    /// 成功进缓存的集数。
    pub cached: u32,
    /// 因为来源报错而没预取成的集数（不报具体原因：那些原因属于播放侧的语义）。
    pub failed: u32,
}

#[tauri::command]
pub async fn prefetch_play(
    app: State<'_, App>,
    cache: State<'_, PlayCache>,
    vids: Vec<String>,
    series_id: Option<String>,
) -> Result<PrefetchReport, CommandError> {
    let series = series_id.unwrap_or_default();
    let mut cached = 0u32;
    let mut failed = 0u32;
    for vid in vids.into_iter().take(MAX_PREFETCH) {
        if vid.is_empty() || cache.get(&series, &vid).is_some() {
            continue;
        }
        match assemble_streams(&app.client, &vid, &series).await {
            Ok((variants, note)) => {
                if cache.get(&series, &vid).is_none() {
                    cache.put(&series, &vid, variants, note);
                }
                cached += 1;
            }
            Err(_) => failed += 1,
        }
    }
    Ok(PrefetchReport { cached, failed })
}

/// 丢掉某一集的会话缓存（播放器探到一条流拉不动时告诉 Rust 别再拿它）。
#[tauri::command]
pub async fn drop_prefetch(
    cache: State<'_, PlayCache>,
    series_id: String,
    vid: String,
) -> Result<bool, CommandError> {
    Ok(cache.remove(&series_id, &vid))
}

// ── 兼容转码（HEVC → H.264）───────────────────────────────────────────────

/// 转码工具与产物占用一览。前端据此决定按钮文案，不感知 FFmpeg 的存在。
#[tauri::command]
pub async fn transcode_status(layout: State<'_, Layout>) -> Result<ToolStatus, CommandError> {
    Ok(transcode::status(&layout))
}

/// 首次需要时按需拉取转码工具（随包不带）。幂等：本地已有则立即返回。
/// 下载进度经 `transcode-progress` 事件出去，命令本身只回最终状态。
#[tauri::command]
pub async fn prepare_transcode(
    app: AppHandle,
    state: State<'_, App>,
    layout: State<'_, Layout>,
    job: String,
) -> Result<ToolStatus, CommandError> {
    transcode::prepare(&state.client, &layout, &job, |event| {
        let _ = app.emit("transcode-progress", &event);
    })
    .await
    .map_err(CommandError::from)?;
    Ok(transcode::status(&layout))
}

/// 把一集转成浏览器能硬解的 H.264。
///
/// 片源仍从会话缓存或现解析拿（与 `resolve_play` 同一份语义，边界不动）；挑一条
/// 匹配 `quality` 的流，缺工具时先就地按需下载，下载与转码进度都经 `transcode-progress`
/// 事件出去。产物只回本地路径，播放仍走前端既有链路。
#[tauri::command]
pub async fn transcode_episode(
    app: AppHandle,
    state: State<'_, App>,
    layout: State<'_, Layout>,
    cache: State<'_, PlayCache>,
    job: String,
    vid: String,
    series_id: Option<String>,
    quality: Option<String>,
) -> Result<TranscodeOutput, CommandError> {
    let series = series_id.unwrap_or_default();
    let variants = match cache.get(&series, &vid) {
        Some((variants, _)) => variants,
        None => {
            let (variants, note) = assemble_streams(&state.client, &vid, &series).await?;
            cache.put(&series, &vid, variants.clone(), note);
            variants
        }
    };
    let want = quality.unwrap_or_default();
    let variant = variants
        .iter()
        .find(|stream| !want.is_empty() && stream.quality == want)
        .or_else(|| variants.iter().find(|stream| stream.content_key.is_some()))
        .or(variants.first())
        .cloned()
        .ok_or_else(|| CommandError {
            code: "NO_PLAYABLE_SOURCE".to_string(),
            hint: Some("换一集或换一条清晰度再试".to_string()),
            message: "本集没有可转码的片源".to_string(),
        })?;

    let handle = app.clone();
    let tool = transcode::prepare(&state.client, &layout, &job, |event| {
        let _ = handle.emit("transcode-progress", &event);
    })
    .await
    .map_err(CommandError::from)?;

    let input = MediaInput {
        url: variant.url.clone(),
        content_key: variant.content_key.clone(),
        user_agent: variant.user_agent.clone(),
        referer: variant.referer.clone(),
    };
    let output = layout.output_path(&vid, &variant.quality, variant.content_key.as_deref());
    let handle = app.clone();
    transcode::transcode(
        &state.client,
        &layout,
        &tool,
        &job,
        &input,
        &output,
        |event| {
            let _ = handle.emit("transcode-progress", &event);
        },
    )
    .await
    .map_err(CommandError::from)
}

/// 取消一路转码（下载或编码）。子进程由 Rust 侧的看门狗终止，前端只管发信号。
#[tauri::command]
pub async fn cancel_transcode(job: String) -> Result<bool, CommandError> {
    Ok(transcode::cancel(&job))
}

// ── 下载到电脑 ────────────────────────────────────────────────────────────

/// 前端排进下载队列的一集：只有编号与集号，没有任何地址或密钥。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadEpisodeInput {
    pub vid: String,
    pub vid_index: u32,
}

/// 当前所有下载任务（含已完成/失败/取消），供「我的」页与详情页渲染。
#[tauri::command]
pub async fn download_snapshot(
    manager: State<'_, DownloadManager>,
) -> Result<Vec<DownloadTask>, CommandError> {
    Ok(manager.snapshot())
}

/// 弹出原生「选择保存目录」对话框，由 Rust 侧完成，前端与 capability 都不碰文件系统。
/// 取消选择回空串（前端据此不再发起下载）。
#[tauri::command]
pub async fn choose_download_dir(app: AppHandle) -> Result<String, CommandError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().pick_folder(move |folder| {
        let _ = tx.send(folder.map(|path| path.to_string()));
    });
    rx.await
        .map_err(|_| CommandError {
            code: "DIALOG".to_string(),
            hint: None,
            message: "目录选择被中断".to_string(),
        })
        .map(|picked| picked.unwrap_or_default())
}

/// 把一部剧的分集整批排进下载队列并起 worker。已在跑/已完成的不重复拉起。
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn start_series_download(
    app: AppHandle,
    manager: State<'_, DownloadManager>,
    series_id: String,
    series_title: String,
    cover: String,
    category: Option<String>,
    quality: Option<String>,
    episodes: Vec<DownloadEpisodeInput>,
    dest_dir: String,
) -> Result<Vec<DownloadTask>, CommandError> {
    if dest_dir.trim().is_empty() {
        return Err(CommandError {
            code: "INPUT".to_string(),
            hint: Some("先选一个保存目录".to_string()),
            message: "还没有选择保存位置".to_string(),
        });
    }
    let quality = quality.unwrap_or_default();
    let category = category.unwrap_or_default();
    let metas = episodes
        .into_iter()
        .filter(|episode| !episode.vid.is_empty())
        .map(|episode| DownloadTask {
            id: episode.vid,
            series_id: series_id.clone(),
            series_title: series_title.clone(),
            cover: cover.clone(),
            category: category.clone(),
            vid_index: episode.vid_index,
            quality: quality.clone(),
            dest_path: String::new(),
            stage: crate::download::stage::QUEUED.to_string(),
            bytes_done: 0,
            bytes_total: None,
            error: None,
            updated_at: api::now_millis(),
        })
        .collect::<Vec<_>>();
    if metas.is_empty() {
        return Err(CommandError {
            code: "INPUT".to_string(),
            hint: Some("这一部没有可下载的分集".to_string()),
            message: "没有可下载的分集".to_string(),
        });
    }
    Ok(manager.begin(&app, metas, &dest_dir))
}

/// 继续一条已暂停/失败/取消的任务（从断点续传）。
#[tauri::command]
pub async fn resume_download(
    app: AppHandle,
    manager: State<'_, DownloadManager>,
    id: String,
) -> Result<bool, CommandError> {
    Ok(manager.resume(&app, &id))
}

/// 暂停一条在跑或排队中的任务，保留已下字节供续传。
#[tauri::command]
pub async fn pause_download(
    manager: State<'_, DownloadManager>,
    id: String,
) -> Result<bool, CommandError> {
    Ok(manager.pause(&id))
}

/// 取消一条任务并删掉它的半截字节（成品不动）。
#[tauri::command]
pub async fn cancel_download(
    manager: State<'_, DownloadManager>,
    id: String,
) -> Result<bool, CommandError> {
    Ok(manager.cancel(&id))
}

/// 从列表里移除一条已结束的任务，`delete_file` 为真时连带删本地成品。
#[tauri::command]
pub async fn forget_download(
    manager: State<'_, DownloadManager>,
    id: String,
    delete_file: Option<bool>,
) -> Result<bool, CommandError> {
    Ok(manager.forget(&id, delete_file.unwrap_or(false)))
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
        // 追剧检测的三个字段不由播放上报提供：它们只由 `check_series_update` /
        // `acknowledge_series` 写，[`history::Store::record`] 会把旧行的值接过来。
        known_total: 0,
        acknowledged_total: 0,
        last_checked_at: 0,
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

/// 一部剧的追剧检测结果：新集 + 续作。
///
/// `known_total` / `acknowledged_total` 直接送回表里算好的两个数字，前端不
/// 自己推「看了几集」——那需要知道确认只能往前走这条规则，而它在 Rust 侧。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesUpdate {
    pub series_id: String,
    pub series_title: String,
    pub known_total: u32,
    pub acknowledged_total: u32,
    /// 已确认之后还有几集没看（角标上的数字）。
    pub new_episodes: u32,
    pub sequels: Vec<crate::api::rank::SequelCandidate>,
    /// 非致命提示：节流期内直接回缓存、该来源不做续作检测、来源查不到等。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// 批量检查返回的一行：只给角标需要的数字，不带续作列表。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSummary {
    pub series_id: String,
    pub known_total: u32,
    pub acknowledged_total: u32,
    pub new_episodes: u32,
    pub sequels: u32,
    /// 这一部本轮没能联网查到（失败不写检查时间，下次还能重试）。
    pub failed: bool,
}

/// 续作检测最多拉几部：一部剧的搜索结果页常有几十条同族候选，角标只关心前几部。
const MAX_SEQUELS_SHOWN: usize = 3;
/// 批量检查上限：再多就把一次刷新变成对来源的一轮冲击。
const MAX_BATCH_CHECKS: usize = 20;

/// 对一部剧跑一次追剧检测（刷分集页 + 搜同族续作）。
///
/// 续作只走已归一的同一基础名：河马/西饭的目录没有可靠的季数语义，也不共享
/// 官网的搜索接口，所以那两路只报新集不报续作。
async fn detect_series_update(
    app: &App,
    store: &history::Store,
    entry: &HistoryEntry,
    fresh_total: Option<u32>,
) -> Result<SeriesUpdate, CommandError> {
    let catalogue = catalogue_of(&entry.series_id);
    let total = match fresh_total {
        Some(total) => total,
        None => match catalogue {
            Catalogue::Hema => {
                api::hema::episode_list(&app.client, &entry.series_id)
                    .await?
                    .total
            }
            Catalogue::Xifan => {
                api::xifan::episode_list(&app.client, &entry.series_id)
                    .await?
                    .total
            }
            Catalogue::Hongguo => {
                api::web::fetch_episode_list(&app.client, &entry.series_id)
                    .await?
                    .total
            }
        },
    };
    // 基线只增不减在 [`history::Store::note_total`] 里面，失败时根本走不到这里。
    let now = api::now_millis();
    let row = store
        .note_total(&entry.series_id, total, now)
        .unwrap_or_else(|| entry.clone());
    // 从没确认过的新历史行：拿当前集号当基线，不然刚看过第 4 集会报「还有 56 集」。
    let acknowledged = row
        .acknowledged_total
        .max(row.vid_index.min(row.known_total));
    let mut sequels = Vec::new();
    let mut note = None;
    if catalogue == Catalogue::Hongguo && !entry.series_title.is_empty() {
        let keyword = api::rank::family_base(&entry.series_title);
        match run_search(&app.client, &keyword, 1, None, None).await {
            Ok(result) => {
                let candidates: Vec<api::rank::SequelCandidate> = result
                    .results
                    .iter()
                    .filter(|card| catalogue_of(&card.series_id) == catalogue)
                    .map(|card| api::rank::SequelCandidate {
                        id: card.series_id.clone(),
                        title: card.series_title.clone(),
                    })
                    .collect();
                sequels = api::rank::find_sequels(&entry.series_title, &candidates)
                    .into_iter()
                    .take(MAX_SEQUELS_SHOWN)
                    .collect();
            }
            // 续作检测失败不影响新集数字：基线已经落了盘，下一轮再试。
            Err(error) => note = Some(format!("续作检测未成功：{}", error.message)),
        }
    } else if catalogue != Catalogue::Hongguo {
        note = Some("该片源不做续作检测".to_string());
    }
    Ok(SeriesUpdate {
        series_id: entry.series_id.clone(),
        series_title: entry.series_title.clone(),
        known_total: row.known_total,
        acknowledged_total: acknowledged,
        new_episodes: row.known_total.saturating_sub(acknowledged),
        sequels,
        note,
    })
}

/// 检查某一部的更新（详情页/「我的」页卡片的「检查更新」）。
#[tauri::command]
pub async fn check_series_update(
    app: State<'_, App>,
    store: State<'_, history::Store>,
    series_id: String,
    force: Option<bool>,
) -> Result<SeriesUpdate, CommandError> {
    let entry = store.latest_for(&series_id).ok_or_else(|| CommandError {
        code: "NO_HISTORY".to_string(),
        hint: Some("先看一集再检查更新".to_string()),
        message: "这部剧不在观看历史里".to_string(),
    })?;
    if !force.unwrap_or(false) {
        if let history::UpdateState::Cached {
            known_total,
            unread,
        } = store.update_state(&series_id, api::now_millis())
        {
            return Ok(SeriesUpdate {
                series_id,
                series_title: entry.series_title,
                known_total,
                acknowledged_total: known_total.saturating_sub(unread),
                new_episodes: unread,
                sequels: Vec::new(),
                note: Some("二十四小时内刚查过，这是上次结果".to_string()),
            });
        }
    }
    detect_series_update(&app, &store, &entry, None).await
}

/// 用户点开看了，把角标清掉。返回清完之后的状态，前端不用自己算。
#[tauri::command]
pub async fn acknowledge_series(
    store: State<'_, history::Store>,
    series_id: String,
    vid_index: Option<u32>,
) -> Result<Option<ProgressRow>, CommandError> {
    Ok(store
        .acknowledge(&series_id, vid_index.unwrap_or(u32::MAX))
        .map(ProgressRow::from))
}

/// 批量检查更新（启动后静默刷一次角标）。
///
/// 逐条失败只记 `failed`，不让一部查不到拖垮整个列表；失败也不写检查时间。
#[tauri::command]
pub async fn check_updates(
    app: State<'_, App>,
    store: State<'_, history::Store>,
    series_ids: Vec<String>,
    force: Option<bool>,
) -> Result<Vec<UpdateSummary>, CommandError> {
    let force = force.unwrap_or(false);
    let mut rows = Vec::new();
    for series_id in series_ids.into_iter().take(MAX_BATCH_CHECKS) {
        let Some(entry) = store.latest_for(&series_id) else {
            continue;
        };
        if !force {
            if let history::UpdateState::Cached {
                known_total,
                unread,
            } = store.update_state(&series_id, api::now_millis())
            {
                rows.push(UpdateSummary {
                    series_id,
                    known_total,
                    acknowledged_total: known_total.saturating_sub(unread),
                    new_episodes: unread,
                    sequels: 0,
                    failed: false,
                });
                continue;
            }
        }
        match detect_series_update(&app, &store, &entry, None).await {
            Ok(update) => rows.push(UpdateSummary {
                series_id: update.series_id,
                known_total: update.known_total,
                acknowledged_total: update.acknowledged_total,
                new_episodes: update.new_episodes,
                sequels: update.sequels.len() as u32,
                failed: false,
            }),
            Err(_) => rows.push(UpdateSummary {
                series_id,
                known_total: entry.known_total,
                acknowledged_total: entry.acknowledged_total,
                new_episodes: entry.unread(),
                sequels: 0,
                failed: true,
            }),
        }
    }
    Ok(rows)
}

impl From<HistoryEntry> for ProgressRow {
    fn from(entry: HistoryEntry) -> Self {
        let finished = entry.finished();
        let resumable = entry.resumable();
        ProgressRow {
            entry,
            finished,
            resumable,
        }
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

/// 删除某一部的观看记录（「我的」页历史卡上的 ✕）。
#[tauri::command]
pub async fn remove_history(
    store: State<'_, history::Store>,
    series_id: String,
) -> Result<(), CommandError> {
    store.remove(&series_id);
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
            hint: None,
            message: "缺少剧集 ID，无法收藏".to_string(),
        })
}

/// 收藏清单，最新点亮在前。历史页把最近观看与收藏融合展示时用它。
#[tauri::command]
pub async fn list_favorites(
    store: State<'_, favorites::Store>,
) -> Result<Vec<FavoriteEntry>, CommandError> {
    Ok(store.list())
}

/// 这一部是否已收藏；详情页的收藏按钮用它初始化。
#[tauri::command]
pub async fn is_favorited(
    store: State<'_, favorites::Store>,
    series_id: String,
) -> Result<bool, CommandError> {
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
    use super::Catalogue;
    use super::catalogue_of;

    #[test]
    fn ids_route_by_their_own_prefix() {
        assert_eq!(catalogue_of("hema:41000448669"), Catalogue::Hema);
        assert_eq!(catalogue_of("hema:41000448669:618017066"), Catalogue::Hema);
        assert_eq!(catalogue_of("xifan:jiuzhou:23280"), Catalogue::Xifan);
        assert_eq!(
            catalogue_of("xifan:jiuzhou:23280:1339243"),
            Catalogue::Xifan
        );
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
