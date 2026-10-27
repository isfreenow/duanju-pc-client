/**
 * Dev-only 预览入口：只渲染一条仿真的标题栏 + UpdateBar，配合 updater.tsx 里的
 * ?updatebar= 假数据开关，在纯浏览器里截图看更新横幅 / 强制弹框的样式（不碰 Tauri，
 * 所以不会像整台 App 那样因 WindowControls 缺运行时崩溃）。
 */

import { createRoot } from "react-dom/client";
import { UpdateBar } from "./updater";
import "./App.css";

createRoot(document.getElementById("root")!).render(
    <div className="app">
        <header className="top">
            <span className="brand">短剧随心看</span>
        </header>
        <UpdateBar />
    </div>,
);
