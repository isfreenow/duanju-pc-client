/**
 * 自动更新 + 全局底部状态栏：
 * - 启动后查一次 GitHub Release 的 latest.json（地址与验签公钥在 tauri.conf.json 的
 *   plugins.updater 里，由 Rust 侧读取，前端不碰）。
 * - 底部常驻一条状态栏：没有新版就显示「已是最新版本 vX」；发现新版就地显示版本号 +
 *   「立即更新」；点更新后就地在状态栏里显示下载进度（百分比 + MB，拿不到总量退化为
 *   滑动条 + 已下载 MB）。
 * - 强制更新（发布说明含「强制更新」或清单 force:true，由 CI 从提交信息烘进 notes）：
 *   额外盖一层全屏锁定弹框，弹框内显示同样的进度，唯一出口是「立即更新并重启」。
 * - dev 构建跳过 check()（无签名产物），状态栏照常显示当前版本；URL 带
 *   ?updatebar=idle|download|install|force 时铺假数据预览各状态，仅 DEV 生效。
 */

import { useEffect, useState } from "react";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { toast } from "./components/Toast";

/** 下载 / 安装所处的阶段，状态栏与弹框按它切换文案和进度条。 */
type Phase = "idle" | "downloading" | "installing" | "error";

/**
 * 这条更新是否强制。优先看清单里的结构化字段，其次看发布说明有没有「强制更新」标记；
 * 两者都没有就只在状态栏提示、不盖弹框。
 */
function isForced(update: Update): boolean {
    if ((update.rawJson as { force?: unknown }).force === true) return true;
    return (update.body ?? "").includes("强制更新");
}

