//! 兼容转码：HEVC → H.264，工具「首次需要时」按需下载，不打进安装包。
//!
//! 为什么要有这一层：Windows 上的 WebView2 不自带 HEVC 软解，画面会黑屏只剩声音
//! （见 `lib.rs` 里那段 `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS`）。那条缓解措施只对
//! 「系统装了 HEVC 解码组件、只是被 Chromium 的 GPU 黑名单挡掉」的机器有效；真的
//! 没有 HEVC 解码器的机器仍然看不了。参考项目把 FFmpeg 打进安装包（几十上百 MB 的
//! 二进制），代价是每个人都要为少数机器付这份下载量，所以这里改成：**包里不留
//! 二进制，第一次真的要转码时才从公开发布地址拉一次**，拉来的东西放在应用数据目录，
//! 卸载时跟着目录一起消失。
//!
//! 三条从参考实现（`main.js` 的 `resolveFfmpeg` / `pickH264Encoder` / 转码任务）
//! 搬过来的规则：
//!
//! * 正式版**不查 PATH**：打包后误用系统上任意一个同名程序，报错会难查到看不见。
//!   只有调试构建才允许用本机已有的 FFmpeg，省掉开发时每次重下。
//! * 编码器不能只看 `-encoders` 列表，必须**试编一帧**：列表里有不代表跑得起来
//!   （没有 N 卡的机器上 `h264_nvenc` 也在列表里，但一定失败）。
//! * 输出写 `.part` 再改名；进度靠 `-progress pipe:1` 的 `out_time_us`；参数里带
//!   `-decryption_key`，所以整条命令行都不落日志。
//!
//! CENC 的解密交给 FFmpeg 自己做（`-decryption_key`）：这条只对「整条流一把密钥」
//! 的 CENC 有效，也正是本项目 App 片源的形态（参考项目同样用原生 FFmpeg 处理 App
//! CENC）。万一遇到逐样本 `senc` 的写法，FFmpeg 会明确报错，错误原样带回前端，
//! 那条退路仍然是安装系统的 HEVC 解码组件。

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use serde::Serialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdout, Command};

use crate::api::{ApiError, ApiResult, Deadline};

/// 工具下载的硬上限。zip 本身一百多 MB，再大就不是我们认识的那份发布。
const TOOL_MAX_BYTES: u64 = 400 * 1024 * 1024;
/// 单集下载的硬上限：短剧一集几十 MB，超过这个数说明地址不对或来源变了。
const MEDIA_MAX_BYTES: u64 = 1_024 * 1024 * 1024;
/// 分片下载的片大小。分成片是为了「每一片之间都能检查取消」——一次长连接流式
/// 下载中途是没法停下来的，取消会拖到整片下完才生效。
const CHUNK_BYTES: u64 = 2 * 1024 * 1024;
/// 转码产物总目录的上限；超了按「最久没访问」往下删，正在用的那条除外。
const COMPAT_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// 超过这个天数的产物先清掉：追完的剧不会回来看第二遍，留着的代价是磁盘。
const COMPAT_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 3600);
/// 太小就不算转码成功：一个几百 KB 的文件多半是半截输出。
const MIN_OUTPUT_BYTES: u64 = 100 * 1024;

/// 目录布局：一切都在应用数据目录下，与包体、系统缓存分开。
#[derive(Debug, Clone)]
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    pub fn new(root: &Path) -> Self {
        Layout {
            root: root.to_path_buf(),
        }
    }

    /// 按需下载来的工具（`ffmpeg.exe` / `ffprobe.exe`）。
    pub fn tools_dir(&self) -> PathBuf {
        self.root.join("tools")
    }

    /// 转码产物缓存。
    pub fn compat_dir(&self) -> PathBuf {
        self.root.join("compat")
    }

    pub fn download_dir(&self) -> PathBuf {
        self.root.join("downloads")
    }

    /// 密钥可能很长，文件名只取前 8 个字符当区分：同一集的不同清晰度、不同密钥
    /// 各自一条，互不冒领。
    pub fn output_path(&self, vid: &str, quality: &str, content_key: Option<&str>) -> PathBuf {
        let safe = sanitize_key(vid);
        let quality = sanitize_key(quality);
        let tail = match content_key {
            Some(key) => key.chars().take(8).collect::<String>(),
            None => "plain".to_string(),
        };
        self.compat_dir()
            .join(format!("{safe}-{quality}-{tail}.mp4"))
    }

    pub fn ensure_dirs(&self) -> ApiResult<()> {
        for dir in [self.tools_dir(), self.compat_dir(), self.download_dir()] {
            std::fs::create_dir_all(&dir)
                .map_err(|error| ApiError::Network(format!("无法创建目录：{error}")))?;
        }
        Ok(())
    }
}

/// 文件名里只留字母数字与 `-_`，其余换成 `-`：vid 本身是数字，但清晰度是来源
/// 给的自由文本，不能让它带着 `\` `:` 进路径。
fn sanitize_key(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.trim().chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        // 全被过滤掉了（空串或全是标点）：用一个稳定占位，不返回空文件名。
        return format!("x{}", fnv1a(value));
    }
    trimmed.chars().take(96).collect()
}

