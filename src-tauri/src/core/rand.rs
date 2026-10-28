//! Randomness source used by the signing layer.
//!
//! Every signing primitive takes its randomness as an explicit parameter so the
//! golden-vector test can replay the reference byte for byte. In production the
//! same calls are fed from the OS RNG.

use rand::Rng;

/// Supplies the four things the reference pulls out of thin air: `Math.random()`
/// doubles, whole-byte buffers, and the two integers derived from them.
#[derive(Clone)]
pub enum SignRng {
    /// Real entropy.
    System,
    /// Replay of a recorded `Math.random` stream plus the fixed byte pool the
    /// generator script uses for `crypto.randomBytes`.
    Replay {
        stream: Vec<f64>,
        position: std::cell::Cell<usize>,
        pool: Vec<u8>,
        cursor: std::cell::Cell<usize>,
    },
}

impl SignRng {
    pub fn system() -> Self {
        SignRng::System
    }

    pub fn replay(stream: &[f64], pool: &[u8], cursor: usize) -> Self {
        SignRng::Replay {
            stream: stream.to_vec(),
            position: std::cell::Cell::new(0),
            pool: pool.to_vec(),
            cursor: std::cell::Cell::new(cursor),
        }
    }

    /// How much of the recorded stream / pool has been consumed. The golden
    /// vectors assert this so an extra or a missing draw fails even when the
    /// remaining bytes happen to coincide.
    pub fn consumed(&self) -> (usize, usize) {
        match self {
            SignRng::System => (0, 0),
            SignRng::Replay {
                position, cursor, ..
            } => (position.get(), cursor.get()),
        }
    }

    /// A double in [0, 1), matching `Math.random()`.
    pub fn next_f64(&self) -> f64 {
        match self {
            SignRng::System => rand::thread_rng().gen_range(0.0..1.0),
            SignRng::Replay {
                stream, position, ..
            } => {
                let index = position.get();
                position.set(index + 1);
                stream.get(index).copied().unwrap_or_else(|| {
                    panic!(
                        "replay stream exhausted at index {index} — regenerate the golden vectors"
                    )
                })
            }
        }
    }

    /// `Math.floor(Math.random() * 0x100000000)` — the reference's 32-bit draw.
    pub fn next_u32(&self) -> u32 {
        (self.next_f64() * 4_294_967_296.0).floor() as u32
    }

    /// `Math.floor(Math.random() * 65536)` — the X-Gorgon 16-bit draw.
    pub fn next_u16(&self) -> u16 {
        (self.next_f64() * 65_536.0).floor() as u16
    }

    /// `envLaunch` / `envPid` style draws inside the Medusa env proto.
    pub fn next_range(&self, low: u32, span: u32) -> u32 {
        low + (self.next_f64() * span as f64).floor() as u32
    }

    /// `crypto.randomBytes(n)`. Replay reads from `cursor % pool.len()`; the
    /// generator's pool is 4096 bytes and no signing path asks for more than a
    /// couple of hundred, so the wrap-around branch never actually triggers.
    pub fn random_bytes(&self, n: usize) -> Vec<u8> {
        match self {
            SignRng::System => {
                let mut out = vec![0u8; n];
                rand::thread_rng().fill(out.as_mut_slice());
                out
            }
            SignRng::Replay { pool, cursor, .. } => {
                let start = cursor.get() % pool.len();
                cursor.set(cursor.get() + n);
                let mut out = Vec::with_capacity(n);
                for i in 0..n {
                    out.push(pool[(start + i) % pool.len()]);
                }
                out
            }
        }
    }

    /// `PROCESS_NONCE = crypto.randomBytes(18).toString('base64url')` and
    /// `localId()` both come from here.
    pub fn nonce(&self) -> Vec<u8> {
        self.random_bytes(18)
    }

    /// `localId()`: `1000000000000000 + BigInt('0x' + randomBytes(6).hex)` as a
    /// decimal string. The 48-bit value keeps the sum inside u64.
    pub fn local_id(&self) -> String {
        let bytes = self.random_bytes(6);
        let mut value: u64 = 0;
        for byte in bytes {
            value = (value << 8) | byte as u64;
        }
        (1_000_000_000_000_000u64 + value).to_string()
    }
}
