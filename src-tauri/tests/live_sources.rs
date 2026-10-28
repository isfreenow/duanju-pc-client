//! Live check of the two extra catalogues behind the top-menu tabs.
//!
//! Ignored by default because it reaches the network; run it explicitly with
//! `cargo test --test live_sources -- --ignored --nocapture`.
//!
//! 只打印数量、主机名、字节数和状态码：河马与西饭给出的是可直接播放的明文地址，
//! 日志里落一条完整 URL 就等于把可播放凭据写进了文件。

use hongguo_desktop_client_lib::api::{self, hema, web, xifan};

fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_else(|| "<非法地址>".to_string())
}

fn why(error: &api::ApiError) -> String {
    format!("[{}] {error}", error.code())
}

/// 取第一条免费分集；两个来源都靠「这一集到底免不免费」决定能不能播。
fn first_free(list: &web::EpisodeList) -> Option<&web::Episode> {
    list.episodes
        .iter()
        .find(|episode| episode.web_available == Some(true))
        .or_else(|| list.episodes.first())
}

#[tokio::test]
#[ignore = "requires network access to the two third-party catalogues"]
async fn hema_browse_search_episodes_and_play() {
    let client = api::build_client().expect("client");
    let mut failures: Vec<String> = Vec::new();
    let mut report = |label: &str, line: String, ok: bool| {
        println!(
            "{} 河马 {label}: {line}",
            if ok { "  ok  " } else { " FAIL " }
        );
        if !ok {
            failures.push(label.to_string());
        }
    };

    let page = match hema::browse_list(&client, "", 1, true).await {
        Ok(page) => page,
        Err(error) => {
            report("browse_list", why(&error), false);
            assert!(failures.is_empty(), "失败的通路: {failures:?}");
            return;
        }
    };
    report(
        "browse_list",
        format!(
            "{} 条，题材 {} 个，标题「{}」，封面 host={}",
            page.results.len(),
            page.genres.len(),
            page.page_title,
            host_of(
                &page
                    .results
                    .first()
                    .map(|card| card.cover.clone())
                    .unwrap_or_default()
            )
        ),
        !page.results.is_empty(),
    );

    // 换一个题材翻一页，确认 slug 校验与「回显页码必须对上」不是纸面规则。
    if let Some(genre) = page.genres.first() {
        match hema::browse_list(&client, &genre.slug, 2, false).await {
            Ok(next) => report(
                "browse_list(题材+第 2 页)",
                format!(
                    "「{}」{} 条，第 {} 页",
                    genre.label,
                    next.results.len(),
                    next.page
                ),
                !next.results.is_empty(),
            ),
            Err(error) => report("browse_list(题材+第 2 页)", why(&error), false),
        }
    }

    let keyword = "逆袭";
    match hema::search(&client, keyword, 1).await {
        Ok(result) => report(
            "search",
            format!(
                "「{keyword}」{} 条，共 {} 条，首条「{}」",
                result.results.len(),
                result.total as i64,
                result
                    .results
                    .first()
                    .map(|card| card.series_title.as_str())
                    .unwrap_or("?")
            ),
            !result.results.is_empty(),
        ),
        Err(error) => report("search", why(&error), false),
    }

    let Some(card) = page.results.first() else {
        assert!(failures.is_empty(), "失败的通路: {failures:?}");
        return;
    };
    match hema::public_details(&client, &card.series_id).await {
        Ok(details) => report(
            "public_details",
            format!(
                "标签「{}」，收藏 {:?}，来源 host={}",
                details.metadata.episode_label,
                details.favorite_count,
                host_of(&details.source_url)
            ),
            true,
        ),
        Err(error) => report("public_details", why(&error), false),
    }

    let list = match hema::episode_list(&client, &card.series_id).await {
        Ok(list) => list,
        Err(error) => {
            report("episode_list", why(&error), false);
            assert!(failures.is_empty(), "失败的通路: {failures:?}");
            return;
        }
    };
    report(
        "episode_list",
        format!(
            "「{}」{} 集，免费 {} 集",
            list.series_title,
            list.total,
            list.web_accessible_episodes.unwrap_or(0)
        ),
        !list.episodes.is_empty(),
    );

    let Some(episode) = first_free(&list) else {
        report("play_source", "没有任何分集".to_string(), false);
        assert!(failures.is_empty(), "失败的通路: {failures:?}");
        return;
    };
    match hema::play_source(&client, &episode.vid, &list.series_id).await {
        Ok(source) => {
            let head = match client
                .get(&source.url)
                .header("Range", "bytes=0-4095")
                .send()
                .await
            {
                Ok(response) => {
                    let status = response.status().as_u16();
                    let length = response.bytes().await.map(|bytes| bytes.len()).unwrap_or(0);
                    (status, length)
                }
                Err(_) => (0, 0),
            };
            report(
                "play_source",
                format!(
                    "第 {} 集 {}，host={}，公网校验={}，Range 首块={}，取了 {} 字节",
                    episode.vid_index,
                    source.quality,
                    host_of(&source.url),
                    api::is_public_media_url(&source.url),
                    head.0,
                    head.1
                ),
                head.0 == 206 && head.1 > 0,
            )
        }
        Err(error) => report("play_source", why(&error), false),
    }

    // 锁定集必须明确报错，不能回一个看起来能播的地址。
    if let Some(locked) = list
        .episodes
        .iter()
        .find(|episode| episode.web_available == Some(false))
    {
        match hema::play_source(&client, &locked.vid, &list.series_id).await {
            Ok(_) => report("play_source(锁定集)", "竟然给出了地址".to_string(), false),
            Err(error) => report("play_source(锁定集)", why(&error), error.code() == "LOCKED"),
        }
    }

    // 编号前缀不是 hema 时应视为输入错误，而不是去查别的来源。
    let wrong = hema::play_source(&client, "1234567890", "").await;
    report(
        "play_source(非本来源编号)",
        wrong
            .as_ref()
            .err()
            .map(|error| why(error))
            .unwrap_or("居然成功".to_string()),
        wrong.is_err(),
    );

    assert!(failures.is_empty(), "失败的通路: {failures:?}");
}

