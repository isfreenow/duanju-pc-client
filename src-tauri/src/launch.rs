//! 外部指令：把「命令行参数 / 环境变量里的一句话」变成前端能执行的一次路由。
//!
//! 给 AI Agent 和脚本用：程序被拉起时带上 `--watch 关键词`，Rust 只解析成一条
//! [`LaunchCommand`]，具体走搜索 → 详情 → 播放哪一路是前端的事，这里不认识任何
//! 接口、也不碰签名与地址。
//!
//! 已经有一个实例在跑时（single-instance 回调），新进程的 argv 会被交回第一个
//! 实例：指令暂存进 [`Pending`]，同时广播 `launch-command` 事件，窗口拉回前台。
//! 前端启动时先 `take` 一次（覆盖冷启动），之后只认事件。
//!
//! 浏览器同样能走这条链路：系统把 `duanju://…` 作为 argv 交给新进程，
//! [`parse`] 认出它再交给 [`parse_url`]——裸负载像剧集 ID（纯数字或 `hema:` /
//! `xifan:` 前缀）就进 [`LaunchCommand::Series`]，否则当关键词；
//! `duanju://watch/<关键词>`、`duanju://series/<剧集 ID>` 则没有歧义。
//!
//! 例外是 [`SearchTask`]（`--search 关键词`）：它不开窗口，就地打一份带序号的
//! 结果清单然后退出，让 Agent 能把「第 3 个」这种回答换成下面那条 `--series`。
//! 它只借用 [`crate::commands::run_search`]，拿不到任何地址或密钥。

use serde::Serialize;
use std::io::Write;
use std::sync::Mutex;

/// 一次外部指令。序列化形状：`{"action":"watch","keyword":"…"}` /
/// `{"action":"series","seriesId":"…"}`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "action")]
pub enum LaunchCommand {
    /// 搜这个关键词，命中后直接播（有历史就续播）。
    #[serde(rename_all = "camelCase")]
    Watch { keyword: String },
    /// 直接进这一部的详情页并开播。
    #[serde(rename_all = "camelCase")]
    Series { series_id: String },
}

fn watch(keyword: String) -> Option<LaunchCommand> {
    let keyword = keyword.trim();
    (!keyword.is_empty()).then(|| LaunchCommand::Watch { keyword: keyword.to_string() })
}

fn series(series_id: String) -> Option<LaunchCommand> {
    let series_id = series_id.trim();
    (!series_id.is_empty()).then(|| LaunchCommand::Series { series_id: series_id.to_string() })
}

/// 深链接协议名：浏览器里的 `duanju://…` 链接与 Windows 注册表认的那个。
pub const URL_SCHEME: &str = "duanju";

/// 解一条 `duanju://…`。地址栏与系统传递时大小写不定，scheme 按不区分比较。
/// 关键词段做百分号解码（浏览器会把空格写成 `%20`）；剧集 ID 不需要。
pub fn parse_url(raw: &str) -> Option<LaunchCommand> {
    let head = raw.get(..URL_SCHEME.len() + 1)?;
    let matched = head.eq_ignore_ascii_case(&format!("{URL_SCHEME}:")) || head.eq_ignore_ascii_case(&format!("{URL_SCHEME}:/"));
    if !matched {
        return None;
    }
    let rest = raw[URL_SCHEME.len() + 1..].trim_start_matches('/').trim_end_matches('/');
    if rest.is_empty() {
        return None;
    }
    if let Some(keyword) = rest.strip_prefix("watch/") {
        return watch(percent_decode(keyword));
    }
    if let Some(series_id) = rest.strip_prefix("series/") {
        return series(series_id.to_string());
    }
    // 裸负载：像剧集 ID 就开播这一部，否则当关键词去搜索，和命令行裸参数同一套逻辑。
    if looks_like_series_id(rest) {
        series(rest.to_string())
    } else {
        watch(percent_decode(rest))
    }
}

/// 纯数字（红果）或带 `hema:` / `xifan:` 这类自描述前缀的才算 ID。
fn looks_like_series_id(value: &str) -> bool {
    value.bytes().all(|byte| byte.is_ascii_digit()) || value.contains(':')
}

