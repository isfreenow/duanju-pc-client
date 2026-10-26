//! The Medusa inner hash (`hashF13`) and the branch selector (`branchOf`).
//!
//! `branchOf` is not cosmetic: the reference rejects some signature variants
//! outright, which is why `signedVideoRequest` jitters `_rticket` until the
//! branch is not 1 and `signedSearchRequest` jitters until it is 0. Reproducing
//! the branch arithmetic exactly is what lets the Rust layer pick the same
//! ticket offsets as the working reference.

use md5::{Digest, Md5};

use super::bits::{bxor, le32, read_le32, ror32, rol32};
use super::sm3::sm3;
use super::tables::{
    BRANCH0_PAD_SEED, BRANCH2_N1, BRANCH2_ORDERS, BRANCH2_PAD_SEED, BRANCH2_SV, F13_CHOSEN,
    F13_INIT, IV_V1_LIST, MD5_V3_SHIFTS, TT02,
};

/// MD5 digest bytes (the reference feeds raw digests into Medusa, not hex).
pub fn md5_bytes(data: &[u8]) -> Vec<u8> {
    let mut hasher = Md5::new();
    hasher.update(data);
    hasher.finalize().to_vec()
}

/// SM3 of the query part of a signed URL — `sm3(url.split('?')[1])`.
pub fn query_sm3(url: &str) -> Vec<u8> {
    let query = url.split('?').nth(1).unwrap_or("");
    sm3(query.as_bytes())
}

/// `getIv` — the running 32-bit mixer that folds a buffer into an IV.
pub fn get_iv(iv: u32, data: &[u8]) -> u32 {
    let mut value = iv;
    for (i, byte) in data.iter().enumerate() {
        let byte = *byte as u32;
        if i & 1 == 0 {
            value = (value >> 4) ^ value ^ value.wrapping_shl(6) ^ byte;
        } else {
            value = !((value >> 7) ^ value ^ (byte | value.wrapping_shl(12)));
        }
    }
    value
}

/// `sumMd5` — the 12-byte checksum folded into every Medusa hash output.
pub fn sum_md5(data: &[u8]) -> u32 {
    let mut check: u32 = 0x2022_0420;
    for i in 0..12 {
        // JS reads past the buffer as `undefined`, which XOR coerces to 0.
        let byte = data.get(i).copied().unwrap_or(0) as u32;
        let temp = if i & 1 == 0 {
            (check >> 3) ^ check
        } else {
            (check >> 5) ^ check
        };
        check = if i & 1 == 0 {
            byte ^ check.wrapping_shl(7)
        } else {
            byte | check.wrapping_shl(11)
        };
        if i & 1 != 0 {
            check = !check;
        }
        check ^= temp;
    }
    (check | 4) ^ 0x0100_0000
}

/// The IV chain shared by `hashF13` and `branchOf`.
fn mix_iv(query_sm3: &[u8], body_md5: &[u8], ts_bytes: &[u8; 4]) -> u32 {
    get_iv(get_iv(get_iv(0x2023_0928, query_sm3), body_md5), ts_bytes)
}

/// `low - ((low * 171) >> 9) * 3` — yields 0, 1 or 2 for low in 0..=15.
fn branch_of_iv(iv: u32) -> u32 {
    let low = iv & 15;
    let iv_v0 = (low * 171) >> 9;
    low - iv_v0 * 3
}

fn md5_v3_step(kind: u32, a: u32, b: u32, c: u32, d: u32, m: u32, shift: u32, constant: u32) -> u32 {
    let f = match kind {
        0 => (b & c) | (!b & d),
        1 => (b & d) | (c & !d),
        2 => b ^ c ^ d,
        _ => c ^ (b | !d),
    };
    let inner = a
        .wrapping_add(f)
        .wrapping_add(m)
        .wrapping_add(constant);
    b.wrapping_add(rol32(inner as i64, shift))
}

