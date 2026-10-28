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
    /// 追剧检测：已知的总集数。每次刷新分集页取 `max(旧值, 来源返回)`，绝不
    /// 因为一次更慢的刷新回了更少的集数就把基线拉低。旧文件按 0 补齐。
    #[serde(default)]
    pub known_total: u32,
    /// 用户已经确认看到过的集数（看到哪儿就等于确认到哪儿）。`known_total`
    /// 比它大就是「有新集没看」。
    #[serde(default)]
    pub acknowledged_total: u32,
    /// 上一次刷新分集页的时间（毫秒）。0 表示还没查过。
    #[serde(default)]
    pub last_checked_at: i64,
}

impl HistoryEntry {
    /// 看到 95% 以上就不再提示续播——用户已经看完了。
    pub fn finished(&self) -> bool {
        self.duration > 0
            && self.seconds.saturating_add(15) >= self.duration
            && self.seconds as f64 / self.duration as f64 > 0.95
    }

    pub fn resumable(&self) -> bool {
        !self.finished() && (self.seconds > 15 || self.vid_index > 1)
    }

    /// 已知总集数减去已确认看到的位置：还有几集没看 / 更新了几集。
    pub fn unread(&self) -> u32 {
        self.known_total.saturating_sub(self.acknowledged_total)
    }
}

const MAX_ENTRIES: usize = 50;
/// 进度上报节流：Rust 侧再兜一层，避免前端高频 timeupdate 把磁盘打满。
const MIN_WRITE_INTERVAL_MS: i64 = 4_000;
/// 追剧检测的最小间隔：一天之内反复「检查更新」不再打来源。
pub const CHECK_INTERVAL_MS: i64 = 24 * 3600 * 1000;

