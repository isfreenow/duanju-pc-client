//! Byte-exact replay of the reference implementation.
//!
//! `tests/vectors/sign.vectors.json` was produced by `generate.cjs` in
//! `hongguo-research/golden`, which compiles `src/native/app-source.js` and calls
//! its internals directly with `Math.random`, `Date.now` and
//! `crypto.randomBytes` pinned to recorded sequences. Every assertion here is a
//! raw byte comparison — no tolerance, no "looks close enough".
//!
//! If a Rust primitive drifts, the failing vector names the exact group, so the
//! fix belongs in the port, never in the vectors.

use serde_json::Value;

use hongguo_desktop_client_lib::core::bits::reverse_bits;
use hongguo_desktop_client_lib::core::f13::{get_iv, sum_md5};
use hongguo_desktop_client_lib::core::device::{
    signed_search_request, signed_video_request, SearchCursor,
};
use hongguo_desktop_client_lib::core::f13::{branch_of, hash_f13};
use hongguo_desktop_client_lib::core::gorgon::x_gorgon;
use hongguo_desktop_client_lib::core::helios::helios;
use hongguo_desktop_client_lib::core::medusa::{
    device_proto, key_hash, xmxor, AesV3, SignIdentity,
};
use hongguo_desktop_client_lib::core::query::python_url_encode;
use hongguo_desktop_client_lib::core::rand::SignRng;
use hongguo_desktop_client_lib::core::sm3::sm3;
use hongguo_desktop_client_lib::core::spade::{decrypt_spade_url, derive_content_key};
use hongguo_desktop_client_lib::core::tables::SIGN_KEY;

fn vectors() -> Value {
    serde_json::from_str(include_str!("vectors/sign.vectors.json"))
        .expect("golden vectors are valid JSON")
}

fn group(name: &str) -> Vec<Value> {
    let all = vectors();
    all[name]
        .as_array()
        .unwrap_or_else(|| panic!("vector group {name} is missing"))
        .clone()
}

fn unhex(value: &str) -> Vec<u8> {
    hex::decode(value).unwrap_or_else(|error| panic!("bad hex {value:?}: {error}"))
}

fn hex_bytes(value: &[u8]) -> String {
    hex::encode(value)
}

fn pool() -> Vec<u8> {
    unhex(vectors()["meta"]["byte_pool"].as_str().unwrap())
}

fn fixed_now_ms() -> i64 {
    vectors()["meta"]["fixed_now_ms"].as_i64().unwrap()
}

fn process_nonce() -> String {
    vectors()["meta"]["process_nonce"].as_str().unwrap().to_string()
}

fn process_started() -> i64 {
    vectors()["meta"]["process_started"].as_i64().unwrap()
}

/// Build the replay RNG for one vector and assert afterwards that it consumed
/// exactly the recorded draws — an extra or a missing `Math.random` call is a
/// real bug even when the output still matches.
fn replay(vector: &Value) -> SignRng {
    let stream: Vec<f64> = vector["random_stream"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_f64().unwrap())
        .collect();
    // The generator resets the pool cursor before every scenario, so replay
    // always starts at zero and must finish at the recorded cursor.
    SignRng::replay(&stream, &pool(), 0)
}

fn cursor_of(vector: &Value) -> usize {
    vector["random_bytes_cursor"].as_u64().unwrap() as usize
}

fn assert_replayed(vector: &Value, rng: &SignRng) {
    let stream = vector["random_stream"].as_array().unwrap();
    let (draws, cursor) = rng.consumed();
    assert_eq!(
        (draws, cursor),
        (stream.len(), cursor_of(vector)),
        "randomness consumption drifted for {vector:?}"
    );
}

#[test]
fn sm3_matches_the_reference_and_openssl() {
    for vector in group("sm3") {
        let input = unhex(&hex_of_base64(vector["input"].as_str().unwrap()));
        assert_eq!(
            hex_bytes(&sm3(&input)),
            vector["digest"].as_str().unwrap(),
            "SM3 vector {:?}",
            vector["input"]
        );
    }
}

fn hex_of_base64(value: &str) -> String {
    use base64::Engine;
    hex::encode(
        base64::engine::general_purpose::STANDARD
            .decode(value)
            .unwrap(),
    )
}

#[test]
fn url_encode_matches_the_reference() {
    for vector in group("urlEncode") {
        let map = vector["values"].as_object().unwrap();
        let values: Vec<(&str, String)> = map
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str().unwrap().to_string()))
            .collect();
        assert_eq!(
            python_url_encode(&values),
            vector["encoded"].as_str().unwrap()
        );
    }
}