/// `md5SumV3` — the rotated-constant MD5 variant used by Medusa branch 2.
/// `message` must be 64 bytes (16 LE words) and `orders` 64 bytes.
fn md5_sum_v3(message: &[u8], count_v2: u32, orders: &[u8], count_v1: u32) -> Vec<u8> {
    let sv: Vec<u32> = BRANCH2_SV.iter().map(|v| ror32(*v as i64, count_v1)).collect();
    let start: [u32; 4] = [
        ror32(0x79e0_f2fb as i64, count_v2),
        ror32(0xc8b5_2570 as i64, count_v2),
        ror32(0xebc2_f8cd as i64, count_v2),
        ror32(0x7c10_4d93 as i64, count_v2),
    ];
    let end_count = (count_v2.wrapping_add(6)) & 255;
    let end: [u32; 4] = [
        ror32(0x19be_4866 as i64, end_count),
        ror32(0xe859_86b4 as i64, end_count),
        ror32(0xe19b_326e as i64, end_count),
        ror32(0x71d1_d7d4 as i64, end_count),
    ];
    let m: Vec<u32> = (0..16).map(|i| read_le32(message, i * 4)).collect();

    let run = |kind: u32, shifts: &[u32; 16], offset: usize, abcd: &mut [u32; 4]| {
        for i in 0..16 {
            let index = orders[offset + i] as usize;
            let (a, b, c, d) = (abcd[0], abcd[1], abcd[2], abcd[3]);
            let next = match i & 3 {
                0 => md5_v3_step(kind, a, b, c, d, m[index], shifts[i], sv[offset + i]),
                1 => md5_v3_step(kind, d, a, b, c, m[index], shifts[i], sv[offset + i]),
                2 => md5_v3_step(kind, c, d, a, b, m[index], shifts[i], sv[offset + i]),
                _ => md5_v3_step(kind, b, c, d, a, m[index], shifts[i], sv[offset + i]),
            };
            match i & 3 {
                0 => abcd[0] = next,
                1 => abcd[3] = next,
                2 => abcd[2] = next,
                _ => abcd[1] = next,
            }
        }
    };

    let mut abcd = start;
    run(0, &MD5_V3_SHIFTS[0], 0, &mut abcd);
    run(1, &MD5_V3_SHIFTS[1], 16, &mut abcd);
    run(2, &MD5_V3_SHIFTS[2], 32, &mut abcd);
    run(3, &MD5_V3_SHIFTS[3], 48, &mut abcd);

    let mut ret = vec![0u8; 16];
    for i in 0..4 {
        let value = start[i].wrapping_add(abcd[i]) ^ end[i];
        ret[i * 4..i * 4 + 4].copy_from_slice(&value.to_le_bytes());
    }
    let mut out = ret.clone();
    out.extend(le32(sum_md5(&ret) as i64));
    out
}

/// Medusa branch 2.
fn branch2_f13(iv: u32, query_sm3: &[u8], body_md5: &[u8], ts_bytes: &[u8; 4], khronos: u32) -> Vec<u8> {
    let v = (iv & 13) * 86;
    let iv_v0 = ((v >> 15) & 255) + ((v >> 8) & 255);
    let n1 = BRANCH2_N1[iv_v0 as usize];
    let count_v1 = (n1.wrapping_add(khronos).wrapping_add(1)) & 255;
    let count_v2 = n1.wrapping_add(khronos);
    let shift = (count_v2.wrapping_add(5)) & 7;
    let pad: Vec<u8> = BRANCH2_PAD_SEED
        .iter()
        .map(|seed| ((((*seed as u32) | (*seed as u32) << 8) >> shift) & 255) as u8)
        .collect();

    let mut input = Vec::with_capacity(64);
    input.extend_from_slice(query_sm3);
    input.extend_from_slice(body_md5);
    input.extend_from_slice(ts_bytes);
    input.extend_from_slice(&pad);
    input.extend_from_slice(&[0xa0, 0x01, 0x00, 0x00]);

    let orders_start = (iv_v0 as usize) << 6;
    md5_sum_v3(
        &input,
        count_v2,
        &BRANCH2_ORDERS[orders_start..orders_start + 64],
        count_v1,
    )
}