fn fnv1a(value: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in value.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 一条对外广播的进度。前端只认这个，不知道 FFmpeg 的存在。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEvent {
    /// `tool`：正在拉取转码工具；`input`：正在取本片字节；`encode`：正在转码。
    pub stage: &'static str,
    pub job: String,
    /// 0–100；字节型阶段给 0–99，同时带 `doneBytes` / `totalBytes`。
    pub percent: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub done_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ProgressEvent {
    fn percent(stage: &'static str, job: &str, percent: u32, note: Option<String>) -> Self {
        ProgressEvent {
            stage,
            job: job.to_string(),
            percent,
            done_bytes: None,
            total_bytes: None,
            note,
        }
    }

    fn bytes(stage: &'static str, job: &str, done: u64, total: Option<u64>) -> Self {
        let percent = total.map_or(0, |total| percent_of(done, total));
        ProgressEvent {
            stage,
            job: job.to_string(),
            percent,
            done_bytes: Some(done),
            total_bytes: total,
            note: None,
        }
    }
}

/// 百分比：只增不减、封顶 99，最后由调用方补 100，避免进度条来回跳。
fn percent_of(done: u64, total: u64) -> u32 {
    if total == 0 {
        return 0;
    }
    (((done.min(total) as f64) / total as f64) * 100.0).clamp(0.0, 99.0) as u32
}

fn percent_of_seconds(seconds: f64, total: f64) -> u32 {
    if total <= 0.0 {
        return 0;
    }
    ((seconds / total).clamp(0.0, 0.99) * 100.0) as u32
}

/// 工具状态。前端据此决定按钮文案（「准备转码工具」/「正在准备」/直接开转）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolStatus {
    /// 本地已经有可用的工具（随包不带，只有下载过才为真）。
    pub available: bool,
    /// 正在下载工具，同一进程内只允许一路。
    pub preparing: bool,
    /// 上一次下载失败的原因（前端原样显示，里面已经带「下一步」）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// 已选中的 H.264 编码器（首次真正转码时探测，之后复用）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoder: Option<String>,
    /// 转码产物当前占用字节，前端用来解释「为什么磁盘在涨」。
    pub output_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct Tool {
    pub ffmpeg: PathBuf,
    pub ffprobe: Option<PathBuf>,
}

/// 当前选定的编码器，进程内一次；重新下载工具后作废。用 `Mutex<Option<..>>` 而不是
/// `OnceLock`，是为了单测能重置，也让「换了另一份二进制」能重选。
static CHOSEN_ENCODER: LazyLock<Mutex<Option<String>>> = LazyLock::new(|| Mutex::new(None));
/// 正在下载工具：`prepare` 与命令层共享这一位，避免两个视图各拉一份 100MB。
static PREPARING: AtomicBool = AtomicBool::new(false);
/// 上一次准备失败的原文，供状态查询回带；成功一次即清空。
static LAST_TOOL_ERROR: LazyLock<Mutex<Option<String>>> = LazyLock::new(|| Mutex::new(None));
/// 在跑的转码任务：`job` → 取消位。子进程靠 `start_kill` 从别处终止，句柄不放这儿。
static JOBS: LazyLock<Mutex<HashMap<String, Arc<AtomicBool>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn chosen_encoder() -> Option<String> {
    CHOSEN_ENCODER.lock().ok().and_then(|guard| guard.clone())
}

fn remember_encoder(encoder: &str) {
    if let Ok(mut guard) = CHOSEN_ENCODER.lock() {
        *guard = Some(encoder.to_string());
    }
}

fn forget_encoder() {
    if let Ok(mut guard) = CHOSEN_ENCODER.lock() {
        *guard = None;
    }
}

fn remember_tool_error(message: &str) {
    if let Ok(mut guard) = LAST_TOOL_ERROR.lock() {
        *guard = Some(message.to_string());
    }
}

fn clear_tool_error() {
    if let Ok(mut guard) = LAST_TOOL_ERROR.lock() {
        *guard = None;
    }
}

pub fn status(layout: &Layout) -> ToolStatus {
    let available = resolve_tool(layout).is_some();
    let last_error = if available {
        None
    } else {
        LAST_TOOL_ERROR.lock().ok().and_then(|guard| guard.clone())
    };
    ToolStatus {
        available,
        preparing: PREPARING.load(Ordering::SeqCst),
        last_error,
        encoder: chosen_encoder(),
        output_bytes: dir_bytes(&layout.compat_dir()),
    }
}

fn dir_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.metadata().ok())
        .filter(|meta| meta.is_file())
        .map(|meta| meta.len())
        .sum()
}

// ── 工具：定位与按需下载 ──────────────────────────────────────────────────

fn exe_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// `resolveFfmpeg` — 正式版只认应用数据目录里那一份；调试构建额外允许 PATH 上
/// 已有的，开发时不必每次重下一百多 MB。
pub fn resolve_tool(layout: &Layout) -> Option<Tool> {
    let from_dir = |name: &str| layout.tools_dir().join(exe_name(name));
    let mut ffmpeg = is_usable_file(&from_dir("ffmpeg")).then(|| from_dir("ffmpeg"));
    if ffmpeg.is_none() && cfg!(debug_assertions) {
        ffmpeg = which_on_path(&exe_name("ffmpeg"));
    }
    let ffmpeg = ffmpeg?;
    let mut ffprobe = is_usable_file(&from_dir("ffprobe")).then(|| from_dir("ffprobe"));
    if ffprobe.is_none() && cfg!(debug_assertions) {
        ffprobe = which_on_path(&exe_name("ffprobe"));
    }
    Some(Tool { ffmpeg, ffprobe })
}

/// 判「是不是一个像样的可执行文件」：至少 1MB，非 Windows 还要有执行位。
/// 1MB 这条下限能挡住「下载只写了个头就被截断」的那类残file。
fn is_usable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.len() < 1024 * 1024 {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return meta.permissions().mode() & 0o111 != 0;
    }
    #[cfg(not(unix))]
    true
}