#[test]
fn gorgon_matches_the_reference() {
    for vector in group("gorgon") {
        let body = match vector["body"].as_str() {
            Some(text) => Some(text.as_bytes().to_vec()),
            None => None,
        };
        let signed = x_gorgon(
            vector["query"].as_str().unwrap(),
            body.as_deref(),
            vector["khronos"].as_u64().unwrap() as u32,
            vector["random"].as_u64().unwrap() as u16,
        );
        assert_eq!(
            signed,
            vector["x_gorgon"].as_str().unwrap(),
            "X-Gorgon vector {:?}",
            vector["name"]
        );
    }
}

#[test]
fn branch_selection_matches_the_reference() {
    for vector in group("branchOf") {
        let body = (vector["label"].as_str().unwrap() == "md5-body")
            .then(|| br#"{"a":1}"#.to_vec());
        let branch = branch_of(
            vector["url"].as_str().unwrap(),
            body.as_deref(),
            vector["khronos"].as_u64().unwrap() as u32,
        );
        assert_eq!(branch, vector["branch"].as_u64().unwrap() as u32);
    }
}

#[test]
fn hash_f13_matches_both_reachable_branches() {
    for vector in group("hashF13") {
        let ts = unhex(vector["tsBytes"].as_str().unwrap());
        let hash = hash_f13(
            &unhex(vector["querySm3"].as_str().unwrap()),
            &unhex(vector["bodyMd5"].as_str().unwrap()),
            ts.as_slice().try_into().unwrap(),
            vector["khronos"].as_u64().unwrap() as u32,
        );
        assert_eq!(
            hex_bytes(&hash),
            vector["hash"].as_str().unwrap(),
            "hashF13 {:?}",
            vector["label"]
        );
    }
}

#[test]
fn aes_v3_matches_the_reference_for_every_word_size() {
    for vector in group("aesV3") {
        let data = unhex(vector["data"].as_str().unwrap());
        let aes = AesV3::new(&unhex(vector["key"].as_str().unwrap()), vector["khronos"].as_u64().unwrap() as u32);
        assert_eq!(
            hex_bytes(&aes.block(&data[..16])),
            vector["block"].as_str().unwrap(),
            "AesV3 block wordSize {:?}",
            vector["wordSize"]
        );
        assert_eq!(
            hex_bytes(&aes.encrypt(&data, &unhex(vector["iv"].as_str().unwrap()))),
            vector["encrypt"].as_str().unwrap(),
            "AesV3 encrypt wordSize {:?}",
            vector["wordSize"]
        );
    }
}

#[test]
fn xmxor_and_key_hash_match_the_reference() {
    let sign_key = unhex(SIGN_KEY);
    for vector in group("keyHash") {
        let (hash, seed) = key_hash(&sign_key, vector["random"].as_u64().unwrap() as u32);
        assert_eq!(hex_bytes(&hash), vector["hash"].as_str().unwrap());
        assert_eq!(hex_bytes(&seed), vector["seed"].as_str().unwrap());
    }
    for vector in group("xmxor") {
        let key = unhex(vector["key"].as_str().unwrap());
        let data = unhex(vector["data"].as_str().unwrap());
        assert_eq!(
            hex_bytes(&xmxor(&data, &key)),
            vector["out"].as_str().unwrap(),
            "xmxor length {}",
            data.len()
        );
    }
}

#[test]
fn device_proto_matches_the_reference() {
    for vector in group("deviceProto") {
        let rng = replay(&vector);
        let encoded = device_proto(
            vector["deviceId"].as_str().unwrap(),
            &process_nonce(),
            process_started(),
        );
        assert_eq!(encoded, base64_of(vector["proto"].as_str().unwrap()));
        assert_replayed(&vector, &rng);
    }
}

fn base64_of(value: &str) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(value).unwrap()
}

#[test]
fn medusa_matches_the_reference() {
    for vector in group("medusa") {
        let rng = replay(&vector);
        // The generator mints the synthetic identity with `crypto.randomBytes`
        // before calling buildMedusa; the port has to do the same or the pool
        // cursor and the signed identity both drift.
        let iid = rng.local_id();
        let device_id = rng.local_id();
        assert_eq!(iid, vector["device_iid"].as_str().unwrap());
        assert_eq!(device_id, vector["device_device_id"].as_str().unwrap());
        let body = vector["body"]
            .as_str()
            .map(base64_of);
        let identity = SignIdentity {
            iid,
            device_id,
            version_code: "71332".to_string(),
            version_name: "7.1.3.32".to_string(),
        };
        let signed = hongguo_desktop_client_lib::core::medusa::build_medusa(
            vector["url"].as_str().unwrap(),
            body.as_deref(),
            vector["khronos"].as_u64().unwrap() as u32,
            &identity,
            &process_nonce(),
            process_started(),
            fixed_now_ms(),
            &rng,
        );
        assert_eq!(
            signed,
            vector["x_medusa"].as_str().unwrap(),
            "X-Medusa {:?}",
            vector["label"]
        );
        assert_replayed(&vector, &rng);
    }
}

