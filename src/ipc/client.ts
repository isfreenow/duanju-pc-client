/**
 * The webview's whole view of the backend.
 *
 * Every type here is a business object: what to render, and — for the episode the
 * user chose — where its bytes are. The signing suite, the spade key derivation,
 * the App hosts and the request headers stay in Rust; `scripts/check-leaks.cjs`
 * fails the build if any of that vocabulary shows up in this directory tree.
 *
 * The DTOs mirror `src-tauri/src/commands.rs` and `src-tauri/src/api/*` exactly
 * (serde `rename_all = "camelCase"`), so a mismatch is a compile error rather
 * than an `undefined` at runtime.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export interface PublicMetadata {
  episodeLabel: string;
  completionStatus: string;
  publishedAt?: number;
}

export interface SeriesCard extends PublicMetadata {
  seriesId: string;
  seriesTitle: string;
  cover: string;
  episodeCount: number;
  tags: string[];
  url: string;
  description?: string;
  rank?: number;
  heat?: string;
}

export interface Genre {
  slug: string;
  label: string;
}

export interface CatalogPage {
  results: SeriesCard[];
  page: number;
  total: number;
  totalPages: number;
  genres: Genre[];
  pageTitle: string;
  sourceUrl: string;
  unsupported?: boolean;
  emptyMessage?: string;
}

export interface Episode {
  vid: string;
  vidIndex: number;
  title: string;
  seriesId: string;
  seriesTitle: string;
  cover: string;
  webAvailable?: boolean;
  /** 前端侧的路由信息：从哪个页签进来的，观看记录就记在哪个类目下。 */
  category?: string;
}

export interface EpisodeList {
  seriesId: string;
  seriesTitle: string;
  cover: string;
  total: number;
  webAccessibleEpisodes?: number;
  episodes: Episode[];
  source: string;
}

export interface Review {
  author: string;
  avatar: string;
  content: string;
  createdAt?: number;
  likes?: number;
  rating?: number;
}

export interface ReviewBlock {
  total?: number;
  items: Review[];
}

export interface PublicDetails extends PublicMetadata {
  seriesId: string;
  episodeId?: string;
  /** 剧情简介，三个渠道都从详情里取；没有就不下发。 */
  description?: string;
  rating?: number;
  ratingCount?: number;
  likeCount?: number;
  favoriteCount?: number;
  reviews: ReviewBlock;
  episodeReviews?: ReviewBlock;
  episodeLikeCount?: number;
  sourceUrl: string;
}

export interface SearchPageCursor {
  searchId?: string;
  passback?: string;
  nextOffset?: number;
}

export interface SearchResult {
  results: SeriesCard[];
  page: number;
  total: number;
  totalPages: number;
  hasMore: boolean;
  nextPage?: number;
  channel: string;
  cursor?: SearchPageCursor;
  note?: string;
}

/**
 * One candidate stream for the episode being played. `url` and `contentKey` are
 * the two values the browser decryptor needs and nothing else: they are short
 * lived, per-episode, and never cached.
 */
export interface PlayableStream {
  url: string;
  contentKey?: string;
  codec: string;
  quality: string;
  width: number;
  height: number;
  source: string;
  encrypted: boolean;
}

export interface PlayResolution {
  vid: string;
  streams: PlayableStream[];
  note?: string;
}

/** The error shape `commands::CommandError` serialises to. */
export interface CommandError {
  code: string;
  message: string;
  /**
   * 失败时的「下一步动作」。只有少数几类错误带它（缺 HEVC 组件、没先看过就查
   * 更新……）；带 hint 时界面按它给具体指引，而不是统一一句「播放出错」。
   */
  hint?: string;
}

/**
 * 一处播放进度。字段来自 `commands::ProgressRow`：Rust 已经算好「看完没看完」
 * 和「值不值得续播」，前端只读结果，不再各自定阈值。
 * 这里没有任何播放地址或密钥——历史里只可能有剧名、集号和秒数。
 */