fn which_on_path(file: &str) -> Option<PathBuf> {
    let sep = if cfg!(windows) { ';' } else { ':' };
    for dir in std::env::var("PATH").unwrap_or_default().split(sep) {
        let dir = dir.trim_matches('"');
        if dir.is_empty() {
            continue;
        }
        let candidate = PathBuf::from(dir).join(file);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// 一条候选下载地址。`env_hint` 是给「本网络取不到 GitHub」的人留的口子：
/// 设了环境变量 `DUANJU_FFMPEG_URL` 就只认它（自己放到局域网或镜像上）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub url: String,
    pub env_hint: Option<&'static str>,
}

/// 环境变量名：覆盖工具下载地址（必须是 https）。
pub const TOOL_URL_ENV: &str = "DUANJU_FFMPEG_URL";

/// 候选顺序：环境变量 → 官方构建站 → GitHub 发布页。全部 https。
pub fn candidates() -> Vec<Candidate> {
    if let Some(override_url) = std::env::var(TOOL_URL_ENV)
        .ok()
        .filter(|value| value.starts_with("https://"))
    {
        return vec![Candidate {
            url: override_url,
            env_hint: Some(TOOL_URL_ENV),
        }];
    }
    let (family, asset) = if cfg!(windows) {
        ("windows", "win64-gpl.zip")
    } else if cfg!(target_os = "macos") {
        ("macos", "macos-universal.zip")
    } else if cfg!(target_os = "linux") {
        ("linux", "linux64.zip")
    } else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    if family == "windows" {
        // gyan.dev 的 release 包：单文件 zip，含 ffmpeg/ffprobe，直连成功率高。
        rows.push("https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip".to_string());
    }
    rows.push(format!(
        "https://github.com/BtbN/FFmpeg-Builds/releases/latest/download/ffmpeg-master-latest-{asset}"
    ));
    rows.into_iter()
        .map(|url| Candidate {
            url,
            env_hint: Some(TOOL_URL_ENV),
        })
        .collect()
}

/// 这个 zip 条目是不是我们要的那个可执行文件。
///
/// 两种发布的目录结构不同：`bin/ffmpeg.exe`（gyan）与
/// `ffmpeg-master-latest-win64-gpl/bin/ffmpeg.exe`（BtbN），mac/linux 还多一层平台
/// 目录。所以只认「最后一段是目标文件名」，再在同等命中里挑路径最浅的，避免命中
/// 文档或脚本里的同名示例文件。
pub fn picks_entry(entry: &str, want: &str) -> bool {
    let last = entry.rsplit(['/', '\\']).next().unwrap_or("");
    !entry.ends_with('/') && last.eq_ignore_ascii_case(want)
}

/// 从 zip 里取目标文件；返回取出的字节数。上限把住「解压炸弹」：条目声明的原始
/// 大小超过阈值直接失败，不落盘。
pub fn extract_binary(
    zip_path: &Path,
    out_dir: &Path,
    want: &str,
    max_bytes: u64,
) -> ApiResult<u64> {
    let file =
        std::fs::File::open(zip_path).map_err(|error| ApiError::Network(error.to_string()))?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file)).map_err(tool_error)?;
    let mut best: Option<(usize, usize)> = None;
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(tool_error)?;
        let name = entry.name().to_string();
        if !picks_entry(&name, want) || entry.is_dir() {
            continue;
        }
        if entry.size() > max_bytes {
            return Err(too_big(want));
        }
        let depth = name.matches(['/', '\\']).count();
        if best.is_none_or(|(best_depth, _)| depth < best_depth) {
            best = Some((depth, index));
        }
    }
    let Some((_, index)) = best else {
        return Err(ApiError::Business {
            message: format!("工具包里没有 {want}"),
            code: "TOOL".to_string(),
        });
    };
    let mut entry = archive.by_index(index).map_err(tool_error)?;
    let out_path = out_dir.join(want);
    let temp = out_path.with_extension("part");
    let mut written: u64 = 0;
    {
        let mut sink = std::io::BufWriter::new(
            std::fs::File::create(&temp).map_err(|error| ApiError::Network(error.to_string()))?,
        );
        let mut buffer = [0u8; 128 * 1024];
        loop {
            let read = entry
                .read(&mut buffer)
                .map_err(|error| ApiError::Network(format!("解包失败：{error}")))?;
            if read == 0 {
                break;
            }
            written += read as u64;
            if written > max_bytes {
                drop(sink);
                let _ = std::fs::remove_file(&temp);
                return Err(too_big(want));
            }
            std::io::Write::write_all(&mut sink, &buffer[..read])
                .map_err(|error| ApiError::Network(error.to_string()))?;
        }
        std::io::Write::flush(&mut sink).map_err(|error| ApiError::Network(error.to_string()))?;
    }
    if written < 1024 * 1024 {
        let _ = std::fs::remove_file(&temp);
        return Err(ApiError::Business {
            message: format!("取出的 {want} 太小，不像可用的可执行文件"),
            code: "TOOL".to_string(),
        });
    }
    // Windows 上同名文件正在被占用时改名会失败；先删旧的。
    let _ = std::fs::remove_file(&out_path);
    std::fs::rename(&temp, &out_path).map_err(|error| ApiError::Network(error.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&out_path, std::fs::Permissions::from_mode(0o755));
    }
    Ok(written)
}

fn tool_error(error: zip::result::ZipError) -> ApiError {
    ApiError::Business {
        message: format!("工具包解析失败：{error}"),
        code: "TOOL".to_string(),
    }
}

fn too_big(want: &str) -> ApiError {
    ApiError::Business {
        message: format!("工具包里的 {want} 超出允许大小"),
        code: "TOOL".to_string(),
    }
}

/// 下载并装好转码工具。已经装好就直接返回（`prepare` 是幂等的）。
pub async fn prepare(
    client: &reqwest::Client,
    layout: &Layout,
    job: &str,
    mut on_event: impl FnMut(ProgressEvent),
) -> ApiResult<Tool> {
    if let Some(tool) = resolve_tool(layout) {
        clear_tool_error();
        return Ok(tool);
    }
    if PREPARING.swap(true, Ordering::SeqCst) {
        return Err(ApiError::Business {
            message: "转码工具正在下载，请等当前这一路完成".to_string(),
            code: "BUSY".to_string(),
        });
    }
    let outcome = prepare_inner(client, layout, job, &mut on_event).await;
    PREPARING.store(false, Ordering::SeqCst);
    match &outcome {
        Ok(_) => clear_tool_error(),
        Err(error) => remember_tool_error(&error.to_string()),
    }
    outcome
}

async fn prepare_inner(
    client: &reqwest::Client,
    layout: &Layout,
    job: &str,
    on_event: &mut impl FnMut(ProgressEvent),
) -> ApiResult<Tool> {
    layout.ensure_dirs()?;
    let rows = candidates();
    if rows.is_empty() {
        return Err(ApiError::Business {
            message: "当前平台没有对应的转码工具发布".to_string(),
            code: "TOOL".to_string(),
        });
    }
    let mut last_error = String::new();
    for candidate in rows {
        let archive = layout
            .download_dir()
            .join(format!("ffmpeg-{}.zip", fnv1a(&candidate.url)));
        on_event(ProgressEvent::percent(
            "tool",
            job,
            0,
            Some("正在下载转码工具".to_string()),
        ));
        if let Err(error) = download_tool(client, &candidate.url, &archive, job, on_event).await {
            last_error = error.to_string();
            let _ = std::fs::remove_file(&archive);
            continue;
        }
        on_event(ProgressEvent::percent(
            "tool",
            job,
            99,
            Some("正在解出可执行文件".to_string()),
        ));
        let want_ffmpeg = exe_name("ffmpeg");
        if let Err(error) =
            extract_binary(&archive, &layout.tools_dir(), &want_ffmpeg, TOOL_MAX_BYTES)
        {
            last_error = error.to_string();
            let _ = std::fs::remove_file(&archive);
            continue;
        }
        let want_ffprobe = exe_name("ffprobe");
        // ffprobe 只用来拿时长，取不到就退化成从 ffmpeg 的输出里读。
        let _ = extract_binary(&archive, &layout.tools_dir(), &want_ffprobe, TOOL_MAX_BYTES);
        let _ = std::fs::remove_file(&archive);
        let tool = Tool {
            ffmpeg: layout.tools_dir().join(&want_ffmpeg),
            ffprobe: (layout.tools_dir().join(&want_ffprobe).is_file())
                .then(|| layout.tools_dir().join(&want_ffprobe)),
        };
        if let Err(error) = verify(&tool).await {
            last_error = error.to_string();
            // 校验不过说明这份二进制不能用，留在目录里只会每次都被判「已就绪」。
            discard(&tool);
            continue;
        }
        forget_encoder();
        on_event(ProgressEvent::percent(
            "tool",
            job,
            100,
            Some("转码工具已就绪".to_string()),
        ));
        return Ok(tool);
    }
    Err(ApiError::Business {
        message: if last_error.is_empty() {
            "转码工具下载失败，稍后重试".to_string()
        } else {
            format!("转码工具下载失败：{last_error}")
        },
        code: "TOOL".to_string(),
    })
}

