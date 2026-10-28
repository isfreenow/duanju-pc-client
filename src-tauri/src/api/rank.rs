//! 搜索结果的标题归一化与季数感知排序（纯函数）。
//!
//! 判「哪条最贴这个关键词」这件事，放在 React 里就等于放在某一个视图里：搜索页
//! 自己写一遍，命令行的 `--search` 拿到的还是来源原序，详情页找续作时又要第三遍。
//! 所以它这里是纯函数——不碰网络、不碰 UI、不碰 `serde_json::Value` 之外的任何东西，
//! 输入输出都是值，能整批喂测试。
//!
//! 三条从参考实现（`searchRanking.mjs` / `series-search.js` / `watch-history.js`）
//! 搬过来的规则：
//!
//! * **归一化**：兼容折叠（全角转半角）+ 大小写 + 去掉空白与标点。不先拉平，
//!   「吃械入宋」对不上「《吃械入宋》」，`AI` 对不上 `ａｉ`。
//! * **四档命中**：完全同名 > 标题包含关键词 > 关键词包含标题 > 只命中简介。
//!   上游顺序只当同分时的稳定依据，不当结论。
//! * **季数感知**：`×× 第二季` / `××（2）` / `×× season 2` 都归到同一个基础名，
//!   季号小的在前；同基础名的续作不会被上游的热度排序顶到第 11 条去。裸括号里的
//!   数超过 99 视为年份之类的元数据，不当季号（参考实现同规则）。
//!
//! 中文数字按 JS 的 `Number(...)` 语义解析：`十六` 是「十」后面跟个「六」，解析
//! 不出合法季号，就当没有季号——不猜、不纠正。

use serde::{Deserialize, Serialize};

/// 全角 ASCII（U+FF01–U+FF5E）折回半角。JS 的 `normalize('NFKC')` 会做这一步，
/// Rust 的 `to_lowercase()` 只做大小写折叠，不补这一刀就会在中文标点上分道。
fn compatibility_fold(text: &str) -> String {
    text.chars()
        .map(|ch| match ch {
            '\u{ff01}'..='\u{ff5e}' => char::from_u32(u32::from(ch) - 0xfee0).unwrap_or(ch),
            '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

/// `isDroppedChar` — 空白与常见标点。列表照抄参考实现的分隔符字符集，外加中文
/// 书名号/引号，因为红果标题里这两种成对出现。
fn is_dropped(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '、' | '。'
                | '，'
                | '；'
                | '：'
                | '？'
                | '！'
                | '·'
                | '「'
                | '」'
                | '『'
                | '』'
                | '“'
                | '”'
                | '‘'
                | '’'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '<'
                | '>'
                | '《'
                | '》'
                | ','
                | '.'
                | ';'
                | ':'
                | '?'
                | '!'
                | '\''
                | '"'
                | '-'
                | '_'
                | '|'
                | '~'
                | '`'
                | '@'
                | '#'
                | '$'
                | '%'
                | '^'
                | '&'
                | '*'
                | '+'
                | '='
                | '/'
                | '\\'
        )
}

/// `normalizeSearchText` — 折叠 + 小写 + 去空白标点。归一后为空也照样返回空串，
/// 由调用方判「这条没有可比信息」。
pub fn normalize_search_text(value: &str) -> String {
    compatibility_fold(value)
        .to_lowercase()
        .chars()
        .filter(|ch| !is_whitespace_or_dropped(*ch))
        .collect()
}

fn is_whitespace_or_dropped(ch: char) -> bool {
    ch.is_whitespace() || is_dropped(ch)
}

/// 季号数字串的解析（与参考实现 `seasonTitle` 内联的那段完全同逻辑）。
///
/// 先按阿拉伯数字读；读不出再按中文数字做**位值**解析：`二十三`→23、`一百零八`→108、
/// `十`→10、`十六`→16（`十` 位当作 10，后面的 `六` 补个位）。纯中文数字位串（`一二三`）
/// 则逐位拼接成 `123`。与 JS 的 `Number(...)` 语义一致：认不出来的写法不当季号。
fn chinese_number(text: &str) -> Option<u32> {
    let digit = |ch: char| -> Option<u32> {
        Some(match ch {
            '零' => 0,
            '一' => 1,
            '二' | '两' => 2,
            '三' => 3,
            '四' => 4,
            '五' => 5,
            '六' => 6,
            '七' => 7,
            '八' => 8,
            '九' => 9,
            _ => return None,
        })
    };
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return None;
    }
    if chars.iter().all(|ch| ch.is_ascii_digit()) {
        return text.parse::<u32>().ok().filter(|value| *value > 0);
    }
    let has_unit = chars.iter().any(|ch| matches!(ch, '十' | '百'));
    if !has_unit {
        // 纯数字位串：逐位拼成一个数（任一字符不认识即失败，不猜）。
        let mut joined = String::new();
        for ch in &chars {
            joined.push_str(&digit(*ch)?.to_string());
        }
        return joined.parse::<u32>().ok().filter(|value| *value > 0);
    }
    let mut total: u32 = 0;
    let mut current: u32 = 0;
    for ch in chars {
        match ch {
            '十' | '百' => {
                let unit = if ch == '百' { 100 } else { 10 };
                total += current.max(1) * unit;
                current = 0;
            }
            other => current = digit(other).unwrap_or(0),
        }
    }
    Some(total + current).filter(|value| *value > 0)
}

/// `seasonTitle` — 拆出基础名与季号。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TitleSeason {
    /// 归一化后的基础名（不含季号那一段）。
    pub base: String,
    pub season: u32,
    /// 标题里是否显式写了季号。显式的排在推断的前面。
    pub explicit: bool,
}

