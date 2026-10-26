//! 播放历史与续播位置。
//!
//! 只存「看到哪儿」这件事：剧名、封面、集号、秒数、时间戳。**不存**片源地址、
//! 内容密钥或任何请求头——那些是一次性的播放凭据，落盘就等于把边界从 Rust
//! 侧挪到了一个可能被备份的文件里。
//!
//! 存储是应用数据目录下的一个 JSON 文件（`history.json`），整表重写：
//! 条目上限很小（默认 50 条），一次播放结束才写一次，没有并发写者；用整表
//! 重写换来「读不到就回退空表、写坏了也不会丢历史目录结构」这条简单性质。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// 每部剧只保留最新的一条位置；列表按这个顺序倒序展示。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub series_id: String,
    pub series_title: String,
    pub cover: String,
    pub vid: String,
    pub vid_index: u32,
    /// 停止播放时所在的秒数；<code>0</code> 表示刚打开还没看。
    pub seconds: u32,
    /// 这一集的总时长，用来判断是否已经看完。
    pub duration: u32,
    pub updated_at: i64,
    /// 进来时所在的列表页签（`hot-drama` / `hema` / `xifan` …），列表页的
    /// 「继续观看」只展示同一类目的记录。旧文件没有这个字段，按空串补齐。
    #[serde(default)]
    pub category: String,
}

impl HistoryEntry {
    /// 看到 95% 以上就不再提示续播——用户已经看完了。
    pub fn finished(&self) -> bool {
        self.duration > 0 && self.seconds.saturating_add(15) >= self.duration
            && self.seconds as f64 / self.duration as f64 > 0.95
    }

    pub fn resumable(&self) -> bool {
        !self.finished() && (self.seconds > 15 || self.vid_index > 1)
    }
}

const MAX_ENTRIES: usize = 50;
/// 进度上报节流：Rust 侧再兜一层，避免前端高频 timeupdate 把磁盘打满。
const MIN_WRITE_INTERVAL_MS: i64 = 4_000;

pub struct Store {
    path: PathBuf,
    entries: Mutex<Vec<HistoryEntry>>,
    /// 上一次真正落盘的时间，用于节流。
    last_write: Mutex<i64>,
}

