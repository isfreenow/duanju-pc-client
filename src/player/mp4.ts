/**
 * ISO Base Media File Format reading plus the CENC container rewrite.
 *
 * Nothing here knows where the bytes came from — it is a container tool, not a
 * site client. The layout it targets is the one this repo measured end to end
 * (see `src-tauri/tests/inspect_container.rs`): a progressive `ftyp` + `moov` +
 * `mdat` file whose sample entries are `encv` / `enca`, each carrying
 * `sinf → { frma, schm('cenc'), schi → tenc(IV_size 8, shared KID) }`, with the
 * per-sample IVs listed in a `senc` box sitting in `stbl` and every sample
 * encrypted in full.
 *
 * Rewriting a decrypted file means two things at once: the bytes change meaning
 * (cipher → plain) and the metadata lies about them (`encv` says "encrypted").
 * So the entry's four-character code goes back to what `frma` recorded, the `sinf`
 * box that declared the scheme is dropped, the `senc` box that carried the IVs is
 * dropped, and — because box sizes are cumulative — every enclosing box is
 * re-sized and every absolute chunk offset is re-based by the amount of metadata
 * removed. Hence a tree, a size pass, then a rebuild.
 */

export interface RawBox {
  /** Four-character code; non-printable bytes replaced with '.'. */
  readonly type: string;
  /** Offset of the box's size field. */
  readonly start: number;
  /** First byte after the 8- or 16-byte box header. */
  readonly headerEnd: number;
  /** One past the last byte of the box. */
  readonly end: number;
}

interface BaseNode {
  readonly box: RawBox;
  /** Set when the box must not appear in the rewritten container. */
  dropped?: boolean;
}

export interface LeafNode extends BaseNode {
  readonly kind: "leaf";
}

export interface BranchNode extends BaseNode {
  readonly kind: "branch";
  /** First byte of the first child; for a sample entry this is past its fixed fields. */
  readonly childStart: number;
  readonly children: Node[];
  /** Code written over this box's type when an encrypted entry is unwrapped. */
  renameTo?: string;
}

export type Node = LeafNode | BranchNode;

/** Boxes whose payload is a sequence of further boxes. */
const CONTAINER_TYPES = new Set(["moov", "trak", "mdia", "minf", "stbl", "sinf", "schi"]);

/** Sample entries whose children start after a 78-byte fixed field. */
const VISUAL_ENTRIES = new Set(["encv", "hvc1", "hev1", "avc1", "avc3", "vpc1", "resv"]);
/** Sample entries whose children start after a 28-byte fixed field. */
const AUDIO_ENTRIES = new Set(["enca", "mp4a", "ac-3", "ec-3", "samr", "sawp"]);

const VISUAL_ENTRY_FIXED = 78;
const AUDIO_ENTRY_FIXED = 28;

/** Only AES in counter mode is implemented; every measured stream is `cenc`. */
const SUPPORTED_SCHEME = "cenc";

export class PlayerError extends Error {}

/**
 * A buffer backed by a plain `ArrayBuffer`, which is what `Blob` accepts. Every
 * allocation in this module returns one, so the caller never has to narrow.
 */
export type Bytes = Uint8Array<ArrayBuffer>;

function view(bytes: Uint8Array): DataView {
  return new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
}

function fourcc(bytes: Uint8Array, at: number): string {
  let out = "";
  for (let i = 0; i < 4; i++) {
    const code = bytes[at + i] ?? 0;
    out += code >= 0x20 && code < 0x7f ? String.fromCharCode(code) : ".";
  }
  return out;
}

export function hexLower(bytes: Uint8Array): string {
  let out = "";
  for (const byte of bytes) out += byte.toString(16).padStart(2, "0");
  return out;
}

export function hexToBytes(text: string): Bytes {
  const clean = text.trim();
  if (!/^[0-9a-fA-F]{32}$/.test(clean)) {
    throw new PlayerError("内容密钥应该是 16 字节的十六进制");
  }
  const bytes = new Uint8Array(16);
  for (let i = 0; i < 16; i++) {
    bytes[i] = Number.parseInt(clean.slice(i * 2, i * 2 + 2), 16);
  }
  return bytes;
}

