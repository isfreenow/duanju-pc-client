//! X-Helios — the MD5-seeded 64-bit Feistel block cipher over `khronos-1588093228-8662`.
//!
//! Port of `helios()` in `app-source.js`. Two details are load-bearing and must
//! not be "cleaned up":
//!
//! * The key schedule reads the *hex string* of the MD5 digest as ASCII bytes
//!   (`Buffer.from(digest.toString('hex'), 'ascii')`), not the raw digest.
//! * The round table is built through a queue that is seeded with words[2..4]
//!   while the first two words are already the running `b0`/`b8` state.
//!
//! Every addition is 64-bit wrapping (`BigInt.asUintN(64, _)` in the reference).

use std::collections::VecDeque;

use base64::Engine;

use super::bits::{le32, read_le64, ror64};
use super::f13::md5_bytes;
use super::rand::SignRng;

/// `helios(khronos)` — the base64 `X-Helios` header value.
pub fn helios(khronos: u32, rng: &SignRng) -> String {
    let random = rng.next_u32();

    // `Buffer.concat([le32(random), Buffer.from('8662')])`
    let mut seed = le32(random as i64).to_vec();
    seed.extend_from_slice(b"8662");
    let ascii = hex::encode(md5_bytes(&seed));
    let ascii = ascii.as_bytes();

    let words: Vec<u64> = (0..4).map(|i| read_le64(ascii, i * 8)).collect();
    let mut table = vec![words[0]];
    let mut b0 = words[0];
    let mut b8 = words[1];
    // `words.splice(0, 2)` leaves words[2] and words[3] in the queue, and each
    // round pushes the new value then shifts the oldest one off the front.
    //
    // The queue and the table deliberately hold different values: the reference
    // pushes the pre-mix `x8` into the queue and only the further
    // `x8 ^ ror64(b0, 61)` mix into the table it encrypts with.
    let mut queue: VecDeque<u64> = VecDeque::from([words[2], words[3]]);
    for i in 0..34u64 {
        let mut x8 = ror64(b8, 8).wrapping_add(b0) ^ i;
        queue.push_back(x8);
        x8 ^= ror64(b0, 61);
        table.push(x8);
        b0 = x8;
        b8 = queue.pop_front().expect("queue never empties: push and shift balance");
    }

    let text = format!("{khronos}-1588093228-8662");
    // `Math.ceil((len + 1) / 16) * 16` with fill byte `16 - (len % 16)`, which is
    // exactly PKCS#7 for every length the reference can produce.
    let mut padded = vec![(16 - (text.len() % 16)) as u8; ((text.len() + 1 + 15) / 16) * 16];
    padded[..text.len()].copy_from_slice(text.as_bytes());

    let mut output: Vec<u8> = Vec::new();
    for block in padded.chunks_exact(16) {
        let mut a = read_le64(block, 0);
        let mut b = read_le64(block, 8);
        for i in 0..34 {
            b = table[i] ^ a.wrapping_add(ror64(b, 8));
            a = b ^ ror64(a, 61);
        }
        output.extend_from_slice(&a.to_le_bytes());
        output.extend_from_slice(&b.to_le_bytes());
    }

    let mut result = le32(random as i64).to_vec();
    result.extend_from_slice(&output);
    base64::engine::general_purpose::STANDARD.encode(result)
}