/** 把字节数说成人话；拿不到总量时只显示已下载量。 */
function mb(bytes: number): string {
    return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

/**
 * dev 预览开关：URL 带 ?updatebar=idle|download|install|force 时直接用假数据渲染各状态，
 * 只为本地截图验收。仅 import.meta.env.DEV 生效，正式发布包里 preview 恒为 ""、逻辑不受影响。
 */
type Preview = "" | "idle" | "download" | "install" | "force";
const preview: Preview = (() => {
    if (!import.meta.env.DEV) return "";
    const match = /[?&]updatebar=(idle|download|install|force)/.exec(location.search);
    return match ? (match[1] as Preview) : "";
})();

/** 造一个不持有 Rust 资源的假 Update，仅供 dev 预览渲染用。 */
function mockUpdate(forced: boolean): Update {
    return {
        available: true,
        currentVersion: "0.1.5",
        version: "0.2.0",
        body: forced
            ? "【强制更新】此版本必须升级后方可继续使用。"
            : "由 GitHub Actions 自动构建发布（Windows / Linux / macOS），已装客户端可自动更新。",
        rawJson: forced ? { force: true } : {},
        downloadAndInstall: async () => { },
        download: async () => { },
        install: async () => { },
        close: async () => { },
    } as unknown as Update;
}

export function UpdateBar() {
    const [update, setUpdate] = useState<Update | null>(null);
    const [checked, setChecked] = useState(false);
    const [phase, setPhase] = useState<Phase>("idle");
    const [checking, setChecking] = useState(false);
    const [got, setGot] = useState(0);
    const [total, setTotal] = useState<number | undefined>(undefined);

    /** dev 预览假下载：按固定总量每隔 220ms 递增，走到头转「安装并重启」。 */
    const simulate = () => {
        const bytes = 78 * 1024 * 1024;
        setPhase("downloading");
        setGot(0);
        setTotal(bytes);
        let acc = 0;
        const id = setInterval(() => {
            acc += Math.round(bytes / 24);
            setGot(acc);
            if (acc >= bytes) {
                clearInterval(id);
                setPhase("installing");
            }
        }, 220);
    };

    useEffect(() => {
        // dev 预览：URL 带 ?updatebar=... 时铺假数据、不碰 Tauri 的 check()。
        if (preview) {
            setUpdate(mockUpdate(preview === "force"));
            setChecked(true);
            if (preview === "download") simulate();
            else if (preview === "install") {
                setTotal(78 * 1024 * 1024);
                setGot(78 * 1024 * 1024);
                setPhase("installing");
            }
            return;
        }
        let alive = true;
        // dev 构建没有签名产物，check 会直接报错：标已检查、状态栏显示当前版本即可。
        if (import.meta.env.DEV) {
            setChecked(true);
            return;
        }
        check()
            .then((found) => {
                if (!alive) return;
                setUpdate(found);
                setChecked(true);
            })
            .catch(() => {
                if (alive) setChecked(true);
            });
        return () => {
            alive = false;
        };
    }, []);

    const forced = update !== null && isForced(update);
    const busy = phase === "downloading" || phase === "installing";
    // 状态栏三态：默认绿（已最新）、普通更新橙、强制更新红。
    const state = forced ? "state-force" : update !== null ? "state-update" : "state-ok";

    const install = async () => {
        if (busy || update === null) return;
        // dev 预览不真下载，直接走一遍假进度动画给你看。
        if (preview) {
            simulate();
            return;
        }
        setPhase("downloading");
        setGot(0);
        setTotal(undefined);
        try {
            await update.downloadAndInstall((event) => {
                if (event.event === "Started") setTotal(event.data.contentLength);
                else if (event.event === "Progress") setGot((previous) => previous + event.data.chunkLength);
                else if (event.event === "Finished") setPhase("installing");
            });
            setPhase("installing");
            // Windows 的 NSIS 安装器接管后会自动重启；这里对 mac / Linux 补一次 relaunch。
            await relaunch();
        } catch {
            setPhase("error");
            toast("更新安装失败，可稍后重试");
        }
    };

    /** 手动再查一次：结果覆盖现有 update 状态；无新版则回落到「已是最新版本」。 */
    const recheck = async () => {
        if (checking || busy) return;
        if (preview || import.meta.env.DEV) {
            toast("开发构建跳过更新检查");
            return;
        }
        setChecking(true);
        try {
            const found = await check();
            setUpdate(found);
            setPhase("idle");
            setChecked(true);
            if (!found) toast("已是最新版本");
        } catch {
            toast("检查更新失败，请稍后重试");
        } finally {
            setChecking(false);
        }
    };

    const pct = phase === "installing"
        ? 100
        : total && total > 0
            ? Math.min(99, Math.round((got / total) * 100))
            : null;

    // 拿到总字节才能画确定的百分比进度条；拿不到（GitHub 偶发不给 Content-Length）
    // 就退化成一条来回滑动的不定量进度条，至少视觉上一直在动，配合「已下载 X MB」不空。
    const determinate = pct !== null;
    const barCls = determinate ? "update-progress" : "update-progress indeterminate";
    const fillStyle = determinate ? { width: `${pct}%` } : undefined;

    let status: string;
    if (!checked) status = "检查更新中…";
    else if (update === null) status = "已是最新版本";
    else if (phase === "downloading")
        status = pct !== null
            ? `正在下载更新… ${pct}%（${mb(got)}${total ? ` / ${mb(total)}` : ""}）`
            : `正在下载更新… 已下载 ${mb(got)}`;
    else if (phase === "installing") status = "下载完成，正在安装并重启…";
    else if (phase === "error") status = "更新安装失败，可稍后重试";
    else status = `发现新版本 v${update.version}`;

    return (
        <>
            <div className={"status-bar " + state} role="status" aria-live="polite">
                <span className="status-dot" aria-hidden="true" />
                <span className="status-brand">短剧随心看</span>
                <span className="status-ver">v{__APP_VERSION__}</span>
                <span className="status-spacer" />
                {busy && (
                    <span className={barCls} aria-hidden="true">
                        <span className="update-progress-fill" style={fillStyle} />
                    </span>
                )}
                <span className="status-text">{status}</span>
                <button type="button" className="status-check" disabled={checking || busy} onClick={() => void recheck()}>
                    {checking ? "检查中…" : "检查更新"}
                </button>
                {update !== null && !busy && (
                    <button type="button" className="status-act" onClick={() => void install()}>
                        {phase === "error" ? "重试" : "立即更新"}
                    </button>
                )}
            </div>

            {/* 强制更新：状态栏之外再盖一层全屏锁定弹框，Esc / 点遮罩都不关，唯一出路是装新版。 */}
            {forced && (
                <div className="update-force-mask">
                    <div className="update-force-panel" role="alertdialog" aria-label="需要更新">
                        <h3>需要更新到最新版本</h3>
                        <div className="update-force-ver-row">
                            <p className="update-force-ver">
                                当前 v{update.currentVersion} → 最新 v{update.version}
                            </p>
                            <button type="button" className="update-force-check" disabled={checking} onClick={() => void recheck()}>
                                {checking ? "检查中…" : "检查最新版"}
                            </button>
                        </div>
                        {update.body && <p className="update-force-notes">{update.body}</p>}
                        {busy ? (
                            <div className="update-force-busy">
                                <span className={barCls}>
                                    <span className="update-progress-fill" style={fillStyle} />
                                </span>
                                <span className="update-force-status">{status}</span>
                            </div>
                        ) : (
                            <button type="button" className="update-force-act" onClick={() => void install()}>
                                {phase === "error" ? "重试更新并重启" : "立即更新并重启"}
                            </button>
                        )}
                    </div>
                </div>
            )}
        </>
    );
}