/** Read one box header at `at`; null when the bytes there cannot be a box. */
function readBox(bytes: Uint8Array, at: number, limit: number): RawBox | null {
  if (at + 8 > limit) return null;
  const data = view(bytes);
  const declared = data.getUint32(at);
  let size = declared;
  let headerEnd = at + 8;
  if (declared === 1) {
    if (at + 16 > limit) return null;
    size = Number(data.getBigUint64(at + 8));
    headerEnd = at + 16;
  } else if (declared === 0) {
    // Runs to the end of the enclosing range.
    size = limit - at;
  }
  const end = at + size;
  if (size < 8 || end > limit) return null;
  return { type: fourcc(bytes, at + 4), start: at, headerEnd, end };
}

/** Every well-formed box between `start` and `end`. */
function siblings(bytes: Uint8Array, start: number, end: number): RawBox[] {
  const boxes: RawBox[] = [];
  let cursor = start;
  while (cursor < end) {
    const box = readBox(bytes, cursor, end);
    if (box === null) break;
    boxes.push(box);
    cursor = box.end;
  }
  return boxes;
}

/** Where a branch's children begin: past the full-box fields for `stsd`, past the fixed fields for an entry. */
function childStartOf(box: RawBox): number {
  if (box.type === "stsd") return box.headerEnd + 8;
  if (VISUAL_ENTRIES.has(box.type) || AUDIO_ENTRIES.has(box.type)) {
    return box.headerEnd + (AUDIO_ENTRIES.has(box.type) ? AUDIO_ENTRY_FIXED : VISUAL_ENTRY_FIXED);
  }
  return box.headerEnd;
}

function childBoxes(bytes: Uint8Array, box: RawBox): RawBox[] {
  if (CONTAINER_TYPES.has(box.type) || box.type === "stsd") {
    return siblings(bytes, childStartOf(box), box.end);
  }
  if (VISUAL_ENTRIES.has(box.type) || AUDIO_ENTRIES.has(box.type)) {
    return siblings(bytes, childStartOf(box), box.end);
  }
  return [];
}

/** Parse a box into a tree; unknown boxes stay opaque leaves and are copied verbatim. */
function parseTree(bytes: Uint8Array, box: RawBox): Node {
  const kids = childBoxes(bytes, box);
  if (kids.length === 0 && !CONTAINER_TYPES.has(box.type) && box.type !== "stsd") {
    return { kind: "leaf", box };
  }
  const children = kids.map((child) => parseTree(bytes, child));
  const node: BranchNode = { kind: "branch", box, childStart: childStartOf(box), children };
  const original = originalFormat(bytes, node);
  if (original !== null) {
    // Keep the `sinf` box in the tree but mark it; the entry takes back the format
    // `frma` names. A decrypted file that still advertises a scheme is a lie the
    // demuxer believes.
    for (const child of children) if (child.box.type === "sinf") child.dropped = true;
    node.renameTo = original;
  }
  return node;
}

function originalFormat(bytes: Uint8Array, node: BranchNode): string | null {
  if (!VISUAL_ENTRIES.has(node.box.type) && !AUDIO_ENTRIES.has(node.box.type)) return null;
  const sinf = node.children.find((child) => child.box.type === "sinf");
  if (sinf === undefined || sinf.kind !== "branch") return null;
  const frma = sinf.children.find((child) => child.box.type === "frma");
  if (frma === undefined || frma.box.end - frma.box.headerEnd < 4) return null;
  const code = fourcc(bytes, frma.box.headerEnd);
  return code === "...." ? null : code;
}

/** First descendant with the given type, depth-first. */
function findNode(root: Node, type: string): Node | null {
  if (root.box.type === type) return root;
  if (root.kind === "branch") {
    for (const child of root.children) {
      const hit = findNode(child, type);
      if (hit !== null) return hit;
    }
  }
  return null;
}