impl Store {
    /// 打开（或初始化）历史表。文件读不出来就当成空表，但会留一条错误日志，
    /// 不让用户以为「历史没了」。
    pub fn open(dir: &Path) -> Self {
        let path = dir.join("history.json");
        let entries = match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<Vec<HistoryEntry>>(&text).unwrap_or_else(|error| {
                eprintln!("历史文件无法解析，按空历史继续：{error}");
                Vec::new()
            }),
            Err(_) => Vec::new(),
        };
        let mut entries = entries
            .into_iter()
            .filter(|entry| !entry.series_id.is_empty() && !entry.vid.is_empty())
            .collect::<Vec<_>>();
        entries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        entries.truncate(MAX_ENTRIES);
        Store {
            path,
            entries: Mutex::new(entries),
            last_write: Mutex::new(0),
        }
    }

    pub fn list(&self) -> Vec<HistoryEntry> {
        self
            .entries
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// 某一部的最新位置；详情页用它决定「继续播放」提示的内容。
    pub fn latest_for(&self, series_id: &str) -> Option<HistoryEntry> {
        self.entries
            .lock()
            .ok()
            .and_then(|guard| guard.iter().find(|entry| entry.series_id == series_id).cloned())
    }

    /// 记录一次播放位置。返回 false 表示这次因为节流没落盘（内存表仍然更新）。
    pub fn record(&self, entry: HistoryEntry) -> bool {
        if entry.series_id.is_empty() || entry.vid.is_empty() || entry.updated_at <= 0 {
            return false;
        }
        // 只锁一次：std Mutex 不可重入，边持锁边再 lock() 会自锁死。
        let rows = {
            let Ok(mut guard) = self.entries.lock() else {
                return false;
            };
            guard.retain(|row| row.series_id != entry.series_id);
            guard.insert(0, entry.clone());
            guard.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
            guard.truncate(MAX_ENTRIES);

            let last = self.last_write.lock().map(|value| *value).unwrap_or(0);
            let force = entry.seconds == 0 || entry.finished();
            if last == 0 || force || entry.updated_at - last >= MIN_WRITE_INTERVAL_MS {
                if let Ok(mut value) = self.last_write.lock() {
                    *value = entry.updated_at;
                }
                guard.clone()
            } else {
                return false;
            }
        };
        self.persist(&rows)
    }

    fn persist(&self, rows: &[HistoryEntry]) -> bool {
        let text = match serde_json::to_string(rows) {
            Ok(text) => text,
            Err(_) => return false,
        };
        // 先写临时文件再改名：进程被强杀也不会留下半截 JSON。
        let temp = self.path.with_extension("json.tmp");
        if fs::write(&temp, text).is_err() {
            return false;
        }
        fs::rename(&temp, &self.path).is_ok()
    }

    pub fn clear(&self) {
        if let Ok(mut guard) = self.entries.lock() {
            guard.clear();
        }
        if let Ok(mut value) = self.last_write.lock() {
            *value = 0;
        }
        self.persist(&[]);
    }

    /// 退出前无条件落盘，绕过节流。
    pub fn flush(&self) {
        if let Ok(mut value) = self.last_write.lock() {
            *value = 0;
        }
        self.persist(&self.list());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(series: &str, index: u32, seconds: u32, at: i64) -> HistoryEntry {
        HistoryEntry {
            series_id: series.to_string(),
            series_title: format!("剧{series}"),
            cover: String::new(),
            vid: format!("{series}:{index}"),
            vid_index: index,
            seconds,
            duration: 60,
            updated_at: at,
            category: String::new(),
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hg-history-{tag}-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn keeps_one_row_per_series_newest_first() {
        let dir = temp_dir("dedupe");
        let store = Store::open(&dir);
        store.record(entry("b", 1, 5, 1_000));
        store.record(entry("a", 1, 5, 2_000));
        store.record(entry("a", 2, 30, 3_000));
        let rows = store.list();
        assert_eq!(
            rows.iter().map(|row| (row.series_id.as_str(), row.vid_index)).collect::<Vec<_>>(),
            vec![("a", 2), ("b", 1)]
        );
        assert_eq!(store.latest_for("a").unwrap().seconds, 30);
        assert!(store.latest_for("zz").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn survives_a_restart() {
        let dir = temp_dir("restart");
        {
            let store = Store::open(&dir);
            store.record(entry("s", 3, 42, 9_000));
            store.flush();
        }
        let reopened = Store::open(&dir);
        let row = reopened.latest_for("s").expect("重启后历史还在");
        assert_eq!((row.vid_index, row.seconds), (3, 42));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_file_is_empty_history_not_a_crash() {
        let dir = temp_dir("corrupt");
        fs::write(dir.join("history.json"), b"{ not json").unwrap();
        let store = Store::open(&dir);
        assert!(store.list().is_empty());
        store.record(entry("s", 1, 1, 1_000));
        store.flush();
        assert_eq!(Store::open(&dir).list().len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_files_without_a_category_still_open() {
        let dir = temp_dir("legacy-category");
        let legacy = r#"[{"seriesId":"s","seriesTitle":"剧","cover":"","vid":"s:1","vidIndex":1,"seconds":20,"duration":60,"updatedAt":1000}]"#;
        fs::write(dir.join("history.json"), legacy).unwrap();
        let rows = Store::open(&dir).list();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].category, "");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_empties_both_memory_and_disk() {
        let dir = temp_dir("clear");
        let store = Store::open(&dir);
        store.record(entry("s", 1, 10, 1_000));
        store.flush();
        store.clear();
        assert!(store.list().is_empty());
        assert!(Store::open(&dir).list().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn finished_and_resumable_boundaries() {
        let mut row = entry("s", 1, 58, 1_000);
        row.duration = 60;
        assert!(row.finished());
        assert!(!row.resumable(), "看完就不再提示续播");

        row.seconds = 20;
        assert!(!row.finished());
        assert!(row.resumable());

        row.seconds = 5;
        assert!(!row.resumable(), "第 1 集前 15 秒不算看过");

        row.vid_index = 4;
        assert!(row.resumable(), "已经翻到第 4 集，即使这一集才 5 秒也要提示");
    }

    #[test]
    fn writes_are_throttled_but_flush_is_not() {
        let dir = temp_dir("throttle");
        let store = Store::open(&dir);
        store.record(entry("s", 1, 10, 1_000));
        let on_disk = || fs::read_to_string(dir.join("history.json")).unwrap_or_default();
        assert!(on_disk().contains("\"seconds\":10"), "首次记录必须落盘");

        store.record(entry("s", 1, 20, 1_500));
        assert!(on_disk().contains("\"seconds\":10"), "500ms 内的重复上报不该写盘");
        assert!(store.latest_for("s").unwrap().seconds == 20, "内存表仍然更新");

        store.flush();
        assert!(on_disk().contains("\"seconds\":20"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_long_history_is_capped() {
        let dir = temp_dir("cap");
        let store = Store::open(&dir);
        for index in 0..(MAX_ENTRIES as i64 + 20) {
            store.record(entry(&format!("s{index}"), 1, 1, 10_000 + index * 10_000));
        }
        assert_eq!(store.list().len(), MAX_ENTRIES);
        assert_eq!(store.list()[0].series_id, format!("s{}", MAX_ENTRIES as i64 + 19));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_ids_are_rejected() {
        let dir = temp_dir("bad");
        let store = Store::open(&dir);
        assert!(!store.record(entry("", 1, 1, 1)));
        let mut row = entry("s", 1, 1, 0);
        row.vid = String::new();
        assert!(!store.record(row));
        assert!(store.list().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn timestamps_come_from_the_same_clock_as_the_api_layer() {
        assert!(crate::api::now_millis() > 1_700_000_000_000);
    }
}
