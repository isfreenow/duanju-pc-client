//! Low-level integer helpers that pin the JavaScript reference's semantics.
//!
//! The reference runs on JS numbers, where `>>> 0` coerces and truncates, and
//! `<<` / `>>` shift by `count % 32`. Rust's `<<` panics on a shift of 32 in
//! debug builds, so every rotation here goes through `rotate_left` /
//! `rotate_right`, which match the JS wrap-for-free behaviour.

/// `u32(value)` in the reference: truncate to 32 bits, unsigned.
#[inline]
pub fn u32_(value: i64) -> u32 {
    value as u32
}

#[inline]
pub fn u32_from_u64(value: u64) -> u32 {
    value as u32
}

/// `rol32` — rotate left, shift count taken modulo 32 like JS.
#[inline]
pub fn rol32(value: i64, count: u32) -> u32 {
    u32_(value).rotate_left(count & 31)
}

#[inline]
pub fn ror32(value: i64, count: u32) -> u32 {
    u32_(value).rotate_right(count & 31)
}

/// `ror64` — 64-bit rotate right; the reference masks with `BigInt.asUintN(64, _)`.
#[inline]
pub fn ror64(value: u64, count: u32) -> u64 {
    value.rotate_right(count & 63)
}

#[inline]
pub fn le32(value: i64) -> [u8; 4] {
    u32_(value).to_le_bytes()
}

#[inline]
pub fn be32(value: i64) -> [u8; 4] {
    u32_(value).to_be_bytes()
}

#[inline]
pub fn read_le32(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

#[inline]
pub fn read_be32(data: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

#[inline]
pub fn read_be64(data: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(data[at..at + 8].try_into().expect("8 bytes"))
}

#[inline]
pub fn read_le64(data: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(data[at..at + 8].try_into().expect("8 bytes"))
}

/// protobuf zig-zag for `sint` fields, matching `zigzag()` in the reference.
#[inline]
pub fn zigzag(value: i64) -> u64 {
    if value < 0 {
        (-(value as i128) * 2 - 1) as u64
    } else {
        (value as u64) * 2
    }
}

/// LEB128 varint. The reference loops `while (n > 127)`, emitting 7 bits at a
/// time with the continuation bit set.
pub fn varint(mut value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    while value > 127 {
        out.push(((value & 127) | 128) as u8);
        value >>= 7;
    }
    out.push(value as u8);
    out
}

/// `Buffer.from(data.subarray(a, b))`-style slice that never panics when the
/// reference would silently produce a short buffer.
pub fn slice(data: &[u8], from: usize, to: usize) -> Vec<u8> {
    let from = from.min(data.len());
    let to = to.min(data.len()).max(from);
    data[from..to].to_vec()
}

/// XOR `a` with `b`, repeating `b` (the reference's `xorBytes`).
pub fn xor_bytes_repeating(a: &[u8], b: &[u8]) -> Vec<u8> {
    a.iter()
        .enumerate()
        .map(|(i, byte)| byte ^ b[i % b.len()])
        .collect()
}

/// XOR two buffers up to the shorter length (the reference's `bxor`).
pub fn bxor(a: &[u8], b: &[u8]) -> Vec<u8> {
    let n = a.len().min(b.len());
    (0..n).map(|i| a[i] ^ b[i]).collect()
}

/// population count, used by the spade_a key derivation (`i.toString(2).split('1').length - 1`).
#[inline]
pub fn popcount(value: u32) -> u32 {
    value.count_ones()
}

/// `reverseBits` — bit-reverse a single byte.
#[inline]
pub fn reverse_bits(value: u8) -> u8 {
    value.reverse_bits()
}
