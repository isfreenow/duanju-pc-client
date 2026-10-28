//! 下载到电脑：把一部剧的分集排队取回本地，存成电脑上能直接打开的 MP4。
//!
//! 与播放共用同一条边界：片源地址与内容密钥只在 Rust 侧出现，前端只交回
//! 「哪个 vid / 存到哪个目录 / 进度百分之几」。落盘的成品是明文 MP4，不再有
//! 任何密钥或短期凭据混在里面。
//!
//! 三步链路，全程可暂停 / 继续 / 取消：
//! * 解析一条新鲜地址（CDN 签发是短期的，续传/重试都要重新解一次）；
//! * 分块 Range 下载（[`crate::api::download_stream`]），块与块之间检查取消/暂停
//!   位，半截字节留在工作目录的 `.part` 里，下次自动从断点续；
//! * 明文流直接改名落目标目录；CENC 加密的流用按需下载的 FFmpeg 做
//!   `-decryption_key … -c copy` 解成普通 MP4（只解密封装、不重编码，快且不掉画质）。
//!
//! 任务表落 `downloads.json`（应用数据目录，与历史/收藏同构），重启后未完成的
//! 任务回到「已暂停」，用户点继续即从断点续传。

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::process::Command;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::api::app_video::MediaVariant;
use crate::api::{self, ApiError, ApiResult, Deadline, DownloadFlow};
use crate::transcode::{self, Layout};

/// 同时在跑几路下载。两路足够填满家庭带宽，又不至于把来源打病、把磁盘写乱。
const MAX_CONCURRENT: usize = 2;
/// 单集下载的重试上限：地址过期、连接抖动、中途超时，都靠续传重试自愈；超了才判失败。
const MAX_RETRY: u32 = 6;
/// 单条连接的字节硬上限（短剧一集几十 MB，留足余量，与转码侧同一道保险）。
const MEDIA_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// 成品小于这个数判为半截，不算下载成功。
const MIN_OUTPUT_BYTES: u64 = 100 * 1024;

/// 任务状态。字符串常量是前后端的共享契约（`client.ts` 里逐一对应），
/// 任一侧改名都要同步另一侧——枚举名与序列化值不一致会让门控静默失效。
pub mod stage {
    pub const QUEUED: &str = "queued";
    pub const DOWNLOADING: &str = "downloading";
    pub const REMUXING: &str = "remuxing";
    pub const PAUSED: &str = "paused";
    pub const DONE: &str = "done";
    pub const FAILED: &str = "failed";
    pub const CANCELLED: &str = "cancelled";
}

/// 一条下载任务。没有任何地址与密钥：`destPath` 是本地成品路径，其余是渲染与
/// 断点续传所需的元数据。`bytesDone` / `bytesTotal` 用来画进度条（总量未知时后者为 `None`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadTask {
    /// 任务号 = 这一集的 vid：同 vid 重复入队会被合并。
    pub id: String,
    pub series_id: String,
    pub series_title: String,
    pub cover: String,
    #[serde(default)]
    pub category: String,
    pub vid_index: u32,
    /// 请求的清晰度（空串表示「默认最高」）；真正解析时按它挑流。
    #[serde(default)]
    pub quality: String,
    pub dest_path: String,
    pub stage: String,
    #[serde(default)]
    pub bytes_done: u64,
    #[serde(default)]
    pub bytes_total: Option<u64>,
    #[serde(default)]
    pub error: Option<String>,
    pub updated_at: i64,
}

/// 一条在跑/待跑任务的取消与暂停位。下载循环在每块之间读它。
struct Control {
    cancel: AtomicBool,
    pause: AtomicBool,
}

impl Control {
    fn new() -> Self {
        Control {
            cancel: AtomicBool::new(false),
            pause: AtomicBool::new(false),
        }
    }

    fn flow(&self) -> DownloadFlow {
        if self.cancel.load(Ordering::SeqCst) {
            DownloadFlow::Cancel
        } else if self.pause.load(Ordering::SeqCst) {
            DownloadFlow::Pause
        } else {
            DownloadFlow::Continue
        }
    }
}

/// 共享的可变状态：任务表、控制位、并发闸、以及 worker 需要的 client/layout。
/// `DownloadManager` 只是它的一层 `Arc` 包装，好让 spawn 出去的 worker 能持有一份。
struct Shared {
    path: PathBuf,
    tasks: Mutex<HashMap<String, DownloadTask>>,
    /// 排队中的任务号，按入队顺序（集号升序）等待派发。
    pending: Mutex<VecDeque<String>>,
    controls: Mutex<HashMap<String, Arc<Control>>>,
    /// `Arc` 包的信号量：worker 要持有着 `OwnedSemaphorePermit` 跑完整集，
    /// 名额跟着 permit 回还，不能借在栈上。
    sem: Arc<Semaphore>,
    client: reqwest::Client,
    layout: Layout,
}