/// 一次「检查更新」的裁决：该查，还是直接用表里已有的数字。
///
/// 节流判定放在 Rust 而不是前端，是因为「我的」页与详情页都可能触发检查，两边
/// 各写一份间隔就总会有一个忘了改。时间由调用方给（命令里传
/// [`crate::api::now_millis`]），测试可以注入固定值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateState {
    /// 表里已有 `known_total`，距上次检查不足一天，直接用缓存值不必联网。
    Cached { known_total: u32, unread: u32 },
    /// 从没查过，或上次检查失败（`known_total` 没被刷新过），应当联网。
    Due,
}

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
        self.entries
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// 某一部的最新位置；详情页用它决定「继续播放」提示的内容。
    pub fn latest_for(&self, series_id: &str) -> Option<HistoryEntry> {
        self.entries.lock().ok().and_then(|guard| {
            guard
                .iter()
                .find(|entry| entry.series_id == series_id)
                .cloned()
        })
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
            // 追剧检测的三个字段属于「这部剧更新到哪了」，不属于「这一次播到哪」。
            // 播放上报不带它们（前端只关心进度），若直接插入就会把已有的基线清零，
            // 所以从被替换掉的旧行把它们接过来。
            let entry = {
                let prior = guard.iter().find(|row| row.series_id == entry.series_id);
                let mut entry = entry;
                if let Some(prior) = prior {
                    entry.known_total = entry.known_total.max(prior.known_total);
                    entry.acknowledged_total =
                        entry.acknowledged_total.max(prior.acknowledged_total);
                    entry.last_checked_at = entry.last_checked_at.max(prior.last_checked_at);
                }
                entry
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

    /// 这次要不要联网查更新。只在成功刷新时写 `last_checked_at`，所以一次失败的
    /// 检查不会把下次尝试推后一整天。
    pub fn update_state(&self, series_id: &str, now: i64) -> UpdateState {
        let Some(entry) = self.latest_for(series_id) else {
            return UpdateState::Due;
        };
        if entry.known_total == 0 {
            return UpdateState::Due;
        }
        if entry.last_checked_at == 0 || now - entry.last_checked_at >= CHECK_INTERVAL_MS {
            return UpdateState::Due;
        }
        UpdateState::Cached {
            known_total: entry.known_total,
            unread: entry.unread(),
        }
    }

    /// 刷新分集页后回填基线：`known_total` 只增不减，一次更慢的刷新回了更少的
    /// 集数，不能把已有的基线拉低（参考实现同规则）。
    pub fn note_total(&self, series_id: &str, fresh_total: u32, now: i64) -> Option<HistoryEntry> {
        let rows = {
            let Ok(mut guard) = self.entries.lock() else {
                return None;
            };
            let mut hit = None;
            for row in guard.iter_mut() {
                if row.series_id == series_id {
                    row.known_total = row.known_total.max(fresh_total);
                    row.last_checked_at = now;
                    hit = Some(row.clone());
                }
            }
            match hit {
                Some(_) => guard.clone(),
                None => return None,
            }
        };
        self.persist(&rows);
        self.latest_for(series_id)
    }

    /// 用户点开看了就算确认：已确认集数追平已知集数，角标清零。
    /// 取 `min(known_total, vid_index)` 而不是直接抄进度，是为了不让「看到第 3 集」
    /// 把「已确认 10 集」回退到 3——确认只能往前走。
    pub fn acknowledge(&self, series_id: &str, vid_index: u32) -> Option<HistoryEntry> {
        let rows = {
            let Ok(mut guard) = self.entries.lock() else {
                return None;
            };
            let mut hit = None;
            for row in guard.iter_mut() {
                if row.series_id == series_id {
                    let up_to = vid_index.min(row.known_total);
                    row.acknowledged_total = row.acknowledged_total.max(up_to);
                    hit = Some(row.clone());
                }
            }
            match hit {
                Some(_) => guard.clone(),
                None => return None,
            }
        };
        self.persist(&rows);
        self.latest_for(series_id)
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

    /// 删掉某一部的观看记录（「我的」页卡片上的 ✕）。找不到就什么也不动，返回是否删过。
    pub fn remove(&self, series_id: &str) -> bool {
        let rows = {
            let Ok(mut guard) = self.entries.lock() else {
                return false;
            };
            let before = guard.len();
            guard.retain(|row| row.series_id != series_id);
            if guard.len() == before {
                return false;
            }
            guard.clone()
        };
        self.persist(&rows)
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
            known_total: 0,
            acknowledged_total: 0,
            last_checked_at: 0,
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
            rows.iter()
                .map(|row| (row.series_id.as_str(), row.vid_index))
                .collect::<Vec<_>>(),
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
    fn remove_deletes_only_that_series() {
        let dir = temp_dir("remove");
        let store = Store::open(&dir);
        store.record(entry("a", 1, 5, 1_000));
        store.record(entry("b", 1, 5, 2_000));
        store.flush();
        assert!(store.remove("a"));
        assert!(!store.remove("a"), "再删一次没有命中");
        assert_eq!(store.list().len(), 1);
        assert_eq!(Store::open(&dir).list()[0].series_id, "b");
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
        assert!(
            row.resumable(),
            "已经翻到第 4 集，即使这一集才 5 秒也要提示"
        );
    }

    #[test]
    fn writes_are_throttled_but_flush_is_not() {
        let dir = temp_dir("throttle");
        let store = Store::open(&dir);
        store.record(entry("s", 1, 10, 1_000));
        let on_disk = || fs::read_to_string(dir.join("history.json")).unwrap_or_default();
        assert!(on_disk().contains("\"seconds\":10"), "首次记录必须落盘");

        store.record(entry("s", 1, 20, 1_500));
        assert!(
            on_disk().contains("\"seconds\":10"),
            "500ms 内的重复上报不该写盘"
        );
        assert!(
            store.latest_for("s").unwrap().seconds == 20,
            "内存表仍然更新"
        );

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
        assert_eq!(
            store.list()[0].series_id,
            format!("s{}", MAX_ENTRIES as i64 + 19)
        );
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

    #[test]
    fn old_files_without_update_fields_still_open() {
        let dir = temp_dir("legacy-total");
        let legacy = r#"[{"seriesId":"s","seriesTitle":"剧","cover":"","vid":"s:1","vidIndex":1,"seconds":20,"duration":60,"updatedAt":1000}]"#;
        fs::write(dir.join("history.json"), legacy).unwrap();
        let row = Store::open(&dir).latest_for("s").unwrap();
        assert_eq!(
            (row.known_total, row.acknowledged_total, row.last_checked_at),
            (0, 0, 0)
        );
        assert_eq!(row.unread(), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn known_total_only_moves_forward() {
        let dir = temp_dir("note-total");
        let store = Store::open(&dir);
        store.record(entry("s", 4, 30, 1_000));
        assert_eq!(
            store.update_state("s", 2_000),
            UpdateState::Due,
            "还没查过就该查"
        );

        let row = store.note_total("s", 60, 2_000).expect("有这一行");
        assert_eq!(
            (row.known_total, row.last_checked_at, row.unread()),
            (60, 2_000, 60)
        );
        // 一次更慢的刷新回了更少的集数：基线不动，但检查时间仍然刷新。
        let row = store.note_total("s", 52, 3_000).unwrap();
        assert_eq!((row.known_total, row.last_checked_at), (60, 3_000));
        assert_eq!(
            store.update_state("s", 3_000 + CHECK_INTERVAL_MS - 1),
            UpdateState::Cached {
                known_total: 60,
                unread: 60
            }
        );
        assert_eq!(
            store.update_state("s", 3_000 + CHECK_INTERVAL_MS),
            UpdateState::Due
        );
        assert!(
            store.note_total("zz", 10, 4_000).is_none(),
            "没看过的剧不建基线"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn progress_updates_keep_the_update_baseline() {
        let dir = temp_dir("carry-over");
        let store = Store::open(&dir);
        store.record(entry("s", 4, 30, 1_000));
        store.note_total("s", 60, 2_000);
        store.acknowledge("s", 60);
        // 播放上报不带这三个字段（前端只给进度），旧行的基线必须被接过来。
        store.record(entry("s", 9, 12, 3_000));
        let row = store.latest_for("s").unwrap();
        assert_eq!(
            (
                row.vid_index,
                row.known_total,
                row.acknowledged_total,
                row.unread()
            ),
            (9, 60, 60, 0)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn acknowledge_never_rolls_back() {
        let dir = temp_dir("acknowledge");
        let store = Store::open(&dir);
        store.record(entry("s", 4, 30, 1_000));
        store.note_total("s", 60, 2_000);

        let row = store.acknowledge("s", 10).unwrap();
        assert_eq!((row.acknowledged_total, row.unread()), (10, 50));
        // 回看第 2 集不会把确认进度拉回 2。
        let row = store.acknowledge("s", 2).unwrap();
        assert_eq!((row.acknowledged_total, row.unread()), (10, 50));
        // 集数基线之外的确认被限幅：看不到比已知集数更多的确认。
        let row = store.acknowledge("s", 99).unwrap();
        assert_eq!((row.acknowledged_total, row.unread()), (60, 0));
        assert!(store.acknowledge("zz", 3).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_check_does_not_buy_a_quiet_day() {
        let dir = temp_dir("failed-check");
        let store = Store::open(&dir);
        store.record(entry("s", 1, 30, 1_000));
        // 失败路径不写 `last_checked_at`（只有 `note_total` 写），所以永远判 Due。
        assert_eq!(store.update_state("s", i64::MAX / 2), UpdateState::Due);
        let _ = fs::remove_dir_all(&dir);
    }
}
