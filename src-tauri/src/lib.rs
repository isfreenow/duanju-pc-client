//! Red Fruit desktop client.
//!
//! Layering, and the rule that keeps it honest:
//!
//! * [`core`] — the signing suite. Byte-exact ports, no network.
//! * [`api`] — transport and parsing. Owns every endpoint, header and key.
//! * [`history`] / [`favorites`] — the two local JSON tables: where you stopped
//!   watching, and what you starred. Titles and covers only, never a stream URL.
//! * [`launch`] — one external command parsed off argv / the environment, handed
//!   to the webview so an AI Agent can say “看这部” and land on the player.
//! * [`state`] — per-process identity used by the signer.
//! * [`commands`] — the only surface the webview can reach. Business DTOs in,
//!   playable stream descriptors out.
//!
//! The React layer must never learn a signature, an App host, a device profile or
//! the spade key derivation. Playback deliberately crosses that line: the site's
//! public MP4 turned out to exist for 0 of 10 sampled titles, so the only real
//! source is a CENC stream, and decrypting it in the browser needs that episode's
//! address and content key. Both are handed over per play request, nothing else.

pub mod api;
pub mod commands;
pub mod core;
pub mod favorites;
pub mod history;
pub mod launch;
pub mod state;

use commands::App;
use tauri::{Emitter, Manager};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Windows 用户反馈「黑屏但有声音」：流是 HEVC，硬解通路会被 Chromium 的 GPU
    // 黑名单挡掉（显卡本身支持）。WebView2 的环境变量必须在 Builder 之前设好，
    // 它会被浏览器进程原样收下；已有人为设值时不覆盖。
    #[cfg(windows)]
    std::env::var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS")
        .ok()
        .filter(|args| args.contains("ignore-gpu-blocklist"))
        .or_else(|| {
            // Safe here: still on the main thread before any thread or child process exists.
            unsafe {
                std::env::set_var(
                    "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS",
                    "--ignore-gpu-blocklist --enable-features=PlatformHEVCDecoderSupport",
                );
            }
            None
        });

    // `--search 关键词` 是一次性查询：不开窗口、不进 single-instance 那一套，打完字就退。
    if let Some(task) = launch::parse_search(std::env::args()) {
        std::process::exit(launch::search_and_print(task));
    }
    let app_state = App {
        client: api::build_client().expect("无法初始化网络客户端"),
    };
    tauri::Builder::default()
        // 再启动一次 = 给已经在跑的那个实例下指令： argv 交回第一个实例，暂存、
        // 广播事件、把窗口从后台拉回来。按插件要求排在其他插件之前。
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            let Some(command) = launch::parse(args) else { return };
            if let Some(pending) = app.try_state::<launch::Pending>() {
                pending.set(command.clone());
            }
            let _ = app.emit("launch-command", &command);
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage(app_state)
        .setup(|app| {
            // 冷启动带来的指令先存着：webview 起来后自己来取（take），取完即清。
            let pending = launch::Pending::default();
            if let Some(command) = launch::from_process() {
                pending.set(command);
            }
            app.manage(pending);
            // 把配置里的 duanju 协议关联到当前可执行文件：安装包由 bundler 登记，
            // 这一步覆盖绿色版；注册失败不拦启动，只是浏览器里点不开而已。
            #[cfg(desktop)]
            {
                use tauri_plugin_deep_link::DeepLinkExt;
                let _ = app.deep_link().register_all();
            }
            // 历史放在应用数据目录，跟包体、缓存分开；目录建不出来就退回内存表，
            // 让「记不住进度」成为一个可见的降级，而不是启动失败。
            let dir = app.path().app_data_dir().ok();
            let fav_dir = dir.clone().unwrap_or_else(|| std::path::PathBuf::from("."));
            let store = match dir {
                Some(dir) => {
                    if std::fs::create_dir_all(&dir).is_err() {
                        eprintln!("无法创建应用数据目录，播放历史只保留在本次运行内");
                    }
                    history::Store::open(&dir)
                }
                None => history::Store::open(std::path::Path::new(".")),
            };
            app.manage(store);
            // 收藏与历史同居应用数据目录，各自一个文件；目录建不出来同样退回内存表。
            app.manage(favorites::Store::open(&fav_dir));
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                if let Some(store) = window.app_handle().try_state::<history::Store>() {
                    store.flush();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::browse_list,
            commands::search_series,
            commands::get_series_episodes,
            commands::get_public_details,
            commands::resolve_play,
            commands::record_progress,
            commands::list_history,
            commands::latest_progress,
            commands::clear_history,
            commands::toggle_favorite,
            commands::list_favorites,
            commands::is_favorited,
            commands::flush_history,
            launch::take_launch_command
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