/// 把任务从排队队列里摘掉（暂停/取消/已在队时重新入队都用它），锁中毒就当作没摘到。
fn dequeue(shared: &Arc<Shared>, id: &str) {
    if let Ok(mut queue) = shared.pending.lock() {
        if let Some(at) = queue.iter().position(|row| row == id) {
            queue.remove(at);
        }
    }
}

/// 把任务排到队尾。锁中毒时静默放弃：宁可这一条靠「继续」按钮重新入队拉起。
fn enqueue(shared: &Arc<Shared>, id: &str) {
    if let Ok(mut queue) = shared.pending.lock() {
        queue.push_back(id.to_string());
    }
}

#[derive(Clone)]
pub struct DownloadManager(Arc<Shared>);

impl DownloadManager {
    /// 打开（或初始化）下载任务表。上一进程里正在下载/排队/转封装的任务一律降级
    /// 为「已暂停」：进程死了连接就断了，但 `.part` 还在，用户点继续即从断点续。
    pub fn open(dir: &Path, client: reqwest::Client, layout: Layout) -> Self {
        let path = dir.join("downloads.json");
        let loaded = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<Vec<DownloadTask>>(&text).unwrap_or_else(|error| {
                eprintln!("下载表无法解析，按空表继续：{error}");
                Vec::new()
            }),
            Err(_) => Vec::new(),
        };
        let tasks = loaded
            .into_iter()
            .filter(|task| !task.id.is_empty())
            .map(|mut task| {
                if matches!(
                    task.stage.as_str(),
                    stage::QUEUED | stage::DOWNLOADING | stage::REMUXING
                ) {
                    task.stage = stage::PAUSED.to_string();
                }
                (task.id.clone(), task)
            })
            .collect::<HashMap<_, _>>();
        // 重启不该记得乱序：排队中的任务按集号升序重新入队。
        let mut by_index = tasks
            .values()
            .filter(|task| task.stage == stage::QUEUED)
            .collect::<Vec<_>>();
        by_index.sort_by(|a, b| a.vid_index.cmp(&b.vid_index).then(a.id.cmp(&b.id)));
        let pending = by_index
            .into_iter()
            .map(|task| task.id.clone())
            .collect::<VecDeque<_>>();
        DownloadManager(Arc::new(Shared {
            path,
            tasks: Mutex::new(tasks),
            pending: Mutex::new(pending),
            controls: Mutex::new(HashMap::new()),
            sem: Arc::new(Semaphore::new(MAX_CONCURRENT)),
            client,
            layout,
        }))
    }

    /// 当前任务一览，最新改动在前；前端与「我的」页据此渲染。
    pub fn snapshot(&self) -> Vec<DownloadTask> {
        let Ok(guard) = self.0.tasks.lock() else {
            return Vec::new();
        };
        let mut rows = guard.values().cloned().collect::<Vec<_>>();
        rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        rows
    }

    /// 把一批分集排进下载队列并起 worker。已经在跑或已完成的任务不会被重复拉起。
    /// 入队按前端传来的集号升序，派发时也从队首取——下载必须一集一集顺序跑。
    pub fn begin(
        &self,
        app: &AppHandle,
        mut metas: Vec<DownloadTask>,
        dest_dir: &str,
    ) -> Vec<DownloadTask> {
        metas.sort_by(|a, b| a.vid_index.cmp(&b.vid_index).then(a.id.cmp(&b.id)));
        // 入队即把剧集文件夹建好（一批分集同属一部剧、同一目录，只建一次）：
        // 这样哪怕还没真正下出文件，界面里的「打开所在文件夹」也能定位到它。
        if let Some(first) = metas.first() {
            let _ = std::fs::create_dir_all(
                Path::new(dest_dir).join(sanitize_title(&first.series_title)),
            );
        }
        let mut added: Vec<String> = Vec::new();
        {
            let Ok(mut guard) = self.0.tasks.lock() else {
                return self.snapshot();
            };
            for mut task in metas {
                task.dest_path = join_dest(dest_dir, &task.series_title, task.vid_index);
                task.updated_at = api::now_millis();
                match guard.get(&task.id) {
                    // 完成/在跑/排队/暂停中的同一条不动；失败或已取消的重新排。
                    Some(existing)
                        if matches!(
                            existing.stage.as_str(),
                            stage::DONE
                                | stage::DOWNLOADING
                                | stage::REMUXING
                                | stage::QUEUED
                                | stage::PAUSED
                        ) =>
                    {
                        continue;
                    }
                    _ => {
                        task.stage = stage::QUEUED.to_string();
                        task.error = None;
                        task.bytes_done = 0;
                        added.push(task.id.clone());
                        guard.insert(task.id.clone(), task);
                    }
                }
            }
        }
        for id in &added {
            dequeue(&self.0, id);
            enqueue(&self.0, id);
        }
        self.persist();
        pump(&self.0, app);
        self.snapshot()
    }

    /// 继续一条已暂停（或失败/已取消后想重来）的任务；重新排进队列，由派发器按序拉起。
    pub fn resume(&self, app: &AppHandle, id: &str) -> bool {
        let resuming = {
            let Ok(mut guard) = self.0.tasks.lock() else {
                return false;
            };
            match guard.get_mut(id) {
                Some(task)
                    if matches!(
                        task.stage.as_str(),
                        stage::PAUSED | stage::FAILED | stage::CANCELLED
                    ) =>
                {
                    task.stage = stage::QUEUED.to_string();
                    task.error = None;
                    task.updated_at = api::now_millis();
                    true
                }
                _ => false,
            }
        };
        if !resuming {
            return false;
        }
        // 清掉上一条遗留的暂停位，否则新 worker 一进来就被判暂停。
        if let Ok(mut controls) = self.0.controls.lock() {
            controls.remove(id);
        }
        dequeue(&self.0, id);
        enqueue(&self.0, id);
        self.persist();
        pump(&self.0, app);
        true
    }

    /// 暂停一条在跑或排队中的任务：当场把状态落成「已暂停」（按钮立刻翻成继续，
    /// 不用等下一个数据块——连接卡住时块可能要等很久），同时置暂停位让 worker 在
    /// 最近的边界停手，已下字节留在 `.part` 里等续传。解密收尾中的任务不接暂停：
    /// 那一步很快，只置位让它跑完直接出成品。
    pub fn pause(&self, id: &str) -> bool {
        let control = self.0.controls.lock().ok().and_then(|m| m.get(id).cloned());
        if let Some(control) = control {
            // 先置位再翻状态：worker 那边任何进度的写回都会看到暂停位，
            // 不会把「已暂停」覆回「下载中」。
            control.pause.store(true, Ordering::SeqCst);
            let swapped = {
                let Ok(mut guard) = self.0.tasks.lock() else {
                    return false;
                };
                match guard.get_mut(id) {
                    Some(task)
                        if matches!(task.stage.as_str(), stage::QUEUED | stage::DOWNLOADING) =>
                    {
                        task.stage = stage::PAUSED.to_string();
                        task.updated_at = api::now_millis();
                        true
                    }
                    _ => false,
                }
            };
            if !swapped {
                return false;
            }
            dequeue(&self.0, id);
            self.persist();
            return true;
        }
        // 没有活着的 worker：要么还在排队（直接落暂停、从派发队列摘掉），要么已经是
        // 暂停态；收尾中（remuxing）的只置位拦不住，不假装已暂停。
        let flipped = {
            let Ok(mut guard) = self.0.tasks.lock() else {
                return false;
            };
            match guard.get_mut(id) {
                Some(task) if task.stage == stage::QUEUED => {
                    task.stage = stage::PAUSED.to_string();
                    task.updated_at = api::now_millis();
                    true
                }
                Some(task) => task.stage == stage::PAUSED,
                None => false,
            }
        };
        if !flipped {
            return false;
        }
        dequeue(&self.0, id);
        self.persist();
        true
    }

    /// 取消一条任务：置取消位让 worker 从下载/转封装里退出，并删掉它的 `.part`。
    pub fn cancel(&self, id: &str) -> bool {
        let existed = self.0.tasks.lock().ok().map(|guard| guard.contains_key(id));
        if existed != Some(true) {
            return false;
        }
        if let Some(control) = self.0.controls.lock().ok().and_then(|m| m.get(id).cloned()) {
            control.cancel.store(true, Ordering::SeqCst);
        }
        let part = self.part_path(id);
        {
            let Ok(mut guard) = self.0.tasks.lock() else {
                return false;
            };
            if let Some(task) = guard.get_mut(id) {
                task.stage = stage::CANCELLED.to_string();
                task.bytes_done = 0;
                task.error = None;
                task.updated_at = api::now_millis();
            }
        }
        dequeue(&self.0, id);
        let _ = std::fs::remove_file(&part);
        self.persist();
        true
    }

    /// 移除一条已结束（完成/失败/取消）的记录，可选删掉本地成品。
    pub fn forget(&self, id: &str, delete_file: bool) -> bool {
        let removed = {
            let Ok(mut guard) = self.0.tasks.lock() else {
                return false;
            };
            match guard.get(id) {
                Some(task)
                    if matches!(
                        task.stage.as_str(),
                        stage::DONE | stage::FAILED | stage::CANCELLED
                    ) =>
                {
                    guard.remove(id)
                }
                _ => None,
            }
        };
        let Some(task) = removed else {
            return false;
        };
        if delete_file && !task.dest_path.is_empty() {
            let _ = std::fs::remove_file(&task.dest_path);
        }
        let _ = std::fs::remove_file(self.part_path(&task.id));
        self.persist();
        true
    }

    fn part_path(&self, id: &str) -> PathBuf {
        self.0
            .layout
            .download_dir()
            .join(format!("{}.part", sanitize(id)))
    }

    fn persist(&self) {
        let rows = self.snapshot();
        let text = match serde_json::to_string(&rows) {
            Ok(text) => text,
            Err(_) => return,
        };
        let temp = self.0.path.with_extension("json.tmp");
        if std::fs::write(&temp, text).is_err() {
            return;
        }
        let _ = std::fs::rename(&temp, &self.0.path);
    }
}