/* ------------------------------------------------------------------ *
 * Sample geometry
 * ------------------------------------------------------------------ */

export interface SampleRange {
  /** Absolute offset inside the encrypted file as downloaded. */
  readonly offset: number;
  readonly size: number;
}

function readSampleSizes(bytes: Uint8Array, box: RawBox): { count: number; sizeOf(index: number): number } {
  const data = view(bytes);
  const uniform = data.getUint32(box.headerEnd + 4);
  const count = data.getUint32(box.headerEnd + 8);
  if (uniform !== 0) return { count, sizeOf: () => uniform };
  const table = box.headerEnd + 12;
  if (table + count * 4 > box.end) throw new PlayerError("stsz 表越界");
  return { count, sizeOf: (index: number) => data.getUint32(table + index * 4) };
}

function readStsc(bytes: Uint8Array, box: RawBox): { firstChunk: number; samplesPerChunk: number }[] {
  const data = view(bytes);
  const count = data.getUint32(box.headerEnd + 4);
  const entries: { firstChunk: number; samplesPerChunk: number }[] = [];
  for (let i = 0; i < count; i++) {
    const at = box.headerEnd + 8 + i * 12;
    if (at + 12 > box.end) throw new PlayerError("stsc 表越界");
    entries.push({ firstChunk: data.getUint32(at), samplesPerChunk: data.getUint32(at + 4) });
  }
  return entries;
}

function readChunkOffsets(bytes: Uint8Array, box: RawBox): number[] {
  const data = view(bytes);
  const count = data.getUint32(box.headerEnd + 4);
  const wide = box.type === "co64";
  const step = wide ? 8 : 4;
  const offsets: number[] = [];
  for (let i = 0; i < count; i++) {
    const at = box.headerEnd + 8 + i * step;
    if (at + step > box.end) throw new PlayerError(`${box.type} 表越界`);
    offsets.push(wide ? Number(data.getBigUint64(at)) : data.getUint32(at));
  }
  return offsets;
}

/**
 * Turn `stsc` + `stsz` + `stco` into one ordered sample list. Chunks hold varying
 * numbers of samples and both tracks can point at the same chunks, so order has
 * to be rebuilt from the tables instead of assumed.
 */
function buildSamples(bytes: Uint8Array, stsc: RawBox, stsz: RawBox, stco: RawBox): SampleRange[] {
  const sizes = readSampleSizes(bytes, stsz);
  const entries = readStsc(bytes, stsc);
  const chunks = readChunkOffsets(bytes, stco);
  const samples: SampleRange[] = [];
  let index = 0;
  for (let chunk = 0; chunk < chunks.length; chunk++) {
    let perChunk = 1;
    for (const entry of entries) {
      if (entry.firstChunk <= chunk + 1) perChunk = entry.samplesPerChunk;
      else break;
    }
    let cursor = chunks[chunk] as number;
    for (let i = 0; i < perChunk && index < sizes.count; i++, index++) {
      const size = sizes.sizeOf(index);
      samples.push({ offset: cursor, size });
      cursor += size;
    }
  }
  if (index !== sizes.count) throw new PlayerError(`样本数对不上：stsz ${sizes.count}，表内 ${index}`);
  return samples;
}

/* ------------------------------------------------------------------ *
 * Protection metadata
 * ------------------------------------------------------------------ */

interface Tenc {
  readonly version: number;
  readonly ivSize: number;
  readonly keyId: string;
  readonly defaultCrypt: number;
  readonly defaultSkip: number;
  readonly defaultIv: Uint8Array | null;
}

