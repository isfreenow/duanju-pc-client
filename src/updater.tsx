/**
 * 自动更新：启动后查一次 GitHub Release 的 latest.json（地址与验签公钥在
 * tauri.conf.json 的 plugins.updater 里，由 Rust 侧读取，前端不碰）。
 *
 * 发现新版不弹窗打扰，右下角挂一颗药丸等人点——正在看剧时不该被强制重启，
 * 但提示要活到用户看见为止，所以它不走 1.8 秒就消失的 toast。
 */

import { useEffect, useState } from "react";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { toast } from "./components/Toast";

export function UpdatePill() {
    const [update, setUpdate] = useState<Update | null>(null);
    const [busy, setBusy] = useState(false);

    useEffect(() => {
        let alive = true;
        // dev 构建没有签名产物，check 会直接报错，静默跳过即可。
        if (import.meta.env.DEV) return;
        check()
            .then((found) => {
                if (alive && found) setUpdate(found);
            })
            .catch(() => { });
        return () => {
            alive = false;
        };
    }, []);

    if (update === null) return null;

    const install = async () => {
        setBusy(true);
        try {
            await update.downloadAndInstall();
            await relaunch();
        } catch {
            setBusy(false);
            toast("更新安装失败，可稍后重试");
        }
    };

    return (
        <button type="button" className="update-pill" onClick={install} disabled={busy}>
            {busy ? "正在安装更新…" : `发现新版本 v${update.version}，点击更新`}
        </button>
    );
}
