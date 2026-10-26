//! Why does the media CDN answer 403 to a webview `fetch()` of a URL that Rust
//! can download? Resolve one fresh stream, then ask for 64 bytes under a matrix
//! of header combinations and print only the status codes.
//!
//! Run with: `cargo test --test probe_media_headers -- --ignored --nocapture`
//!
//! Never prints the signed URL or the content key.

use hongguo_desktop_client_lib::api::{self, app_video, web};

const WEBVIEW_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/157.0.0.0 Safari/537.36 Edg/157.0.0.0";

#[tokio::test]
#[ignore = "touches the media CDN; downloads 64 bytes per case"]
async fn compare_header_combinations() {
    let client = api::build_client().expect("client");
    let catalog = web::browse_list(&client, "hot-drama", "", 1, true).await.expect("榜单");
    let list = web::fetch_episode_list(&client, &catalog.results[0].series_id)
        .await
        .expect("分集");
    let variants = app_video::fetch_app_play_url(&client, &list.episodes[0].vid)
        .await
        .expect("片源");
    let variant = &variants[0];
    let app_ua: &str = variant.user_agent.as_str();
    let url = variant.url.clone();

    let cases: Vec<(&str, Vec<(&str, &str)>)> = vec![
        ("A app UA (控制组)", vec![("user-agent", app_ua)]),
        ("B webview UA", vec![("user-agent", WEBVIEW_UA)]),
        ("C 无 UA", vec![]),
        ("D app UA + Origin", vec![("user-agent", app_ua), ("origin", "http://localhost:1420")]),
        ("E app UA + Referer", vec![("user-agent", app_ua), ("referer", "http://localhost:1420/")]),
        ("H app UA + Accept-Encoding identity", vec![("user-agent", app_ua), ("accept-encoding", "identity")]),
    ];

    for (label, headers) in &cases {
        let mut request = client.get(&url).header("range", "bytes=0-63");
        if *label != "C 无 UA" {
            for &(name, value) in headers {
                request = request.header(name, value);
            }
        }
        match request.send().await {
            Ok(response) => {
                let status = response.status().as_u16();
                let len = response.bytes().await.map(|bytes| bytes.len()).unwrap_or(0);
                println!("{label} → HTTP {status}，正文 {len} 字节");
            }
            Err(error) => println!("{label} → 请求失败：{}", error.to_string().lines().next().unwrap_or("未知")),
        }
    }

    // Same URL, browser-ish UA but from Rust: if this also 403s, the gate is the
    // User-Agent alone and the webview needs no proxy beyond a UA override.
    for ua in [WEBVIEW_UA, app_ua] {
        let response = client
            .get(&url)
            .header("user-agent", ua)
            .header("range", "bytes=0-15")
            .send()
            .await
            .expect("请求");
        let status = response.status().as_u16();
        let server = response
            .headers()
            .get("server")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("-")
            .to_string();
        let bytes = response.bytes().await.map(|value| value.len()).unwrap_or(0);
        let kind = if ua == WEBVIEW_UA { "webview UA" } else { "app UA" };
        println!("复查 {kind} → HTTP {status}，server={server}，{bytes} 字节");
    }
}
