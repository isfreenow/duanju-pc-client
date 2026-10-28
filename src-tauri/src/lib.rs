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
pub mod play_cache;
pub mod state;
pub mod transcode;

use commands::App;
use tauri::{tray::TrayIconBuilder, Emitter, Manager};
use tauri::menu::{MenuBuilder, MenuItemBuilder};
use tauri::WindowEvent;
#[cfg(all(desktop, not(debug_assertions)))]
use tauri_plugin_updater::UpdaterExt;

/// 后台巡检间隔：6 小时。对 GitHub Releases 的 latest.json 来说这个频率无压力，
/// 也能在发版后半天内把红点挂上托盘。
#[cfg(all(desktop, not(debug_assertions)))]
const UPDATE_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// 在 32x32 图标右下角合成红点（带不透明描边，任何底色都看得见）。
#[cfg(desktop)]
fn stamp_red_dot(mut rgba: Vec<u8>, size: u32) -> Vec<u8> {
    let (r, cx, cy) = (size as f32 * 0.30, size as f32 * 0.72, size as f32 * 0.72);
    for y in 0..size {
        for x in 0..size {
            let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            let p = ((y * size + x) * 4) as usize;
            if d <= r - 1.0 {
                rgba[p..p + 4].copy_from_slice(&[225, 19, 31, 255]);
            } else if d <= r {
                rgba[p..p + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
    }
    rgba
}

/// 切换托盘角标：Windows 托盘没有原生 badge API，只能整图替换。
/// 拿不到 exe 内嵌图标（非 Windows / 无资源）就静默跳过，不影响主流程。
#[cfg(desktop)]
fn set_tray_badge(app: &tauri::AppHandle, on: bool, version: Option<&str>) {
    let Some(tray) = app.tray_by_id("main-tray") else { return };
    if on {
        #[cfg(windows)]
        if let Ok(img) = tauri::image::Image::from_app_icon_resource(32) {
            let (w, h) = (img.width(), img.height());
            let dotted = stamp_red_dot(img.rgba().to_vec(), w);
            let _ = tray.set_icon(Some(tauri::image::Image::new_owned(dotted, w, h)));
        }
    } else if let Some(icon) = app.default_window_icon() {
        let _ = tray.set_icon(Some(icon.clone()));
    }
    let tip = match (on, version) {
        (true, Some(v)) => format!("短剧随心看 · 发现新版本 v{v}"),
        _ => "短剧随心看".to_string(),
    };
    let _ = tray.set_tooltip(Some(tip));
}

#[cfg(all(test, desktop))]
mod tray_badge_tests {
    use super::stamp_red_dot;

    #[test]
    fn red_dot_lands_in_the_lower_right_corner() {
        let size = 32u32;
        let base = vec![0u8; (size * size * 4) as usize];
        let out = stamp_red_dot(base, size);
        let px = |x: u32, y: u32| {
            let p = ((y * size + x) * 4) as usize;
            (out[p], out[p + 1], out[p + 2], out[p + 3])
        };
        // 圆心 (23, 23) 附近是纯红；左上角不受波及
        assert_eq!(px(23, 23), (225, 19, 31, 255), "dot centre");
        assert_eq!(px(3, 3), (0, 0, 0, 0), "untouched far corner");
        // 描边白环的存在性用计数断言，不赌边界单像素的浮点比较
        let whites = out.chunks_exact(4).filter(|p| **p == [255, 255, 255, 255]).count();
        let reds = out.chunks_exact(4).filter(|p| **p == [225, 19, 31, 255]).count();
        assert!((20..200).contains(&whites), "stroke ring pixels = {whites}");
        assert!((20..300).contains(&reds), "dot body pixels = {reds}");
        assert_eq!(out.len(), (size * size * 4) as usize, "same buffer size");
    }
}

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
            let Some(command) = launch::parse(args) else {
                return;
            };
            if let Some(pending) = app.try_state::<launch::Pending>() {
                pending.set(command.clone());
            }
            let _ = app.emit("launch-command", &command);
            if let Some(window) = app.get_webview_window("main") {
                // 窗口可能被「关闭」藏进了托盘，先 show 再拉起
                let _ = window.show();
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
            // 会话级播放缓存：只在内存里，不落盘（播放地址是短期凭据）。
            app.manage(play_cache::PlayCache::new());
            // 转码工具的目录布局钉在应用数据目录；目录拿不到就钉在当前目录，
            // 让「转码不可用」成为一个可见的降级，而不是启动失败。
            app.manage(transcode::Layout::new(&fav_dir));
            // 常驻托盘：主窗口「关闭」只是隐藏（见 on_window_event），进程不退出，
            // AI 调用（duanju:// 深链接、CLI）才能秒级拉起窗口而不必冷启动整个应用。
            // 右键菜单「退出程序」才是唯一真正的退出出口。
            let show_item = MenuItemBuilder::with_id("show", "打开主窗口").build(app)?;
            let update_item = MenuItemBuilder::with_id("update", "更新程序").build(app)?;
            let quit_item = MenuItemBuilder::with_id("quit", "退出程序").build(app)?;
            let tray_menu = MenuBuilder::new(app)
                .items(&[&show_item, &update_item, &quit_item])
                .build()?;
            let _tray = TrayIconBuilder::with_id("main-tray")
                .menu(&tray_menu)
                .tooltip("短剧随心看")
                .icon(
                    app.default_window_icon()
                        .cloned()
                        .expect("tauri.conf 未内嵌窗口图标，托盘无法常驻"),
                )
                .on_menu_event(|app, event| {
                    if event.id().as_ref() == "quit" {
                        if let Some(store) = app.try_state::<history::Store>() {
                            store.flush();
                        }
                        app.exit(0);
                        return;
                    }
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.unminimize();
                        let _ = window.set_focus();
                    }
                    // 「更新程序」：拉起窗口后交给 UpdateBar 走检查→自动安装的既有链路。
                    if event.id().as_ref() == "update" {
                        let _ = app.emit("tray-update", ());
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let tauri::tray::TrayIconEvent::Click {
                        button: tauri::tray::MouseButton::Left,
                        button_state: tauri::tray::MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(window) = app.get_webview_window("main") {
                            if window.is_visible().unwrap_or(false) {
                                let _ = window.minimize();
                            } else {
                                let _ = window.show();
                                let _ = window.set_focus();
                            }
                        }
                    }
                })
                .build(app)?;
            // 后台定时巡检更新：不依赖前端挂载，窗口藏在托盘里也能查；发现新版就把
            // 托盘图标换成带红点的、tooltip 带上版本号，并广播事件给已打开的状态栏。
            // dev 无签名产物，check 会验签报错，整段跳过（沿用既有约定）。
            #[cfg(all(desktop, not(debug_assertions)))]
            {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    loop {
                        match handle.updater() {
                            Ok(updater) => match updater.check().await {
                                Ok(Some(update)) => {
                                    set_tray_badge(&handle, true, Some(&update.version));
                                    let _ = handle.emit("update-available", &update.version);
                                }
                                Ok(None) => set_tray_badge(&handle, false, None),
                                Err(_) => {}
                            },
                            Err(_) => {}
                        }
                        tokio::time::sleep(UPDATE_CHECK_INTERVAL).await;
                    }
                });
            }
            #[cfg(all(desktop, debug_assertions))]
            if std::env::var("DUANJU_TRAY_BADGE_DEMO").is_ok() {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    set_tray_badge(&handle, true, Some("0.9.9"));
                });
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                if let Some(store) = window.app_handle().try_state::<history::Store>() {
                    store.flush();
                }
                // 常驻任务栏：关闭按钮不退出进程，落盘后藏进托盘。
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::browse_list,
            commands::search_series,
            commands::get_series_episodes,
            commands::get_public_details,
            commands::resolve_play,
            commands::prefetch_play,
            commands::drop_prefetch,
            commands::transcode_status,
            commands::prepare_transcode,
            commands::transcode_episode,
            commands::cancel_transcode,
            commands::record_progress,
            commands::list_history,
            commands::latest_progress,
            commands::clear_history,
            commands::remove_history,
            commands::check_series_update,
            commands::acknowledge_series,
            commands::check_updates,
            commands::toggle_favorite,
            commands::list_favorites,
            commands::is_favorited,
            commands::flush_history,
            launch::take_launch_command,
            commands::set_tray_badge
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