function readTenc(bytes: Uint8Array, box: RawBox): Tenc {
  const body = box.headerEnd;
  if (box.end - body < 24) throw new PlayerError("tenc 盒过短");
  const version = bytes[body] ?? 0;
  const ivSize = bytes[body + 7] ?? 0;
  if (ivSize !== 8 && ivSize !== 16) throw new PlayerError(`不支持的 IV_size=${ivSize}`);
  const keyId = hexLower(bytes.subarray(body + 8, body + 24));
  let defaultCrypt = 0;
  let defaultSkip = 0;
  let defaultIv: Uint8Array | null = null;
  if (version === 0 && box.end - body >= 26) {
    defaultCrypt = bytes[body + 24] ?? 0;
    defaultSkip = bytes[body + 25] ?? 0;
  } else if (version === 1 && box.end - body >= 40) {
    defaultIv = bytes.subarray(body + 24, body + 40);
  }
  return { version, ivSize, keyId, defaultCrypt, defaultSkip, defaultIv };
}

/** `senc`: one IV per sample. Sub-sample patterns are refused, not guessed. */
function readSampleIvs(bytes: Uint8Array, box: RawBox, ivSize: number, expected: number): Uint8Array[] {
  const data = view(bytes);
  const flags = data.getUint32(box.headerEnd) & 0x00_ff_ff_ff;
  if (flags & 1) throw new PlayerError("子样本加密模式暂未支持");
  const count = data.getUint32(box.headerEnd + 4);
  if (count !== expected) throw new PlayerError(`senc 有 ${count} 条，样本有 ${expected} 个`);
  const ivs: Uint8Array[] = [];
  for (let i = 0; i < count; i++) {
    const at = box.headerEnd + 8 + i * ivSize;
    if (at + ivSize > box.end) throw new PlayerError("senc IV 越界");
    ivs.push(bytes.subarray(at, at + ivSize));
  }
  return ivs;
}

/** Without `senc`, CENC derives each IV from `tenc`'s default IV plus the sample number. */
function derivedIvs(tenc: Tenc, count: number): Uint8Array[] {
  if (tenc.defaultIv === null || tenc.ivSize !== 16) throw new PlayerError("既没有 senc，也没有可用的默认 IV");
  const ivs: Uint8Array[] = [];
  for (let i = 0; i < count; i++) {
    const iv = Uint8Array.from(tenc.defaultIv);
    const data = view(iv);
    // Low 8 bytes hold the counter base for that sample.
    data.setBigUint64(8, BigInt(i));
    ivs.push(iv);
  }
  return ivs;
}

export interface TrackSource {
  readonly kind: "video" | "audio";
  /** Encrypted entry code as stored, e.g. `encv`. */
  readonly storedType: string;
  /** What the samples really are once plain, e.g. `hvc1`. */
  readonly originalType: string;
  readonly scheme: string;
  readonly ivSize: number;
  readonly keyId: string;
  readonly samples: SampleRange[];
  readonly ivs: Uint8Array[];
  readonly defaultCrypt: number;
  readonly defaultSkip: number;
  /** Bytes of ciphertext this track contributes, for progress reporting. */
  readonly totalBytes: number;
}

export interface CencPlan {
  readonly bytes: Bytes;
  readonly moov: BranchNode;
  readonly moovStart: number;
  readonly moovEnd: number;
  /** `moov` size after the rewrite minus its original size; negative. */
  readonly shift: number;
  readonly tracks: TrackSource[];
  readonly keyId: string;
  readonly totalBytes: number;
}

/**
 * Inspect a whole file and describe what has to change. Throws when the layout is
 * not a non-fragmented, moov-first CENC MP4 — guessing here would only produce a
 * broken Blob that the decoder rejects with a useless message.
 */
