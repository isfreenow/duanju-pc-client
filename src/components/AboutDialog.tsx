/**
 * 「关于」弹层：版本号与开发申明。
 *
 * 版本以 package.json 为唯一来源，__APP_VERSION__ 由 Vite define 在构建时注入
 * （与 tauri.conf.json 的值由 scripts/sync-version.cjs 保证一致，这里不再走 IPC）。
 *
 * 申明要说的就两件事：内容目录来自各版权方的公开接口（部分由 REA Agent
 * 分析技术识别）；本工程是独立的全新实现（clean-room reimplementation），
 * 仅参考公开文档与原产品的实际运行行为，未使用任何非公开资料。
 */

import { useEffect } from "react";
import { openUrl } from "@tauri-apps/plugin-opener";

export function AboutDialog({ onClose }: { onClose: () => void }) {
    // Esc 关掉，跟播放器退出同一个习惯。
    useEffect(() => {
        const onKeyDown = (event: KeyboardEvent) => {
            if (event.key === "Escape") onClose();
        };
        window.addEventListener("keydown", onKeyDown);
        return () => window.removeEventListener("keydown", onKeyDown);
    }, [onClose]);

    return (
        <div className="about-mask" onClick={onClose}>
            <div className="about-panel" role="dialog" aria-label="关于" onClick={(event) => event.stopPropagation()}>
                <h3>短剧随心看</h3>
                <p className="about-version">版本 {__APP_VERSION__}</p>
                <p>
                    本应用的影片目录、封面与播放数据均来自相应版权方的公开接口，其中部分接口通过最新的
                    REA Agent 分析技术获得；本应用不存储、不转载任何内容源。
                </p>
                <p>
                    本工程为独立的全新实现（clean-room reimplementation），仅参考公开文档与原产品的实际运行行为，
                    未使用任何非公开的资料、代码或接口文档。
                </p>
                <p className="about-foot">观看历史与收藏仅保存在本机。</p>
                <p className="about-site">
                    官网：
                    <button
                        type="button"
                        className="link"
                        onClick={() => {
                            void openUrl("https://duanju.isfreenow.com").catch(() => undefined);
                        }}
                    >
                        DuanJu.isFreeNow.com
                    </button>
                </p>
                <p className="about-site">
                    <button
                        type="button"
                        className="link"
                        title="播不出来、缺剧集、界面有问题？到 GitHub 提个 issue"
                        onClick={() => {
                            void openUrl("https://github.com/isfreenow/duanju-pc-client/issues").catch(() => undefined);
                        }}
                    >
                        反馈问题
                    </button>
                    {" · "}
                    <button
                        type="button"
                        className="link"
                        title="不想登 GitHub？在官网页面底部的反馈表里填两句就行"
                        onClick={() => {
                            void openUrl("https://duanju.isfreenow.com/#feedback").catch(() => undefined);
                        }}
                    >
                        官网反馈表
                    </button>
                </p>
                <button type="button" className="about-close" onClick={onClose}>
                    知道了
                </button>
            </div>
        </div>
    );
}