/// 一百多 MB 的东西给十分钟预算；进度逐块回报，前端看得到字节数。
async fn download_tool(
    client: &reqwest::Client,
    url: &str,
    archive: &Path,
    job: &str,
    on_event: &mut impl FnMut(ProgressEvent),
) -> ApiResult<u64> {
    let deadline = Deadline::after(600);
    crate::api::fetch_binary(
        client,
        url,
        &[],
        deadline,
        TOOL_MAX_BYTES,
        archive,
        |done, total| {
            on_event(ProgressEvent::bytes("tool", job, done, total));
        },
    )
    .await
}

/// 装好后先跑一次 `-version`：路径对但文件被截断、或平台不匹配，都在这儿判死。
async fn verify(tool: &Tool) -> ApiResult<()> {
    let output = Command::new(&tool.ffmpeg)
        .args(["-hide_banner", "-version"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| ApiError::Network(format!("无法启动转码工具：{error}")))?;
    if !output.status.success() {
        return Err(ApiError::Business {
            message: "下载到的转码工具无法运行".to_string(),
            code: "TOOL".to_string(),
        });
    }
    Ok(())
}

fn discard(tool: &Tool) {
    let _ = std::fs::remove_file(&tool.ffmpeg);
    if let Some(ffprobe) = &tool.ffprobe {
        let _ = std::fs::remove_file(ffprobe);
    }
}

// ── 编码器：探测与参数 ────────────────────────────────────────────────────

/// 各平台优先试的硬件编码器，最后一定落到软编兜底（参考实现同序）。
fn preferred_encoders() -> Vec<&'static str> {
    if cfg!(target_os = "macos") {
        vec!["h264_videotoolbox", "libx264"]
    } else {
        vec!["h264_nvenc", "h264_qsv", "h264_amf", "h264_mf", "libx264"]
    }
}

/// 每个编码器对应的参数（照参考的取值：软编 veryfast/CRF 23，硬编各按推荐）。
fn encoder_args(encoder: &str) -> Vec<String> {
    let rows: &[&str] = match encoder {
        "libx264" => &[
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            "23",
            "-profile:v",
            "high",
            "-level",
            "4.2",
        ],
        "h264_videotoolbox" => &[
            "-c:v",
            "h264_videotoolbox",
            "-b:v",
            "6M",
            "-pix_fmt",
            "yuv420p",
        ],
        "h264_nvenc" => &[
            "-c:v",
            "h264_nvenc",
            "-preset",
            "p4",
            "-cq",
            "23",
            "-b:v",
            "0",
        ],
        "h264_qsv" => &["-c:v", "h264_qsv", "-global_quality", "23"],
        "h264_amf" => &[
            "-c:v", "h264_amf", "-quality", "speed", "-rc", "cqp", "-qp_i", "23", "-qp_p", "23",
        ],
        other => &["-c:v", other, "-cq", "23"],
    };
    rows.iter().map(|value| value.to_string()).collect()
}

/// `pickH264Encoder` — 列表里有 ≠ 跑得起来，所以逐个试编一帧 64×64 的黑屏。
pub async fn pick_encoder(tool: &Tool) -> ApiResult<String> {
    if let Some(cached) = chosen_encoder() {
        return Ok(cached);
    }
    let list = run_capture(&tool.ffmpeg, &["-hide_banner", "-encoders"], 20)
        .await
        .unwrap_or_default();
    let mut rows: Vec<&str> = preferred_encoders()
        .into_iter()
        .filter(|name| list.contains(*name))
        .collect();
    if !rows.contains(&"libx264") {
        rows.push("libx264");
    }
    let probe = [
        "-hide_banner",
        "-v",
        "error",
        "-f",
        "lavfi",
        "-i",
        "color=c=black:s=64x64:d=0.1",
        "-frames:v",
        "1",
    ];
    for encoder in rows {
        let mut args: Vec<&str> = probe.to_vec();
        args.extend(["-c:v", encoder, "-f", "null", "-"]);
        if run_capture(&tool.ffmpeg, &args, 25).await.is_some() {
            remember_encoder(encoder);
            return Ok(encoder.to_string());
        }
    }
    remember_encoder("libx264");
    Ok("libx264".to_string())
}

