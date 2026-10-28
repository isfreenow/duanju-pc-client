//! X-Medusa: protobuf-serialised envelope, custom AES ("AesV3"), `xmxor`
//! mixer and SM3-derived key stream.
//!
//! `AesV3` is *not* AES as published: the S-box, key expansion, round order and
//! the column mix are all selected by `khronos & 3`, and the plaintext is a
//! bit-packed digest of the input rather than the input itself. It is ported
//! literally, including the arithmetic right shift in the key expansion — JS
//! `init >> n` sign-extends because `init` exceeds 2^31, which changes the
//! result. The Rust port uses `i32 >>` for exactly that reason.

use md5::{Digest, Md5};

use super::bits::{le32, slice, varint, xor_bytes_repeating, zigzag};
use super::f13::{hash_f13, md5_bytes, query_sm3};
use super::rand::SignRng;
use super::sm3::sm3;
use super::tables::{AES_IV, AES_KEY, APP_TOKEN_MD5, SBOX, SIGN_KEY, SPADE_URL_CONSTANTS};

pub fn md5_hex_upper(data: &[u8]) -> String {
    let mut hasher = Md5::new();
    hasher.update(data);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect()
}

// ── protobuf wire writer ─────────────────────────────────────────────────

/// A field value in the reference's `[tag, value, type]` triples.
pub enum Field<'a> {
    Str(&'a str),
    Bytes(Vec<u8>),
    Message(Vec<u8>),
    Int(i64),
    Sint(i64),
    Float(f32),
}

fn field(tag: u32, value: Field<'_>) -> Vec<u8> {
    match value {
        Field::Str(text) => {
            if text.is_empty() {
                return Vec::new();
            }
            let body = text.as_bytes();
            let mut out = varint((tag as u64) << 3 | 2);
            out.extend(varint(body.len() as u64));
            out.extend_from_slice(body);
            out
        }
        Field::Bytes(body) => {
            if body.is_empty() {
                return Vec::new();
            }
            let mut out = varint((tag as u64) << 3 | 2);
            out.extend(varint(body.len() as u64));
            out.extend(body);
            out
        }
        Field::Message(body) => {
            if body.is_empty() {
                return Vec::new();
            }
            let mut out = varint((tag as u64) << 3 | 2);
            out.extend(varint(body.len() as u64));
            out.extend(body);
            out
        }
        Field::Float(value) => {
            let mut out = varint((tag as u64) << 3 | 5);
            out.extend_from_slice(&value.to_le_bytes());
            out
        }
        Field::Int(value) => {
            let mut out = varint((tag as u64) << 3);
            out.extend(varint(value.max(0) as u64));
            out
        }
        Field::Sint(value) => {
            let mut out = varint((tag as u64) << 3);
            out.extend(varint(zigzag(value)));
            out
        }
    }
}