/// 把排队队列里能跑的任务派给空闲名额：从队首按集号顺序取，取满并发闸为止。
/// 旧实现是每条队列任务各自 spawn 后一起挂在一个信号量上抢名额，而 tokio
/// 的信号量不保证先到先得——用户看到的是「不按集数顺序、像从中间某集开始」；
/// 现在只有一个派发口，名额永远交给队首。
fn pump(shared: &Arc<Shared>, app: &AppHandle) {
    loop {
        let Ok(permit) = Arc::clone(&shared.sem).try_acquire_owned() else {
            return;
        };
        let next = match shared.pending.lock() {
            Ok(mut queue) => queue.pop_front(),
            Err(_) => return,
        };
        let Some(id) = next else {
            return; // 名额还回一个，队列空了。
        };
        tauri::async_runtime::spawn(worker(shared.clone(), app.clone(), id, permit));
    }
}

/// 一路 worker：持有名额跑完这一集；退出时归还名额并继续派发后面的排队项。
async fn worker(shared: Arc<Shared>, app: AppHandle, id: String, permit: OwnedSemaphorePermit) {
    run_one(&shared, &app, &id).await;
    drop(permit);
    pump(&shared, &app);
}

async fn run_one(shared: &Arc<Shared>, app: &AppHandle, id: &str) {
    let control = Arc::new(Control::new());
    if let Ok(mut controls) = shared.controls.lock() {
        controls.insert(id.to_string(), control.clone());
    }
    let meta = shared
        .tasks
        .lock()
        .ok()
        .and_then(|guard| guard.get(id).cloned());
    let Some(task) = meta else {
        finish_control(shared, id, &control);
        return;
    };
    // 排队期间被取消或被改成暂停（信号量等待时状态可能被别的操作动过），不再起跑。
    if !matches!(task.stage.as_str(), stage::QUEUED) {
        finish_control(shared, id, &control);
        return;
    }

    let part = shared
        .layout
        .download_dir()
        .join(format!("{}.part", sanitize(id)));
    let outcome = drive_download(shared, app, id, &task, &control, &part).await;
    let outcome = match outcome {
        Ok(Phase::Paused) => {
            // 状态已由 `pause()` 当场落定；带着自己的控制位写，若用户已经点了「继续」
            // 就自动作废，不会被这个退场中的旧 worker 把新状态覆回「已暂停」。
            set_stage(shared, app, id, stage::PAUSED, None, Some(&control));
            finish_control(shared, id, &control);
            return;
        }
        Ok(Phase::Cancelled) => {
            let _ = std::fs::remove_file(&part);
            finish_control(shared, id, &control);
            return;
        }
        Ok(Phase::Done) => {
            // drive_download 只会回 Downloaded/Paused/Cancelled，Done 属防御性分支。
            finish_control(shared, id, &control);
            return;
        }
        Ok(Phase::Downloaded(variant)) => {
            // 最后一块落盘到这里之间按了暂停/取消：`.part` 已全量在盘上，原地留着，
            // 这一路直接停手——下次「继续」从断点接着走（数据其实已齐，只会快速收尾）。
            match stop_signal(&control) {
                Some(stop) => {
                    set_stage(
                        shared,
                        app,
                        id,
                        stage_from_stop(&stop),
                        None,
                        Some(&control),
                    );
                    finish_control(shared, id, &control);
                    return;
                }
                None => phase_remux(shared, app, &task, &control, &part, &variant).await,
            }
        }
        Err(error) => {
            set_stage(
                shared,
                app,
                id,
                stage::FAILED,
                Some(error_message(&error)),
                None,
            );
            finish_control(shared, id, &control);
            return;
        }
    };
    match outcome {
        Ok(Phase::Done) | Ok(Phase::Cancelled) => {
            let _ = std::fs::remove_file(&part);
            finish_control(shared, id, &control);
        }
        // 收尾被打断（暂停/取消）：半成品字节不动，等「继续」再收尾一次。
        Ok(Phase::Paused) => {
            set_stage(shared, app, id, stage::PAUSED, None, Some(&control));
            finish_control(shared, id, &control);
        }
        Err(error) => {
            // 解密途中被取消：`.part`（全量字节）已由 cancel() 清掉，这里不补删。
            let stopped = error_code(&error) == "CANCEL";
            if !stopped {
                let _ = std::fs::remove_file(&part);
            }
            set_stage(
                shared,
                app,
                id,
                if stopped {
                    stage::CANCELLED
                } else {
                    stage::FAILED
                },
                if stopped {
                    None
                } else {
                    Some(error_message(&error))
                },
                None,
            );
            finish_control(shared, id, &control);
        }
        // 收尾阶段正常只会回 Done，这里保底不再动状态。
        Ok(_) => {
            finish_control(shared, id, &control);
        }
    }
}

