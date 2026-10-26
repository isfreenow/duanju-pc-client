/**
 * Script behind `player-lab.html`.
 *
 * Proves the shipped decrypt path inside a real browser: same-origin fixture →
 * container plan → WebCrypto decrypt → Blob → `<video>`, and then samples painted
 * pixels so "it played" cannot mean "it showed a black rectangle".
 *
 * Two things are deliberate because the page is often driven headlessly, where a
 * hidden tab throttles timers and freezes `requestAnimationFrame`: progress is
 * published to `window.__LAB_STATE__` on a wall-clock throttle, and the frame
 * sample waits on media events rather than animation frames.
 */

import { hexToBytes, planCencFile, describePlan, type Bytes } from "./mp4";
import { decryptCencFile } from "./cenc";

interface LabState {
  stage: string;
  detail: string;
  at: number;
}

interface LabResult extends LabState {
  ok: boolean;
  hevcProbe: string;
  summary: string;
  ms: number;
  bytes: number;
  videoWidth: number;
  videoHeight: number;
  duration: number;
  pixels: { mean: number; spread: number };
  error?: string;
}

const report = document.getElementById("report") as HTMLParagraphElement;
const log = document.getElementById("log") as HTMLPreElement;
const player = document.getElementById("player") as HTMLVideoElement;

declare global {
  interface Window {
    __LAB_STATE__?: LabState;
    __LAB__?: LabResult;
  }
}

const started = Date.now();
let lastPublish = 0;

/** Publish where the run is; a driver polls this instead of guessing. */
function say(stage: string, detail: string, force = false): void {
  const at = Date.now() - started;
  if (!force && at - lastPublish < 250) return;
  lastPublish = at;
  window.__LAB_STATE__ = { stage, detail, at };
  report.textContent = `${stage} · ${detail}`;
}

async function load(path: string): Promise<Bytes> {
  const response = await fetch(path);
  if (!response.ok) throw new Error(`${path} → HTTP ${response.status}`);
  return new Uint8Array(await response.arrayBuffer());
}

/** Wait for one event, with a hard ceiling so a silent failure still reports. */
function once(target: HTMLVideoElement, event: string, ms: number): Promise<void> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      target.removeEventListener(event, onEvent);
      reject(new Error(`等待 ${event} 超时（${ms}ms）`));
    }, ms);
    const onEvent = () => {
      clearTimeout(timer);
      target.removeEventListener(event, onEvent);
      resolve();
    };
    target.addEventListener(event, onEvent, { once: true });
  });
}

/**
 * Paint the current frame small and measure it. A black or frozen picture has a
 * near-zero spread across channels; a real frame does not.
 */
function measurePixels(video: HTMLVideoElement): { mean: number; spread: number } {
  const canvas = document.createElement("canvas");
  canvas.width = 64;
  canvas.height = 36;
  const context = canvas.getContext("2d");
  if (context === null) throw new Error("没有 2D 画布，无法取样");
  context.drawImage(video, 0, 0, canvas.width, canvas.height);
  const data = context.getImageData(0, 0, canvas.width, canvas.height).data;
  const luma = new Float64Array(data.length / 4);
  let sum = 0;
  let squared = 0;
  for (let i = 0, p = 0; i < data.length; i += 4, p++) {
    const value = 0.2126 * (data[i] ?? 0) + 0.7152 * (data[i + 1] ?? 0) + 0.0722 * (data[i + 2] ?? 0);
    luma[p] = value;
    sum += value;
    squared += value * value;
  }
  const mean = sum / luma.length;
  const spread = Math.sqrt(squared / luma.length - mean * mean);
  return { mean: Math.round(mean * 10) / 10, spread: Math.round(spread * 10) / 10 };
}

async function run(): Promise<LabResult> {
  const hevcProbe = player.canPlayType('video/mp4; codecs="hvc1.1.6.L93.B0"');
  say("探测", `内核 HEVC：${hevcProbe || "不支持"}`, true);

  const bytes = await load("/fixtures/ep01.mp4");
  const keyText = await (await fetch("/fixtures/ep01.mp4.keyhex")).text();
  say("下载", `样本 ${(bytes.length / 1048576).toFixed(1)}MB`, true);

  const plan = planCencFile(bytes);
  say("解析", describePlan(plan), true);
  const plain = await decryptCencFile(plan, hexToBytes(keyText.trim()), (progress) => {
    say("解密", `${(progress.done / 1048576).toFixed(1)} / ${(progress.total / 1048576).toFixed(1)} MB`);
  });

  player.src = URL.createObjectURL(new Blob([plain], { type: "video/mp4" }));
  say("装载", "等待元数据", true);
  await once(player, "loadedmetadata", 30_000);
  const meta = {
    videoWidth: player.videoWidth,
    videoHeight: player.videoHeight,
    duration: Math.round(player.duration * 100) / 100,
  };
  if (meta.videoWidth === 0) throw new Error("解码器没有给出画面尺寸");

  player.currentTime = Math.min(100, meta.duration - 1);
  await once(player, "seeked", 30_000);
  // One media event later the picture is presented; rAF would never fire hidden.
  await once(player, "timeupdate", 5_000).catch(() => undefined);
  const pixels = measurePixels(player);
  if (pixels.spread < 1) throw new Error(`画面是纯色（亮度 ${pixels.mean}，方差 ${pixels.spread}）`);

  return {
    ok: true,
    stage: "完成",
    detail: `${meta.videoWidth}×${meta.videoHeight}`,
    at: Date.now() - started,
    hevcProbe,
    summary: describePlan(plan),
    ms: Date.now() - started,
    bytes: plain.length,
    ...meta,
    pixels,
  };
}

run()
  .then((result) => {
    log.textContent = JSON.stringify(result, null, 2);
    window.__LAB__ = result;
    say(result.stage, `画面 ${result.videoWidth}×${result.videoHeight}，${result.duration}s，用时 ${(result.ms / 1000).toFixed(1)}s`, true);
  })
  .catch((error: unknown) => {
    const message = error instanceof Error ? error.message : String(error);
    log.textContent = `失败：${message}`;
    window.__LAB__ = {
      ok: false,
      error: message,
      stage: "失败",
      detail: message,
      at: Date.now() - started,
    } as LabResult;
    say("失败", message, true);
  });