/// `proto([[tag, value, type], ...])`
pub fn proto(fields: Vec<(u32, Field<'_>)>) -> Vec<u8> {
    fields
        .into_iter()
        .flat_map(|(tag, value)| field(tag, value))
        .collect()
}

// ── AesV3 ────────────────────────────────────────────────────────────────

/// The reference assigns `this.con` but never reads it, so the table is kept
/// for fidelity only.
#[allow(dead_code)]
const CON: [[usize; 4]; 4] = [[1, 0, 2, 3], [1, 3, 0, 2], [0, 1, 3, 2], [1, 0, 2, 3]];
const CON2: [[usize; 4]; 4] = [[1, 0, 2, 3], [2, 0, 3, 1], [0, 1, 3, 2], [1, 0, 2, 3]];
const ORDER: [[usize; 16]; 4] = [
    [0, 9, 14, 11, 4, 13, 2, 7, 8, 1, 6, 15, 12, 5, 10, 3],
    [0, 9, 14, 15, 4, 13, 2, 7, 8, 1, 6, 3, 12, 5, 10, 11],
    [0, 9, 14, 7, 4, 13, 2, 11, 8, 1, 6, 3, 12, 5, 10, 15],
    [0, 9, 14, 11, 4, 13, 2, 7, 8, 1, 6, 15, 12, 5, 10, 3],
];
const INIT_WORD: [u32; 4] = [0xca025ddc, 0x823dc546, 0xc9420583, 0xc298225f];

type Rows = [[u8; 4]; 4];

fn matrix(bytes: &[u8]) -> Rows {
    let mut rows = [[0u8; 4]; 4];
    for (i, row) in rows.iter_mut().enumerate() {
        row.copy_from_slice(&bytes[i * 4..i * 4 + 4]);
    }
    rows
}

fn flat(rows: &Rows) -> Vec<u8> {
    rows.iter().flatten().copied().collect()
}

fn aes_mix(rows: &mut Rows) {
    let xt = |x: u8| -> u8 { (((x as u16) << 1) ^ if x & 128 != 0 { 0x1b } else { 0 }) as u8 };
    for i in 0..4 {
        let t = rows[0][i] ^ rows[1][i] ^ rows[2][i] ^ rows[3][i];
        let u = rows[0][i];
        rows[0][i] ^= t ^ xt(rows[0][i] ^ rows[1][i]);
        rows[1][i] ^= t ^ xt(rows[1][i] ^ rows[2][i]);
        rows[2][i] ^= t ^ xt(rows[2][i] ^ rows[3][i]);
        rows[3][i] ^= t ^ xt(rows[3][i] ^ u);
    }
}

pub struct AesV3 {
    word_size: usize,
    box_table: Vec<u8>,
    order: [usize; 16],
    keys: Vec<[u8; 4]>,
}

impl AesV3 {
    pub fn new(key: &[u8], khronos: u32) -> Self {
        let word_size = (khronos & 3) as usize;
        let box_table = SBOX[word_size * 256..(word_size + 1) * 256].to_vec();
        let init = INIT_WORD[word_size];

        let mut initial = [0u8; 16];
        for i in 0..4 {
            initial[i * 4..i * 4 + 4].copy_from_slice(&init.to_le_bytes());
        }
        let mk = xor_bytes_repeating(&initial, key);
        let mut expanded = mk.clone();
        expanded.extend(vec![0u8; 32]);

        let mut rounds: u32 = 8;
        for i in 4..12usize {
            let at = 4 * (i - 1);
            let (mut a, mut b, mut c, mut d) = (
                expanded[at],
                expanded[at + 1],
                expanded[at + 2],
                expanded[at + 3],
            );
            if i & 3 == 0 {
                // JS `init >> (rounds & 24)` is an arithmetic shift on a value
                // above 2^31, so the sign extension is part of the algorithm.
                let shifted = ((init as i32) >> (rounds & 24)) as u32;
                let t = (shifted ^ box_table[b as usize] as u32) as u8;
                b = box_table[c as usize];
                c = box_table[d as usize];
                d = box_table[a as usize];
                a = t;
            }
            rounds += 2;
            expanded[at + 4] = a ^ expanded[at - 12];
            expanded[at + 5] = b ^ expanded[at - 11];
            expanded[at + 6] = c ^ expanded[at - 10];
            expanded[at + 7] = d ^ expanded[at - 9];
        }

        let keys = (0..12)
            .map(|i| {
                let mut row = [0u8; 4];
                row.copy_from_slice(&expanded[i * 4..i * 4 + 4]);
                row
            })
            .collect();

        AesV3 {
            word_size,
            box_table,
            order: ORDER[word_size],
            keys,
        }
    }

    fn add(rows: &mut Rows, key: &[[u8; 4]]) {
        for i in 0..4 {
            for j in 0..4 {
                rows[i][j] ^= key[i][j];
            }
        }
    }

    fn add_con(&self, rows: &mut Rows, keys: &[[u8; 4]]) {
        let con2 = CON2[self.word_size];
        for i in 0..4 {
            for j in 0..4 {
                rows[i][j] ^= keys[i][con2[j]];
            }
        }
    }

    fn sub(&self, rows: &mut Rows) {
        for i in 0..4 {
            for j in 0..4 {
                rows[i][j] = self.box_table[rows[i][j] as usize];
            }
        }
        let old = *rows;
        let con2 = CON2[self.word_size];
        for i in 0..4 {
            rows[i] = old[con2[i]];
        }
    }

    fn shift(&self, rows: &mut Rows) {
        let b = flat(rows);
        for i in 0..16 {
            rows[i / 4][i % 4] = b[self.order[i]];
        }
    }

    fn shift_con(&self, rows: &mut Rows) {
        let old = *rows;
        let con2 = CON2[self.word_size];
        for i in 0..4 {
            rows[i] = [
                old[i][con2[0]],
                old[i][con2[1]],
                old[i][con2[2]],
                old[i][con2[3]],
            ];
        }
    }

    /// One custom block permutation over 16 bytes.
    pub fn block(&self, value: &[u8]) -> Vec<u8> {
        let mut rows = matrix(value);
        self.add_con(&mut rows, &self.keys[0..4]);
        for i in 1..3usize {
            self.sub(&mut rows);
            self.shift(&mut rows);
            if i == 1 {
                self.shift_con(&mut rows);
                aes_mix(&mut rows);
            }
            self.add_con(&mut rows, &self.keys[i * 4..]);
        }
        Self::add(&mut rows, &self.keys[4..]);
        flat(&rows)
    }

    /// The reference's CBC-like key derivation over a bit-packed summary of
    /// `data`, then 248 scattered bit edits. Returns 32-byte key + edited data.
    pub fn encrypt(&self, data: &[u8], iv: &[u8]) -> Vec<u8> {
        let source = data.to_vec();
        let mut plaintext = vec![0u8; 32];
        for i in 0..31 {
            let at = i * 8;
            let get = |k: usize| source.get(at + k).copied().unwrap_or(0) as u32;
            let n0 = (get(0) >> 4) & 2;
            let n1 = n0 | (get(1) & 64);
            let n2 = n1 | ((get(2) >> 2) & 1);
            let n3 = n2 | ((get(3) << 3) & 128);
            let n4 = n3 | ((get(4) >> 1) & 4);
            let n5 = n4 | ((get(5) << 3) & 16);
            let n6 = n5 | ((get(6) << 5) & 32);
            plaintext[i] = (n6 | ((get(7) >> 4) & 8)) as u8;
        }
        plaintext[31] = 1;

        let mut blocks: Vec<Vec<u8>> = Vec::new();
        let mut previous = iv.to_vec();
        for chunk in plaintext.chunks(16) {
            let mixed: Vec<u8> = chunk
                .iter()
                .enumerate()
                .map(|(i, byte)| *byte ^ previous.get(i).copied().unwrap_or(0))
                .collect();
            let value = self.block(&mixed);
            previous = value.clone();
            blocks.push(value);
        }
        let key: Vec<u8> = blocks.concat();

        let mut out = source.clone();
        for i in 0..31 {
            let at = i * 8;
            if at + 7 >= out.len() {
                break;
            }
            let k = key[i];
            out[at] = (out[at] & 0xdf) | ((k << 4) & 32);
            out[at + 1] = (out[at + 1] & 0xbf) | (k & 64);
            out[at + 2] = (out[at + 2] & 0xfb) | ((k << 2) & 4);
            out[at + 3] = (out[at + 3] & 0xef) | ((k >> 3) & 16);
            out[at + 4] = (out[at + 4] & 0xf7) | ((k.wrapping_add(k)) & 8);
            out[at + 5] = (out[at + 5] & 0xfd) | ((k >> 3) & 2);
            out[at + 6] = (out[at + 6] & 0xfe) | ((k >> 5) & 1);
            out[at + 7] = (out[at + 7] & 0x7f) | ((k << 4) & 128);
        }

        let mut result = Vec::with_capacity(1 + out.len());
        // `key.subarray(-1)` in the reference is the LAST byte of the 32-byte
        // key, not the first 31 — only one byte is prepended.
        result.push(key[key.len() - 1]);
        result.extend(out);
        result
    }
}

// ── xmxor + keyHash ──────────────────────────────────────────────────────

/// 8-bit rotate left, the shape JS builds with `((x<<3)|(x>>>5))&255`.
#[inline]
fn rot8l(value: u32, count: u32) -> u32 {
    (((value << count) | (value >> (8 - count))) & 255) as u32
}

/// The reference's `xmxor`: per-byte transform, then two diffusion passes.
///
/// JS intermediates are 32-bit signed, and the first accumulate deliberately
/// stays above 255 before masking; the port keeps everything in `u32` and masks
/// where the reference masks, which is bit-identical for these operations.
pub fn xmxor(data: &[u8], key: &[u8]) -> Vec<u8> {
    let length = data.len();
    let mut encoded = vec![0u8; length];
    for i in 0..length {
        let at = (i * 4) & 28;
        let d0 = key[at] as u32;
        let d1 = key[at + 1] as u32;
        let v = data[i] as u32;
        let mut d2 = (((v << 4) | (v >> 4)) & 255) + d0;
        d2 = (!d2) ^ d1;
        d2 = rot8l(d2 & 255, 3);
        d2 = (d2 + d1) & 255;
        d2 = (d2 ^ d0) & 255;
        encoded[length - i - 1] = (!(d2 as u8)) as u8;
    }
    if length < 3 {
        return encoded;
    }
    let last = encoded[length - 1] ^ encoded[length - 2];
    let first = encoded[0];
    encoded[0] = (!last).wrapping_add(first);
    encoded[1] = (encoded[0] ^ encoded[length - 1] ^ 254).wrapping_add(encoded[1]);
    encoded[2] = encoded[2]
        .wrapping_add((last.wrapping_sub(first)) ^ (rot8l(encoded[1] as u32, 3) as u8) ^ 2);
    for i in 0..length.saturating_sub(4) {
        let temp = (rot8l(encoded[i + 2] as u32, 3) as u8) ^ encoded[i + 1] ^ ((i + 3) as u8);
        encoded[i + 3] = (!temp).wrapping_add(encoded[i + 3]);
    }
    encoded[length - 1] ^= encoded[length - 2];
    let sum: u32 = encoded[1..].iter().map(|b| *b as u32).sum();
    encoded[0] = (encoded[0] ^ encoded[1]).wrapping_add(sum as u8);
    encoded
}

/// `keyHash(sign_key, random)` — SM3 over key||random||key plus a 4-byte seed.
pub fn key_hash(sign_key: &[u8], random: u32) -> (Vec<u8>, [u8; 4]) {
    let mut input = Vec::with_capacity(sign_key.len() * 2 + 4);
    input.extend_from_slice(sign_key);
    input.extend(le32(random as i64));
    input.extend_from_slice(sign_key);
    let hash = sm3(&input);
    let d1 = ((random as i32) >> 16) as u32 & 255;
    let mut d2 = ((d1 << 11) | (random >> 24)) ^ ((d1 as i32 >> 5) as u32) ^ d1;
    d2 = !d2;
    (hash, le32(d2 as i64))
}

// ── device / env / message protos ────────────────────────────────────────

/// `deviceProto(deviceId)` — the identity block nested inside env (field 12).
pub fn device_proto(device_id: &str, nonce: &str, process_started: i64) -> Vec<u8> {
    proto(vec![
        (1, Field::Sint(1)),
        (2, Field::Sint(2)),
        (3, Field::Str("8662")),
        (4, Field::Str(device_id)),
        (5, Field::Str(nonce)),
        (6, Field::Str("!noperm!")),
        (7, Field::Sint(-888_888)),
        (8, Field::Sint(-888_888)),
        (9, Field::Sint(3)),
        (10, Field::Sint(-888_888)),
        (11, Field::Str("!notset!")),
        (12, Field::Str("Asia/Shanghai,8")),
        (13, Field::Str("zh_CN")),
        (14, Field::Sint(4)),
        (16, Field::Float(0.0)),
        (17, Field::Float(0.0)),
        (18, Field::Float(0.0)),
        (19, Field::Float(0.0)),
        (20, Field::Float(0.0)),
        (21, Field::Float(0.0)),
        (22, Field::Str("16")),
        (23, Field::Sint(41)),
        (24, Field::Sint(36)),
        (25, Field::Sint(process_started)),
        (26, Field::Sint(process_started)),
        (27, Field::Sint(process_started)),
        (28, Field::Sint(process_started)),
        (29, Field::Sint(-1)),
        (30, Field::Str("25053RT47C")),
        (31, Field::Str("Redmi")),
        (32, Field::Str("25053RT47C")),
        (33, Field::Str("25053RT47C")),
        (34, Field::Str("Xiaomi")),
        (35, Field::Str("Redmi")),
        (36, Field::Str("Redmi")),
        (38, Field::Sint(31)),
    ])
}

/// The identity that must stay consistent between the query and the signature.
pub struct SignIdentity {
    pub iid: String,
    pub device_id: String,
    pub version_code: String,
    pub version_name: String,
}

/// `buildMedusa(url, body, khronos, device)`.
///
/// `process_nonce` / `process_started` are the per-launch values the reference
/// captures once at module load; they are threaded through explicitly so the
/// Rust port can replay golden vectors.
pub fn build_medusa(
    url: &str,
    body: Option<&[u8]>,
    khronos: u32,
    identity: &SignIdentity,
    process_nonce: &str,
    process_started: i64,
    now_ms: i64,
    rng: &SignRng,
) -> String {
    let body_md5 = match body {
        Some(value) if !value.is_empty() => md5_bytes(value),
        _ => vec![0u8; 16],
    };
    let q_sm3 = query_sm3(url);
    let ts = le32(khronos as i64);
    let query_body_ts = hash_f13(&q_sm3, &body_md5, &ts, khronos);

    let nested = proto(vec![
        (1, Field::Sint(111)),
        (2, Field::Sint(10)),
        (3, Field::Sint(694_367)),
        (5, Field::Sint(586_952_199)),
    ]);

    let message_rand = rng.next_u32();
    let env_launch = rng.next_range(0, 21) + 100;
    let env_pid = rng.next_range(0, 2000) + 10_001;
    let env = proto(vec![
        (1, Field::Sint(env_launch as i64)),
        (2, Field::Sint(146_331_399)),
        (3, Field::Sint(146_331_396)),
        (5, Field::Sint(7)),
        (6, Field::Str("v04.06.04.03-bugfix")),
        (7, Field::Sint(env_pid as i64)),
        (
            12,
            Field::Message(device_proto(
                &identity.device_id,
                process_nonce,
                process_started,
            )),
        ),
        (
            13,
            Field::Message(proto(vec![
                (1, Field::Sint(now_ms / 1000)),
                (2, Field::Sint(-2)),
                (4, Field::Sint(200)),
            ])),
        ),
        (14, Field::Str(&identity.version_name)),
    ]);

    let mut query_hash_input = Vec::new();
    query_hash_input.extend_from_slice(url.split('?').nth(1).unwrap_or("").as_bytes());
    query_hash_input.extend_from_slice(&body_md5);
    query_hash_input.extend_from_slice(b"none");
    let query_hash = sm3(&query_hash_input);

    let cmr = "{\"cmr\":16777216,\"cmr2\":16777216,\"un_h\":1879194040,\"vpn\":0,\"kd\":0,\"fkd\":3672518972,\"pd\":-1872573247,\"dyn\":\"\",\"do\":0,\"tk\":true}";
    let message = proto(vec![
        (1, Field::Bytes(hex::decode(APP_TOKEN_MD5).unwrap())),
        (2, Field::Sint(3)),
        (3, Field::Sint(message_rand as i64)),
        (4, Field::Str("8662")),
        (5, Field::Str(&identity.device_id)),
        (6, Field::Str("1588093228")),
        (7, Field::Str(&identity.version_name)),
        (8, Field::Str("v04.06.04-ml-android")),
        (9, Field::Sint(67_503_104)),
        (10, Field::Bytes(hex::decode("4001000000000000").unwrap())),
        (12, Field::Sint(khronos as i64)),
        (13, Field::Bytes(query_body_ts.clone())),
        (14, Field::Bytes(slice(&q_sm3, 0, 6))),
        (15, Field::Message(nested)),
        (16, Field::Str(process_nonce)),
        (17, Field::Sint(khronos as i64)),
        (19, Field::Bytes(query_hash)),
        (20, Field::Str("none")),
        (21, Field::Sint(312)),
        (23, Field::Message(env)),
        (24, Field::Str(cmr)),
    ]);

    let random = rng.next_u32();
    let sign_key = hex::decode(SIGN_KEY).unwrap();
    let (hash, seed) = key_hash(&sign_key, random);
    let transformed = xmxor(&message, &hash);
    let mut prefixed = hex::decode("4001000000000000").unwrap();
    prefixed.extend_from_slice(&transformed);
    let mut transformed = prefixed;
    transformed.reverse();
    for (i, byte) in transformed.iter_mut().enumerate() {
        // JS `seed[(~i)&3]` walks the seed backwards 3,2,1,0 — `!i` in Rust is
        // boolean NOT, so negate the integer explicitly.
        *byte ^= seed[((!(i as i32)) & 3) as usize];
    }

    let check =
        ((q_sm3[0] as u32 & 63) << 14) | 0x1800_0001 | ((query_body_ts[0] as u32 & 63) << 8);
    let xm_rand = rng.next_u32();

    let mut packed: Vec<u8> = Vec::new();
    packed.push(0x35);
    packed.extend(le32(xm_rand as i64));
    packed.extend(le32(check as i64));
    packed.extend_from_slice(&transformed);
    packed.push(((random as i32 >> 16) & 255) as u8);
    packed.push(((random as i32 >> 24) & 255) as u8);

    let aes = AesV3::new(&hex::decode(AES_KEY).unwrap(), khronos);
    let encrypted = aes.encrypt(&packed, &hex::decode(AES_IV).unwrap());

    let version = hex::decode(format!("03000000{APP_TOKEN_MD5}")).unwrap();
    let mut prefix = vec![0u8; 20];
    for i in (0..20).step_by(4) {
        let value = read_u32_le(&version, i) ^ khronos;
        prefix[i..i + 4].copy_from_slice(&value.to_le_bytes());
    }

    let mut result = prefix;
    result.push((random & 255) as u8);
    result.push(((random >> 8) & 255) as u8);
    result.push(0);
    result.push(1);
    result.extend(encrypted);
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, result)
}

/// `version.readUInt32LE(i)` — little-endian, unlike the word reads inside
/// `hashF13` which are big-endian.
fn read_u32_le(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

/// Kept for the golden-vector harness: the constants that must not drift.
pub fn protocol_constants_digest() -> String {
    let mut joined = Vec::new();
    joined.extend(hex::decode(SIGN_KEY).unwrap());
    joined.extend(hex::decode(AES_KEY).unwrap());
    joined.extend(hex::decode(AES_IV).unwrap());
    joined.extend(hex::decode(SPADE_URL_CONSTANTS).unwrap());
    hex::encode(md5_bytes(&joined))
}