/// 控制位上是否已请求停止；`None` 表示照常往下跑。
fn stop_signal(control: &Control) -> Option<DownloadFlow> {
    match control.flow() {
        DownloadFlow::Continue => None,
        other => Some(other),
    }
}

/// 停止信号对应的落定状态：暂停位落「已暂停」，取消位落「已取消」。
fn stage_from_stop(stop: &DownloadFlow) -> &'static str {
    match stop {
        DownloadFlow::Cancel => stage::CANCELLED,
        _ => stage::PAUSED,
    }
}

/// 下载/收尾阶段的结果。`Downloaded` 带上真正拿去下载的那条流（含密钥），
/// 交给 [`phase_remux`] 判断要不要解密。
enum Phase {
    Paused,
    Cancelled,
    Downloaded(MediaVariant),
    Done,
}

/// 解析一条新鲜地址 → 分块 Range 续传下载。地址过期/连接抖动都重解析重试。
async fn drive_download(
    shared: &Arc<Shared>,
    app: &AppHandle,
    id: &str,
    task: &DownloadTask,
    control: &Arc<Control>,
    part: &Path,
) -> ApiResult<Phase> {
    for _attempt in 0..MAX_RETRY {
        if control.cancel.load(Ordering::SeqCst) {
            return Ok(Phase::Cancelled);
        }
        if control.pause.load(Ordering::SeqCst) {
            return Ok(Phase::Paused);
        }
        // 每次都重新解析：CDN 地址是短期凭据，续传/重试都可能遇到旧的已失效。
        let variant = resolve_variant(&shared.client, &task.series_id, id, &task.quality)
            .await
            .map_err(|error| ApiError::Network(error.message))?;
        // 暂停/取消是在下一个边界停手，不再把状态写回「下载中」：否则前端刚把
        // 「已暂停」画上去，会被这一句覆回「下载中」，按钮看起来没反应。
        if control.flow() == DownloadFlow::Continue {
            set_stage(shared, app, id, stage::DOWNLOADING, None, Some(control));
        }
        let headers = variant_headers(&variant);
        let header_refs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        let progress = ProgressEmitter {
            shared: shared.clone(),
            app: app.clone(),
            id: id.to_string(),
            control: control.clone(),
            last_emit: 0,
        };
        match api::download_stream(
            &shared.client,
            &variant.url,
            &header_refs,
            Deadline::after(600),
            MEDIA_MAX_BYTES,
            part,
            progress.into_callback(),
        )
        .await
        {
            Ok((_done, _total, true)) => return Ok(Phase::Downloaded(variant)),
            // download_stream 只在「主动暂停」时回 `completed=false`，半截字节已留在 part。
            Ok((_, _, false)) => return Ok(Phase::Paused),
            Err(error) => {
                if error_code(&error) == "CANCEL" {
                    return Ok(Phase::Cancelled);
                }
                // 其它错误（超时/网络/HTTP）：留给下一次循环重解析续传，直到用尽重试。
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            }
        }
    }
    Err(ApiError::Network("多次重试仍未下完，稍后再试".to_string()))
}