/// 只解 `%XX`，解不动的原样保留；URL 里的中文已由浏览器保持原形，不经过这里。
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() && bytes[i + 1].is_ascii_hexdigit() && bytes[i + 2].is_ascii_hexdigit() {
            if let Ok(byte) = u8::from_str_radix(&input[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 从 argv 取第一条认识的指令；`args[0]` 是可执行文件自身，跳过。
///
/// 认 `--watch|-w <关键词>`、`--series|-s <剧集 ID>`，以及一个裸参数
/// （`app.exe 吃械入宋` 等价于 `--watch 吃械入宋`）。不认识的其他标志跳过。
/// `duanju://…` 深链接参数优先于一切标志（见 [`parse_url`]）。
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Option<LaunchCommand> {
    let mut it = args.into_iter();
    it.next();
    let mut bare: Option<String> = None;
    while let Some(arg) = it.next() {
        // 深链接：打包安装后浏览器点 duanju://…，Windows 把它作为 argv 交给新进程，
        // 冷启动和 single-instance 转交都落回这里，认出即结束，不再看其他标志。
        if arg.to_lowercase().starts_with(&format!("{URL_SCHEME}:")) {
            return parse_url(&arg);
        }
        match arg.as_str() {
            "--watch" | "-w" => return it.next().and_then(watch),
            "--series" | "-s" => return it.next().and_then(series),
            // 只查列表的那条命令不会走到这里（进程在开窗口之前就退出了），
            // 但混在一行里时不能把它的参数误当成剧名。
            "--search" | "-q" | "--limit" | "--category" => {
                it.next();
            }
            _ if arg.starts_with('-') => {}
            _ => {
                if bare.is_none() {
                    bare = Some(arg);
                }
            }
        }
    }
    bare.and_then(watch)
}

/// `--search 关键词`：查一次、打完就走的一次性任务。
pub struct SearchTask {
    pub keyword: String,
    pub limit: usize,
    /// 顶栏页签的 key（`xifan` 等）；空就是红果。
    pub category: String,
}

const DEFAULT_SEARCH_LIMIT: usize = 10;
const MAX_SEARCH_LIMIT: usize = 30;

fn trimmed(value: String) -> Option<String> {
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// 认 `--search|-q <关键词>`，可带 `--limit <1..=30>` 与 `--category <页签>`。
pub fn parse_search<I: IntoIterator<Item = String>>(args: I) -> Option<SearchTask> {
    let mut it = args.into_iter();
    it.next();
    let mut keyword: Option<String> = None;
    let mut limit = DEFAULT_SEARCH_LIMIT;
    let mut category = String::new();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--search" | "-q" => keyword = it.next().and_then(trimmed),
            "--limit" => {
                if let Some(value) = it.next().and_then(|value| value.parse::<usize>().ok()) {
                    limit = value.clamp(1, MAX_SEARCH_LIMIT);
                }
            }
            "--category" => category = it.next().and_then(trimmed).unwrap_or_default(),
            _ => {}
        }
    }
    keyword.map(|keyword| SearchTask { keyword, limit, category })
}

/// 一行结果：` 3  剧名  ID 7653…  全 24 集`。
fn search_line(index: usize, card: &crate::api::SeriesCard) -> String {
    let episodes = if card.metadata.episode_label.is_empty() {
        format!("{}集", card.episode_count)
    } else {
        card.metadata.episode_label.clone()
    };
    format!("{:>2}  {}  ID {}  {}", index, card.series_title, card.series_id, episodes)
}

/// 不开窗口，把第一页结果按序号打到 stdout。返回进程退出码（有结果为 0）。
pub fn search_and_print(task: SearchTask) -> i32 {
    let fail = |message: String| {
        eprintln!("搜索失败：{message}");
        1
    };
    let Ok(client) = crate::api::build_client() else {
        return fail("网络客户端初始化失败".to_string());
    };
    let Ok(runtime) = tokio::runtime::Runtime::new() else {
        return fail("无法启动查询所需的运行时".to_string());
    };
    let result = runtime.block_on(crate::commands::run_search(
        &client,
        &task.keyword,
        1,
        None,
        Some(&task.category),
    ));
    let cards = match result {
        Ok(result) => result.results,
        Err(error) => return fail(format!("{} · {}", error.code, error.message)),
    };
    if cards.is_empty() {
        return fail(format!("没有找到「{}」", task.keyword));
    }
    let out = std::io::stdout();
    let mut out = out.lock();
    // Windows 下控制台默认不是 UTF-8，中文剧名到了调用方那里可能是乱码；
    // 顺手把输出码页切到 65001，读管道的一侧就能直接解出中文。
    set_utf8_output();
    let shown = cards.len().min(task.limit);
    let _ = writeln!(out, "{} · {} 条结果，显示前 {shown} 条", task.keyword, cards.len());
    for (index, card) in cards.iter().take(shown).enumerate() {
        let _ = writeln!(out, "{}", search_line(index + 1, card));
    }
    let _ = out.flush();
    0
}

#[cfg(windows)]
fn set_utf8_output() {
    // 直接调 kernel32，不为这一行往依赖表里再塞一个 windows crate。
    unsafe extern "system" {
        fn SetConsoleOutputCP(code_page: u32) -> i32;
    }
    unsafe { SetConsoleOutputCP(65001) };
}

#[cfg(not(windows))]
fn set_utf8_output() {}

/// 本进程该执行的外部指令：命令行优先，其次环境变量。
///
/// 环境变量（`DUANJU_WATCH` / `DUANJU_SERIES`）是给 `pnpm tauri dev` 留的后门——
/// tauri CLI 不会把多余参数转交给二进制，开发态只能靠环境传。
pub fn from_process() -> Option<LaunchCommand> {
    parse(std::env::args()).or_else(|| {
        std::env::var("DUANJU_SERIES")
            .ok()
            .and_then(series)
            .or_else(|| std::env::var("DUANJU_WATCH").ok().and_then(watch))
    })
}

/// 还没被前端取走的指令。冷启动时由 `setup` 写入，二次启动由 single-instance
/// 回调写入；`take` 之后清空，保证同一条指令不会被路由两次。
#[derive(Default)]
pub struct Pending(Mutex<Option<LaunchCommand>>);

impl Pending {
    pub fn set(&self, command: LaunchCommand) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(command);
        }
    }

    pub fn take(&self) -> Option<LaunchCommand> {
        self.0.lock().ok().and_then(|mut slot| slot.take())
    }
}

