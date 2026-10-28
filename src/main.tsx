import ReactDOM from "react-dom/client";
import App from "./App";

/**
 * No StrictMode here on purpose. Opening an episode kicks off a ~27 MB download
 * and a full decrypt; StrictMode's deliberate mount → unmount → remount would
 * double that traffic on every navigation in development, which reads like a bug
 * in the player rather than a dev-only artefact.
 */
ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(<App />);

// 窗口内右键不弹 WebView2 的默认菜单（后退/刷新/检查等）。输入框例外：
// 那里还需要右键复制粘贴，浏览器默认菜单是唯一入口。
window.addEventListener("contextmenu", (event) => {
    const target = event.target as HTMLElement | null;
    if (target?.closest("input, textarea")) return;
    event.preventDefault();
});