/// 下载完成后的收尾：明文流直接改名到目标；加密流用 FFmpeg 解密封装成普通 MP4。
async fn phase_remux(
    shared: &Arc<Shared>,
    app: &AppHandle,
    task: &DownloadTask,
    control: &Arc<Control>,
    part: &Path,
    variant: &MediaVariant,
) -> ApiResult<Phase> {
    if let Some(stop) = stop_signal(control) {
        // 收尾（改名/解密）很快，且一跑就出成品：按暂停只把已全量下完的字节留在
        // `.part` 里，等「继续」再收尾一次；按取消则照旧清掉半成品。
        if stop == DownloadFlow::Pause {
            return Ok(Phase::Paused);
        }
        return Ok(Phase::Cancelled);
    }
    let dest = Path::new(&task.dest_path);
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // 已经下过、成品还在且够大：判为完成，不重跑。
    if let Ok(meta) = std::fs::metadata(dest) {
        if meta.len() >= MIN_OUTPUT_BYTES {
            set_progress(
                shared,
                app,
                &task.id,
                meta.len(),
                Some(meta.len()),
                stage::DONE,
                Some(control),
            );
            return Ok(Phase::Done);
        }
    }
    let outcome = match variant.content_key.as_deref().filter(|key| !key.is_empty()) {
        // 明文流：`.part` 就是成品，改名即可。
        None => {
            let _ = std::fs::remove_file(dest);
            std::fs::rename(part, dest).map_err(|error| ApiError::Network(error.to_string()))?;
            Phase::Done
        }
        // CENC 加密流：交给按需下载的 FFmpeg `-decryption_key … -c copy` 解封成普通 MP4。
        Some(key) => {
            set_stage(shared, app, &task.id, stage::REMUXING, None, Some(control));
            match remux_to_plain(
                &shared.client,
                &shared.layout,
                id_of(task),
                control,
                part,
                dest,
                key,
            )
            .await
            {
                Ok(phase) => phase,
                Err(error) => return Err(error),
            }
        }
    };
    // 解密途中按了暂停/取消：不报完成，把结果原样交回 `run_one` 落定；
    // `.part`（全量字节）不删，「继续」会直接重跑这一步出成品。
    if !matches!(outcome, Phase::Done) {
        return Ok(outcome);
    }
    let size = std::fs::metadata(dest).map(|meta| meta.len()).unwrap_or(0);
    if size < MIN_OUTPUT_BYTES {
        let _ = std::fs::remove_file(dest);
        return Err(ApiError::Business {
            code: "DOWNLOAD".to_string(),
            message: "下载产物过小，判定为失败".to_string(),
        });
    }
    set_progress(
        shared,
        app,
        &task.id,
        size,
        Some(size),
        stage::DONE,
        Some(control),
    );
    Ok(Phase::Done)
}

