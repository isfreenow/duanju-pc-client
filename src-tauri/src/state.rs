//! Process identity for the signer.
//!
//! `PROCESS_NONCE` / `PROCESS_STARTED` are the two per-process values the Medusa
//! payload mixes in; the reference generates them once at module load and reuses
//! them for every request, so they live behind a `OnceLock` here. They are not
//! secrets the UI needs: no command returns them.

use std::sync::OnceLock;

use base64::Engine;

pub struct ProcessIdentity {
    /// `crypto.randomBytes(18).toString('base64url')`
    pub nonce: String,
    /// `Date.now()` at process start, in milliseconds.
    pub started_ms: i64,
}

impl ProcessIdentity {
    pub fn current() -> &'static Self {
        static IDENTITY: OnceLock<ProcessIdentity> = OnceLock::new();
        IDENTITY.get_or_init(|| {
            let mut bytes = [0u8; 18];
            rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
            ProcessIdentity {
                nonce: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes),
                started_ms: crate::api::now_millis(),
            }
        })
    }
}