/// 季号解析专用的**轻归一**：兼容折叠 + 小写 + 去空白，但**保留括号与 `第/季/部`**。
///
/// 与 [`normalize_search_text`] 的区别是有意的：匹配搜索词时要把标点全抹掉，
/// 而拆季号恰恰要靠括号、`第`、`季/部` 这些标记。参考实现里 `seasonTitle` 也只用
/// `NFKC + 去空白`，不动标点。
fn normalize_title_for_season(value: &str) -> String {
    compatibility_fold(value)
        .to_lowercase()
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect()
}

/// 标题结尾那一段季号标记。`marker_start` 是标记在**轻归一标题**里的字符起点
/// （基础名到此为止）；`run` 是标记里代表季号的那串字符；`bracketed` 表示裸括号写法。
#[derive(Debug, Clone, PartialEq, Eq)]
struct SeasonMarker {
    marker_start: usize,
    run: String,
    /// 裸括号里的数（可能是年份），需要额外的上限把关。
    bracketed: bool,
}

/// 季号串允许出现的字符：阿拉伯数字 + 中文数字与 `十/百`。
fn season_run_char(ch: char) -> bool {
    ch.is_ascii_digit()
        || matches!(
            ch,
            '零' | '一'
                | '二'
                | '两'
                | '三'
                | '四'
                | '五'
                | '六'
                | '七'
                | '八'
                | '九'
                | '十'
                | '百'
        )
}

/// `第N季` / `第N部` / `（N）` / `(N)` / `seasonN`，只认结尾那一段。与参考的
/// 锚定正则同形：没有 `第` 光写「十六季」不算，裸 `第N`（缺季/部）也不算。
fn season_marker(normalized: &str) -> Option<SeasonMarker> {
    let chars: Vec<char> = normalized.chars().collect();
    let len = chars.len();
    if len == 0 {
        return None;
    }
    // 结尾往回取一串季号字符；`stop` 是这串字符的右边界（不含）。
    let run_before = |stop: usize| -> Option<(usize, String)> {
        let mut index = stop;
        while index > 0 && season_run_char(chars[index - 1]) {
            index -= 1;
        }
        (index < stop).then(|| (index, chars[index..stop].iter().collect()))
    };
    // `×× seasonN`：轻归一已去空白、转小写，所以 `Season 2` 也长成这样。
    if chars[len - 1].is_ascii_digit() {
        let (start, run) = run_before(len)?;
        if start >= 6 && chars[start - 6..start].iter().collect::<String>() == "season" {
            return Some(SeasonMarker {
                marker_start: start - 6,
                run,
                bracketed: false,
            });
        }
    }
    // `×× 第N季` / `×× 第N部`。
    if matches!(chars[len - 1], '季' | '部') {
        let (start, run) = run_before(len - 1)?;
        if start > 0 && chars[start - 1] == '第' {
            return Some(SeasonMarker {
                marker_start: start - 1,
                run,
                bracketed: false,
            });
        }
    }
    // `××（N）` / `××(N)`（轻归一把全角括号折成半角）。
    if chars[len - 1] == ')' {
        let (start, run) = run_before(len - 1)?;
        if start > 0 && chars[start - 1] == '(' {
            return Some(SeasonMarker {
                marker_start: start - 1,
                run,
                bracketed: true,
            });
        }
    }
    None
}