fn id_of(task: &DownloadTask) -> &str {
    &task.id
}

/// 用 `-decryption_key … -c copy` 把加密流解成明文 MP4；全程可在块外暂停/取消（终止子进程）。
async fn remux_to_plain(
    client: &reqwest::Client,
    layout: &Layout,
    id: &str,
    control: &Arc<Control>,
    source: &Path,
    dest: &Path,
    key: &str,
) -> ApiResult<Phase> {
    let tool = transcode::prepare(client, layout, id, |_| {})
        .await
        .map_err(|error| ApiError::Network(error.to_string()))?;
    let temp = dest.with_extension("mp4.part");
    let _ = std::fs::remove_file(&temp);
    let child = Command::new(&tool.ffmpeg)
        .args(["-y", "-hide_banner", "-nostdin", "-loglevel", "error"])
        .arg("-decryption_key")
        .arg(key)
        .arg("-i")
        .arg(source)
        .args(["-c", "copy", "-movflags", "+faststart", "-f", "mp4"])
        .arg(&temp)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| ApiError::Network(format!("无法启动解密工具：{error}")))?;
    // 停止轮询：置了暂停/取消位就把子进程杀掉（解密很快，但用户不该等它跑完才看到反馈）。
    let watcher = {
        let flag = control.clone();
        let shared = Arc::new(tokio::sync::Mutex::new(child));
        // 借用 `child` 前先移交所有权给看门狗任务，等待在别处做。
        let wait_shared = shared.clone();
        let handle = tokio::spawn(async move {
            loop {
                if flag.flow() != DownloadFlow::Continue {
                    let _ = wait_shared.lock().await.start_kill();
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        });
        (shared, handle)
    };
    let status = {
        let mut guard = watcher.0.lock().await;
        guard.wait().await
    }
    .map_err(|error| ApiError::Network(format!("解密进程异常：{error}")))?;
    watcher.1.abort();
    // 只看取消位：`Cancel` 优先级高于 `Pause`，不能被暂停分支吃掉。
    if control.cancel.load(Ordering::SeqCst) {
        let _ = std::fs::remove_file(&temp);
        return Ok(Phase::Cancelled);
    }
    if control.pause.load(Ordering::SeqCst) {
        let _ = std::fs::remove_file(&temp);
        return Ok(Phase::Paused);
    }
    if !status.success() {
        let _ = std::fs::remove_file(&temp);
        return Err(ApiError::Business {
            code: "DOWNLOAD".to_string(),
            message: "解密失败，该片源可能需要系统解码组件".to_string(),
        });
    }
    let _ = std::fs::remove_file(dest);
    std::fs::rename(&temp, dest).map_err(|error| ApiError::Network(error.to_string()))?;
    Ok(Phase::Done)
}

/// 从解析出的一组流里挑一条用于下载：优先匹配 `quality`，其次第一条。
fn pick_variant(variants: Vec<MediaVariant>, quality: &str) -> Option<MediaVariant> {
    if !quality.is_empty() {
        if let Some(matched) = variants.iter().find(|stream| stream.quality == quality) {
            return Some(matched.clone());
        }
    }
    variants.into_iter().next()
}

/// 解析一条可用于下载/续传的流（带传输头与密钥，只在 Rust 侧流转）。
async fn resolve_variant(
    client: &reqwest::Client,
    series_id: &str,
    vid: &str,
    quality: &str,
) -> Result<MediaVariant, crate::commands::CommandError> {
    let (variants, _note) = crate::commands::assemble_streams(client, vid, series_id).await?;
    pick_variant(variants, quality).ok_or_else(|| crate::commands::CommandError {
        code: "NO_PLAYABLE_SOURCE".to_string(),
        hint: Some("换一集或稍后再试".to_string()),
        message: "本集没有可下载的片源".to_string(),
    })
}

fn variant_headers(variant: &MediaVariant) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    if !variant.user_agent.is_empty() {
        rows.push(("User-Agent".to_string(), variant.user_agent.clone()));
    }
    if let Some(referer) = &variant.referer {
        rows.push(("Referer".to_string(), referer.clone()));
    }
    rows
}