/// Medusa branch 0 — the long SHA-1-like compression the reference inlines.
fn branch0_f13(iv: u32, query_sm3: &[u8], body_md5: &[u8], ts_bytes: &[u8; 4], khronos: u32) -> Vec<u8> {
    let low = iv & 15;
    let iv_v0 = ((low * 171) >> 9) as usize;
    let iv_v1 = IV_V1_LIST[iv_v0];
    let count_v1 = iv_v1.wrapping_add(khronos).wrapping_add(1) & 255;
    let count_v2 = iv_v1.wrapping_add(khronos);

    let tt: Vec<u32> = TT02.iter().map(|v| ror32(*v as i64, count_v1)).collect();
    let n0 = (count_v2.wrapping_add(2)) & 7;
    let pad: Vec<u8> = BRANCH0_PAD_SEED
        .iter()
        .map(|seed| ((((*seed as u32) | (*seed as u32) << 8) >> n0) & 255) as u8)
        .collect();

    let mut data = Vec::with_capacity(64);
    data.extend_from_slice(query_sm3);
    data.extend_from_slice(body_md5);
    data.extend_from_slice(ts_bytes);
    data.extend_from_slice(&pad);
    data.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0x01, 0xa0]);

    // Message schedule: 16 words, then 112 more exactly as the reference fills them.
    let mut di: Vec<u32> = (0..data.len() / 4)
        .map(|i| u32::from_be_bytes(data[i * 4..i * 4 + 4].try_into().unwrap()))
        .collect();
    let mut di0 = di[0];
    for i in 0..112 {
        let di1 = di[i + 1];
        let di14 = di[i + 14];
        let r1 = rol32(di1 as i64, 14) ^ rol32(di1 as i64, 25) ^ (di1 >> 3);
        let r2 = rol32(di14 as i64, 13) ^ rol32(di14 as i64, 15) ^ (di14 >> 10);
        di.push(
            di0.wrapping_add(di[i + 9])
                .wrapping_add(r1)
                .wrapping_add(r2),
        );
        di0 = di1;
    }

    let init: Vec<u32> = F13_INIT
        .iter()
        .map(|v| ror32(*v as i64, count_v2 & 31))
        .collect();
    let chosen = &F13_CHOSEN[iv_v0];
    let mut d = init.clone();
    for i in 0..chosen[0] {
        let base = iv_v1.wrapping_add(i);
        let n0v = di[(base & 127) as usize];
        let d1 = d[chosen[1] as usize];
        let d2 = d[chosen[2] as usize];
        let d3 = d[chosen[3] as usize];
        let d4 = d[chosen[4] as usize];
        let d5 = d[chosen[5] as usize];
        let d6 = d[chosen[6] as usize];
        let d7 = d[chosen[7] as usize];
        let n1v = ((d3 ^ d4) & d1) ^ d3;
        let n2 = rol32(d1 as i64, 26) ^ rol32(d1 as i64, 21) ^ rol32(d1 as i64, 7);
        let n4 = n0v
            .wrapping_add(n1v)
            .wrapping_add(n2)
            .wrapping_add(tt[(base & 63) as usize])
            .wrapping_add(d5);
        let n5 = rol32(d2 as i64, 30) ^ rol32(d2 as i64, 19) ^ rol32(d2 as i64, 10);
        let n6 = (d2 & d6) | ((d2 | d6) & d7);
        let n7 = n5.wrapping_add(n6);
        let old = d[chosen[9] as usize];
        let popped = d.pop().expect("8 lanes");
        d.insert(0, popped);
        d[chosen[10] as usize] = n7.wrapping_add(n4);
        d[chosen[8] as usize] = old.wrapping_add(n4);
    }

    let mut ret = vec![0u8; 32];
    for i in 0..8 {
        let value = d[i].wrapping_add(init[i]);
        ret[i * 4..i * 4 + 4].copy_from_slice(&value.to_be_bytes());
    }
    let folded = bxor(&ret[..16], &ret[16..]);
    ret[..16].copy_from_slice(&folded);

    let mut out = ret[..16].to_vec();
    out.extend(le32(sum_md5(&ret[..16]) as i64));
    out
}

/// `hashF13(querySm3, bodyMd5, tsBytes, khronos)` — 36 or 20 bytes depending on
/// which branch the IV lands in.
pub fn hash_f13(
    query_sm3: &[u8],
    body_md5: &[u8],
    ts_bytes: &[u8; 4],
    khronos: u32,
) -> Vec<u8> {
    let iv = mix_iv(query_sm3, body_md5, ts_bytes);
    let branch = branch_of_iv(iv);
    if branch == 2 {
        return branch2_f13(iv, query_sm3, body_md5, ts_bytes, khronos);
    }
    if branch != 0 {
        let mut joined = query_sm3.to_vec();
        joined.extend_from_slice(body_md5);
        let mut out = query_sm3[..16].to_vec();
        out.extend_from_slice(body_md5);
        out.extend(le32(sum_md5(&joined) as i64));
        return out;
    }
    branch0_f13(iv, query_sm3, body_md5, ts_bytes, khronos)
}

/// `branchOf(url, body, khronos)` — the variant gate used by the ticket jitter.
pub fn branch_of(url: &str, body: Option<&[u8]>, khronos: u32) -> u32 {
    let body_md5 = match body {
        Some(value) if !value.is_empty() => md5_bytes(value),
        _ => vec![0u8; 16],
    };
    let q = query_sm3(url);
    // `le32(khronos)` — little-endian, unlike the big-endian timestamp that
    // X-Gorgon mixes in.
    let ts = le32(khronos as i64);
    let iv = mix_iv(&q, &body_md5, &ts);
    branch_of_iv(iv)
}