export interface ProgressRow {
  seriesId: string;
  seriesTitle: string;
  cover: string;
  vid: string;
  vidIndex: number;
  seconds: number;
  duration: number;
  updatedAt: number;
  finished: boolean;
  resumable: boolean;
  category: string;
}

export interface ProgressInput {
  seriesId: string;
  seriesTitle: string;
  cover: string;
  vid: string;
  vidIndex: number;
  seconds: number;
  duration: number;
  /** 进来时所在的顶栏页签，列表页的「继续观看」按它过滤。 */
  category?: string;
}

function isCommandError(value: unknown): value is CommandError {
  return (
    typeof value === "object" &&
    value !== null &&
    typeof (value as CommandError).code === "string" &&
    typeof (value as CommandError).message === "string"
  );
}

/** Turn whatever Tauri rejected into something the UI can print. */
export function errorText(error: unknown): string {
  if (isCommandError(error)) return `${error.code} · ${error.message}`;
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  return JSON.stringify(error);
}

/**
 * 失败时的「下一步动作」文案；没有就返回空串，界面回落到通用提示。
 * 与 [`errorText`] 分开，是因为 message 归 message（是什么错了）、hint 归 hint
 * （接下来能做什么），两者在 UI 上占不同的位置。
 */
export function errorHint(error: unknown): string {
  return isCommandError(error) ? error.hint ?? "" : "";
}

/** 一条命令失败的完整描述：`message`（可能带 hint），供只有一行文案的落点用。 */
export function errorWithHint(error: unknown): string {
  const text = errorText(error);
  const hint = errorHint(error);
  return hint.length > 0 ? `${text} — ${hint}` : text;
}

/**
 * IPC 失败的带类型版本：保留 Rust 侧的 `code` 与 `hint`，让视图能为某一类
 * 错误定制下一步（缺 HEVC 就引导装组件、没先看过就提示先看一集），而不只是
 * 把整条错误拼成一句文案。
 */
export class IpcError extends Error {
  readonly code: string;
  readonly hint: string;
  constructor(code: string, message: string, hint = "") {
    super(message);
    this.name = "IpcError";
    this.code = code;
    this.hint = hint;
  }
}

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (typeof window === "undefined" || !("__TAURI_INTERNALS__" in window)) {
    throw new Error("这个页面需要在桌面客户端里运行（找不到 Tauri 运行时）");
  }
  try {
    return await invoke<T>(command, args);
  } catch (error) {
    if (isCommandError(error)) throw new IpcError(error.code, error.message, error.hint ?? "");
    throw new Error(errorText(error));
  }
}

export function browseList(category: string, genre = "", page = 1, forceRefresh = false): Promise<CatalogPage> {
  return call<CatalogPage>("browse_list", { category, genre, page, forceRefresh });
}

/**
 * 搜索跟着当前大类走：红果走官网接口，河马/西饭走各自来源的搜索。
 * `category` 就是顶栏那一个 tab 的 key，前端不需要知道任何域名。
 */
export function searchSeries(
  keyword: string,
  page = 1,
  cursor?: SearchPageCursor,
  category = "",
): Promise<SearchResult> {
  return call<SearchResult>("search_series", { keyword, page, cursor, category });
}

export function getSeriesEpisodes(seriesId: string): Promise<EpisodeList> {
  return call<EpisodeList>("get_series_episodes", { seriesId });
}

export function getPublicDetails(seriesId: string, vid?: string): Promise<PublicDetails> {
  return call<PublicDetails>("get_public_details", { seriesId, vid });
}

export function resolvePlay(vid: string, seriesId?: string, forceRefresh = false): Promise<PlayResolution> {
  return call<PlayResolution>("resolve_play", { vid, seriesId, forceRefresh });
}

/**
 * 后续几集的预取：提前把「签接口 + 取回源地址 + 解 spade」跑完，结果只进 Rust 的
 * 会话缓存。返回只有条数（cached/failed）——地址不会因为「预取」多一条下发通道。
 */