#[tauri::command]
pub fn take_launch_command(pending: tauri::State<'_, Pending>) -> Option<LaunchCommand> {
    pending.take()
}

#[cfg(test)]
mod tests {
    use super::{parse, parse_search, parse_url, LaunchCommand};

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn flag_forms_carry_a_keyword_or_an_id() {
        assert_eq!(
            parse(args(&["app.exe", "--watch", "吃械入宋"])),
            Some(LaunchCommand::Watch { keyword: "吃械入宋".into() })
        );
        assert_eq!(
            parse(args(&["app.exe", "-s", "7458123456789"])),
            Some(LaunchCommand::Series { series_id: "7458123456789".into() })
        );
    }

    #[test]
    fn a_bare_word_is_a_keyword_and_unknown_flags_are_skipped() {
        assert_eq!(
            parse(args(&["app.exe", "--flagled", "吃械入宋"])),
            Some(LaunchCommand::Watch { keyword: "吃械入宋".into() })
        );
        assert_eq!(parse(args(&["app.exe"])), None);
        assert_eq!(parse(args(&["app.exe", "--watch", "  "])), None);
    }

    #[test]
    fn search_takes_a_keyword_and_optional_limit() {
        let task = parse_search(args(&["app.exe", "--search", "持械入宋"])).unwrap();
        assert_eq!((task.keyword.as_str(), task.limit, task.category.as_str()), ("持械入宋", 10, ""));
        let task = parse_search(args(&["app.exe", "-q", "持械入宋", "--limit", "3", "--category", "xifan"])).unwrap();
        assert_eq!((task.keyword.as_str(), task.limit, task.category.as_str()), ("持械入宋", 3, "xifan"));
        // 没有 --search 时不拦路；它的参数也不会被误当成剧名（见上条的 parse）。
        assert!(parse_search(args(&["app.exe", "--watch", "持械入宋"])).is_none());
        assert_eq!(parse(args(&["app.exe", "--search", "持械入宋"])), None);
    }

    #[test]
    fn a_deep_link_carries_an_id_a_prefixed_id_or_a_keyword() {
        assert_eq!(
            parse_url("duanju://7458123456789"),
            Some(LaunchCommand::Series { series_id: "7458123456789".into() })
        );
        // 带自描述前缀的 ID 在 URL 里照原样进负载，不会被当成端口解掉。
        assert_eq!(
            parse_url("duanju://hema:41000448669"),
            Some(LaunchCommand::Series { series_id: "hema:41000448669".into() })
        );
        assert_eq!(
            parse_url("duanju://吃械入宋"),
            Some(LaunchCommand::Watch { keyword: "吃械入宋".into() })
        );
        assert_eq!(
            parse_url("DUANJU://watch/%E6%8C%81%E6%A2%B0%E5%85%A5%E5%AE%8B"),
            Some(LaunchCommand::Watch { keyword: "持械入宋".into() })
        );
        assert_eq!(
            parse_url("duanju://series/7458123456789"),
            Some(LaunchCommand::Series { series_id: "7458123456789".into() })
        );
        // 浏览器地址栏里 duanju://id 可能被归一成 duanju:/id 或带尾斜杠。
        assert_eq!(
            parse_url("duanju:/7458123456789/"),
            Some(LaunchCommand::Series { series_id: "7458123456789".into() })
        );
        assert_eq!(parse_url("duanju://"), None);
        assert_eq!(parse_url("https://example.com"), None);
    }

    #[test]
    fn argv_deep_link_beats_every_other_flag() {
        assert_eq!(
            parse(args(&["app.exe", "duanju://7458123456789"])),
            Some(LaunchCommand::Series { series_id: "7458123456789".into() })
        );
        assert_eq!(
            parse(args(&["app.exe", "Duanju://watch/持械入宋", "--watch", "别的"])),
            Some(LaunchCommand::Watch { keyword: "持械入宋".into() })
        );
    }
}
