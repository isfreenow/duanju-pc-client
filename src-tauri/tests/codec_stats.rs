//! 红果片源的视频编码分布统计：从四个类目随机抽若干部，逐部解析第一集的
//! 全部候选流，按编码聚合。给「HEVC 兜底转码值不值得做、覆盖多少比例」提供
//! 真实数据，不参与任何产品路径。
//!
//! Run with: `cargo test --test codec_stats -- --ignored --nocapture`
//! 可加 `-- --test-threads=1 SAMPLE=50` 前用环境变量调样本量（默认 100）。
//!
//! 只打印聚合计数与失败错误码，绝不打印片源地址与密钥。

use std::collections::{HashMap, HashSet};

use hongguo_desktop_client_lib::api::{self, app_video, web};
use rand::seq::SliceRandom;
use tokio::task::JoinSet;

const CATEGORIES: [&str; 4] = ["hot-drama", "real-drama", "comic-drama", "ai-drama"];

/// 归一：接口给出 h264/avc/avc1 与 hevc/h265/hvc1/hev1/bytevc1 两族写法。
fn bucket(codec: &str) -> &'static str {
    let lowered = codec.to_lowercase();
    if ["h264", "avc", "avc1"].contains(&lowered.as_str()) {
        "H.264"
    } else if ["hevc", "h265", "hvc1", "hev1", "bytevc1"].contains(&lowered.as_str()) {
        "HEVC"
    } else {
        "其它/未声明"
    }
}

fn height_band(height: u32) -> &'static str {
    if height >= 2160 {
        "4K+"
    } else if height >= 1080 {
        "1080p"
    } else if height >= 720 {
        "720p"
    } else if height > 0 {
        "≤480p"
    } else {
        "未给出"
    }
}

#[derive(Default)]
struct Stats {
    /// 成功解析出至少一条流的部数。
    resolved: usize,
    /// 分集或片源请求失败（含熔断/付费拦）的部数。
    failed: usize,
    /// 每条候选流按编码计数。
    per_stream: HashMap<&'static str, usize>,
    /// 按「编码 × 分辨率档」计数的候选流。
    per_stream_band: HashMap<(&'static str, &'static str), usize>,
    /// 有至少一条 H.264 可切的部数。
    h264_available: usize,
    /// 只有 HEVC、没有 H.264 兜底的部数（无扩展机器必须转码的比例来源）。
    hevc_only: usize,
    /// 自动选流（数组第一条，H.264 优先）落在哪个编码。
    auto_choice: HashMap<&'static str, usize>,
    /// HEVC-only 的剧里，最高画质的分辨率分布。
    hevc_only_bands: HashMap<&'static str, usize>,
    /// 失败错误码分布（只记 code，不带正文）。
    failures: HashMap<String, usize>,
}

fn is_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

