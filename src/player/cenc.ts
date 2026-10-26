/**
 * AES-128-CTR decryption of a CENC sample table, in the browser.
 *
 * The maths is the part worth writing down, because WebCrypto is picky about it:
 *
 * * `tenc` says `IV_size = 8`, so each sample carries an 8-byte initialisation
 *   vector and the counter block is `IV ‖ BE64(0)`. WebCrypto's `AES-CTR` takes a
 *   full 16-byte counter and increments only its trailing `length` bits, so
 *   `{ counter, length: 64 }` reproduces CENC's per-sample counter exactly — the
 *   high 8 bytes stay fixed, the low 8 wrap within the sample.
 * * Full-sample encryption (`DefaultCryptByteCount`/`DefaultSkipByteCount` both
 *   zero) means one call per sample covers the whole thing. A pattern would need
 *   alternating clear/protected ranges, which is handled but never observed.
 *
 * Samples decrypt into a freshly allocated buffer, so the ciphertext stays intact
 * until every sample lands — a mid-way failure leaves the caller with the
 * original file rather than a half-rewritten one.
 */

import type { Bytes, CencPlan, TrackSource } from "./mp4";
import { PlayerError, buildContainer, outputOffset } from "./mp4";

export interface Progress {
  /** Bytes decrypted so far. */
  readonly done: number;
  readonly total: number;
  readonly stage: string;
}

type Subtle = SubtleCrypto;

function subtle(): Subtle {
  const api = globalThis.crypto?.subtle;
  if (api === undefined) throw new PlayerError("当前环境没有 WebCrypto，无法解密");
  return api;
}

/** Import the per-episode content key. It is never stored, only used. */
export async function importContentKey(key: Bytes): Promise<CryptoKey> {
  if (key.length !== 16) throw new PlayerError("只支持 AES-128 的内容密钥");
  return subtle().importKey("raw", key, "AES-CTR", false, ["decrypt"]);
}

/** `IV ‖ BE64(0)` for an 8-byte IV; for a 16-byte IV the low 8 bytes are reset to 0. */
export function counterBlock(iv: Uint8Array): Bytes {
  const counter = new Uint8Array(16);
  if (iv.length === 8) {
    counter.set(iv, 0);
  } else if (iv.length === 16) {
    counter.set(iv.subarray(0, 8), 0);
  } else {
    throw new PlayerError(`IV 长度 ${iv.length} 不合法`);
  }
  return counter;
}

/**
 * Protected byte ranges inside one sample. `crypt === 0` means the whole sample;
 * otherwise the pattern is `skip` clear bytes then `crypt` protected bytes,
 * repeating, as CENC defines it.
 */
export function protectedRanges(size: number, crypt: number, skip: number): [number, number][] {
  if (crypt === 0 && skip === 0) return [[0, size]];
  const ranges: [number, number][] = [];
  let cursor = skip;
  while (cursor < size) {
    const length = crypt === 0 ? size - cursor : Math.min(crypt, size - cursor);
    ranges.push([cursor, length]);
    cursor += length + skip;
  }
  return ranges;
}

/**
 * Let the event loop breathe so the UI keeps painting during a long decrypt.
 *
 * A `MessageChannel` round trip rather than `setTimeout(0)`: background tabs clamp
 * nested timers to a second each, and a decrypt loop yields ~100 times, which
 * would stretch a two-second job over a minute the moment the window is hidden.
 */
let channel: MessageChannel | null = null;

function yieldToUi(): Promise<void> {
  if (typeof MessageChannel === "undefined") {
    return new Promise((resolve) => setTimeout(resolve, 0));
  }
  channel ??= new MessageChannel();
  const port = channel.port1;
  return new Promise((resolve) => {
    port.onmessage = () => {
      port.onmessage = null;
      resolve();
    };
    channel?.port2.postMessage(0);
  });
}

async function decryptTrack(
  plan: CencPlan,
  track: TrackSource,
  key: CryptoKey,
  out: Bytes,
  state: { done: number },
  onProgress?: (progress: Progress) => void,
): Promise<void> {
  const api = subtle();
  for (let index = 0; index < track.samples.length; index++) {
    const sample = track.samples[index];
    if (sample === undefined) continue;
    const iv = track.ivs[index];
    if (iv === undefined) throw new PlayerError(`${track.kind} 轨道缺第 ${index + 1} 个 IV`);
    const counter = counterBlock(iv);
    const base = outputOffset(plan, sample.offset);
    for (const [start, length] of protectedRanges(sample.size, track.defaultCrypt, track.defaultSkip)) {
      if (length === 0) continue;
      const cipher = plan.bytes.subarray(sample.offset + start, sample.offset + start + length);
      const plain = await api.decrypt({ name: "AES-CTR", counter: counter.slice(), length: 64 }, key, cipher);
      out.set(new Uint8Array(plain), base + start);
      state.done += length;
    }
    if (index % 96 === 95) {
      onProgress?.({ done: state.done, total: plan.totalBytes, stage: "解密" });
      await yieldToUi();
    }
  }
}

/**
 * Decrypt a whole file: rebuild the container metadata, then overwrite every
 * sample with its plaintext. Returns the bytes of a plain progressive MP4.
 */
export async function decryptCencFile(
  plan: CencPlan,
  contentKey: Bytes,
  onProgress?: (progress: Progress) => void,
): Promise<Bytes> {
  if (plan.keyId === "") throw new PlayerError("计划里没有 KID");
  const key = await importContentKey(contentKey);
  const out = buildContainer(plan);
  const state = { done: 0 };
  for (const track of plan.tracks) {
    await decryptTrack(plan, track, key, out, state, onProgress);
  }
  onProgress?.({ done: plan.totalBytes, total: plan.totalBytes, stage: "解密" });
  return out;
}