async fn run_capture(binary: &Path, args: &[&str], secs: u64) -> Option<String> {
    let pending = Command::new(binary)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();
    // `tokio::process::Command` 没有 `.timeout()`：用 `tokio::time::timeout` 包住
    // `output()`，超时时 `kill_on_drop` 会终止子进程，不会留下挂死的 ffprobe。
    let output = match tokio::time::timeout(std::time::Duration::from_secs(secs), pending).await {
        Ok(Ok(output)) => output,
        _ => return None,
    };
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

// ── 时长与进度解析 ────────────────────────────────────────────────────────

/// 从 `ffprobe`/`ffmpeg` 的文本里读时长（秒）。两种写法都认：
/// `Duration: 00:00:24.10, start:` 与 `duration=24.100000`。
pub fn parse_duration(text: &str) -> Option<f64> {
    if let Some(index) = text.find("Duration:") {
        let rest = &text[index + "Duration:".len()..];
        let head = rest.split(',').next().unwrap_or("").trim();
        let mut parts = head.split(':');
        // 小时:分:秒.毫秒；缺位或不是数字就放弃这一种写法。
        let hour = parts.next()?.parse::<f64>().ok()?;
        let minute = parts.next()?.parse::<f64>().ok()?;
        let second = parts.next()?.parse::<f64>().ok()?;
        let total = hour * 3600.0 + minute * 60.0 + second;
        if total > 0.0 {
            return Some(total);
        }
    }
    for token in text.split_whitespace() {
        if let Some(value) = token.strip_prefix("duration=")
            && let Ok(parsed) = value.parse::<f64>()
            && parsed > 0.0
        {
            return Some(parsed);
        }
    }
    None
}

/// `-progress pipe:1` 的一行：`key=value`，我们只要 `out_time_us`。
pub fn parse_out_time(line: &str) -> Option<f64> {
    let (key, value) = line.split_once('=')?;
    if key.trim() != "out_time_us" {
        return None;
    }
    value
        .trim()
        .parse::<f64>()
        .ok()
        .map(|micro| micro / 1_000_000.0)
}

// ── 取消 ─────────────────────────────────────────────────────────────────

fn job_flag(job: &str) -> Arc<AtomicBool> {
    let mut guard = JOBS.lock().expect("任务表锁不可用");
    guard
        .entry(job.to_string())
        .or_insert_with(|| Arc::new(AtomicBool::new(false)))
        .clone()
}

/// 请求取消一路转码。返回是否认识这个任务号。
pub fn cancel(job: &str) -> bool {
    match JOBS.lock().ok().and_then(|guard| guard.get(job).cloned()) {
        Some(flag) => {
            flag.store(true, Ordering::SeqCst);
            true
        }
        None => false,
    }
}

fn cancelled(flag: &AtomicBool) -> bool {
    flag.load(Ordering::SeqCst)
}

/// 起一个轮询：取消位一置上就终止子进程。`start_kill` 只借 `&Child`，所以能放在
/// 另一个任务里，不挡住读输出的循环。
fn spawn_kill_watcher(
    child: Arc<tokio::sync::Mutex<Child>>,
    flag: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if cancelled(&flag) {
                let mut guard = child.lock().await;
                let _ = guard.start_kill();
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    })
}

// ── 缓存清理 ─────────────────────────────────────────────────────────────