#[tokio::test]
#[ignore = "requires network access to the two third-party catalogues"]
async fn xifan_browse_search_episodes_and_play() {
    let client = api::build_client().expect("client");
    let mut failures: Vec<String> = Vec::new();
    let mut report = |label: &str, line: String, ok: bool| {
        println!(
            "{} 西饭 {label}: {line}",
            if ok { "  ok  " } else { " FAIL " }
        );
        if !ok {
            failures.push(label.to_string());
        }
    };

    let page = match xifan::browse_list(&client, "", 1, true).await {
        Ok(page) => page,
        Err(error) => {
            report("browse_list", why(&error), false);
            assert!(failures.is_empty(), "失败的通路: {failures:?}");
            return;
        }
    };
    report(
        "browse_list",
        format!(
            "{} 条，分类 {} 个，标题「{}」，封面 host={}",
            page.results.len(),
            page.genres.len(),
            page.page_title,
            host_of(
                &page
                    .results
                    .first()
                    .map(|card| card.cover.clone())
                    .unwrap_or_default()
            )
        ),
        !page.results.is_empty(),
    );

    if let Some(genre) = page.genres.get(1) {
        match xifan::browse_list(&client, &genre.slug, 2, false).await {
            Ok(next) => report(
                "browse_list(分类+第 2 页)",
                format!(
                    "「{}」{} 条，第 {} 页",
                    genre.label,
                    next.results.len(),
                    next.page
                ),
                !next.results.is_empty(),
            ),
            Err(error) => report("browse_list(分类+第 2 页)", why(&error), false),
        }
    }

    let keyword = "赘婿";
    match xifan::search(&client, keyword, 1).await {
        Ok(result) => report(
            "search",
            format!(
                "「{keyword}」{} 条，首条「{}」共 {} 集",
                result.results.len(),
                result
                    .results
                    .first()
                    .map(|card| card.series_title.as_str())
                    .unwrap_or("?"),
                result
                    .results
                    .first()
                    .map(|card| card.episode_count)
                    .unwrap_or(0)
            ),
            !result.results.is_empty(),
        ),
        Err(error) => report("search", why(&error), false),
    }

    let Some(card) = page.results.first() else {
        assert!(failures.is_empty(), "失败的通路: {failures:?}");
        return;
    };
    match xifan::public_details(&client, &card.series_id).await {
        Ok(details) => report(
            "public_details",
            format!(
                "标签「{}」，状态 {}",
                details.metadata.episode_label, details.metadata.completion_status
            ),
            true,
        ),
        Err(error) => report("public_details", why(&error), false),
    }

    let list = match xifan::episode_list(&client, &card.series_id).await {
        Ok(list) => list,
        Err(error) => {
            report("episode_list", why(&error), false);
            assert!(failures.is_empty(), "失败的通路: {failures:?}");
            return;
        }
    };
    report(
        "episode_list",
        format!(
            "「{}」{} 集，解锁 {} 集",
            list.series_title,
            list.total,
            list.web_accessible_episodes.unwrap_or(0)
        ),
        !list.episodes.is_empty(),
    );

    let Some(episode) = first_free(&list) else {
        report("play_source", "没有任何分集".to_string(), false);
        assert!(failures.is_empty(), "失败的通路: {failures:?}");
        return;
    };
    match xifan::play_source(&client, &episode.vid, &list.series_id).await {
        Ok(source) => {
            // 客户端禁用自动重定向（签名通路要求如此），这里自己跟一跳：
            // 地址先 302 到下载域，Range 首块才是真文件。
            let first = client
                .get(&source.url)
                .header("Range", "bytes=0-4095")
                .send()
                .await
                .map(|response| {
                    (
                        response.status().as_u16(),
                        response
                            .headers()
                            .get("location")
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_string),
                    )
                })
                .unwrap_or((0, None));
            let target = first.1.clone().unwrap_or_else(|| source.url.clone());
            let fetched = match client
                .get(&target)
                .header("Range", "bytes=0-4095")
                .send()
                .await
            {
                Ok(response) => {
                    let status = response.status().as_u16();
                    let length = response.bytes().await.map(|bytes| bytes.len()).unwrap_or(0);
                    (status, length)
                }
                Err(_) => (0, 0),
            };
            report(
                "play_source",
                format!(
                    "第 {} 集 {}×{}，首跳={}，跳转 host={}，公网校验={}，首块状态={}，取了 {} 字节",
                    episode.vid_index,
                    source.width,
                    source.height,
                    first.0,
                    host_of(&target),
                    api::is_public_media_url(&target),
                    fetched.0,
                    fetched.1
                ),
                fetched.0 == 206 && fetched.1 > 0,
            )
        }
        Err(error) => report("play_source", why(&error), false),
    }

    if let Some(locked) = list
        .episodes
        .iter()
        .find(|episode| episode.web_available == Some(false))
    {
        match xifan::play_source(&client, &locked.vid, &list.series_id).await {
            Ok(_) => report("play_source(锁定集)", "竟然给出了地址".to_string(), false),
            Err(error) => report("play_source(锁定集)", why(&error), error.code() == "LOCKED"),
        }
    }

    let mismatch = xifan::play_source(&client, &episode.vid, "xifan:other:1").await;
    report(
        "play_source(分集不属于该剧)",
        mismatch
            .as_ref()
            .err()
            .map(|error| why(error))
            .unwrap_or("居然成功".to_string()),
        mismatch.is_err(),
    );

    assert!(failures.is_empty(), "失败的通路: {failures:?}");
}