export interface PrefetchReport {
  cached: number;
  failed: number;
}

export function prefetchPlay(vids: string[], seriesId?: string): Promise<PrefetchReport> {
  return call<PrefetchReport>("prefetch_play", { vids, seriesId });
}

/** 播放器探到一条流拉不动时，告诉 Rust 别再拿这一集的缓存。 */
export function dropPrefetch(seriesId: string, vid: string): Promise<boolean> {
  return call<boolean>("drop_prefetch", { seriesId, vid });
}

/**
 * 一部剧的追剧检测结果（`commands::SeriesUpdate`）：新集数 + 续作候选。
 * `acknowledgedTotal` 是 Rust 算好的「已确认看到第几集」，前端不自推。
 */
export interface SequelCandidate {
  id: string;
  title: string;
}

export interface SeriesUpdate {
  seriesId: string;
  seriesTitle: string;
  knownTotal: number;
  acknowledgedTotal: number;
  newEpisodes: number;
  sequels: SequelCandidate[];
  note?: string;
}

/** 批量检查返回的一行（`commands::UpdateSummary`）：只给角标需要的数字。 */
export interface UpdateSummary {
  seriesId: string;
  knownTotal: number;
  acknowledgedTotal: number;
  newEpisodes: number;
  sequels: number;
  failed: boolean;
}

/** 检查某一部的更新（详情页/「我的」页卡片的「检查更新」）。 */
export function checkSeriesUpdate(seriesId: string, force = false): Promise<SeriesUpdate> {
  return call<SeriesUpdate>("check_series_update", { seriesId, force });
}

/** 用户点开看了，把角标清掉；返回清完之后的状态。 */
export function acknowledgeSeries(seriesId: string, vidIndex?: number): Promise<ProgressRow | null> {
  return call<ProgressRow | null>("acknowledge_series", { seriesId, vidIndex });
}

/** 批量检查更新（启动后静默刷一次角标）。 */
export function checkUpdates(seriesIds: string[], force = false): Promise<UpdateSummary[]> {
  return call<UpdateSummary[]>("check_updates", { seriesIds, force });
}

/**
 * 兼容转码（HEVC → H.264）：工具「首次需要时按需下载」，随包不带。
 * 前端只认状态与产物路径，不知道 FFmpeg 的存在。
 */
export interface ToolStatus {
  available: boolean;
  preparing: boolean;
  lastError?: string;
  encoder?: string;
  outputBytes: number;
}

export interface TranscodeOutput {
  /** 转码产物的本地文件路径（用 asset 协议喂给 `<video>`）。 */
  path: string;
  bytes: number;
  encoder: string;
  cached: boolean;
}

/** `transcode-progress` 事件的一帧（`transcode::ProgressEvent`）。 */
export interface TranscodeProgress {
  stage: "tool" | "input" | "encode" | string;
  job: string;
  percent: number;
  doneBytes?: number;
  totalBytes?: number;
  note?: string;
}

export function transcodeStatus(): Promise<ToolStatus> {
  return call<ToolStatus>("transcode_status");
}

export function prepareTranscode(job: string): Promise<ToolStatus> {
  return call<ToolStatus>("prepare_transcode", { job });
}

export function transcodeEpisode(
  job: string,
  vid: string,
  seriesId?: string,
  quality?: string,
): Promise<TranscodeOutput> {
  return call<TranscodeOutput>("transcode_episode", { job, vid, seriesId, quality });
}

export function cancelTranscode(job: string): Promise<boolean> {
  return call<boolean>("cancel_transcode", { job });
}

/** 订阅转码进度事件；返回取消订阅。纯浏览器预览下没有运行时，给一个空操作。 */
export async function onTranscodeProgress(
  handler: (event: TranscodeProgress) => void,
): Promise<() => void> {
  if (typeof window === "undefined" || !("__TAURI_INTERNALS__" in window)) return () => { };
  return listen<TranscodeProgress>("transcode-progress", (event) => handler(event.payload));
}