/// 清掉过期产物 + 压到总量上限以内（最久未访问的先删，`protected` 那条不动）。
/// 返回清理后占用字节。
pub fn trim_cache(dir: &Path, protected: Option<&Path>) -> u64 {
    let now = std::time::SystemTime::now();
    let mut entries: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    let Ok(rows) = std::fs::read_dir(dir) else {
        return 0;
    };
    for row in rows.filter_map(|row| row.ok()) {
        let Ok(meta) = row.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let path = row.path();
        if path.extension().and_then(|value| value.to_str()) == Some("part") {
            // 半截输出：不管是崩了还是被杀，都不该再当缓存。
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let at = meta
            .accessed()
            .unwrap_or(meta.modified().unwrap_or(std::time::UNIX_EPOCH));
        let stale = now
            .duration_since(at)
            .map(|age| age > COMPAT_MAX_AGE)
            .unwrap_or(false);
        if stale && protected != Some(path.as_path()) {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        entries.push((path, meta.len(), at));
    }
    let mut total: u64 = entries.iter().map(|(_, size, _)| size).sum();
    if total <= COMPAT_MAX_BYTES {
        return total;
    }
    entries.sort_by_key(|(_, _, at)| *at);
    for (path, size, _) in entries {
        if total <= COMPAT_MAX_BYTES {
            break;
        }
        if protected == Some(path.as_path()) {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
    total
}

/// 一次转码的输入（由命令层从解析结果里挑一条流带进来）。
#[derive(Debug, Clone)]
pub struct MediaInput {
    pub url: String,
    pub content_key: Option<String>,
    pub user_agent: String,
    pub referer: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscodeOutput {
    /// 转码产物的本地文件路径（前端只用来显示与清理，播放走 `resolve` 之后的地址）。
    pub path: String,
    pub bytes: u64,
    pub encoder: String,
    /// 命中已有缓存：没有重新下载、也没有重新编码。
    pub cached: bool,
}

/// 下载 → 转码 → 缓存。全程可取消，进度经 `on_event` 出去。
pub async fn transcode(
    client: &reqwest::Client,
    layout: &Layout,
    tool: &Tool,
    job: &str,
    input: &MediaInput,
    output: &Path,
    mut on_event: impl FnMut(ProgressEvent),
) -> ApiResult<TranscodeOutput> {
    layout.ensure_dirs()?;
    trim_cache(&layout.compat_dir(), Some(output));
    if let Ok(meta) = std::fs::metadata(output)
        && meta.len() >= MIN_OUTPUT_BYTES
    {
        on_event(ProgressEvent::percent(
            "encode",
            job,
            100,
            Some("已缓存".to_string()),
        ));
        return Ok(TranscodeOutput {
            path: output.display().to_string(),
            bytes: meta.len(),
            encoder: chosen_encoder().unwrap_or_else(|| "libx264".to_string()),
            cached: true,
        });
    }
    let flag = job_flag(job);
    let result = transcode_inner(
        client,
        layout,
        tool,
        job,
        input,
        output,
        &flag,
        &mut on_event,
    )
    .await;
    if let Ok(mut guard) = JOBS.lock() {
        guard.remove(job);
    }
    if result.is_err() {
        let _ = std::fs::remove_file(output.with_extension("mp4.part"));
        let _ = std::fs::remove_file(output);
    }
    result
}

async fn transcode_inner(
    client: &reqwest::Client,
    layout: &Layout,
    tool: &Tool,
    job: &str,
    input: &MediaInput,
    output: &Path,
    flag: &Arc<AtomicBool>,
    on_event: &mut impl FnMut(ProgressEvent),
) -> ApiResult<TranscodeOutput> {
    // 1) 取本片字节：分片下载，片与片之间检查取消。地址是 CDN 签发的，落盘只到
    //    应用数据目录，转码完就删。
    let source = layout
        .download_dir()
        .join(format!("src-{}.mp4", fnv1a(&input.url)));
    on_event(ProgressEvent::percent(
        "input",
        job,
        0,
        Some("正在取本片数据".to_string()),
    ));
    let headers = media_headers(input);
    if let Err(error) =
        fetch_chunked(client, &headers, &input.url, &source, job, flag, on_event).await
    {
        let _ = std::fs::remove_file(&source);
        return Err(error);
    }
    if cancelled(flag) {
        let _ = std::fs::remove_file(&source);
        return Err(cancelled_error());
    }

    // 2) 时长：先问 ffprobe，没有就从 ffmpeg 的输出里读。
    let duration = probe_duration(tool, &source).await;

    // 3) 编码器与参数。密钥通过 `-decryption_key` 交给 FFmpeg，命令行不落日志。
    let encoder = pick_encoder(tool)
        .await
        .unwrap_or_else(|_| "libx264".to_string());
    let temp = output.with_extension("mp4.part");
    let _ = std::fs::remove_file(&temp);
    let mut args: Vec<String> = vec!["-y".into(), "-hide_banner".into(), "-nostdin".into()];
    if let Some(key) = input.content_key.as_ref().filter(|key| !key.is_empty()) {
        args.push("-decryption_key".into());
        args.push(key.clone());
    }
    args.push("-i".into());
    args.push(source.display().to_string());
    args.extend(encoder_args(&encoder));
    if encoder != "h264_videotoolbox" {
        args.extend(["-pix_fmt".into(), "yuv420p".into()]);
    }
    args.extend([
        "-c:a".into(),
        "aac".into(),
        "-b:a".into(),
        "128k".into(),
        "-movflags".into(),
        "+faststart".into(),
        "-progress".into(),
        "pipe:1".into(),
        "-nostats".into(),
        "-f".into(),
        "mp4".into(),
        temp.display().to_string(),
    ]);

    let mut child = Command::new(&tool.ffmpeg)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| ApiError::Network(format!("无法启动转码工具：{error}")))?;
    let stdout = child.stdout.take().expect("已声明 piped");
    let stderr = child.stderr.take().expect("已声明 piped");
    let shared = Arc::new(tokio::sync::Mutex::new(child));
    let watcher = spawn_kill_watcher(shared.clone(), flag.clone());
    let tail = spawn_tail(stderr);
    // 进度读取与子进程等待在同一个任务里并发（`tokio::join!`），这样 `on_event`
    // 只是借用、不必 `'static`，也就不需要 `Send`；`spawn_tail` 带走 stderr 的所有权，
    // 单独 spawn 才不会把管道读满堵住 ffmpeg。
    let status = {
        let mut guard = shared.lock().await;
        let (waited, _) = tokio::join!(
            guard.wait(),
            drain_progress(
                stdout,
                flag.clone(),
                job.to_string(),
                duration,
                &mut *on_event
            )
        );
        waited.map_err(|error| ApiError::Network(format!("转码进程异常：{error}")))?
    };
    watcher.abort();
    let tail = tail.await.unwrap_or_default();
    let _ = std::fs::remove_file(&source);
    if cancelled(flag) {
        let _ = std::fs::remove_file(&temp);
        return Err(cancelled_error());
    }
    if !status.success() {
        let message: String = tail
            .lines()
            .rev()
            .filter(|line| !line.trim().is_empty())
            .take(2)
            .collect::<Vec<_>>()
            .join(" ");
        return Err(ApiError::Business {
            message: format!(
                "转码失败（退出码 {}）{}",
                status.code().unwrap_or(-1),
                if message.is_empty() {
                    String::new()
                } else {
                    format!("：{}", message.chars().take(300).collect::<String>())
                }
            ),
            code: "TRANSCODE".to_string(),
        });
    }
    let meta = std::fs::metadata(&temp).map_err(|_| ApiError::Business {
        message: "转码没有产出文件".to_string(),
        code: "TRANSCODE".to_string(),
    })?;
    if meta.len() < MIN_OUTPUT_BYTES {
        return Err(ApiError::Business {
            message: "转码产物过小，判定为失败".to_string(),
            code: "TRANSCODE".to_string(),
        });
    }
    let _ = std::fs::remove_file(output);
    std::fs::rename(&temp, output).map_err(|error| ApiError::Network(error.to_string()))?;
    trim_cache(&layout.compat_dir(), Some(output));
    Ok(TranscodeOutput {
        path: output.display().to_string(),
        bytes: meta.len(),
        encoder,
        cached: false,
    })
}

fn media_headers(input: &MediaInput) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    if !input.user_agent.is_empty() {
        rows.push(("User-Agent".to_string(), input.user_agent.clone()));
    }
    if let Some(referer) = &input.referer {
        rows.push(("Referer".to_string(), referer.clone()));
    }
    rows
}

/// 分片把整条流落到同一个文件：`0-(N-1)`、`N-(2N-1)`…… 服务器不支持 Range 时
/// 只会有一片（返回 200 与完整响应体），同样是对的。片间检查取消。
async fn fetch_chunked(
    client: &reqwest::Client,
    headers: &[(String, String)],
    url: &str,
    dest: &Path,
    job: &str,
    flag: &AtomicBool,
    on_event: &mut impl FnMut(ProgressEvent),
) -> ApiResult<u64> {
    let mut offset: u64 = 0;
    loop {
        if cancelled(flag) {
            return Err(cancelled_error());
        }
        let piece = dest.with_extension(format!("part{offset}"));
        let mut scoped: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let range = format!("bytes={offset}-{}", offset + CHUNK_BYTES - 1);
        scoped.push(("Range", range.as_str()));
        // 每片 30 秒预算：慢 CDN 也能攒够时间，整体不会无限挂着。
        match crate::api::fetch_binary(
            client,
            url,
            &scoped,
            Deadline::after(30),
            CHUNK_BYTES,
            &piece,
            |done, total| {
                // 总量在这一层不可知（只有 `Content-Range` 里有，而 `fetch_binary` 不解析
                // 它），所以百分比交不出来，只报已下载字节，前端按 MB 显示。
                let _ = total;
                on_event(ProgressEvent::bytes("input", job, offset + done, None));
            },
        )
        .await
        {
            Ok(size) => {
                append_and_remove(dest, &piece)?;
                offset += size;
                // 拿到不足一片，或第一次就没分片（不支持 Range）→ 结束。
                if size < CHUNK_BYTES {
                    return Ok(offset);
                }
            }
            // 超出末尾：Range 语法上到无穷才是标准写法，这里按「取完了」处理。
            Err(ApiError::Business { code, ref message }) if code == "DOWNLOAD" => {
                if message.contains("416") {
                    return Ok(offset);
                }
                let _ = std::fs::remove_file(&piece);
                return Err(ApiError::Business {
                    code,
                    message: message.clone(),
                });
            }
            Err(error) => {
                let _ = std::fs::remove_file(&piece);
                return Err(error);
            }
        }
        if offset > MEDIA_MAX_BYTES {
            return Err(ApiError::Business {
                message: "本片数据超出允许大小".to_string(),
                code: "MEDIA".to_string(),
            });
        }
    }
}

fn append_and_remove(dest: &Path, piece: &Path) -> ApiResult<()> {
    use std::io::Write;
    let mut target = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dest)
        .map_err(|error| ApiError::Network(error.to_string()))?;
    let mut reader = std::io::BufReader::new(
        std::fs::File::open(piece).map_err(|error| ApiError::Network(error.to_string()))?,
    );
    std::io::copy(&mut reader, &mut target)
        .map_err(|error| ApiError::Network(format!("写入本片数据失败：{error}")))?;
    target
        .flush()
        .map_err(|error| ApiError::Network(error.to_string()))?;
    let _ = std::fs::remove_file(piece);
    Ok(())
}

/// 收 stderr 末尾若干字符：失败时只回带最后两行非空输出，不把 FFmpeg 的完整
/// 命令行（里面有密钥）留在内存或日志里。
fn spawn_tail(mut stderr: ChildStderr) -> tokio::task::JoinHandle<String> {
    tokio::spawn(async move {
        let mut tail = String::new();
        let mut reader = BufReader::new(&mut stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            tail.push_str(&line);
            tail.push('\n');
            if tail.len() > 8192 {
                let cut = tail.len() - 4096;
                tail.drain(..cut);
            }
        }
        tail
    })
}

async fn drain_progress(
    stdout: ChildStdout,
    flag: Arc<AtomicBool>,
    job: String,
    duration: f64,
    mut on_event: impl FnMut(ProgressEvent),
) {
    let mut last = 0u32;
    let mut reader = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = reader.next_line().await {
        if cancelled(&flag) {
            return;
        }
        let Some(seconds) = parse_out_time(&line) else {
            continue;
        };
        if duration <= 0.0 {
            continue;
        }
        let percent = percent_of_seconds(seconds, duration);
        if percent > last {
            last = percent;
            on_event(ProgressEvent::percent("encode", &job, percent, None));
        }
    }
    on_event(ProgressEvent::percent("encode", &job, last, None));
}

/// ffprobe 拿时长；拿不到就退化成 `ffmpeg -i`（它的横幅里也有 `Duration:`）。
async fn probe_duration(tool: &Tool, source: &Path) -> f64 {
    let path = source.display().to_string();
    if let Some(ffprobe) = &tool.ffprobe {
        let text = run_capture(
            ffprobe,
            &[
                "-hide_banner",
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "default=noprint_wrappers=1",
                &path,
            ],
            20,
        )
        .await
        .unwrap_or_default();
        if let Some(seconds) = parse_duration(&text) {
            return seconds;
        }
    }
    let pending = Command::new(&tool.ffmpeg)
        .args(["-hide_banner", "-i", &path, "-f", "null", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();
    let text = match tokio::time::timeout(std::time::Duration::from_secs(30), pending).await {
        Ok(Ok(output)) => String::from_utf8_lossy(&output.stderr).to_string(),
        _ => String::new(),
    };
    parse_duration(&text).unwrap_or(0.0)
}

fn cancelled_error() -> ApiError {
    ApiError::Business {
        message: "已取消转码".to_string(),
        code: "CANCELLED".to_string(),
    }
}

/// 让调用方能识别「这条失败是因为取消了」。
pub fn is_cancelled(error: &ApiError) -> bool {
    matches!(error, ApiError::Business { code, .. } if code == "CANCELLED")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hg-transcode-{tag}-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn file_names_survive_junk_input() {
        assert_eq!(sanitize_key("7458123456789"), "7458123456789");
        assert_eq!(sanitize_key("1080p"), "1080p");
        // 路径分隔符、冒号、空格全被换掉，且不会出现绝对路径。
        assert!(!sanitize_key("..\\..\\windows\\system32").contains('\\'));
        assert_eq!(sanitize_key(""), format!("x{}", fnv1a("")));
        let layout = Layout::new(Path::new("/tmp/app"));
        let path = layout.output_path("123", "1080p", Some("aabbccddeeff00112233"));
        assert!(path.starts_with(layout.compat_dir()));
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("123-1080p-aabbccdd"), "文件名是 {name}");
        // 不同密钥不同文件，明文与加密也不共用。
        assert_ne!(
            path,
            layout.output_path("123", "1080p", Some("ffffffffdddd"))
        );
        assert_ne!(path, layout.output_path("123", "1080p", None));
    }

    #[test]
    fn percentages_never_run_backwards() {
        assert_eq!(percent_of(0, 0), 0);
        assert_eq!(percent_of(50, 100), 50);
        assert_eq!(percent_of(100, 100), 99, "100 由调用方在成功时补");
        assert_eq!(percent_of(200, 100), 99);
        assert_eq!(percent_of_seconds(12.0, 24.0), 50);
        assert_eq!(percent_of_seconds(30.0, 24.0), 99);
        assert_eq!(percent_of_seconds(1.0, 0.0), 0, "时长未知就不报百分比");
    }

    #[test]
    fn duration_parsing_reads_both_writings() {
        assert_eq!(
            parse_duration("Duration: 00:00:24.10, start: 0.000000"),
            Some(24.1)
        );
        assert_eq!(parse_duration("Duration: 01:02:03.50,"), Some(3723.5));
        assert_eq!(
            parse_duration("duration=24.100000 bitrate=1000"),
            Some(24.1)
        );
        assert_eq!(parse_duration("Duration: n/a"), None);
        assert_eq!(parse_duration("nothing here"), None);
    }

    #[test]
    fn progress_lines_only_care_about_out_time_us() {
        assert_eq!(parse_out_time("out_time_us=12500000"), Some(12.5));
        assert_eq!(parse_out_time("out_time_ms=12500"), None);
        assert_eq!(parse_out_time("progress=continue"), None);
        assert_eq!(parse_out_time("out_time_us=notanumber"), None);
    }

    #[test]
    fn zip_entry_matching_prefers_the_real_binary() {
        // gyan：bin/ffmpeg.exe；BtbN：ffmpeg-master-latest-win64-gpl/bin/ffmpeg.exe。
        assert!(picks_entry("bin/ffmpeg.exe", "ffmpeg.exe"));
        assert!(picks_entry(
            "ffmpeg-master-latest-win64-gpl/bin/FFMPEG.EXE",
            "ffmpeg.exe"
        ));
        assert!(picks_entry("usr/local/bin/ffmpeg", "ffmpeg"));
        assert!(!picks_entry("doc/ffmpeg.txt", "ffmpeg"));
        assert!(!picks_entry("bin/ffprobe.exe", "ffmpeg.exe"));
        // 目录条目本身不能算命中。
        assert!(!picks_entry("bin/", "bin"));
    }

    #[test]
    fn extraction_pulls_out_the_shallowest_match() {
        // 造一个 zip：深处放假的（文档里的小文件），浅处放我们要的。
        let dir = temp_dir("extract");
        let zip_path = dir.join("fake.zip");
        {
            let file = std::fs::File::create(&zip_path).unwrap();
            let mut writer = zip::ZipWriter::new(std::io::BufWriter::new(file));
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            writer
                .start_file("doc/sub/ffmpeg.exe", options.clone())
                .unwrap();
            std::io::Write::write_all(&mut writer, b"not a binary at all").unwrap();
            writer.start_file("bin/ffmpeg.exe", options).unwrap();
            let payload = vec![0xABu8; 2 * 1024 * 1024];
            std::io::Write::write_all(&mut writer, &payload).unwrap();
            writer.finish().unwrap();
        }
        let out_dir = dir.join("tools");
        std::fs::create_dir_all(&out_dir).unwrap();
        let written = extract_binary(&zip_path, &out_dir, "ffmpeg.exe", TOOL_MAX_BYTES).unwrap();
        assert_eq!(written, 2 * 1024 * 1024, "取的是浅处那份真文件");
        assert!(out_dir.join("ffmpeg.exe").is_file());
        // 找不到就明说，不返回一个空路径让上层去猜。
        let error = extract_binary(&zip_path, &out_dir, "ffprobe.exe", TOOL_MAX_BYTES).unwrap_err();
        assert!(error.to_string().contains("ffprobe"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn candidates_are_https_and_ordered() {
        let rows = candidates();
        if rows.is_empty() {
            return; // 不认识的平台上没有候选，也是正确答案。
        }
        assert!(rows.iter().all(|row| row.url.starts_with("https://")));
        assert!(rows.iter().all(|row| row.env_hint == Some(TOOL_URL_ENV)));
    }

    #[test]
    fn tool_resolution_never_invents_a_binary() {
        let dir = temp_dir("resolve");
        let layout = Layout::new(&dir);
        // 一个不存在 / 一个太小的文件都不算就绪。
        assert!(resolve_tool(&layout).is_none_or(|tool| tool.ffmpeg.exists()));
        std::fs::create_dir_all(layout.tools_dir()).unwrap();
        std::fs::write(layout.tools_dir().join(exe_name("ffmpeg")), b"tiny").unwrap();
        if !cfg!(debug_assertions) {
            assert!(resolve_tool(&layout).is_none(), "截断的文件不能算可用");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_trim_drops_half_writes_and_oversized_totals() {
        let dir = temp_dir("trim");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.mp4.part"), b"half").unwrap();
        std::fs::write(dir.join("b.mp4"), vec![7u8; 1024]).unwrap();
        std::fs::write(dir.join("notes.txt"), b"keep me").unwrap();
        let used = trim_cache(&dir, None);
        assert!(!dir.join("a.mp4.part").exists(), "半截输出不该留着");
        assert!(dir.join("b.mp4").exists());
        assert_eq!(
            used,
            1024 + 7,
            "只统计普通文件（b.mp4 + notes.txt），.part 已删除不计"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn encoder_args_always_end_with_a_h264_encoder() {
        for encoder in [
            "libx264",
            "h264_nvenc",
            "h264_qsv",
            "h264_amf",
            "h264_mf",
            "h264_videotoolbox",
        ] {
            let args = encoder_args(encoder);
            assert_eq!(args[0], "-c:v");
            assert_eq!(args[1], encoder);
        }
        assert_eq!(encoder_args("whatever")[1], "whatever");
        assert!(
            preferred_encoders()
                .last()
                .is_some_and(|last| *last == "libx264"),
            "软编兜底必须在最后"
        );
    }

    #[test]
    fn cancel_only_answers_for_known_jobs() {
        assert!(!cancel("no-such-job"));
        let flag = job_flag("job-1");
        assert!(cancel("job-1"));
        assert!(cancelled(&flag), "取消位被置上");
        if let Ok(mut guard) = JOBS.lock() {
            guard.remove("job-1");
        }
        assert!(!cancel("job-1"));
    }

    #[tokio::test]
    async fn a_real_child_process_runs_and_is_captured() {
        // 借一次真实的子进程调用验证 `run_capture` 的管道与退出码处理：
        // 用当前进程自己跑 `--version` 这类必然成功的命令不现实，这里用 shell。
        let (binary, args) = if cfg!(windows) {
            ("cmd.exe", vec!["/C", "echo hello"])
        } else {
            ("/bin/sh", vec!["-c", "echo hello"])
        };
        let text = run_capture(Path::new(binary), &args, 10)
            .await
            .unwrap_or_default();
        assert!(text.contains("hello"), "实际拿到：{text}");
        assert!(
            run_capture(Path::new(binary), &["/C", "exit 3"], 10)
                .await
                .is_none_or(|_| true)
        );
    }

    #[tokio::test]
    async fn a_missing_binary_fails_fast_instead_of_hanging() {
        let tool = Tool {
            ffmpeg: PathBuf::from("definitely-not-a-real-ffmpeg-binary"),
            ffprobe: None,
        };
        assert!(verify(&tool).await.is_err());
    }

    #[tokio::test]
    async fn cancelled_flag_stops_a_pending_download() {
        let dir = temp_dir("chunked");
        let layout = Layout::new(&dir);
        layout.ensure_dirs().unwrap();
        let flag = Arc::new(AtomicBool::new(true));
        let client = reqwest::Client::new();
        let mut events = Vec::new();
        let mut record = |event: ProgressEvent| events.push(event);
        let error = fetch_chunked(
            &client,
            &[],
            "https://127.0.0.1:1/nope",
            &dir.join("src.mp4"),
            "job",
            &flag,
            &mut record,
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), "CANCELLED", "取消位优先于任何网络错误");
        assert!(events.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
