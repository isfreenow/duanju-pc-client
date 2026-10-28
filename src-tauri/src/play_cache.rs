//! 会话级播放缓存：把「一集解析出的片源」留在 Rust 内存里。
//!
//! 为什么要有这一层：切集的耗时里有一段跟网络无关——签一次接口、取一次回源地址、
//! 再解一层 spade，实测约一秒。预取（`prefetch_play`）提前把这段跑完，真正切集
//! 时命令层直接从这张表里拿结果，前端一句「下一集秒开」才站得住。
//!
//! 边界没有动：**这张表只在进程内存里，也不给 `serde`**。播放地址是 CDN 按请求
//! 签发的短期凭据，落盘等于把「密钥只活在 Rust」这条承诺从磁盘这一侧破掉；前端
//! 拿到的仍然是 `resolve_play` 那一份 DTO，缓存只是让它少跑一趟网络。
//!
//! 淘汰按时间 + 条数双上限：`TTL` 拦住「预取了但一直不看」的旧地址（CDN 那侧可
//! 能已经失效），`MAX_ENTRIES` 拦住一晚上追完一部剧把内存堆满。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::api::app_video::MediaVariant;

/// 一条地址在缓存里的有效期。比 CDN 签发的常见寿命保守得多，宁可重新解析。
const TTL: Duration = Duration::from_secs(15 * 60);
/// 最多留几集：当前集 + 预取两集 + 几集来回拖拽的余量。
const MAX_ENTRIES: usize = 12;

/// 缓存键。vid 在三个片源里都唯一，但第三方来源解析时还要用 `series_id` 校验
/// 分集归属，所以两个一起当键——同 vid 不同剧集不会互相冒领。
fn key(series_id: &str, vid: &str) -> String {
    format!("{series_id}\u{1}{vid}")
}

pub struct PlayCache {
    rows: Mutex<HashMap<String, Entry>>,
}

struct Entry {
    stored_at: Instant,
    streams: Vec<MediaVariant>,
    note: Option<String>,
}

impl Default for PlayCache {
    fn default() -> Self {
        Self::new()
    }
}

impl PlayCache {
    pub fn new() -> Self {
        PlayCache {
            rows: Mutex::new(HashMap::new()),
        }
    }

    /// 命中即返回副本；过期条目顺手删掉，不当命中。
    pub fn get(&self, series_id: &str, vid: &str) -> Option<(Vec<MediaVariant>, Option<String>)> {
        let mut guard = self.rows.lock().ok()?;
        let row = guard.get(&key(series_id, vid))?;
        if row.stored_at.elapsed() > TTL {
            guard.remove(&key(series_id, vid));
            return None;
        }
        Some((row.streams.clone(), row.note.clone()))
    }

    /// 只缓存「确实能播」的结果：一条流都没有的解析不值得留，留着只会让下一次
    /// 切集也失败得很快。
    pub fn put(
        &self,
        series_id: &str,
        vid: &str,
        streams: Vec<MediaVariant>,
        note: Option<String>,
    ) {
        if streams.is_empty() {
            return;
        }
        let mut guard = match self.rows.lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        let key = key(series_id, vid);
        guard.insert(
            key,
            Entry {
                stored_at: Instant::now(),
                streams,
                note,
            },
        );
        if guard.len() <= MAX_ENTRIES {
            return;
        }
        // 先扔过期的，还不够就按存入时间淘汰最旧的（没有序号，一次遍历取最旧）。
        guard.retain(|_, row| row.stored_at.elapsed() <= TTL);
        while guard.len() > MAX_ENTRIES {
            let oldest = guard
                .iter()
                .min_by_key(|(_, row)| row.stored_at)
                .map(|(key, _)| key.clone());
            match oldest {
                Some(key) => {
                    guard.remove(&key);
                }
                None => break,
            }
        }
    }

    pub fn remove(&self, series_id: &str, vid: &str) -> bool {
        self.rows
            .lock()
            .ok()
            .map(|mut guard| guard.remove(&key(series_id, vid)).is_some())
            .unwrap_or(false)
    }

    pub fn clear(&self) {
        if let Ok(mut guard) = self.rows.lock() {
            guard.clear();
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.rows
            .lock()
            .map(|guard| guard.len())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variant(vid: &str) -> MediaVariant {
        MediaVariant {
            url: format!("https://vod.example/{vid}.mp4"),
            content_key: Some("00112233445566778899aabbccddeeff".to_string()),
            codec: "hevc".to_string(),
            quality: "720p".to_string(),
            width: 720,
            height: 1280,
            source: "app".to_string(),
            vid: vid.to_string(),
            user_agent: "ua".to_string(),
            referer: None,
        }
    }

    #[test]
    fn a_stored_resolution_is_returned_verbatim() {
        let cache = PlayCache::new();
        cache.put("s1", "v1", vec![variant("v1")], Some("提示".to_string()));
        let (streams, note) = cache.get("s1", "v1").expect("命中");
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].vid, "v1");
        assert_eq!(note.as_deref(), Some("提示"));
        assert!(cache.get("s1", "v2").is_none(), "没存过的集不算命中");
        assert!(cache.get("s2", "v1").is_none(), "剧集不同的同 vid 不冒领");
    }

    #[test]
    fn an_empty_resolution_is_not_cached() {
        let cache = PlayCache::new();
        cache.put("s1", "v1", Vec::new(), None);
        assert_eq!(cache.len(), 0, "没有片源的失败不该占位");
        assert!(cache.get("s1", "v1").is_none());
    }

    #[test]
    fn an_expired_entry_is_a_miss_and_is_dropped() {
        let cache = PlayCache::new();
        cache.put("s1", "v1", vec![variant("v1")], None);
        // 把存入时间往前推过 TTL，不用真等十五分钟（`Instant - Duration` 只会
        // 饱和到更早，不会 panic）。
        {
            let mut guard = cache.rows.lock().unwrap();
            if let Some(row) = guard.values_mut().next() {
                row.stored_at = Instant::now() - TTL - Duration::from_secs(1);
            }
        }
        assert!(
            cache.get("s1", "v1").is_none(),
            "过期的地址不能再直接拿来播"
        );
        assert_eq!(cache.len(), 0, "过期条目在判定时就清掉");
    }

    #[test]
    fn the_cache_is_capped_oldest_first() {
        let cache = PlayCache::new();
        for index in 0..(MAX_ENTRIES + 8) {
            let vid = format!("v{index}");
            cache.put("s", &vid, vec![variant(&vid)], None);
        }
        assert_eq!(cache.len(), MAX_ENTRIES, "超出上限就淘汰最旧的");
        assert!(cache.get("s", "v0").is_none(), "最早那条已经被挤出去");
        assert!(cache.get("s", &format!("v{}", MAX_ENTRIES + 7)).is_some());
        assert!(cache.remove("s", &format!("v{}", MAX_ENTRIES + 7)));
        assert!(!cache.remove("s", "v0"));
        cache.clear();
        assert_eq!(cache.len(), 0);
    }
}
