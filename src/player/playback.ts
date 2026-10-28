/**
 * Turn a resolved stream into something a `<video>` element can play.
 *
 * The webview is given exactly two secrets for the episode it is about to watch —
 * the media address and its content key — and nothing else: no signing, no device
 * profile, no endpoint table. Everything above this file deals in business
 * objects; everything below it deals in bytes.
 *
 * Flow: download (with progress) → plan the container rewrite → decrypt sample by
 * sample → hand back an object URL. The whole file has to be in memory because
 * the metadata that describes the samples lives in `moov`, which sits before them,
 * and because the only thing a plain `<video>` element understands is a complete
 * file. A 1080p episode is ~27 MB, which is unremarkable for a desktop window.
 */

import type { PlayableStream } from "../ipc/client";
import { PlayerError, hexToBytes, planCencFile, type Bytes } from "./mp4";
import { decryptCencFile, type Progress } from "./cenc";

export interface Stage {
  /**
   * 界面只看这个词，所以它必须是中性动词。「处理」这一段在加密流上是逐样本
   * 还原，在明文流上根本不存在；界面上没有任何地方需要知道这件事。
   */
  readonly stage: "下载" | "处理" | "就绪";
  readonly done: number;
  readonly total: number;
}

export interface PreparedVideo {
  /**
   * 喂给 `<video>` 的地址。明文流就是来源给的那条地址，整集不进内存；
   * 需要处理的流是本地 object URL。
   */
  readonly url: string;
  /** 只有本地 object URL 能 revoke；直连地址 revoke 会打穿正在播的视频。 */
  readonly local: boolean;
  readonly mime: string;
  /** 本地文件的字节数；直连时为 0，长度只有来源知道。 */
  readonly bytes: number;
  readonly ms: number;
  /** 技术摘要，只给 lab 页和自检用；应用界面不打印它。 */
  readonly summary: string;
}

export type StageListener = (stage: Stage) => void;

/** Percentage helper for the UI; total 0 means the server hid the length. */
function ratio(done: number, total: number): number {
  return total > 0 ? Math.min(1, done / total) : 0;
}

export function stagePercent(stage: Stage): number {
  return Math.round(ratio(stage.done, stage.total) * 100);
}

async function download(url: string, onStage: StageListener): Promise<Bytes> {
  // `referrerPolicy: "no-referrer"` is load-bearing, not hygiene. Measured
  // 2026-10-26 against the media CDN: the same signed URL answers 206 with no
  // Referer, 206 with the App User-Agent, 206 with an Origin — and 403
  // (openresty, 552-byte HTML) the moment a Referer is present. A webview
  // sends its own document URL as Referer on cross-origin requests, so without
  // this the whole playback path is a 403. The CDN does not care which UA
  // fetches the bytes, so the browser default is fine here.
  const response = await fetch(url, { credentials: "omit", referrerPolicy: "no-referrer" });
  if (!response.ok) {
    throw new PlayerError(`下载失败 HTTP ${response.status}${response.statusText ? ` ${response.statusText}` : ""}`);
  }
  const declared = Number(response.headers.get("content-length") ?? 0);
  if (response.body === null) {
    const buffer = await response.arrayBuffer();
    onStage({ stage: "下载", done: buffer.byteLength, total: buffer.byteLength });
    return new Uint8Array(buffer);
  }
  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let done = 0;
  for (; ;) {
    const { value, done: finished } = await reader.read();
    if (finished) break;
    if (value === undefined || value.length === 0) continue;
    chunks.push(value);
    done += value.length;
    onStage({ stage: "下载", done, total: declared });
  }
  const bytes = new Uint8Array(done);
  let cursor = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, cursor);
    cursor += chunk.length;
  }
  return bytes;
}

/**
 * Ask the decoder stack whether it can handle this codec at all. HEVC is the
 * common answer for these streams and it is not universal: hardware support is
 * what decides it, so probing beats assuming.
 *
 * 调试开关：localStorage 里 `duanju.sim-no-hevc=1` 时把 HEVC 一律判为不可播，
 * 用于在有硬解的机器上演示「缺扩展」的引导与转码入口。正式使用没人写这个键。
 */
export function codecSupported(mime: string): boolean {
  if (typeof document === "undefined") return true;
  if (isHevcMime(mime) && typeof localStorage !== "undefined" && localStorage.getItem("duanju.sim-no-hevc") === "1") {
    return false;
  }
  const probe = document.createElement("video");
  const verdict = probe.canPlayType(mime);
  return verdict === "probably" || verdict === "maybe";
}

function isHevcMime(mime: string): boolean {
  const lowered = mime.toLowerCase();
  // 探测用的 RFC 6381 串是 hvc1 前缀（见 mimeFor），品牌标签别漏掉。
  return lowered.includes("hev") || lowered.includes("hvc") || lowered.includes("h265") || lowered.includes("bytevc");
}

/**
 * The backend reports a codec family (`hevc`, `avc`); `canPlayType` wants a
 * concrete RFC 6381 string, so probe a conservative profile of the family.
 */
export function mimeFor(stream: Pick<PlayableStream, "codec">): string {
  const family = stream.codec.toLowerCase();
  const profile =
    family.includes("avc") || family.includes("h264")
      ? "avc1.640028"
      : family.includes("hev") || family.includes("h265") || family.includes("bytevc")
        ? "hvc1.1.6.L93.B0"
        : family.includes("av1")
          ? "av01.0.08M.08"
          : "";
  return profile.length === 0 ? "video/mp4" : `video/mp4; codecs="${profile}"`;
}

/**
 * Prepare one stream for playback.
 *
 * 明文流直接把地址交给 `<video>`：整集可以是 180 MB，把它整块拉进内存再造一个
 * Blob 没有任何收益，进度条反而要等下载完成才会动。需要处理的流必须先进内存
 * ——描述样本的元数据在样本前面，只有拿到完整文件才能逐段还原。
 */
export async function prepareStream(stream: PlayableStream, onStage: StageListener): Promise<PreparedVideo> {
  const started = Date.now();
  if (!stream.encrypted) {
    onStage({ stage: "就绪", done: 0, total: 0 });
    return {
      url: stream.url,
      local: false,
      mime: "video/mp4",
      bytes: 0,
      ms: 0,
      summary: "来源直连",
    };
  }
  const bytes = await download(stream.url, onStage);
  if (stream.contentKey === undefined || stream.contentKey === "") {
    throw new PlayerError("这一集的片源不完整");
  }
  const plan = planCencFile(bytes);
  const progress = (p: Progress) => onStage({ stage: "处理", done: p.done, total: p.total });
  const plain = await decryptCencFile(plan, hexToBytes(stream.contentKey), progress);
  const blob = new Blob([plain], { type: "video/mp4" });
  const ms = Date.now() - started;
  onStage({ stage: "就绪", done: plan.totalBytes, total: plan.totalBytes });
  return {
    url: URL.createObjectURL(blob),
    local: true,
    mime: "video/mp4",
    bytes: plain.length,
    ms,
    summary: plan.tracks
      .map((track) => `${track.kind === "video" ? "视频" : "音频"} ${track.originalType} ${track.samples.length} 帧`)
      .join("，"),
  };
}

export function revokePrepared(prepared: PreparedVideo | null): void {
  if (prepared !== null && prepared.local) URL.revokeObjectURL(prepared.url);
}

export function isPlayerError(error: unknown): error is PlayerError {
  return error instanceof PlayerError;
}

/** Short, user-facing description of whatever went wrong. */
export function describeError(error: unknown): string {
  if (isPlayerError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return String(error);
}
