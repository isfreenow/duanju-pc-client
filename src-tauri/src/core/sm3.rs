//! SM3 (GB/T 32905-2016) digest, ported byte-for-byte from the reference's
//! `src/native/sm3.js`.
//!
//! The reference carries its own pure implementation because Electron ships
//! BoringSSL without SM3. In Rust we could reach for a crate, but keeping the
//! same algorithm here means the golden vectors from `generate.cjs` validate the
//! exact code path the app uses.

/// SM3 over `input`, returning 32 bytes.
pub fn sm3(input: &[u8]) -> Vec<u8> {
    // Padding: 0x80, zeros, then the 64-bit big-endian bit length, to a
    // multiple of 64. `Math.ceil((len + 9) / 64) * 64` in the reference.
    let total = (input.len() + 9 + 63) / 64 * 64;
    let mut data = vec![0u8; total];
    data[..input.len()].copy_from_slice(input);
    data[input.len()] = 0x80;
    let bit_len = (input.len() as u64) * 8;
    data[total - 8..].copy_from_slice(&bit_len.to_be_bytes());

    let mut state: [u32; 8] = [
        0x7380_166f,
        0x4914_b2b9,
        0x1724_42d7,
        0xda8a_0600,
        0xa96f_30bc,
        0x1631_38aa,
        0xe38d_ee4d,
        0xb0fb_0e4e,
    ];
    let rotate = |x: u32, n: u32| x.rotate_left(n & 31);

    for offset in (0..data.len()).step_by(64) {
        let mut words = vec![0u32; 68];
        for i in 0..16 {
            words[i] =
                u32::from_be_bytes(data[offset + i * 4..offset + i * 4 + 4].try_into().unwrap());
        }
        for i in 16..68 {
            let x = words[i - 16] ^ words[i - 9] ^ rotate(words[i - 3], 15);
            words[i] = x ^ rotate(x, 15) ^ rotate(x, 23) ^ rotate(words[i - 13], 7) ^ words[i - 6];
        }

        let (mut a, mut b, mut c, mut d) = (state[0], state[1], state[2], state[3]);
        let (mut e, mut f, mut g, mut h) = (state[4], state[5], state[6], state[7]);
        for i in 0..64 {
            let a12 = rotate(a, 12);
            let k: u32 = if i < 16 { 0x79cc_4519 } else { 0x7a87_9d8a };
            let s1 = rotate(
                a12.wrapping_add(e)
                    .wrapping_add(k.rotate_left((i & 31) as u32)),
                7,
            );
            let s2 = s1 ^ a12;
            let ff = if i < 16 {
                a ^ b ^ c
            } else {
                (a & b) | (a & c) | (b & c)
            };
            let gg = if i < 16 {
                e ^ f ^ g
            } else {
                (e & f) | (!e & g)
            };
            let t1 = ff
                .wrapping_add(d)
                .wrapping_add(s2)
                .wrapping_add(words[i] ^ words[i + 4]);
            let t2 = gg.wrapping_add(h).wrapping_add(s1).wrapping_add(words[i]);
            d = c;
            c = rotate(b, 9);
            b = a;
            a = t1;
            h = g;
            g = rotate(f, 19);
            f = e;
            e = t2 ^ rotate(t2, 9) ^ rotate(t2, 17);
        }

        let next = [a, b, c, d, e, f, g, h];
        for i in 0..8 {
            state[i] ^= next[i];
        }
    }

    let mut digest = vec![0u8; 32];
    for i in 0..8 {
        digest[i * 4..i * 4 + 4].copy_from_slice(&state[i].to_be_bytes());
    }
    digest
}