/// `seasonTitle` — 基础名 + 季号。没有季号时 `season = 1`、`explicit = false`。
pub fn season_title(title: &str) -> TitleSeason {
    let normalized = normalize_title_for_season(title);
    let fallback = || TitleSeason {
        base: normalized.clone(),
        season: 1,
        explicit: false,
    };
    let Some(marker) = season_marker(&normalized) else {
        return fallback();
    };
    // 季号串既可能是阿拉伯数字，也可能是中文数字，`chinese_number` 两种都读。
    let Some(season) = chinese_number(&marker.run) else {
        return fallback();
    };
    // 裸括号里的数超过 99，多半是年份或其它标题元数据，不当季号（参考同规则）。
    if marker.bracketed && season > 99 {
        return fallback();
    }
    let base: String = normalized.chars().take(marker.marker_start).collect();
    TitleSeason {
        base,
        season,
        explicit: true,
    }
}

/// 一部剧的「族」标识：基础名。续作检测与季数排序都以它归组。
pub fn family_base(title: &str) -> String {
    season_title(title).base
}

/// `findSequels` 的一条候选：只带判定用的标题与展示用的标识。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SequelCandidate {
    pub id: String,
    pub title: String,
}

/// 续作上限：一次搜索页可能带回几十条同族结果，角标只报最靠前的几部。
const MAX_SEQUELS: usize = 10;

/// `findSequels` — 从候选里挑出「同一基础名、且显式季号更大」的续作。
///
/// 参考实现把这条定得很保守：**只认显式季号**。标题里没写季号的剧就当它是本传，
/// 不去猜「这大概是第二部」——独立续作（《××之××》那种）没有权威的血缘关系，
/// 硬归族会把不相干的剧报成更新。所以这里也要求 `explicit`，且基础名**逐字相等**。
///
/// 返回按季号升序、至多 [`MAX_SEQUELS`] 条；候选里与当前剧同名的自命中、重复 ID
/// 都会被去重。注意它不要求当前剧自己带季号——本传（`season = 1`）也能找到续作。
/// 来源归属不在这里判：调用方（知道片源前缀的那一层）先把不同源的候选滤掉。
pub fn find_sequels(current_title: &str, candidates: &[SequelCandidate]) -> Vec<SequelCandidate> {
    let current = season_title(current_title);
    if current.base.is_empty() {
        return Vec::new();
    }
    let mut found: Vec<(u32, SequelCandidate)> = Vec::new();
    let mut seen_ids: Vec<&str> = Vec::new();
    for candidate in candidates {
        let parsed = season_title(&candidate.title);
        // 季号不大于当前就是本传或更早的一季；非显式的不算续作。
        if !parsed.explicit || parsed.base != current.base || parsed.season <= current.season {
            continue;
        }
        if seen_ids.contains(&candidate.id.as_str()) || candidate.id.is_empty() {
            continue;
        }
        seen_ids.push(candidate.id.as_str());
        found.push((parsed.season, candidate.clone()));
    }
    found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.id.cmp(&b.1.id)));
    found.truncate(MAX_SEQUELS);
    found.into_iter().map(|(_, candidate)| candidate).collect()
}

/// 命中档位。数值即优先级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchScore {
    Intro = 0,
    KeywordContainsTitle = 1,
    TitleContainsKeyword = 2,
    Exact = 3,
}

/// `scoreMatch` — 归一化后的四档判定。返回 `None` 表示这条与关键词无关。
pub fn score_match(title: &str, description: Option<&str>, keyword: &str) -> Option<MatchScore> {
    let want = normalize_search_text(keyword);
    let have = normalize_search_text(title);
    if want.is_empty() || have.is_empty() {
        return None;
    }
    if have == want {
        return Some(MatchScore::Exact);
    }
    if have.contains(&want) {
        return Some(MatchScore::TitleContainsKeyword);
    }
    if want.contains(&have) {
        return Some(MatchScore::KeywordContainsTitle);
    }
    description
        .map(normalize_search_text)
        .filter(|intro| !intro.is_empty() && intro.contains(&want))
        .map(|_| MatchScore::Intro)
}

