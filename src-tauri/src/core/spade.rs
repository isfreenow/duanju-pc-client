//! Spade — the App-side content-key derivation and encrypted play-URL recovery.
//!
//! `deriveContentKey` turns the `spade_a` blob into the AES key for the media
//! segment; `decryptSpadeUrl` recovers the real CDN URL from the encrypted
//! `downloadable_mp4_url`. Both are byte-for-byte ports: the popcount
//! recurrence and the SHA-512 chain are not guessable from their intent.
//!
//! Nothing here may be reachable from the webview. The derived key and the
//! decrypted URL stay inside Rust and are consumed by the playback protocol.

use aes::Aes128;
use cbc::cipher::{block_padding::NoPadding, BlockDecryptMut, KeyIvInit};
use sha2::{Digest, Sha512};

use super::bits::popcount;
use super::tables::SPADE_URL_CONSTANTS;

type Aes128CbcDec = cbc::Decryptor<Aes128>;

/// `Buffer.from(value, 'base64')` is lenient — it maps `-`/`_` and skips
/// characters outside the alphabet, and it tolerates missing padding. The Rust
/// decoder is strict, so normalise the same way before handing it the bytes.
pub fn base64_lenient(value: &str) -> Vec<u8> {
    let mut cleaned = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '-' => cleaned.push('+'),
            '_' => cleaned.push('/'),
            'A'..='Z' | 'a'..='z' | '0'..='9' | '+' | '/' => cleaned.push(ch),
            _ => {}
        }
    }
    // A trailing single character carries no bits; Node drops it the same way.
    if cleaned.len() % 4 == 1 {
        cleaned.pop();
    }
    base64::Engine::decode(&base64::engine::general_purpose::STANDARD_NO_PAD, &cleaned)
        .unwrap_or_default()
}

/// `cleanUrl` — unescape the three forms the App payload uses.
pub fn clean_url(value: &str) -> String {
    value
        .trim()
        .replace("\\/", "/")
        .replace("\\u0026", "&")
        .replace("&amp;", "&")
}

/// `deriveContentKey(spade_a)` → the 16-byte AES key for the media segment.
pub fn derive_content_key(value: &str) -> Result<Vec<u8>, String> {
    let raw = base64_lenient(value.trim());
    if raw.len() < 3 {
        return Err("spade_a too short".to_string());
    }
    // The reference computes this in JavaScript's number space, where a block that
    // claims more than it holds simply goes negative and is then rejected. Do the
    // same in i64 instead of wrapping or panicking on subtraction overflow.
    let claimed = raw.len() as i64 - i64::from(raw[0] ^ raw[1] ^ raw[2]) + 47;
    let key_len = claimed.min(raw.len() as i64 - 1);
    if key_len < 33 {
        return Err("spade_a key length invalid".to_string());
    }
    let key_len = key_len as usize;
    let mut work = raw[1..1 + key_len].to_vec();
    let mut a: u8 = 85;
    let mut b: u8 = 246;
    for i in 0..key_len {
        let old = work[i];
        let previous = if i & 1 == 1 { a } else { b };
        if i & 1 == 1 {
            a = old;
        } else {
            b = old;
        }
        // JS: `(-21 - pop + (previous ^ old)) & 255` — the negation happens in
        // arbitrary-precision land before the mask, so compute in i32.
        let value = (-21i32 - popcount(i as u32) as i32 + (previous ^ old) as i32) & 255;
        work[i] = value as u8;
    }
    let hex_part = String::from_utf8_lossy(&work[1..33]).to_string();
    if hex_part.len() != 32 || !hex_part.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("App 视频密钥格式无效".to_string());
    }
    Ok(hex::decode(hex_part).map_err(|error| error.to_string())?)
}

/// `decryptSpadeUrl(value, seed)` — the plaintext CDN URL behind the App's
/// encrypted `downloadable_mp4_url`.
pub fn decrypt_spade_url(value: &str, seed: &[u8]) -> Result<String, String> {
    let encoded = value.trim();
    if encoded.is_empty() {
        return Ok(String::new());
    }
    let raw = base64_lenient(encoded);
    if raw.len() < 5 {
        return Err("spade URL ciphertext too short".to_string());
    }
    if raw[0] != 0xa8 || raw[2] != 0x01 || raw[3] != 0x00 {
        return Err("spade URL header format error".to_string());
    }
    let cipher_length = (raw.len() - 4) / 16 * 16;
    if cipher_length == 0 {
        return Err("spade URL ciphertext has no complete block".to_string());
    }
    let constants = hex::decode(SPADE_URL_CONSTANTS).expect("constant table is valid hex");

    let h1 = {
        let mut hasher = Sha512::new();
        hasher.update(seed);
        hasher.finalize()
    };
    let h2 = {
        let mut hasher = Sha512::new();
        hasher.update(h1);
        hasher.update(constants);
        hasher.finalize()
    };

    let decrypter = Aes128CbcDec::new((&h2[..16]).into(), (&h2[16..32]).into());
    let mut ciphertext = raw[4..4 + cipher_length].to_vec();
    let mut plaintext = decrypter
        .decrypt_padded_mut::<NoPadding>(&mut ciphertext)
        .map_err(|_| "spade URL 解密失败".to_string())?
        .to_vec();

    if !plaintext.is_empty() {
        let padding = *plaintext.last().unwrap();
        if (1..=16).contains(&padding) && (padding as usize) <= plaintext.len() {
            plaintext.truncate(plaintext.len() - padding as usize);
        }
    }
    while plaintext.last() == Some(&0) {
        plaintext.pop();
    }
    Ok(String::from_utf8_lossy(&plaintext).to_string())
}

