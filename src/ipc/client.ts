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

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (typeof window === "undefined" || !("__TAURI_INTERNALS__" in window)) {
    throw new Error("这个页面需要在桌面客户端里运行（找不到 Tauri 运行时）");
  }
  try {
    return await invoke<T>(command, args);
  } catch (error) {
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

export function resolvePlay(vid: string, seriesId?: string): Promise<PlayResolution> {
  return call<PlayResolution>("resolve_play", { vid, seriesId });
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