export function planCencFile(bytes: Bytes): CencPlan {
  const top = siblings(bytes, 0, bytes.length);
  const moovBox = top.find((box) => box.type === "moov");
  const mdatBox = top.find((box) => box.type === "mdat");
  if (moovBox === undefined) throw new PlayerError("文件里没有 moov，可能不是渐进式 MP4");
  if (mdatBox === undefined) throw new PlayerError("文件里没有 mdat");
  if (top.some((box) => box.type === "moof")) throw new PlayerError("分片 MP4 暂未支持");
  if (mdatBox.start < moovBox.end) throw new PlayerError("mdat 不在 moov 之后，非渐进式文件，暂不支持");

  const node = parseTree(bytes, moovBox);
  if (node.kind !== "branch") throw new PlayerError("moov 解析失败");
  const traks = node.children.filter((child) => child.box.type === "trak");
  const tracks: TrackSource[] = [];
  let keyId = "";
  for (const trak of traks) {
    const track = readTrack(bytes, trak);
    if (track === null) continue;
    if (keyId === "") keyId = track.keyId;
    else if (track.keyId !== keyId) throw new PlayerError("音视频使用了不同的 KID，暂不支持");
    tracks.push(track);
  }
  if (tracks.length === 0) throw new PlayerError("没有发现加密样本");

  const shift = sizeOf(node) - (moovBox.end - moovBox.start);
  const totalBytes = tracks.reduce((sum, track) => sum + track.totalBytes, 0);
  return {
    bytes,
    moov: node,
    moovStart: moovBox.start,
    moovEnd: moovBox.end,
    shift,
    tracks,
    keyId,
    totalBytes,
  };
}

function readTrack(bytes: Uint8Array, trak: Node): TrackSource | null {
  const stbl = findNode(trak, "stbl");
  if (stbl === null || stbl.kind !== "branch") return null;
  const leafs = stbl.children;
  const stsd = leafs.find((child) => child.box.type === "stsd");
  const stsz = leafs.find((child) => child.box.type === "stsz");
  const stsc = leafs.find((child) => child.box.type === "stsc");
  const stco = leafs.find((child) => child.box.type === "stco" || child.box.type === "co64");
  if (stsd === undefined || stsz === undefined || stsc === undefined || stco === undefined) return null;
  if (stsd.kind !== "branch") throw new PlayerError("stsd 解析失败");
  const entry = stsd.children[0];
  if (entry === undefined || entry.kind !== "branch" || entry.renameTo === undefined) {
    // A plain track (no scheme declared) — nothing to decrypt here.
    return null;
  }
  const tencNode = findNode(stsd, "tenc");
  const schmNode = findNode(stsd, "schm");
  if (tencNode === null || schmNode === null) return null;

  const scheme = fourcc(bytes, schmNode.box.headerEnd + 4);
  if (scheme !== SUPPORTED_SCHEME) throw new PlayerError(`加密方案 ${scheme} 暂不支持，只支持 ${SUPPORTED_SCHEME}`);
  const tenc = readTenc(bytes, tencNode.box);
  const samples = buildSamples(bytes, stsc.box, stsz.box, stco.box);
  const senc = leafs.find((child) => child.box.type === "senc");
  const ivs =
    senc !== undefined
      ? readSampleIvs(bytes, senc.box, tenc.ivSize, samples.length)
      : derivedIvs(tenc, samples.length);
  if (senc !== undefined) senc.dropped = true;

  for (const sample of samples) {
    if (sample.offset + sample.size > bytes.length) throw new PlayerError("样本位置越界");
  }
  const totalBytes = samples.reduce((sum, sample) => sum + sample.size, 0);
  return {
    kind: VISUAL_ENTRIES.has(entry.box.type) ? "video" : "audio",
    storedType: entry.box.type,
    originalType: entry.renameTo,
    scheme,
    ivSize: tenc.ivSize,
    keyId: tenc.keyId,
    samples,
    ivs,
    defaultCrypt: tenc.defaultCrypt,
    defaultSkip: tenc.defaultSkip,
    totalBytes,
  };
}

/* ------------------------------------------------------------------ *
 * Container rebuild
 * ------------------------------------------------------------------ */

/** Size a node contributes to the rewritten container; dropped boxes contribute nothing. */
function sizeOf(node: Node): number {
  if (node.dropped === true) return 0;
  if (node.kind === "leaf") return node.box.end - node.box.start;
  let total = node.childStart - node.box.start;
  for (const child of node.children) total += sizeOf(child);
  return total;
}