/** 上报一次播放位置；Rust 侧带节流，调用方按 timeupdate 直接发就行。 */
export function recordProgress(input: ProgressInput): Promise<boolean> {
  return call<boolean>("record_progress", {
    seriesId: input.seriesId,
    seriesTitle: input.seriesTitle,
    cover: input.cover,
    vid: input.vid,
    vidIndex: input.vidIndex,
    seconds: Math.max(0, Math.floor(input.seconds)),
    duration: Math.max(0, Math.floor(input.duration)),
    category: input.category ?? "",
  });
}

/** 带 `category` 只返回同一类目的记录；列表页的「继续观看」用它，历史页不传拿全量。 */
export function listHistory(category = ""): Promise<ProgressRow[]> {
  return call<ProgressRow[]>("list_history", { category });
}

/** 这一部看到的哪儿；没有看过就是 null。 */
export function latestProgress(seriesId: string): Promise<ProgressRow | null> {
  return call<ProgressRow | null>("latest_progress", { seriesId });
}

export function clearHistory(): Promise<void> {
  return call<void>("clear_history");
}

/** 删除某一部的观看记录（「我的」页历史卡上的 ✕）。 */
export function removeHistory(seriesId: string): Promise<void> {
  return call<void>("remove_history", { seriesId });
}

/**
 * 一条收藏。字段来自 `commands::list_favorites`（FavoriteEntry 直接序列化），
 * `updatedAt` 是点亮收藏的时间。和历史一样，这里没有任何播放地址或密钥。
 */
export interface FavoriteEntry {
  seriesId: string;
  seriesTitle: string;
  cover: string;
  category: string;
  updatedAt: number;
}

export interface FavoriteInput {
  seriesId: string;
  seriesTitle: string;
  cover: string;
  category?: string;
}

/** 收藏清单，最新点亮在前。 */
export function listFavorites(): Promise<FavoriteEntry[]> {
  return call<FavoriteEntry[]>("list_favorites");
}

/** 点亮或取消收藏，返回操作后是否为已收藏。 */
export function toggleFavorite(input: FavoriteInput): Promise<boolean> {
  return call<boolean>("toggle_favorite", {
    seriesId: input.seriesId,
    seriesTitle: input.seriesTitle,
    cover: input.cover,
    category: input.category ?? "",
  });
}

/** 这一部是否已收藏；详情页的收藏按钮用它初始化。 */
export function isFavorited(seriesId: string): Promise<boolean> {
  return call<boolean>("is_favorited", { seriesId });
}

/** 离开播放器时无条件落盘，绕过节流。 */
export function flushHistory(): Promise<void> {
  return call<void>("flush_history");
}

/**
 * 一条外部指令：命令行或环境变量带进来的那一句（Rust 侧 `launch::LaunchCommand`）。
 * 只有动作和参数，没有地址——去哪儿搜、播哪一集仍是各视图自己的事。
 */
export type LaunchCommand =
  | { action: "watch"; keyword: string }
  | { action: "series"; seriesId: string };

/** 取走冷启动带来的那条指令（取完即清，不会被路由两次）；没有就是 null。 */
export function takeLaunchCommand(): Promise<LaunchCommand | null> {
  return call<LaunchCommand | null>("take_launch_command");
}

/**
 * 订阅后续指令（程序已在跑时再次启动它）。返回取消订阅；纯浏览器预览下没有
 * 运行时，直接给一个空操作，不让 App 挂载就报错。
 */
export async function onLaunchCommand(
  handler: (command: LaunchCommand) => void,
): Promise<() => void> {
  if (typeof window === "undefined" || !("__TAURI_INTERNALS__" in window)) return () => { };
  return listen<LaunchCommand>("launch-command", (event) => handler(event.payload));
}