/// 季数感知排序后的结果索引：命中分高的在前，同基础名的续作紧跟在本传后面、
/// 按季号升序，其余保持来源顺序（上游顺序是同分时的稳定依据）。
///
/// 返回的是 `0..items.len()` 的一个排列，调用方按它重排，不在这儿动业务类型。
pub fn rank_order<T, F>(items: &[T], keyword: &str, mut key: F) -> Vec<usize>
where
    F: FnMut(&T) -> (String, Option<String>),
{
    // (命中分, 基础名, 是否显式季号, 季号, 原序)。分高、同族、显式在前、季号升序。
    let mut scored: Vec<(u8, String, bool, u32, usize)> = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let (title, description) = key(item);
        // 未命中不是丢弃理由：来源可能只给了热度排序，把它垫到命中项之后。
        // 命中分 +1（Intro=1…Exact=4），未命中记 0，降序排序后未命中自然落末尾。
        let score =
            score_match(&title, description.as_deref(), keyword).map_or(0, |value| value as u8 + 1);
        let parsed = season_title(&title);
        scored.push((score, parsed.base, parsed.explicit, parsed.season, index));
    }
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| b.2.cmp(&a.2))
            .then_with(|| a.3.cmp(&b.3))
            .then_with(|| a.4.cmp(&b.4))
    });
    scored.into_iter().map(|row| row.4).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_folds_width_case_and_punctuation() {
        assert_eq!(normalize_search_text("《吃械入宋》"), "吃械入宋");
        assert_eq!(normalize_search_text("ＡＩ  追凶！"), "ai追凶");
        assert_eq!(normalize_search_text("Forward 2026"), "forward2026");
        assert_eq!(normalize_search_text("  ,,  "), "");
    }

    #[test]
    fn season_markers_cover_the_site_three_writings() {
        assert_eq!(
            season_title("吃械入宋 第二季"),
            TitleSeason {
                base: "吃械入宋".into(),
                season: 2,
                explicit: true
            }
        );
        assert_eq!(
            season_title("吃械入宋（2）"),
            TitleSeason {
                base: "吃械入宋".into(),
                season: 2,
                explicit: true
            }
        );
        assert_eq!(
            season_title("重生之我在南宋当宦官 Season 10"),
            TitleSeason {
                base: "重生之我在南宋当宦官".into(),
                season: 10,
                explicit: true
            }
        );
        assert_eq!(
            season_title("吃械入宋 第2部"),
            TitleSeason {
                base: "吃械入宋".into(),
                season: 2,
                explicit: true
            }
        );
        // 中文位值写法：第一百部 → 100。
        assert_eq!(
            season_title("吃械入宋 第一百部"),
            TitleSeason {
                base: "吃械入宋".into(),
                season: 100,
                explicit: true
            }
        );
    }

    #[test]
    fn unparsable_seasons_stay_unparsed() {
        // 没有 `第` 前缀的「十六季」不当季号（与参考的锚定正则一致）。
        assert_eq!(season_title("小镇十六季").season, 1);
        assert!(!season_title("小镇十六季").explicit);
        // 裸括号里的四位数是年份，不是第 2024 季。
        assert_eq!(season_title("孤注一掷（2024）").season, 1);
        // 裸 `第N`（缺季/部）不算；只有基础名也不硬拆。
        assert_eq!(season_title("吃械入宋第3").season, 1);
        assert_eq!(season_title("第").base, "第");
        assert_eq!(season_title("未命名").season, 1);
    }

    #[test]
    fn family_groups_sequels_together() {
        assert_eq!(family_base("吃械入宋 第二季"), family_base("吃械入宋(1)"));
        assert_ne!(family_base("吃械入宋"), family_base("吃械入宋别传"));
    }

    #[test]
    fn scores_rank_exact_above_containment_above_intro() {
        let cards = vec![
            (
                "吃械入宋别传".to_string(),
                Some("讲述吃械入宋的背景".to_string()),
            ),
            ("《吃械入宋》".to_string(), None),
            ("吃械入宋".to_string(), None),
            ("无关剧".to_string(), Some("完全不相干".to_string())),
        ];
        assert_eq!(
            score_match("《吃械入宋》", None, "吃械入宋"),
            Some(MatchScore::Exact)
        );
        assert_eq!(
            score_match("吃械入宋别传", None, "吃械入宋"),
            Some(MatchScore::TitleContainsKeyword)
        );
        assert_eq!(
            score_match("吃械入宋", None, "吃械入宋第二季"),
            Some(MatchScore::KeywordContainsTitle)
        );
        assert_eq!(
            score_match("甲", Some("…吃械入宋…"), "吃械入宋"),
            Some(MatchScore::Intro)
        );
        assert_eq!(score_match("无关剧", Some("完全不相干"), "吃械入宋"), None);

        let order = rank_order(&cards, "吃械入宋", |(title, description)| {
            (title.clone(), description.clone())
        });
        assert_eq!(order[0], 1, "书名号归一后也算完全同名，同分按原序");
        assert_eq!(order[1], 2, "裸同名也是完全同名");
        assert_eq!(order[2], 0, "标题包含其次，简介命中排最后");
        assert_eq!(order[3], 3, "没命中的留在末尾，不丢条目");
    }

    #[test]
    fn sequels_follow_their_original_by_season() {
        let titles = vec![
            "吃械入宋 第三季".to_string(),
            "吃械入宋".to_string(),
            "吃械入宋 第二季".to_string(),
            "另一部".to_string(),
        ];
        let order = rank_order(&titles, "吃械入宋", |title| (title.clone(), None));
        let placed: Vec<&str> = order.iter().map(|index| titles[*index].as_str()).collect();
        assert_eq!(
            placed,
            vec!["吃械入宋", "吃械入宋 第二季", "吃械入宋 第三季", "另一部"],
            "同族按季号升序，第一季排最前"
        );
    }

    #[test]
    fn chinese_numbers_parse_up_to_999() {
        assert_eq!(chinese_number("十"), Some(10));
        assert_eq!(chinese_number("二十三"), Some(23));
        assert_eq!(chinese_number("一百零八"), Some(108));
        assert_eq!(chinese_number("三百"), Some(300));
        assert_eq!(chinese_number("两"), Some(2));
        // 与参考的位值解析一致：`十六` 就是 16，不猜、也不归为无效。
        assert_eq!(chinese_number("十六"), Some(16));
        assert_eq!(chinese_number("一百一十"), Some(110));
    }

    fn candidate(id: &str, title: &str) -> SequelCandidate {
        SequelCandidate {
            id: id.to_string(),
            title: title.to_string(),
        }
    }

    #[test]
    fn sequels_only_qualify_on_an_explicit_higher_season() {
        let candidates = vec![
            candidate("9", "吃械入宋 第三季"),
            candidate("1", "吃械入宋"),
            candidate("2", "吃械入宋 第二季"),
            candidate("3", "吃械入宋(3)"),
            // 没写季号 → 不当续作（参考实现同保守规则）。
            candidate("4", "吃械入宋之大结局"),
            candidate("5", "吃械入宋 第一季"),
            // 基础名逐字不等（轻归一后）也不当续作。
            candidate("6", "吃械入宋别传 第二季"),
        ];
        let titles: Vec<String> = find_sequels("吃械入宋", &candidates)
            .into_iter()
            .map(|item| item.title)
            .collect();
        assert_eq!(
            titles,
            vec!["吃械入宋 第二季", "吃械入宋(3)", "吃械入宋 第三季"],
            "只认显式季号、按季号升序，本传与无季号的不计"
        );

        // 当前剧自己已经在第三季，那第三季以下全部不落选。
        let later: Vec<String> = find_sequels("吃械入宋 第三季", &candidates)
            .into_iter()
            .map(|item| item.title)
            .collect();
        assert!(later.is_empty(), "已经是最新一季就没有续作");
    }

    #[test]
    fn sequel_lookup_survives_duplicates_and_empty_base() {
        let dup = vec![
            candidate("7", "吃械入宋 第二季"),
            candidate("7", "吃械入宋 第二季"),
            candidate("", "吃械入宋 第三季"),
        ];
        let found = find_sequels("吃械入宋", &dup);
        assert_eq!(found.len(), 1, "重复 ID 去重；没 ID 的不能当候选");
        assert_eq!(found[0].id, "7");
        // 空标题 / 整串都是标点 → 基础名为空，不报续作也不崩。
        assert!(find_sequels("", &dup).is_empty());
        assert!(find_sequels("（）", &dup).is_empty());
    }
}