fn error_code(error: &ApiError) -> String {
    error.code()
}

fn error_message(error: &ApiError) -> String {
    error.to_string()
}

/// 文件名里只留安全字符：剧名是来源给的自由文本，不能带着 `\` `/` `:` 进路径。
fn sanitize(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.trim().chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        return "episode".to_string();
    }
    trimmed.chars().take(80).collect()
}

/// 剧名做目录名时保留中文等非 ASCII 字符（`sanitize` 会把它们压成 `-`，多个
/// 中文剧名会全塌成同一个 `episode` 而挤进同一文件夹），只把各文件系统非法的
/// 路径字符（`/ \ : * ? " < > |`、控制字符、首尾的点与空格）换成下划线。
fn sanitize_title(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.trim().chars() {
        if matches!(
            ch,
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0'
        ) || (ch as u32) < 0x20
        {
            out.push('_');
        } else {
            out.push(ch);
        }
    }
    let trimmed = out.trim().trim_matches('.').trim();
    if trimmed.is_empty() {
        return "series".to_string();
    }
    trimmed.chars().take(80).collect()
}

/// 先在所选目录下建一个以剧名命名的剧集文件夹，各集按集号放进其中。
fn join_dest(dest_dir: &str, series_title: &str, vid_index: u32) -> String {
    let folder = sanitize_title(series_title);
    let name = format!("{:03}.mp4", vid_index);
    Path::new(dest_dir)
        .join(folder)
        .join(name)
        .display()
        .to_string()
}

