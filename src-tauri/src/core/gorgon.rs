//! X-Gorgon request signature.
//!
//! Ported from `xGorgon` / `rc4Gorgon` / `reverseBits` in the reference's
//! `app-source.js`. Two things must not be "fixed":
//!
//! * the RC4 key-scheduling step is `s[i] = s[j]` (an assignment), not the usual
//!   swap — the server accepts this variant and the swap would break signing;
//! * the per-byte post-mix reads `out[i + 1]`, falling back to `out[0]` on the
//!   last byte, while writing back into `out[i]` in the same pass.

use md5::{Digest, Md5};

use super::bits::reverse_bits;

/// The reference's mutated RC4 keystream generator.
fn rc4_gorgon(data: &[u8], key: &[u8]) -> Vec<u8> {
    let mut s: [u8; 256] = std::array::from_fn(|i| i as u8);
    let mut j: u32 = 0;
    for i in 0..256usize {
        j = (j + s[i] as u32 + key[i % key.len()] as u32) & 255;
        s[i] = s[j as usize]; // assignment on purpose; see the module note
    }
    let mut i: usize = 0;
    let mut j: u32 = 0;
    data.iter()
        .map(|value| {
            i += 1;
            let x = s[i];
            j += x as u32;
            let y = s[(j & 255) as usize];
            s[i] = y;
            *value ^ s[((y as u32 + y as u32) & 255) as usize]
        })
        .collect()
}

fn md5_prefix4(value: &[u8]) -> Vec<u8> {
    let mut hasher = Md5::new();
    hasher.update(value);
    let digest = hasher.finalize();
    digest[..4].to_vec()
}

/// `xGorgon(query, body, timestamp, random)` — hex string, 52 characters.
///
/// `body` is `None`/empty for GET-style requests, where the reference substitutes
/// four zero bytes.
pub fn x_gorgon(query: &str, body: Option<&[u8]>, timestamp: u32, random: u16) -> String {
    let sdk = 67_503_104u32.to_le_bytes();
    let mut input: Vec<u8> = Vec::with_capacity(20);
    input.extend(md5_prefix4(query.as_bytes()));
    match body {
        Some(value) if !value.is_empty() => input.extend(md5_prefix4(value)),
        _ => input.extend([0u8; 4]),
    }
    input.extend([0u8; 4]);
    input.extend(sdk);
    input.extend(timestamp.to_be_bytes());

    let key = [
        0x4a,
        0x40,
        0x16,
        ((random >> 8) & 255) as u8,
        0x47,
        0x6c,
        0x01,
        (random & 255) as u8,
    ];
    let mut out = rc4_gorgon(&input, &key);
    let length = out.len();
    for i in 0..length {
        let value = ((out[i] >> 4) | (out[i] << 4)) & 0xff;
        let next = if i + 1 < length { out[i + 1] } else { out[0] };
        out[i] = (!(reverse_bits((next ^ value) & 255) ^ 20) & 255) as u8;
    }

    let mut result = Vec::with_capacity(26);
    result.extend([0x84, 0x04]);
    result.extend([
        (random & 255) as u8,
        ((random >> 8) & 255) as u8,
        0x40,
        0x01,
    ]);
    result.extend(out);
    hex::encode(result)
}
