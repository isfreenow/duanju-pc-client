/**
 * 无边框窗口的自绘控制按钮：最小化 / 最大化（还原）/ 关闭。
 *
 * 窗口去掉了系统标题栏（tauri.conf 里 `decorations: false`），内容顶到窗口上沿，
 * 与边框融为一体；缩放和拖动由系统在原生的无边框窗口上照常处理（拖动区见顶栏的
 * `data-tauri-drag-region`），只有这三颗按钮需要我们自己画、自己接 IPC。
 *
 * 最大化图标要跟着真实状态在「最大化」和「还原」之间切换，所以订阅一次
 * `onResized`——按钮点了之后由这里回读，不靠前端自己猜。
 */

import { useEffect, useMemo, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";

export function WindowControls() {
    const win = useMemo(() => getCurrentWindow(), []);
    const [maximized, setMaximized] = useState(false);

    useEffect(() => {
        let cancelled = false;
        let unlisten: (() => void) | undefined;
        const sync = () => {
            win
                .isMaximized()
                .then((value) => {
                    if (!cancelled) setMaximized(value);
                })
                .catch(() => undefined);
        };
        sync();
        win
            .onResized(sync)
            .then((fn) => {
                if (cancelled) fn();
                else unlisten = fn;
            })
            .catch(() => undefined);
        return () => {
            cancelled = true;
            unlisten?.();
        };
    }, [win]);

    return (
        <div className="win-controls" data-tauri-drag-region="false">
            <button
                type="button"
                className="win-btn"
                aria-label="最小化"
                onClick={() => {
                    void win.minimize().catch(() => undefined);
                }}
            >
                <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
                    <line x1="1" y1="5" x2="9" y2="5" stroke="currentColor" strokeWidth="1" />
                </svg>
            </button>
            <button
                type="button"
                className="win-btn"
                aria-label={maximized ? "还原" : "最大化"}
                onClick={() => {
                    void win.toggleMaximize().catch(() => undefined);
                }}
            >
                {maximized ? (
                    <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
                        <rect x="1.5" y="3" width="6" height="6" rx="1" fill="none" stroke="currentColor" strokeWidth="1" />
                        <path d="M3 3V1.5H9V7H7.5" fill="none" stroke="currentColor" strokeWidth="1" />
                    </svg>
                ) : (
                    <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
                        <rect x="1" y="1" width="8" height="8" rx="1" fill="none" stroke="currentColor" strokeWidth="1" />
                    </svg>
                )}
            </button>
            <button
                type="button"
                className="win-btn win-close"
                aria-label="关闭"
                onClick={() => {
                    void win.close().catch(() => undefined);
                }}
            >
                <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
                    <line x1="1" y1="1" x2="9" y2="9" stroke="currentColor" strokeWidth="1" />
                    <line x1="9" y1="1" x2="1" y2="9" stroke="currentColor" strokeWidth="1" />
                </svg>
            </button>
        </div>
    );
}
