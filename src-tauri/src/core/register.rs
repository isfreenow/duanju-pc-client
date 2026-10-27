//! Anonymous playback-device registration (`playback-device.js` in the reference).
//!
//! The App gateway accepts our signature over an unregistered `device_id`, but it
//! answers that request with a **200 and an empty body** — no `video_model`, so
//! playback can never resolve. The reference avoids this by registering one
//! anonymous device per process against the applog endpoint, then injecting the
//! server-issued `device_id` / `iid` / `cdid` into every signed video request.
//! That handshake lives here.
//!
//! The registration body is a fixed Android phone profile (not a credential: it
//! ships inside the app), gzip-compressed, wrapped in the `ttEncrypt` envelope
//! (a SHA-512 double-round KDF over 32 random bytes plus a salt, then
//! AES-128-CBC with PKCS#7), and POSTed as `application/octet-stream;tt-data=a`.

use std::io::Write;

use aes::Aes128;
use cbc::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
use flate2::{write::GzEncoder, Compression};
use serde_json::{json, Value};
use sha2::{Digest, Sha512};

use crate::api::{request_json, ApiError, ApiResult, API_DOMAINS, Deadline};

use super::tables::SPADE_URL_CONSTANTS;

const REGISTER_URL: &str = "https://log5-applog-lf.fqnovel.com/service/2/device_register/?tt_data=a&cronet_version=8d40f833_2026-03-03&ttnet_version=4.2.243.31-douyin&use_store_region_cookie=1";
const REGISTER_UA: &str = "com.phoenix.read/72232 (Linux; U; Android 16; zh_CN; 25053RT47C; Build/BP2A.250605.031.A3)";
const TIMEOUT_SECS: u64 = 8;

type Aes128CbcEnc = cbc::Encryptor<Aes128>;

/// The server-issued identity every signed video request must carry.
#[derive(Debug, Clone)]
pub struct RegisteredDevice {
    pub device_id: String,
    pub iid: String,
    pub cdid: String,
}

fn sha512(bytes: &[u8]) -> Vec<u8> {
    let mut hasher = Sha512::new();
    hasher.update(bytes);
    hasher.finalize().to_vec()
}

/// `crypto.randomUUID()` — a version-4 UUID with the variant nibble set.
fn uuid_v4() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex = hex::encode(b);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// `ttEncrypt`: build the `ss_app_log` envelope for one anonymous device.
fn build_envelope(compressed: &[u8], random: &[u8]) -> ApiResult<Vec<u8>> {
    let derived = {
        let mut hasher = Sha512::new();
        hasher.update(sha512(random));
        hasher.update(hex::decode(SPADE_URL_CONSTANTS).expect("salt table is valid hex"));
        hasher.finalize()
    };
    let mut plaintext = sha512(compressed);
    plaintext.extend_from_slice(compressed);
    let msg_len = plaintext.len();
    // PKCS#7 always appends 1..=16 pad bytes, so the working buffer needs the
    // next whole block even when the message is already block-aligned.
    let mut block = plaintext;
    block.resize((msg_len / 16 + 1) * 16, 0);
    let cipher = Aes128CbcEnc::new((&derived[..16]).into(), (&derived[16..32]).into());
    let ciphertext = cipher
        .encrypt_padded_mut::<Pkcs7>(&mut block, msg_len)
        .map_err(|_| ApiError::Network("设备注册加密失败".to_string()))?
        .to_vec();
    let mut data = Vec::with_capacity(6 + random.len() + ciphertext.len());
    data.extend_from_slice(&hex::decode("746305100000").unwrap());
    data.extend_from_slice(random);
    data.extend_from_slice(&ciphertext);
    Ok(data)
}