/// 更新任务状态并广播一帧进度。`error` 为空则沿用现有值。`control` 是这一路 worker
/// 自己的控制位：已请求暂停/取消时，除了终态以外的写回一律拦下——否则 worker 收尾
/// 时的一句「下载中」会把 `pause()` 刚画上去的「已暂停」覆回去，界面看起来按了没反应。
fn set_stage(
    shared: &Arc<Shared>,
    app: &AppHandle,
    id: &str,
    stage: &str,
    error: Option<String>,
    control: Option<&Control>,
) {
    let stopped = control.is_some_and(|control| control.flow() != DownloadFlow::Continue);
    let row = {
        let Ok(mut guard) = shared.tasks.lock() else {
            return;
        };
        let Some(task) = guard.get_mut(id) else {
            return;
        };
        if stopped && !matches!(stage, stage::DONE | stage::FAILED) {
            return;
        }
        if task.stage == stage && error.is_none() {
            // 状态没变（例如反复 downloading）不重复落盘，只让进度回调去刷新字节数。
            Some(task.clone())
        } else {
            task.stage = stage.to_string();
            task.updated_at = api::now_millis();
            if error.is_some() {
                task.error = error;
            }
            let row = task.clone();
            drop(guard);
            persist_ref(shared);
            Some(row)
        }
    };
    if let Some(row) = row {
        let _ = app.emit("download-progress", &row);
    }
}

fn set_progress(
    shared: &Arc<Shared>,
    app: &AppHandle,
    id: &str,
    done: u64,
    total: Option<u64>,
    stage: &str,
    control: Option<&Control>,
) {
    let stopped = control.is_some_and(|control| control.flow() != DownloadFlow::Continue);
    let row = {
        let Ok(mut guard) = shared.tasks.lock() else {
            return;
        };
        let Some(task) = guard.get_mut(id) else {
            return;
        };
        if stopped && !matches!(stage, stage::DONE | stage::FAILED) {
            return;
        }
        task.bytes_done = done;
        task.bytes_total = total;
        task.stage = stage.to_string();
        task.updated_at = api::now_millis();
        let row = task.clone();
        drop(guard);
        persist_ref(shared);
        row
    };
    let _ = app.emit("download-progress", &row);
}

fn persist_ref(shared: &Arc<Shared>) {
    let rows = {
        let Ok(guard) = shared.tasks.lock() else {
            return;
        };
        guard.values().cloned().collect::<Vec<_>>()
    };
    let mut rows = rows;
    rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    let text = match serde_json::to_string(&rows) {
        Ok(text) => text,
        Err(_) => return,
    };
    let temp = shared.path.with_extension("json.tmp");
    if std::fs::write(&temp, text).is_err() {
        return;
    }
    let _ = std::fs::rename(&temp, &shared.path);
}

/// 收掉这一路 worker 的控制位。只摘自己那一份：若「继续」已经拉起新 worker 并登记了
/// 新的控制位，旧的退出时不能把它抢走，否则暂停/取消按钮会对现在这条下载失效。
fn finish_control(shared: &Arc<Shared>, id: &str, control: &Arc<Control>) {
    if let Ok(mut controls) = shared.controls.lock() {
        if controls
            .get(id)
            .is_some_and(|current| Arc::ptr_eq(current, control))
        {
            controls.remove(id);
        }
    }
}

/// 进度回调的小工厂：把「节流 + 写回字节 + 广播」打包成一个 `FnMut`，同时把控制位
/// 翻译成 [`DownloadFlow`] 交回给 `download_stream`。
struct ProgressEmitter {
    shared: Arc<Shared>,
    app: AppHandle,
    id: String,
    control: Arc<Control>,
    last_emit: u64,
}

impl ProgressEmitter {
    fn into_callback(mut self) -> impl FnMut(u64, Option<u64>) -> DownloadFlow {
        move |done, total| match self.control.flow() {
            DownloadFlow::Continue => {
                // 每 ~256KB 广播一帧，避免把 IPC 打满；末帧由完成时补。
                if done.saturating_sub(self.last_emit) >= 262_144 {
                    self.last_emit = done;
                    set_bytes(&self.shared, &self.app, &self.id, done, total);
                }
                DownloadFlow::Continue
            }
            other => other,
        }
    }
}

/// 只刷新字节数与百分比，不改状态、不落盘（终态才落盘，进度只是广播给前端）。
/// 已请求暂停/取消时直接丢帧：这一路 worker 已经停手，再报只会让前端闪回「下载中」。
fn set_bytes(shared: &Arc<Shared>, app: &AppHandle, id: &str, done: u64, total: Option<u64>) {
    let row = {
        let Ok(mut guard) = shared.tasks.lock() else {
            return;
        };
        let Some(task) = guard.get_mut(id) else {
            return;
        };
        if !matches!(
            task.stage.as_str(),
            stage::QUEUED | stage::DOWNLOADING | stage::REMUXING
        ) {
            return;
        }
        task.bytes_done = done;
        task.bytes_total = total;
        task.clone()
    };
    let _ = app.emit("download-progress", &row);
}
