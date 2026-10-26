//! 收藏：手动点亮的「想再看」清单。
//!
//! 与播放历史同构——只存剧名、封面、类目和时间戳，**不存**片源地址或内容密钥。
//! 收藏是低频手动动作，没有并发写者，所以不学历史那样节流：每次变更整表重写，
//! 换来「点了立刻就在盘上」这条简单性质。上限放宽到 200 条，够一个重度用户。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// 一部被收藏的剧。`updated_at` 是点亮收藏的时间。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FavoriteEntry {
    pub series_id: String,
    pub series_title: String,
    pub cover: String,
    /// 进来时所在的列表页签（`hot-drama` / `hema` / `xifan` …），与历史同一套词表。
    #[serde(default)]
    pub category: String,
    pub updated_at: i64,
}

const MAX_ENTRIES: usize = 200;

pub struct Store {
    path: PathBuf,
    entries: Mutex<Vec<FavoriteEntry>>,
}

impl Store {
    /// 打开（或初始化）收藏表。文件读不出来就当成空表，但会留一条错误日志。
    pub fn open(dir: &Path) -> Self {
        let path = dir.join("favorites.json");
        let entries = match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<Vec<FavoriteEntry>>(&text).unwrap_or_else(|error| {
                eprintln!("收藏文件无法解析，按空收藏继续：{error}");
                Vec::new()
            }),
            Err(_) => Vec::new(),
        };
        let mut entries = entries
            .into_iter()
            .filter(|entry| !entry.series_id.is_empty())
            .collect::<Vec<_>>();
        entries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        entries.truncate(MAX_ENTRIES);
        Store {
            path,
            entries: Mutex::new(entries),
        }
    }

    pub fn list(&self) -> Vec<FavoriteEntry> {
        self.entries.lock().map(|guard| guard.clone()).unwrap_or_default()
    }

    pub fn contains(&self, series_id: &str) -> bool {
        self.entries
            .lock()
            .map(|guard| guard.iter().any(|entry| entry.series_id == series_id))
            .unwrap_or(false)
    }

    /// 点亮或取消收藏，返回操作后是否为已收藏。空 `series_id` 直接拒绝。
    pub fn toggle(&self, entry: FavoriteEntry) -> Option<bool> {
        if entry.series_id.is_empty() {
            return None;
        }
        let (favorited, rows) = {
            let Ok(mut guard) = self.entries.lock() else {
                return None;
            };
            let existed = guard.iter().any(|row| row.series_id == entry.series_id);
            if existed {
                guard.retain(|row| row.series_id != entry.series_id);
            } else {
                guard.insert(0, entry);
                guard.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
                guard.truncate(MAX_ENTRIES);
            }
            (!existed, guard.clone())
        };
        self.persist(&rows);
        Some(favorited)
    }

    fn persist(&self, rows: &[FavoriteEntry]) -> bool {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(series: &str, at: i64) -> FavoriteEntry {
        FavoriteEntry {
            series_id: series.to_string(),
            series_title: format!("剧{series}"),
            cover: String::new(),
            category: String::new(),
            updated_at: at,
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hg-favorites-{tag}-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn toggle_flips_membership_and_persists() {
        let dir = temp_dir("toggle");
        let store = Store::open(&dir);
        assert_eq!(store.toggle(entry("a", 1_000)), Some(true));
        assert!(store.contains("a"));
        assert_eq!(store.toggle(entry("a", 2_000)), Some(false));
        assert!(!store.contains("a"));
        assert!(Store::open(&dir).list().is_empty(), "取消后重启也不再回来");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn favorites_survive_a_restart_newest_first() {
        let dir = temp_dir("restart");
        {
            let store = Store::open(&dir);
            store.toggle(entry("a", 1_000));
            store.toggle(entry("b", 2_000));
        }
        let rows = Store::open(&dir).list();
        assert_eq!(
            rows.iter().map(|row| row.series_id.as_str()).collect::<Vec<_>>(),
            vec!["b", "a"]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_file_is_empty_favorites_not_a_crash() {
        let dir = temp_dir("corrupt");
        fs::write(dir.join("favorites.json"), b"[ nope").unwrap();
        let store = Store::open(&dir);
        assert!(store.list().is_empty());
        store.toggle(entry("s", 1_000));
        assert_eq!(Store::open(&dir).list().len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_ids_are_rejected() {
        let dir = temp_dir("bad");
        let store = Store::open(&dir);
        assert_eq!(store.toggle(entry("", 1)), None);
        assert!(store.list().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