class Writer {
  private chunks: Uint8Array[] = [];
  private length = 0;

  get cursor(): number {
    return this.length;
  }

  push(bytes: Uint8Array): void {
    this.chunks.push(bytes);
    this.length += bytes.length;
  }

  finish(): Uint8Array {
    const out = new Uint8Array(this.length);
    let at = 0;
    for (const chunk of this.chunks) {
      out.set(chunk, at);
      at += chunk.length;
    }
    return out;
  }
}

/** Copy a branch's header region with its size — and sometimes its type — rewritten. */
function headerBytes(bytes: Uint8Array, node: BranchNode, total: number): Uint8Array {
  const head = Uint8Array.from(bytes.subarray(node.box.start, node.childStart));
  const data = new DataView(head.buffer);
  const wide = node.box.headerEnd - node.box.start === 16;
  if (wide) data.setBigUint64(8, BigInt(total));
  else data.setUint32(0, total);
  if (node.renameTo !== undefined) {
    for (let i = 0; i < 4; i++) head[4 + i] = node.renameTo.charCodeAt(i);
  }
  return head;
}

/** Re-base every entry of a `stco`/`co64` so it still points at its chunk. */
function patchChunkOffsets(bytes: Uint8Array, box: RawBox, shift: number): Uint8Array {
  const out = Uint8Array.from(bytes.subarray(box.start, box.end));
  const data = new DataView(out.buffer);
  const at = box.headerEnd - box.start;
  const count = data.getUint32(at + 4);
  const wide = box.type === "co64";
  const step = wide ? 8 : 4;
  for (let i = 0; i < count; i++) {
    const entry = at + 8 + i * step;
    if (wide) {
      const next = Number(data.getBigUint64(entry)) + shift;
      data.setBigUint64(entry, BigInt(Math.max(0, next)));
    } else {
      const next = data.getUint32(entry) + shift;
      data.setUint32(entry, Math.max(0, next));
    }
  }
  return out;
}

function writeNode(bytes: Uint8Array, node: Node, shift: number, out: Writer): void {
  if (node.dropped === true) return;
  if (node.kind === "leaf") {
    if (node.box.type === "stco" || node.box.type === "co64") {
      out.push(patchChunkOffsets(bytes, node.box, shift));
    } else {
      out.push(bytes.subarray(node.box.start, node.box.end));
    }
    return;
  }
  out.push(headerBytes(bytes, node, sizeOf(node)));
  for (const child of node.children) writeNode(bytes, child, shift, out);
}

/** Where a byte of the source file lands in the rewritten file. */
export function outputOffset(plan: CencPlan, source: number): number {
  return source >= plan.moovEnd ? source + plan.shift : source;
}

/**
 * Assemble the output skeleton: everything up to `moov`, the rewritten `moov`,
 * then the rest of the file. The media bytes are still ciphertext — the caller
 * overwrites them sample by sample at `outputOffset`.
 */
export function buildContainer(plan: CencPlan): Bytes {
  const moovBytes = new Writer();
  writeNode(plan.bytes, plan.moov, plan.shift, moovBytes);
  const moov = moovBytes.finish();
  if (moov.length !== sizeOf(plan.moov)) throw new PlayerError("容器重建后长度与计划不一致");

  const out = new Uint8Array(plan.bytes.length + plan.shift);
  out.set(plan.bytes.subarray(0, plan.moovStart), 0);
  out.set(moov, plan.moovStart);
  out.set(plan.bytes.subarray(plan.moovEnd), plan.moovStart + moov.length);
  return out;
}

/** Human-readable summary for the player UI and for tests. */
export function describePlan(plan: CencPlan): string {
  return plan.tracks
    .map(
      (track) =>
        `${track.kind} ${track.storedType}→${track.originalType} ${track.scheme} IV=${track.ivSize}B ` +
        `样本=${track.samples.length} (${(track.totalBytes / 1048576).toFixed(1)}MB)`,
    )
    .join("，");
}
