//! The Hongguo signing core.
//!
//! Everything in this module tree exists to reproduce, byte for byte, what the
//! reference JavaScript implementation produces, because the App gateway
//! validates signatures over the exact query bytes. Two rules govern edits here:
//!
//! 1. Do not "fix" anything in the reference that looks like a bug. The X-Gorgon
//!    RC4 key schedule assigns instead of swapping; `AesV3` is a bespoke cipher
//!    that is not AES; `branchOf` needs `_rticket` jitter. All of it is accepted
//!    by the server, so all of it is reproduced.
//! 2. Do not change bit widths or truncation points. JavaScript coerces through
//!    arbitrary-precision BigInt and `>>> 0`; Rust has to mask explicitly.
//!
//! Correctness is pinned by `tests/golden_sign.rs`, which replays vectors
//! generated from the reference implementation itself.

pub mod bits;
pub mod device;
pub mod f13;
pub mod gorgon;
pub mod helios;
pub mod medusa;
pub mod query;
pub mod rand;
pub mod register;
pub mod sm3;
pub mod spade;
pub mod tables;

pub use device::{playback_request, signed_search_request, signed_video_request, SearchCursor, SignedRequest};
pub use rand::SignRng;
pub use register::{playback_device, RegisteredDevice};