/// `responseId(response, name)` — prefer the `*_str` form, digits only, non-zero.
fn response_id(response: &Value, name: &str) -> String {
    let preferred = response
        .get(format!("{name}_str"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let value = if preferred.is_empty() {
        response.get(name)
    } else {
        return sanitize_id(preferred);
    };
    match value {
        Some(Value::Number(number)) => number
            .as_i64()
            .filter(|v| *v > 0)
            .map(|v| v.to_string())
            .unwrap_or_default(),
        Some(other) => sanitize_id(&other.to_string()),
        None => String::new(),
    }
}

fn sanitize_id(value: &str) -> String {
    let value = value.trim();
    let valid = value.len() <= 30
        && !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value.bytes().any(|byte| byte != b'0');
    valid.then(|| value.to_string()).unwrap_or_default()
}

/// `getPlaybackDevice` — register one anonymous device and read back its identity.
pub async fn register_device(client: &reqwest::Client) -> ApiResult<RegisteredDevice> {
    let now_ms = crate::api::now_millis();
    let cdid = uuid_v4();
    let openudid = {
        let mut b = [0u8; 8];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut b);
        hex::encode(b)
    };
    let clientudid = uuid_v4();
    let req_id = uuid_v4();

    // The header is one flat JSON object; it is split across a few `json!` blocks
    // only to keep each macro expansion under the crate recursion limit.
    let mut header = json!({
        "display_name": "红果免费短剧", "update_version_code": 73932, "manifest_version_code": 73932,
        "app_version_minor": "7.3.9.32", "aid": 8662, "channel": "update_64", "package": "com.phoenix.read",
        "app_version": "7.3.9.32", "version_code": 73932, "sdk_version": "3.7.0-rc.34-fanqie-xiaoshuo",
        "sdk_target_version": 29, "git_hash": "3a88f4e", "os": "Android", "os_version": "16", "os_api": 36,
        "device_model": "25053RT47C", "device_brand": "Redmi", "device_manufacturer": "Redmi",
    })
    .as_object()
    .unwrap()
    .clone();
    for part in [
        json!({
            "cpu_abi": "arm64-v8a", "release_build": "b5e187f", "density_dpi": 520, "display_density": "mdpi",
            "resolution": "1280x2772", "language": "zh", "timezone": 8, "access": "wifi", "not_request_sender": 0,
            "carrier": "CHINA MOBILE", "mcc_mnc": "46000", "rom": "eng.root.20231116.160340",
            "rom_version": "W528JS release-keys", "cdid": cdid.clone(),
            "sig_hash": "56a962410c494bbaf0b58dba20cae56f", "openudid": openudid,
            "clientudid": clientudid, "ipv6_list": [], "region": "CN", "tz_name": "Asia/Shanghai",
        }),
        json!({
            "tz_offset": 28800, "sim_region": "cn", "sim_serial_number": [], "oaid_may_support": false,
            "req_id": req_id, "device_platform": "android",
            "custom": { "account_region_src": "fallback", "account_region": "cn", "host_bit": 64, "dragon_device_type": "phone" },
            "pre_installed_channel": "",
            "apk_first_install_time": now_ms, "is_system_app": 0, "sdk_flavor": "china", "guest_mode": 0,
        }),
    ] {
        if let Some(rows) = part.as_object() {
            header.extend(rows.clone());
        }
    }
    let header = Value::Object(header);
    let payload = json!({ "magic_tag": "ss_app_log", "header": header, "_gen_time": now_ms }).to_string();

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(payload.as_bytes())
        .map_err(|_| ApiError::Network("设备注册打包失败".to_string()))?;
    let compressed = encoder
        .finish()
        .map_err(|_| ApiError::Network("设备注册压缩失败".to_string()))?;

    let random = {
        let mut b = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut b);
        b.to_vec()
    };
    let data = build_envelope(&compressed, &random)?;
    let stub = super::medusa::md5_hex_upper(&data);

    let headers = vec![
        ("User-Agent".to_string(), REGISTER_UA.to_string()),
        ("Accept".to_string(), "application/json".to_string()),
        (
            "Content-Type".to_string(),
            "application/octet-stream;tt-data=a".to_string(),
        ),
        ("log-encode-type".to_string(), "gzip".to_string()),
        ("x-ss-dp".to_string(), "8662".to_string()),
        ("x-ss-req-ticket".to_string(), now_ms.to_string()),
        ("x-vc-bdturing-sdk-version".to_string(), "4.0.3.cn".to_string()),
        ("lc".to_string(), "101".to_string()),
        ("sdk-version".to_string(), "2".to_string()),
        ("passport-sdk-version".to_string(), "5051452".to_string()),
        ("x-neptune".to_string(), "-8|50:51:59:30:40:47:49:39".to_string()),
        ("x-ss-stub".to_string(), stub),
    ];

    let response = request_json(
        client,
        REGISTER_URL,
        &headers,
        Some(&data),
        API_DOMAINS,
        Deadline::after(TIMEOUT_SECS),
    )
    .await?;

    let rejected = super::super::api::business_error(&response).is_some();
    let device_id = response_id(&response, "device_id");
    let iid = response_id(&response, "install_id");
    if device_id.is_empty() || iid.is_empty() || rejected {
        return Err(ApiError::Business {
            message: "匿名播放设备注册失败，请重启应用后重试".to_string(),
            code: "DEVICE_REGISTRATION_FAILED".to_string(),
        });
    }
    Ok(RegisteredDevice { device_id, iid, cdid })
}

/// `registration ||= …` — register once per process and reuse the identity, so
/// retries and prefetches never mint extra anonymous devices. A failed attempt is
/// retried on the next call rather than cached.
pub async fn playback_device(client: &reqwest::Client) -> ApiResult<&'static RegisteredDevice> {
    static DEVICE: tokio::sync::OnceCell<RegisteredDevice> = tokio::sync::OnceCell::const_new();
    if let Some(device) = DEVICE.get() {
        return Ok(device);
    }
    let device = register_device(client).await?;
    Ok(DEVICE.get_or_init(|| async move { device }).await)
}