#[tokio::test]
#[ignore = "每部打两次签名接口（分集页 + 片源解析），默认抽 100 部"]
async fn codec_distribution_over_random_sample() {
    let sample: usize = std::env::var("SAMPLE")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(100);
    let client = api::build_client().expect("client");

    // 组池：四个类目各取前两页，只留官网数字 id，去重后洗牌抽样。
    let mut pool: Vec<(String, String)> = Vec::new();
    let mut seen = HashSet::new();
    for category in CATEGORIES {
        for page in 1..=2u32 {
            match web::browse_list(&client, category, "", page, false).await {
                Ok(catalog) => {
                    for card in catalog.results {
                        if is_digits(&card.series_id) && seen.insert(card.series_id.clone()) {
                            pool.push((card.series_id, card.series_title));
                        }
                    }
                }
                Err(error) => println!("类目 {category} 第 {page} 页拉取失败：{}", error.code()),
            }
        }
    }
    let mut rng = rand::thread_rng();
    pool.shuffle(&mut rng);
    let picked: Vec<(String, String)> = pool.into_iter().take(sample).collect();
    println!(
        "抽样 {} 部（类目池共 {} 部），并发 {}：",
        picked.len(),
        seen.len(),
        4
    );

    let mut stats = Stats::default();
    let mut set = JoinSet::new();
    let mut inbox = picked.into_iter().peekable();
    while inbox.peek().is_some() || !set.is_empty() {
        while set.len() < 4 {
            match inbox.next() {
                Some((series_id, title)) => {
                    let client = client.clone();
                    let key = series_id.clone();
                    set.spawn(async move { (key, title, resolve(&client, &series_id).await) });
                }
                None => break,
            }
        }
        let Some(joined) = set.join_next().await else {
            continue;
        };
        // join_next：None 池空；Some(Err) 是 worker panic，丢掉继续；Some(Ok) 才是结果。
        let Ok((_, title, outcome)) = joined else {
            continue;
        };
        match outcome {
            Ok(variants) => {
                stats.resolved += 1;
                let mut has_h264 = false;
                let mut has_hevc = false;
                let mut best_hevc = 0u32;
                for variant in &variants {
                    let kind = bucket(&variant.codec);
                    *stats.per_stream.entry(kind).or_default() += 1;
                    *stats
                        .per_stream_band
                        .entry((kind, height_band(variant.height)))
                        .or_default() += 1;
                    if kind == "H.264" {
                        has_h264 = true;
                    }
                    if kind == "HEVC" {
                        has_hevc = true;
                        best_hevc = best_hevc.max(variant.height);
                    }
                }
                if let Some(first) = variants.first() {
                    *stats.auto_choice.entry(bucket(&first.codec)).or_default() += 1;
                }
                if has_h264 {
                    stats.h264_available += 1;
                } else if has_hevc {
                    stats.hevc_only += 1;
                    *stats
                        .hevc_only_bands
                        .entry(height_band(best_hevc))
                        .or_default() += 1;
                }
            }
            Err(code) => {
                stats.failed += 1;
                if stats.failed <= 5 {
                    println!("  失败样本：《{title}》 code={code}");
                }
                *stats.failures.entry(code).or_default() += 1;
            }
        }
    }

    let total = stats.resolved.max(1);
    println!(
        "\n==== 编码分布（成功解析 {} 部，失败 {} 部）====",
        stats.resolved, stats.failed
    );
    for (kind, count) in sorted(&stats.per_stream) {
        println!("候选流  {kind:<12} {count:>4} 条");
    }
    println!("-- 分辨率 × 编码 --");
    for ((kind, band), count) in sorted2(&stats.per_stream_band) {
        println!("  {kind:<8} {band:<8} {count:>4} 条");
    }
    println!("-- 按部计（{} 部）--", stats.resolved);
    println!(
        "有 H.264 可切 {:>4} 部（{:.1}%）；只有 HEVC {:>4} 部（{:.1}%）",
        stats.h264_available,
        100.0 * stats.h264_available as f64 / total as f64,
        stats.hevc_only,
        100.0 * stats.hevc_only as f64 / total as f64,
    );
    println!("-- 自动选流命中编码 --");
    for (kind, count) in sorted(&stats.auto_choice) {
        println!("  {kind:<12} {count:>4} 部");
    }
    println!("-- HEVC-only 的最高画质分布 --");
    for (band, count) in sorted(&stats.hevc_only_bands) {
        println!("  {band:<8} {count:>4} 部");
    }
    if !stats.failures.is_empty() {
        println!("-- 失败错误码 --");
        for (code, count) in sorted(&stats.failures) {
            println!("  {code:<24} {count:>4} 部");
        }
    }
}

/// 一部剧：拉分集、取第一集的全部候选流；失败只回错误码。
async fn resolve(
    client: &reqwest::Client,
    series_id: &str,
) -> Result<Vec<app_video::MediaVariant>, String> {
    let list = web::fetch_episode_list(client, series_id)
        .await
        .map_err(|e| e.code())?;
    let Some(episode) = list.episodes.first() else {
        return Err("NO_EPISODE".to_string());
    };
    app_video::fetch_app_play_url(client, &episode.vid)
        .await
        .map_err(|e| e.code())
}

fn sorted<K: Clone>(map: &HashMap<K, usize>) -> Vec<(K, usize)> {
    let mut rows: Vec<_> = map.iter().map(|(k, v)| (k.clone(), *v)).collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1));
    rows
}

fn sorted2(
    map: &HashMap<(&'static str, &'static str), usize>,
) -> Vec<((&'static str, &'static str), usize)> {
    let mut rows: Vec<_> = map.iter().map(|(k, v)| (*k, *v)).collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    rows
}
