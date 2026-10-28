import ReactDOM from "react-dom/client";
import { getCurrentWindow } from "@tauri-apps/api/window";
import App from "./App";

/**
 * No StrictMode here on purpose. Opening an episode kicks off a ~27 MB download
 * and a full decrypt; StrictMode's deliberate mount → unmount → remount would
 * double that traffic on every navigation in development, which reads like a bug
 * in the player rather than a dev-only artefact.
 */
ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(<App />);

// 配置里窗口 visible:false（WebView2 子进程冷启动那段不暴露白底空窗）；
// React 完成首帧挂载后由这里拉起窗口。事件监听类方案（tauri://ready、
// Rust 侧 on_page_load）在本机实测都收不到，不依赖它们。
void getCurrentWindow().show();
void getCurrentWindow().setFocus();

// 统计脚本挂载后动态注入：不占启动时序，域名不通时静默失败，不会再白屏等超时。
const analytics = document.createElement("script");
analytics.src = "https://data.autolinks.site/api/loader/duanju.js";
document.head.appendChild(analytics);

// 窗口内右键不弹 WebView2 的默认菜单（后退/刷新/检查等）。输入框例外：
// 那里还需要右键复制粘贴，浏览器默认菜单是唯一入口。
window.addEventListener("contextmenu", (event) => {
    const target = event.target as HTMLElement | null;
    if (target?.closest("input, textarea")) return;
    event.preventDefault();
});
