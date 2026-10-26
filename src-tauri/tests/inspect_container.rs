//! Dev-time measurement of what a resolved App stream actually is: box layout,
//! the `encv`/`enca` protection signalling and the first bytes of the first
//! sample. Nothing here ships to the webview; it exists so the browser-side
//! decryptor can be written against observed facts instead of guesses.
//!
//! Run with: `cargo test --test inspect_container -- --ignored --nocapture`
//!
//! Logs never print the content key or the signed URL — the KID, IV and sample
//! geometry are container metadata and are safe to show.

use hongguo_desktop_client_lib::api::{self, app_video, web};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect::<Vec<_>>().join(" ")
}

fn fourcc(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .map(|ch| if ch.is_ascii_graphic() { ch } else { '.' })
        .collect()
}

fn u32_at(bytes: &[u8], at: usize) -> usize {
    u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(bytes[at..at + 8].try_into().unwrap())
}

struct Mp4Box {
    kind: String,
    /// First byte of the payload.
    body: usize,
    /// One past the last byte of the box.
    end: usize,
}

fn next(bytes: &[u8], at: usize, limit: usize) -> Option<Mp4Box> {
    if at + 8 > limit || bytes.len() < limit {
        return None;
    }
    let declared = u32_at(bytes, at);
    let (size, header_len) = match declared {
        0 => (limit - at, 8),
        1 => {
            if at + 16 > limit {
                return None;
            }
            (u64_at(bytes, at + 8) as usize, 16)
        }
        other => (other, 8),
    };
    let end = at + size;
    if size < header_len || end > limit {
        return None;
    }
    Some(Mp4Box {
        kind: fourcc(&bytes[at + 4..at + 8]),
        body: at + header_len,
        end,
    })
}

const CONTAINERS: &[&str] = &["moov", "trak", "mdia", "minf", "stbl", "edts", "udta", "sinf", "schi", "mvex", "mfra"];
const LEAVES: &[&str] = &[
    "stsd", "stsz", "stco", "co64", "stsc", "stts", "tenc", "senc", "schm", "frma", "keyg", "kagr", "pssh", "btrt",
    "hvcC", "avcC",
];

fn walk(bytes: &[u8], path: &str, at: usize, limit: usize, depth: usize, lines: &mut Vec<String>) {
    if depth > 9 {
        return;
    }
    let mut cursor = at;
    while let Some(box_) = next(bytes, cursor, limit) {
        let here = format!("{path}/{}", box_.kind);
        let size = box_.end - cursor;
        if CONTAINERS.contains(&box_.kind.as_str()) {
            lines.push(format!("{here}[{size}]"));
            walk(bytes, &here, box_.body, box_.end, depth + 1, lines);
        } else if LEAVES.contains(&box_.kind.as_str()) {
            let preview = &bytes[box_.body..box_.end.min(box_.body + 96)];
            lines.push(format!("{here}[{size}] {}", hex(preview)));
        }
        if box_.kind == "stsd" {
            let mut entry = box_.body + 8;
            while let Some(sample) = next(bytes, entry, box_.end) {
                let name = format!("{here}/{}", sample.kind);
                lines.push(format!("{name}[{}] {}", sample.end - entry, hex(&bytes[entry..entry + 16])));
                lines.push(format!("{name}/tail {}", hex(&bytes[(sample.body + 60).min(sample.end)..sample.end])));
                // A visual sample entry carries 74 fixed bytes before its children
                // (sinf/hvcC/btrt…), an audio one only 8.
                let fixed = match sample.kind.as_str() {
                    "encv" | "hvc1" | "hev1" | "avc1" | "avc3" | "vpc1" | "resv" => 74,
                    _ => 8,
                };
                walk(bytes, &name, sample.body + fixed, sample.end, depth + 1, lines);
                entry = sample.end;
            }
        }
        cursor = box_.end;
    }
}

#[tokio::test]
#[ignore = "downloads a whole episode (~27 MB) from the media CDN"]
async fn measure_container_and_encryption() {
    let client = api::build_client().expect("client");
    let catalog = web::browse_list(&client, "hot-drama", "", 1, true).await.expect("榜单");
    let card = &catalog.results[0];
    let list = web::fetch_episode_list(&client, &card.series_id).await.expect("分集");
    let variants = app_video::fetch_app_play_url(&client, &list.episodes[0].vid).await.expect("片源");
    let variant = variants
        .iter()
        .find(|row| row.quality == "1080p")
        .unwrap_or(&variants[0]);
    let key_hex = variant.content_key.as_deref().expect("候选应带 spade 密钥");

    let response = client
        .get(&variant.url)
        .header("User-Agent", &variant.user_agent)
        .send()
        .await
        .expect("下载请求");
    let body = response.bytes().await.expect("字节");
    println!(
        "「{}」第 1 集 {}，{} 字节，密钥 {} 位十六进制（值不外泄）",
        card.series_title,
        variant.quality,
        body.len(),
        key_hex.len()
    );

    let mut lines = Vec::new();
    walk(&body, "", 0, body.len(), 0, &mut lines);
    for line in &lines {
        println!("{line}");
    }

    if let Some(at) = body.windows(8).position(|window| &window[4..8] == b"mdat") {
        let head = at + 8;
        println!("mdat 数据起点={head}，前 64 字节：{}", hex(&body[head..head + 64]));
    }

    // `HG_FIXTURE=<path>` keeps one whole episode out of band so the browser-side
    // decryptor can be tested against real bytes instead of a hand-written fixture.
    if let Ok(path) = std::env::var("HG_FIXTURE") {
        std::fs::write(&path, &body).expect("写入样本");
        if let Some(key) = variant.content_key.as_deref() {
            std::fs::write(format!("{path}.keyhex"), key).expect("写入样本密钥");
        }
        println!("样本已存到 {path}（{} 字节），密钥同目录 .keyhex", body.len());
    }
}