#[test]
fn helios_matches_the_reference() {
    for vector in group("helios") {
        let rng = replay(&vector);
        assert_eq!(
            helios(vector["khronos"].as_u64().unwrap() as u32, &rng),
            vector["x_helios"].as_str().unwrap()
        );
        assert_replayed(&vector, &rng);
    }
}

#[test]
fn spade_key_derivation_and_url_decryption_match_the_reference() {
    for vector in group("spadeKey") {
        let key = derive_content_key(vector["spade_a"].as_str().unwrap()).unwrap();
        assert_eq!(hex_bytes(&key), vector["content_key"].as_str().unwrap());
    }
    for vector in group("spadeUrl") {
        let seed = base64_of(vector["seed"].as_str().unwrap());
        let plaintext =
            decrypt_spade_url(vector["ciphertext"].as_str().unwrap(), &seed).unwrap();
        assert_eq!(plaintext, vector["plaintext"].as_str().unwrap());
        assert_eq!(plaintext, vector["decrypted"].as_str().unwrap());
    }
}

#[test]
fn signed_video_request_matches_the_reference() {
    for vector in group("signedVideoRequest") {
        let rng = replay(&vector);
        let payload = vector["payload"].as_str().unwrap().as_bytes();
        let request = signed_video_request(
            payload,
            &rng,
            fixed_now_ms(),
            &process_nonce(),
            process_started(),
        );
        assert_eq!(request.url, vector["url"].as_str().unwrap());
        assert_eq!(
            request.body.as_deref().unwrap(),
            base64_of(vector["body"].as_str().unwrap())
        );
        let expected = vector["headers"].as_object().unwrap();
        assert_eq!(request.headers.len(), expected.len());
        for (name, value) in expected {
            assert_eq!(
                request.header(name),
                Some(value.as_str().unwrap()),
                "header {name}"
            );
        }
        assert_replayed(&vector, &rng);
    }
}

#[test]
fn signed_search_request_matches_the_reference() {
    for vector in group("signedSearchRequest") {
        let rng = replay(&vector);
        let cursor = vector["cursor"].as_object().map(|value| SearchCursor {
            search_id: value.get("search_id").and_then(|v| v.as_str()).map(str::to_string),
            passback: value.get("passback").and_then(|v| v.as_str()).map(str::to_string),
            next_offset: value
                .get("next_offset")
                .map(|v| match v {
                    Value::String(text) => text.to_string(),
                    other => other.to_string(),
                }),
        });
        let request = signed_search_request(
            vector["keyword"].as_str().unwrap(),
            vector["page"].as_i64().unwrap(),
            cursor.as_ref(),
            &rng,
            fixed_now_ms(),
            &process_nonce(),
            process_started(),
        )
        .expect("search signing");
        assert_eq!(request.url, vector["url"].as_str().unwrap());
        assert!(request.body.is_none());
        let expected = vector["headers"].as_object().unwrap();
        assert_eq!(request.headers.len(), expected.len());
        for (name, value) in expected {
            assert_eq!(
                request.header(name),
                Some(value.as_str().unwrap()),
                "header {name}"
            );
        }
        assert_replayed(&vector, &rng);
    }
}

#[test]
fn bit_helpers_match_the_reference() {
    // `bits` vectors cover getIv / sumMd5 / reverseBits, the three primitives the
    // F13 hash is built from.
    for vector in group("bits") {
        let data = unhex(vector["data"].as_str().unwrap());
        assert_eq!(
            get_iv(0x2023_0928, &data),
            vector["getIv_0x20230928"].as_u64().unwrap() as u32
        );
        assert_eq!(
            get_iv(0, &data),
            vector["getIv_zero"].as_u64().unwrap() as u32
        );
        assert_eq!(
            sum_md5(&data),
            vector["sumMd5"].as_u64().unwrap() as u32
        );
        let expected = vector["reverseBits"].as_array().unwrap();
        for (i, want) in expected.iter().enumerate() {
            assert_eq!(reverse_bits(data[i]), want.as_u64().unwrap() as u8);
        }
    }
}

#[test]
fn meta_is_self_consistent() {
    // Guards against a truncated vectors file silently weakening the suite.
    let all = vectors();
    let mut total = 0usize;
    for key in all.as_object().unwrap().keys() {
        if key == "meta" {
            continue;
        }
        let entries = all[key].as_array().unwrap();
        assert!(!entries.is_empty(), "group {key} is empty");
        total += entries.len();
    }
    assert!(total >= 60, "expected the full vector suite, saw {total}");
}
