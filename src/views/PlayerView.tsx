/**
 * The player.
 *
 * 它是窗口全屏的：进入播放就占满整个窗口，顶栏只留「退出 / 上一集 / 下一集 /
 * 清晰度」，退出回到进来的那个页面。Esc 与按钮等价，因为一只手在键盘上时没人
 * 想去摸鼠标。
 *
 * 横屏和竖屏是两套摆法，因为要优先保住的东西不一样：横屏的剧（红果 1080p 这类）
 * 画面本身就要占满宽度，两侧没有余量；竖屏的剧（短剧的主流形态）在 16:9 窗口里
 * 只占中间一条，两侧全是空的，所以那里放分集栏，背后铺一张虚化的海报，而不是
 * 留两块死黑。方向从 `<video>` 的元数据里读，不猜——猜错就把画面挡了。
 *
 * 一集播完直接切下一集，不等、不问：短剧的场景就是一口气连着看，退出方式是 Esc
 * 和「退出」按钮，它们一直都在。下一集是付费集时不切，停在原地。
 *
 * 清晰度按 Netflix 的方式收进顶栏的一个菜单，不再在画面下面铺一排按钮：普通
 * 观看根本不需要碰它，摆出来反而像在提示「这里可能有问题」。
 *
 * 界面不解释片源是怎么变成可播文件的。这里唯一需要说的技术事实是「这台机器的
 * 播放内核放不出这条流」，因为它决定用户下一步点哪儿。
 *
 * 四件不能松的事：画面永远完整落在舞台里（不被挡、不越窗）、切换中的旧结果不能
 * 覆盖新选择、离开时本地文件必须释放、看到哪儿必须记住（进度上报有节流，退出时补一次 flush）。
 */

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  flushHistory,
  getSeriesEpisodes,
  recordProgress,
  resolvePlay,
  type Episode,
  type PlayableStream,
} from "../ipc/client";
import {
  codecSupported,
  describeError,
  mimeFor,
  prepareStream,
  revokePrepared,
  type PreparedVideo,
  type Stage,
} from "../player/playback";
import { useAsync } from "../hooks/useAsync";
import { Status } from "../components/Cards";
import { WindowControls } from "../components/WindowControls";

/** 上报节流：Rust 侧也会挡，但别把 IPC 打在每一次 timeupdate 上。 */
const REPORT_EVERY_MS = 5_000;

function qualityLabel(stream: PlayableStream): string {
  if (stream.height >= 2160 || stream.width >= 3840) return "4K";
  if (stream.height >= 1080 || stream.width >= 1920) return "1080p";
  if (stream.height >= 720 || stream.width >= 1280) return "720p";
  if (stream.height >= 480 || stream.width >= 640) return "480p";
  const named = stream.quality.trim();
  if (named.length > 0) return named;
  return stream.width > 0 && stream.height > 0 ? `${stream.width}×${stream.height}` : "自动";
}

export function PlayerView({
  episode,
  resumeAt = 0,
  category = "",
  onBack,
  onHome,
  onSeries,
  onSwitch,
}: {
  episode: Episode;
  /** 从「继续播放」进来时带上，单位秒。 */
  resumeAt?: number;
  /** 进来时所在的顶栏页签，写进观看记录供列表页按类目筛选。 */
  category?: string;
  onBack: () => void;
  /** 面包屑首段：直接回首页；没给就退回 onBack。 */
  onHome?: () => void;
  /** 点剧名：不论从哪进来的，都回该剧详情页；没给就退回 onBack。 */
  onSeries?: () => void;
  /** 换一集：由外层路由决定去哪儿，播放器自己不存列表。 */
  onSwitch: (episode: Episode, resumeAt?: number) => void;
}) {
  const resolution = useAsync(() => resolvePlay(episode.vid, episode.seriesId), [episode.vid]);
  const siblings = useAsync(() => getSeriesEpisodes(episode.seriesId), [episode.seriesId]);
  const [selected, setSelected] = useState(0);
  const [stage, setStage] = useState<Stage | null>(null);
  const [prepared, setPrepared] = useState<PreparedVideo | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [decodeFailed, setDecodeFailed] = useState(false);
  /** null = 元数据还没到，先按横屏摆（不铺背景、不出侧栏），到了再改。 */
  const [portrait, setPortrait] = useState<boolean | null>(null);
  /** 清晰度菜单的开关；换集或选定时自动收起。 */
  const [menuOpen, setMenuOpen] = useState(false);
  const generation = useRef(0);
  const heldRef = useRef<PreparedVideo | null>(null);
  const videoRef = useRef<HTMLVideoElement | null>(null);
  const lastReport = useRef(0);
  const currentRowRef = useRef<HTMLButtonElement | null>(null);
  const qualityRef = useRef<HTMLDivElement | null>(null);

  const streams = resolution.data?.streams ?? [];
  const chosen: PlayableStream | undefined = streams[selected];
  const watchCategory = episode.category ?? category;

  // Default to the best stream the machine can actually decode; the backend
  // already ordered them by quality, so this only skips ones the probe rejects.
  useEffect(() => {
    if (resolution.data === null) return;
    const index = resolution.data.streams.findIndex((stream) => codecSupported(mimeFor(stream)));
    setSelected(index < 0 ? 0 : index);
    setDecodeFailed(false);
  }, [resolution.data]);

  useEffect(() => {
    if (chosen === undefined) return;
    const id = generation.current + 1;
    generation.current = id;
    setPrepared(null);
    setFailure(null);
    setDecodeFailed(false);
    setMenuOpen(false);
    setStage({ stage: "下载", done: 0, total: 0 });
    let cancelled = false;
    prepareStream(chosen, (next) => {
      if (!cancelled && generation.current === id) setStage(next);
    })
      .then((result) => {
        if (cancelled || generation.current !== id) {
          // Stale: drop it immediately so its Blob cannot outlive the view.
          revokePrepared(result);
          return;
        }
        if (heldRef.current !== null) revokePrepared(heldRef.current);
        heldRef.current = result;
        setPrepared(result);
      })
      .catch((error: unknown) => {
        if (cancelled || generation.current !== id) return;
        setStage(null);
        setFailure(describeError(error));
      });
    return () => {
      cancelled = true;
    };
  }, [chosen]);

  // 离开播放器：释放本地文件，并把这一集看到哪儿补记一次。
  useEffect(
    () => () => {
      generation.current += 1;
      if (heldRef.current !== null) revokePrepared(heldRef.current);
      heldRef.current = null;
      const node = videoRef.current;
      if (node !== null && node.duration > 0 && node.currentTime > 0) {
        void recordProgress({
          seriesId: episode.seriesId,
          seriesTitle: episode.seriesTitle,
          cover: episode.cover,
          vid: episode.vid,
          vidIndex: episode.vidIndex,
          seconds: node.currentTime,
          duration: node.duration,
          category: watchCategory,
        }).catch(() => undefined);
      }
      void flushHistory().catch(() => undefined);
    },
    // episode 的标识在卸载回调里只用来定位该记哪一集，不需要重挂。
    [episode.cover, episode.seriesId, episode.seriesTitle, episode.vid, episode.vidIndex, watchCategory],
  );

  const report = useCallback(() => {
    const node = videoRef.current;
    if (node === null || !(node.duration > 0) || node.currentTime <= 0) return;
    const now = Date.now();
    if (now - lastReport.current < REPORT_EVERY_MS) return;
    lastReport.current = now;
    void recordProgress({
      seriesId: episode.seriesId,
      seriesTitle: episode.seriesTitle,
      cover: episode.cover,
      vid: episode.vid,
      vidIndex: episode.vidIndex,
      seconds: node.currentTime,
      duration: node.duration,
      category: watchCategory,
    }).catch(() => undefined);
  }, [episode.cover, episode.seriesId, episode.seriesTitle, episode.vid, episode.vidIndex, watchCategory]);

  const onMetadata = useCallback(() => {
    const node = videoRef.current;
    if (node === null) return;
    // 方向以 `<video>` 报出来的画面尺寸为准，不在前端猜、也不信接口给的宽高：
    // CSS 的 object-fit 用的就是这两个值，摆法跟着它们才和实际画出来的框一致。
    if (node.videoWidth > 0 && node.videoHeight > 0) setPortrait(node.videoHeight > node.videoWidth);
    // 只在确实看过、而且还没看完时跳；否则从头播。
    if (resumeAt > 15 && node.duration > resumeAt + 10) {
      node.currentTime = resumeAt;
    }
  }, [resumeAt]);

  // 竖屏时侧栏可能很长（一部剧上千集），进来就得让当前这一集在视野里。
  useEffect(() => {
    if (portrait !== true) return;
    currentRowRef.current?.scrollIntoView({ block: "nearest" });
  }, [portrait, episode.vid, siblings.data]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        if (menuOpen) {
          setMenuOpen(false);
          return;
        }
        event.preventDefault();
        onBack();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onBack, menuOpen]);

  // 点菜单外面就收起，和 Netflix 的弹层一致。
  useEffect(() => {
    if (!menuOpen) return;
    const onDown = (event: MouseEvent) => {
      if (qualityRef.current !== null && !qualityRef.current.contains(event.target as Node)) {
        setMenuOpen(false);
      }
    };
    window.addEventListener("mousedown", onDown);
    return () => window.removeEventListener("mousedown", onDown);
  }, [menuOpen]);

  const list = siblings.data?.episodes ?? [];
  // 只有河马的付费集取不出地址（直接报 LOCKED），才需要锁住；红果和西饭的付费集能直接播。
  const payLocked = siblings.data?.source === "hema";
  const position = list.findIndex((row) => row.vid === episode.vid);
  const prev = position > 0 ? list[position - 1] : null;
  const next = position >= 0 && position < list.length - 1 ? list[position + 1] : null;
  // 付费集点了也是一句 LOCKED 报错，「下一集」直接不给按。
  const nextLocked = payLocked && next !== null && next.webAvailable === false;
  const goto = (target: Episode | null) => {
    if (target !== null) onSwitch({ ...target, category: watchCategory }, 0);
  };

  const support = useMemo(() => (chosen === undefined ? null : codecSupported(mimeFor(chosen))), [chosen]);

  return (
    <section
      className={portrait === null ? "player-shell" : portrait ? "player-shell orient-portrait" : "player-shell orient-landscape"}
    >
      <header className="player-bar" data-tauri-drag-region>
        <div className="crumbs player-crumbs">
          <button type="button" className="back crumb-home" onClick={onHome ?? onBack}>
            首页
          </button>
          <span className="crumb-sep" aria-hidden="true">›</span>
          <h2 className="player-title">
            <button type="button" className="crumb-series" onClick={onSeries ?? onBack} title="返回该剧详情页">
              {episode.seriesTitle}
            </button>
            <span className="crumb-sep" aria-hidden="true">›</span>
            {`第 ${episode.vidIndex} 集`}
            {list.length > 0 && ` / 共 ${list.length} 集`}
          </h2>
        </div>
        <div className="player-nav">
          {streams.length > 1 && (
            <div className="quality" ref={qualityRef}>
              <button type="button" className="quality-toggle" onClick={() => setMenuOpen((open) => !open)}>
                {chosen !== undefined ? qualityLabel(chosen) : "清晰度"}
              </button>
              {menuOpen && (
                <div className="quality-menu" role="menu">
                  {streams.map((stream, index) => (
                    <button
                      key={`${stream.source}-${stream.quality}-${index}`}
                      type="button"
                      role="menuitem"
                      className={index === selected ? "quality-item current" : "quality-item"}
                      onClick={() => {
                        setSelected(index);
                        setMenuOpen(false);
                      }}
                    >
                      {qualityLabel(stream)}
                      {!codecSupported(mimeFor(stream)) && <span className="quality-na">本机不支持</span>}
                    </button>
                  ))}
                </div>
              )}
            </div>
          )}
          <button type="button" className="prev" disabled={prev === null} onClick={() => goto(prev)}>
            上一集
          </button>
          <button
            type="button"
            className="next"
            disabled={next === null || nextLocked}
            title={nextLocked ? `第 ${next!.vidIndex} 集为付费集，暂不支持在此播放` : undefined}
            onClick={() => goto(next)}
          >
            下一集
          </button>
        </div>
        <WindowControls />
      </header>

      <div className="player-body">
        <div className="player-stage">
          {/* 竖屏片源两侧一定是空的，铺一层虚化的同一张海报，比留两块死黑像话。 */}
          {portrait === true && episode.cover.length > 0 && (
            <img className="stage-backdrop" src={episode.cover} alt="" aria-hidden="true" referrerPolicy="no-referrer" />
          )}
          {prepared !== null ? (
            <video
              key={prepared.url}
              className="video"
              ref={videoRef}
              src={prepared.url}
              controls
              autoPlay
              playsInline
              onLoadedMetadata={onMetadata}
              onTimeUpdate={report}
              onPause={report}
              onSeeked={report}
              onError={() => setDecodeFailed(true)}
              onEnded={() => {
                if (!nextLocked) goto(next);
              }}
            />
          ) : failure === null ? (
            <div className="stage-loading">
              {episode.cover.length > 0 && (
                <img className="loading-backdrop" src={episode.cover} alt="" aria-hidden="true" referrerPolicy="no-referrer" />
              )}
              <div className="loading-center">
                <span className="spinner" aria-hidden="true" />
                <p className="loading-title">{episode.seriesTitle}</p>
                <p className="view-sub">
                  {next === null && !resolution.loading ? "正在准备" : resolution.loading ? "正在获取播放地址" : "正在缓冲"}
                  {` · 第 ${episode.vidIndex} 集`}
                </p>
                {stage !== null && stage.total > 0 && (
                  <div className="stage" role="progressbar" aria-valuenow={Math.min(100, Math.round((stage.done / stage.total) * 100))} aria-valuemin={0} aria-valuemax={100}>
                    <div className="stage-fill" style={{ width: `${Math.min(100, Math.round((stage.done / stage.total) * 100))}%` }} />
                  </div>
                )}
              </div>
            </div>
          ) : (
            <div className="screen-blank">
              这一集暂时播不了：{failure}
              {next !== null && (
                <button type="button" className="next" onClick={() => goto(next)}>
                  跳过，看第 {next.vidIndex} 集
                </button>
              )}
            </div>
          )}
        </div>

        {portrait === true && list.length > 0 && (
          <aside className="player-side" aria-label="本剧分集">
            <header className="side-head">
              <span className="side-title">{episode.seriesTitle}</span>
              <span className="view-sub">共 {list.length} 集</span>
            </header>
            <div className="side-list">
              {list.map((row) => {
                const current = row.vid === episode.vid;
                const locked = payLocked && row.webAvailable === false;
                return (
                  <button
                    key={row.vid}
                    type="button"
                    ref={current ? currentRowRef : null}
                    className={current ? "side-row current" : locked ? "side-row locked" : "side-row"}
                    disabled={current || locked}
                    title={locked && !current ? `第 ${row.vidIndex} 集为付费集，暂不支持在此播放` : undefined}
                    onClick={() => onSwitch({ ...row, category: watchCategory }, 0)}
                  >
                    {row.vidIndex}
                    {locked && !current && <span className="side-pay">付费</span>}
                  </button>
                );
              })}
            </div>
          </aside>
        )}
      </div>

      {(support === false || decodeFailed || resolution.error !== null) && (
        <footer className="player-foot">
          {support === false && <Status tone="warn">这台机器的播放内核不支持这条流，从右上角换一条清晰度试试。</Status>}
          {decodeFailed && support !== false && (
            <Status tone="warn">播放出错。换一条清晰度，或稍后重试。</Status>
          )}
          {resolution.error !== null && (
            <Status tone="error">
              <button type="button" onClick={onBack}>
                返回
              </button>{" "}
              {resolution.error}
            </Status>
          )}
        </footer>
      )}
    </section>
  );
}
