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
  Maximize2,
  Minimize2,
  Pause,
  Play,
  SkipBack,
  SkipForward,
  Volume1,
  Volume2,
  VolumeX,
} from "lucide-react";
import { openUrl } from "@tauri-apps/plugin-opener";
import { convertFileSrc } from "@tauri-apps/api/core";
import {
  cancelTranscode,
  flushHistory,
  getSeriesEpisodes,
  onTranscodeProgress,
  prefetchPlay,
  recordProgress,
  resolvePlay,
  transcodeEpisode,
  type Episode,
  type PlayableStream,
  type TranscodeProgress,
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

/** 倍速档位：短剧节奏快，2x 是常用档，0.5x 给想听清台词的时候；对齐抖音最高支持 3x。 */
const RATES = [0.5, 0.75, 1, 1.25, 1.5, 2, 3, 3.5, 4];

/** 倍速/静音的本地记忆键：跨集、跨启动都沿用用户上次选的那一档。 */
const RATE_STORAGE_KEY = "player.rate";
const MUTE_STORAGE_KEY = "player.muted";

function loadStoredRate(): number {
  if (typeof window === "undefined") return 1;
  const stored = Number(window.localStorage.getItem(RATE_STORAGE_KEY));
  return RATES.includes(stored) ? stored : 1;
}

function loadStoredMuted(): boolean {
  if (typeof window === "undefined") return false;
  return window.localStorage.getItem(MUTE_STORAGE_KEY) === "1";
}

/** 一次进入播放后往后预取几集（Rust 侧还会再夹一次上限）。 */
const PREFETCH_AHEAD = 2;

/** 快捷键跳进的秒数。 */
const SEEK_STEP = 5;

/** 控制条自动收起前的静置时长（毫秒）。 */
const UI_IDLE_MS = 2_500;

/** mm:ss，超过一小时才带小时位。 */
function fmtTime(seconds: number): string {
  const s = Math.max(0, Math.floor(seconds));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = String(s % 60).padStart(2, "0");
  return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${sec}` : `${m}:${sec}`;
}

function qualityLabel(stream: PlayableStream): string {
  if (stream.height >= 2160 || stream.width >= 3840) return "4K";
  if (stream.height >= 1080 || stream.width >= 1920) return "1080p";
  if (stream.height >= 720 || stream.width >= 1280) return "720p";
  if (stream.height >= 480 || stream.width >= 640) return "480p";
  const named = stream.quality.trim();
  if (named.length > 0) return named;
  return stream.width > 0 && stream.height > 0 ? `${stream.width}×${stream.height}` : "自动";
}

const ISSUES_URL = "https://github.com/isfreenow/duanju-pc-client/issues";
const FEEDBACK_URL = "https://duanju.isfreenow.com/#feedback";

/** 转码三个阶段的人话文案；界面不解释 FFmpeg，只说现在在干哪一步。 */
function transcodeStageLabel(stage: string): string {
  if (stage === "tool") return "正在下载转码工具";
  if (stage === "input") return "正在取本片数据";
  if (stage === "encode") return "正在转成 H.264";
  return "正在兼容处理";
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
  // 分集清单是典型会被反复拉的片单（切集、返回详情页都用到）：进会话缓存，失败留旧。
  const siblings = useAsync(() => getSeriesEpisodes(episode.seriesId), [episode.seriesId], {
    cacheKey: `episodes:${episode.seriesId}`,
    keepStale: true,
  });
  const [selected, setSelected] = useState(0);
  const [stage, setStage] = useState<Stage | null>(null);
  const [prepared, setPrepared] = useState<PreparedVideo | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [decodeFailed, setDecodeFailed] = useState(false);
  /** null = 元数据还没到，先按横屏摆（不铺背景、不出侧栏），到了再改。 */
  const [portrait, setPortrait] = useState<boolean | null>(null);
  /** 清晰度菜单的开关；换集或选定时自动收起。 */
  const [menuOpen, setMenuOpen] = useState(false);
  // ── 播放控制与兼容转码 ──
  /** 倍速：切集保留、跨启动也保留（存 localStorage）。 */
  const [rate, setRate] = useState<number>(loadStoredRate);
  /** 静音：与倍速同样跨集、跨启动保留。 */
  const [muted, setMuted] = useState<boolean>(loadStoredMuted);
  /** 这台机器的内核支不支持画中画（不支持就不出按钮）。 */
  const [pipAvailable, setPipAvailable] = useState(false);
  /** 正在转码：null 表示没在转；否则带百分比与阶段的进度。 */
  const [transcoding, setTranscoding] = useState<TranscodeProgress | null>(null);
  /** 转码失败的一行文案（带下一步）；成功则直接进 prepared。 */
  const [transcodeError, setTranscodeError] = useState<string | null>(null);
  // ── 自建控制条的镜像态：原生控件被接管后，界面只看这几个值 ──
  const [playing, setPlaying] = useState(false);
  const [time, setTime] = useState(0);
  const [duration, setDuration] = useState(0);
  const [buffered, setBuffered] = useState(0);
  const [volume, setVolume] = useState(1);
  const [fullscreen, setFullscreen] = useState(false);
  /** 控制条是否露出：鼠标静置一会儿就收起，动着就一直在。 */
  const [uiVisible, setUiVisible] = useState(true);
  /** 正在拖进度条：拖的时候收起计时不打扰，松手再收。 */
  const [scrubbing, setScrubbing] = useState(false);
  /** 上一秒是不是有声：在静音键上再按一次能回到这个音量。 */
  const lastVolume = useRef(1);
  /** 倍速/静音/音量的当前值快照（以 React 状态为准）：不回写依赖、切集后重新应用时用。 */
  const settingsRef = useRef({ rate, muted, volume });
  settingsRef.current = { rate, muted, volume };
  /** 正在换源（切集/换清晰度）的窗口期：true 时不信元素报的 mute/unmute，
   * 避免 WebView2 在新源装载后期把 muted 复位时反向同步错把静音抹掉；
   * 并在 playing/canplay 重新应用用户意图后收尾。 */
  const switchingRef = useRef(false);
  const settleTimer = useRef(0);
  const uiTimer = useRef(0);
  const stageRef = useRef<HTMLDivElement | null>(null);
  const generation = useRef(0);
  const heldRef = useRef<PreparedVideo | null>(null);
  const videoRef = useRef<HTMLVideoElement | null>(null);
  const lastReport = useRef(0);
  const currentRowRef = useRef<HTMLButtonElement | null>(null);
  const qualityRef = useRef<HTMLDivElement | null>(null);
  /** 本集转码任务号：进度事件按它过滤，取消也用它。 */
  const jobRef = useRef("");

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

  // 切集时清掉转码态：上一集的进度/错误不该带到这一集。倍速与静音不再重置，跨集保留。
  useEffect(() => {
    jobRef.current = "";
    setTranscoding(null);
    setTranscodeError(null);
  }, [episode.vid]);

  // 画中画能不能用由内核决定（Chromium 都有，但探测比假设稳）；没有就不出按钮。
  useEffect(() => {
    setPipAvailable(
      typeof document !== "undefined" &&
      "pictureInPictureEnabled" in document &&
      (document as Document & { pictureInPictureEnabled?: boolean }).pictureInPictureEnabled === true,
    );
  }, []);

  // 把用户意图（倍速/静音/音量）写回元素。换源这一刻先写一次；WebView2 可能
  // 在新源装载后期（loadedmetadata 之后、playing 附近）才把 playbackRate/muted 复位，
  // 所以只写这一刻不够——靠下方装载尾期监听再应用一次。
  useEffect(() => {
    const node = videoRef.current;
    if (node !== null) {
      node.playbackRate = rate;
      node.muted = muted;
      node.volume = volume;
    }
  }, [rate, muted, volume, prepared]);

  // 装载尾期重申：新源开始出画（playing/canplay/loadeddata）时，元素可能已被内核
  // 把倍速打回 1x、静音复位。按用户当前快照再应用一次，并结束换源窗口期。
  // 元素常驻，一次挂上即可。
  useEffect(() => {
    const node = videoRef.current;
    if (node === null) return;
    const reassert = () => {
      const { rate, muted, volume } = settingsRef.current;
      node.playbackRate = rate;
      node.muted = muted;
      node.volume = volume;
    };
    const onSettled = () => {
      reassert();
      switchingRef.current = false;
      window.clearTimeout(settleTimer.current);
    };
    const onLoading = () => {
      if (!switchingRef.current) return;
      reassert();
    };
    node.addEventListener("playing", onSettled);
    node.addEventListener("canplay", onLoading);
    node.addEventListener("loadeddata", onLoading);
    return () => {
      node.removeEventListener("playing", onSettled);
      node.removeEventListener("canplay", onLoading);
      node.removeEventListener("loadeddata", onLoading);
    };
  }, []);

  // 反向同步：画中画小窗上的原生静音按钮只改元素属性、不进 React 状态，
  // 只靠自有按钮接不回来。mute/unmute 事件把它接回去。
  // 但换源窗口期（switchingRef）内核会补发 mute/unmute/volumechange，那不是用户操的——
  // 此时一律不回写状态，免得把上一集设好的静音在切集时错抹掉。
  useEffect(() => {
    const node = videoRef.current;
    if (node === null) return;
    const onMute = () => {
      if (switchingRef.current) return;
      setMuted(node.muted);
    };
    const onVolume = () => {
      if (switchingRef.current) return;
      setVolume(node.volume);
    };
    node.addEventListener("mute", onMute);
    node.addEventListener("unmute", onMute);
    node.addEventListener("volumechange", onVolume);
    return () => {
      node.removeEventListener("mute", onMute);
      node.removeEventListener("unmute", onMute);
      node.removeEventListener("volumechange", onVolume);
    };
  }, []);

  // 记忆落盘：用户改一次，下次进来（含重启）仍是这一档。
  useEffect(() => {
    if (typeof window === "undefined") return;
    window.localStorage.setItem(RATE_STORAGE_KEY, String(rate));
  }, [rate]);
  useEffect(() => {
    if (typeof window === "undefined") return;
    window.localStorage.setItem(MUTE_STORAGE_KEY, muted ? "1" : "0");
  }, [muted]);

  // 后续两集预取：把「签接口 + 取回源地址 + 解 spade」提前跑完，结果只进 Rust 的
  // 会话缓存，真正切集时秒开。付费集（河马）不预取，取了也是 LOCKED。
  useEffect(() => {
    const list = siblings.data?.episodes ?? [];
    if (list.length === 0) return;
    const payLocked = siblings.data?.source === "hema";
    const pos = list.findIndex((row) => row.vid === episode.vid);
    if (pos < 0) return;
    const upcoming = list
      .slice(pos + 1, pos + 1 + PREFETCH_AHEAD)
      .filter((row) => row.vid.length > 0 && !(payLocked && row.webAvailable === false))
      .map((row) => row.vid);
    if (upcoming.length === 0) return;
    void prefetchPlay(upcoming, episode.seriesId).catch(() => undefined);
  }, [siblings.data, episode.vid, episode.seriesId]);

  // 订阅转码进度（工具下载 / 取片 / 编码三个阶段都从这里出）。
  useEffect(() => {
    let alive = true;
    let dispose: (() => void) | undefined;
    onTranscodeProgress((event) => {
      if (!alive) return;
      if (event.job !== jobRef.current) return;
      setTranscoding(event);
    })
      .then((stop) => {
        if (alive) dispose = stop;
        else stop();
      })
      .catch(() => undefined);
    return () => {
      alive = false;
      dispose?.();
    };
  }, []);

  const startTranscode = useCallback(() => {
    if (chosen === undefined) return;
    const job = `${episode.seriesId}:${episode.vid}:${Date.now()}`;
    jobRef.current = job;
    setTranscodeError(null);
    setTranscoding({ stage: "tool", job, percent: 0 });
    // 让在飞的 prepareStream 结果作废，免得它回头覆盖转码好的本地文件。
    generation.current += 1;
    void (async () => {
      try {
        const out = await transcodeEpisode(job, episode.vid, episode.seriesId, chosen.quality);
        if (jobRef.current !== job) return; // 期间已切集/取消
        setPrepared({
          url: convertFileSrc(out.path),
          local: false,
          mime: "video/mp4",
          bytes: out.bytes,
          ms: 0,
          summary: `本机转码 ${out.encoder}`,
        });
        setFailure(null);
        setStage(null);
        setTranscoding(null);
      } catch (error) {
        if (jobRef.current !== job) return;
        setTranscoding(null);
        setTranscodeError(error instanceof Error ? error.message : String(error));
      }
    })();
  }, [chosen, episode.vid, episode.seriesId]);

  const stopTranscode = useCallback(() => {
    if (jobRef.current.length > 0) void cancelTranscode(jobRef.current).catch(() => undefined);
    jobRef.current = "";
    setTranscoding(null);
  }, []);

  const stepRate = useCallback((direction: 1 | -1) => {
    setRate((current) => {
      const index = RATES.indexOf(current);
      if (index < 0) return RATES[2];
      // 两端循环：升过 4x 回 0.5x，降过 0.5x 回 4x，和顶栏倍速键的点击循环同一套档位环。
      return RATES[(index + direction + RATES.length) % RATES.length];
    });
  }, []);

  /** 小窗是不是用户开着的状态：切集/换源后照这个意图续播。 */
  const pipOpen = useRef(false);

  const pipState = () => {
    const doc = document as Document & { pictureInPictureElement?: Element | null };
    return doc.pictureInPictureElement ?? null;
  };

  const togglePip = useCallback(() => {
    const node = videoRef.current;
    if (node === null) return;
    if (pipState() !== null) {
      pipOpen.current = false; // 用户主动关：切集时不再自动重开
      const doc = document as Document & { exitPictureInPicture?: () => Promise<void> };
      void doc.exitPictureInPicture?.().catch(() => undefined);
    } else {
      pipOpen.current = true;
      const withPip = node as HTMLVideoElement & { requestPictureInPicture: () => Promise<unknown> };
      void withPip.requestPictureInPicture().catch(() => undefined);
    }
  }, []);

  // 换源而不是换元素：同一棵 <video> 从头到尾只改 src，小窗才能跨集/换清晰度
  // 不断。prepared 为 null（切集在下载、或这一集播不了）时先把源清掉停播，
  // 免得元素停在已被回收的旧 Blob 上报错。
  useEffect(() => {
    const node = videoRef.current;
    if (node === null) return;
    // 进换源窗口：直到新源 playing 才收尾（见装载尾期重申 effect）。期间内核
    // 会补发 mute/unmute/volumechange，都不得回写状态；安全超时防卡死。
    switchingRef.current = true;
    window.clearTimeout(settleTimer.current);
    settleTimer.current = window.setTimeout(() => {
      switchingRef.current = false;
    }, 4_000);
    if (prepared !== null) {
      if (node.getAttribute("src") !== prepared.url) node.src = prepared.url;
      // 小窗若被内核关掉了（pictureInPictureElement 变 null）而用户意图还开着，
      // 在这里送回去；小窗还挂在这棵元素上则不重复请求。autoPlay 接上新源。
      if (pipOpen.current && pipState() === null) {
        void (node as HTMLVideoElement & { requestPictureInPicture: () => Promise<unknown> })
          .requestPictureInPicture()
          .catch(() => undefined);
      }
      return;
    }
    if (node.getAttribute("src") !== null) {
      node.pause();
      node.removeAttribute("src");
      node.load();
    }
  }, [prepared]);

  const seekBy = useCallback((delta: number) => {
    const node = videoRef.current;
    if (node === null || !(node.duration > 0)) return;
    node.currentTime = Math.max(0, Math.min(node.duration, node.currentTime + delta));
  }, []);

  const togglePlay = useCallback(() => {
    const node = videoRef.current;
    if (node === null) return;
    if (node.paused) void node.play().catch(() => undefined);
    else node.pause();
  }, []);

  // 离开播放器：释放本地文件，并把这一集看到哪儿补记一次。
  useEffect(
    () => () => {
      generation.current += 1;
      if (heldRef.current !== null) revokePrepared(heldRef.current);
      heldRef.current = null;
      // 半途的转码任务不等它跑完，离开就取消（子进程由 Rust 侧看门狗终止）。
      if (jobRef.current.length > 0) void cancelTranscode(jobRef.current).catch(() => undefined);
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

  // 自建控制条的状态来源：元素事件是唯一的真相，界面只镜像这几个值。
  // timeupdate 约 4Hz，对一条进度线够了；buffered 一并读，进度条上的已缓冲灰段。
  useEffect(() => {
    const node = videoRef.current;
    if (node === null) return;
    const syncPlay = () => setPlaying(!node.paused && !node.ended);
    const syncTime = () => {
      setTime(node.currentTime);
      if (node.buffered.length > 0) setBuffered(node.buffered.end(node.buffered.length - 1));
    };
    const syncDuration = () => setDuration(node.duration || 0);
    const syncFull = () => setFullscreen(document.fullscreenElement !== null);
    node.addEventListener("play", syncPlay);
    node.addEventListener("playing", syncPlay);
    node.addEventListener("pause", syncPlay);
    node.addEventListener("ended", syncPlay);
    node.addEventListener("timeupdate", syncTime);
    node.addEventListener("loadedmetadata", syncDuration);
    document.addEventListener("fullscreenchange", syncFull);
    return () => {
      node.removeEventListener("play", syncPlay);
      node.removeEventListener("playing", syncPlay);
      node.removeEventListener("pause", syncPlay);
      node.removeEventListener("ended", syncPlay);
      node.removeEventListener("timeupdate", syncTime);
      node.removeEventListener("loadedmetadata", syncDuration);
      document.removeEventListener("fullscreenchange", syncFull);
    };
  }, [prepared]);

  // 控制条自动收起：只在播着的时候收；暂停、拖条、拖音量时强制露出。
  const wakeUi = useCallback(() => {
    setUiVisible(true);
    window.clearTimeout(uiTimer.current);
    uiTimer.current = window.setTimeout(() => setUiVisible(false), UI_IDLE_MS);
  }, []);
  useEffect(() => {
    if (!playing || scrubbing) {
      window.clearTimeout(uiTimer.current);
      setUiVisible(true);
      return;
    }
    wakeUi();
    return () => window.clearTimeout(uiTimer.current);
  }, [playing, scrubbing, wakeUi]);

  const toggleFullscreen = useCallback(() => {
    if (document.fullscreenElement !== null) {
      void document.exitFullscreen().catch(() => undefined);
      return;
    }
    const stage = stageRef.current;
    if (stage === null) return;
    void stage.requestFullscreen().catch(() => undefined);
  }, []);

  const toggleMute = useCallback(() => {
    const node = videoRef.current;
    if (node === null) return;
    // 直接以 React 状态为真相同时写元素，不依赖事件回环（换源窗口期事件被屏蔽）。
    // 顶栏按钮、M 键、控制条图标键全部复用这里。
    if (node.muted) {
      // 取消静音保证听得见：音量被单独拖到 0 时回补到上次档位（没有则满量）。
      node.muted = false;
      if (node.volume === 0) node.volume = lastVolume.current > 0 ? lastVolume.current : 1;
      setMuted(false);
    } else {
      if (node.volume > 0) lastVolume.current = node.volume;
      node.muted = true;
      setMuted(true);
    }
  }, []);

  const nudgeVolume = useCallback((delta: number) => {
    const node = videoRef.current;
    if (node === null) return;
    node.volume = Math.max(0, Math.min(1, node.volume + delta));
  }, []);

  const seekTo = useCallback((seconds: number) => {
    const node = videoRef.current;
    if (node === null || !(node.duration > 0)) return;
    node.currentTime = Math.max(0, Math.min(node.duration, seconds));
    setTime(node.currentTime);
  }, []);

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
    // 装载中途内核可能把倍速/静音打回默认（时机随内核而异），这里按快照先回写
    // 一次；出画后还有 playing/canplay 的重申兑现（见装载尾期重申 effect）。
    const { rate, muted, volume } = settingsRef.current;
    node.playbackRate = rate;
    node.muted = muted;
    node.volume = volume;
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

  // 快捷键：一只手在键盘上时不用去摸鼠标。焦点在输入框里不抢键。
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const tag = target?.tagName ?? "";
      if (tag === "INPUT" || tag === "TEXTAREA" || target?.isContentEditable) return;
      switch (event.key) {
        case " ":
        case "k":
        case "K":
          event.preventDefault();
          togglePlay();
          break;
        case "ArrowLeft":
          event.preventDefault();
          seekBy(-SEEK_STEP);
          break;
        case "ArrowRight":
          event.preventDefault();
          seekBy(SEEK_STEP);
          break;
        case "ArrowUp":
          event.preventDefault();
          nudgeVolume(0.1);
          break;
        case "ArrowDown":
          event.preventDefault();
          nudgeVolume(-0.1);
          break;
        case "f":
        case "F":
          toggleFullscreen();
          break;
        case "m":
        case "M":
          toggleMute();
          break;
        case ",":
          stepRate(-1);
          break;
        case ".":
          stepRate(1);
          break;
        case "n":
        case "N":
          if (next !== null && !nextLocked) goto(next);
          break;
        case "p":
        case "P":
          if (prev !== null) goto(prev);
          break;
        default:
          break;
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // goto 每次渲染新建，但它只读 onSwitch（稳定）与 watchCategory，重挂监听代价可忽略。
  }, [togglePlay, seekBy, stepRate, next, prev, nextLocked, watchCategory, nudgeVolume, toggleFullscreen, toggleMute]);

  const support = useMemo(() => (chosen === undefined ? null : codecSupported(mimeFor(chosen))), [chosen]);
  // 转码只在「这台机器解不了这条流」时才摆出来：有能播的清晰度就不必多一道。
  const canTranscode = chosen !== undefined && (support === false || decodeFailed);
  // 已经在播转码好的本地文件时，不再重申「解不了、装扩展」那套。
  const playingTranscoded = prepared?.summary.startsWith("本机转码") === true;

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
          {prepared !== null && (
            <button
              type="button"
              className="quality-toggle"
              title="倍速（逗号 / 句号 键可升降）"
              onClick={() => setRate((current) => RATES[(RATES.indexOf(current) + 1 + RATES.length) % RATES.length])}
            >
              {`${rate}x`}
            </button>
          )}
          {prepared !== null && (
            <button
              type="button"
              className="quality-toggle"
              title="静音（M 键可切换）"
              onClick={toggleMute}
            >
              {muted ? "已静音" : "静音"}
            </button>
          )}
          {prepared !== null && pipAvailable && (
            <button type="button" className="quality-toggle" title="画中画" onClick={togglePip}>
              小窗
            </button>
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
        <div
          className={uiVisible || !playing ? "player-stage" : "player-stage ui-idle"}
          ref={stageRef}
          onPointerMove={wakeUi}
          onPointerLeave={() => {
            if (playing && !scrubbing) setUiVisible(false);
          }}
        >
          {/* 竖屏片源两侧一定是空的，铺一层虚化的同一张海报，比留两块死黑像话。 */}
          {portrait === true && episode.cover.length > 0 && (
            <img className="stage-backdrop" src={episode.cover} alt="" aria-hidden="true" referrerPolicy="no-referrer" />
          )}
          {/* 元素常驻：加载/失败态是盖在它上面的叠层，不拆 <video>，小窗才能跨集不断。
              原生控件被接管（无 controls）：Chromium 不开放样式定制，自绘一套才贴得住主题。 */}
          <video
            className="video"
            ref={videoRef}
            autoPlay
            playsInline
            onClick={togglePlay}
            onDoubleClick={toggleFullscreen}
            onLoadedMetadata={onMetadata}
            onTimeUpdate={report}
            onPause={report}
            onSeeked={report}
            onError={() => setDecodeFailed(true)}
            onEnded={() => {
              if (!nextLocked) goto(next);
            }}
          />
          {prepared !== null && (
            <div className="player-controls" role="group" aria-label="播放控制" onPointerDown={wakeUi}>
              <div
                className="progress-track"
                onPointerDown={(event) => {
                  if (!(duration > 0)) return;
                  event.currentTarget.setPointerCapture(event.pointerId);
                  setScrubbing(true);
                  const rect = event.currentTarget.getBoundingClientRect();
                  seekTo(Math.max(0, Math.min(1, (event.clientX - rect.left) / rect.width)) * duration);
                }}
                onPointerMove={(event) => {
                  if (!scrubbing || !(duration > 0)) return;
                  const rect = event.currentTarget.getBoundingClientRect();
                  seekTo(Math.max(0, Math.min(1, (event.clientX - rect.left) / rect.width)) * duration);
                }}
                onPointerUp={() => setScrubbing(false)}
                onPointerCancel={() => setScrubbing(false)}
                role="slider"
                aria-label="播放进度"
                aria-valuemin={0}
                aria-valuemax={Math.round(duration)}
                aria-valuenow={Math.round(time)}
                aria-valuetext={`${fmtTime(time)} / ${fmtTime(duration)}`}
              >
                <span className="progress-buffered" style={{ width: duration > 0 ? `${Math.min(100, (buffered / duration) * 100)}%` : "0%" }} />
                <span className="progress-fill" style={{ width: duration > 0 ? `${Math.min(100, (time / duration) * 100)}%` : "0%" }} />
              </div>
              <div className="controls-row">
                <button type="button" className="ctrl-btn" title={playing ? "暂停（空格 / K）" : "播放（空格 / K）"} onClick={togglePlay}>
                  {playing ? <Pause size={18} aria-hidden="true" /> : <Play size={18} aria-hidden="true" />}
                </button>
                <button type="button" className="ctrl-btn" title="上一集（P）" disabled={prev === null} onClick={() => goto(prev)} aria-label="上一集">
                  <SkipBack size={18} aria-hidden="true" />
                </button>
                <button type="button" className="ctrl-btn" title="下一集（N）" disabled={next === null || nextLocked} onClick={() => goto(next)} aria-label="下一集">
                  <SkipForward size={18} aria-hidden="true" />
                </button>
                <span className="ctrl-time">{`${fmtTime(time)} / ${fmtTime(duration)}`}</span>
                <span className="ctrl-spacer" />
                <div className="ctrl-volume">
                  <div
                    className="volume-track"
                    role="slider"
                    aria-label="音量（上下方向键）"
                    aria-valuemin={0}
                    aria-valuemax={100}
                    aria-valuenow={Math.round((muted ? 0 : volume) * 100)}
                    onPointerDown={(event) => {
                      const node = videoRef.current;
                      if (node === null) return;
                      const rect = event.currentTarget.getBoundingClientRect();
                      const ratio = Math.max(0, Math.min(1, (event.clientX - rect.left) / rect.width));
                      // 从 0 拖起来视为想听见声：顺带把静音解掉。
                      if (node.muted && ratio > 0) node.muted = false;
                      node.volume = ratio;
                    }}
                    onPointerMove={(event) => {
                      if (event.buttons === 0) return;
                      const node = videoRef.current;
                      if (node === null) return;
                      const rect = event.currentTarget.getBoundingClientRect();
                      const ratio = Math.max(0, Math.min(1, (event.clientX - rect.left) / rect.width));
                      if (node.muted && ratio > 0) node.muted = false;
                      node.volume = ratio;
                    }}
                  >
                    <span className="volume-fill" style={{ width: `${(muted ? 0 : volume) * 100}%` }} />
                  </div>
                  <button type="button" className="ctrl-btn" title="静音（M）" onClick={toggleMute}>
                    {muted ? <VolumeX size={18} aria-hidden="true" /> : volume <= 0.5 ? <Volume1 size={18} aria-hidden="true" /> : <Volume2 size={18} aria-hidden="true" />}
                  </button>
                </div>
                <button
                  type="button"
                  className="ctrl-btn"
                  title={fullscreen ? "退出全屏（F）" : "全屏（F）"}
                  onClick={toggleFullscreen}
                >
                  {fullscreen ? <Minimize2 size={16} aria-hidden="true" /> : <Maximize2 size={16} aria-hidden="true" />}
                </button>
              </div>
            </div>
          )}
          {prepared === null && failure === null && (
            <div className="stage-loading">
              {episode.cover.length > 0 && (
                <img className="loading-backdrop" src={episode.cover} alt="" aria-hidden="true" referrerPolicy="no-referrer" />
              )}
              <div className="loading-center">
                <span className="spinner" aria-hidden="true" />
                <p className="loading-title">{episode.seriesTitle}</p>
                {transcoding !== null ? (
                  <>
                    <p className="view-sub">
                      {transcodeStageLabel(transcoding.stage)}
                      {` · 第 ${episode.vidIndex} 集 · ${transcoding.percent}%`}
                      {transcoding.note !== undefined && transcoding.note.length > 0 ? ` · ${transcoding.note}` : ""}
                    </p>
                    <div className="stage" role="progressbar" aria-valuenow={transcoding.percent} aria-valuemin={0} aria-valuemax={100}>
                      <div className="stage-fill" style={{ width: `${Math.min(100, transcoding.percent)}%` }} />
                    </div>
                    <button type="button" className="next" onClick={stopTranscode}>
                      取消
                    </button>
                  </>
                ) : (
                  <>
                    <p className="view-sub">
                      {next === null && !resolution.loading ? "正在准备" : resolution.loading ? "正在获取播放地址" : "正在缓冲"}
                      {` · 第 ${episode.vidIndex} 集`}
                    </p>
                    {stage !== null && stage.total > 0 && (
                      <div className="stage" role="progressbar" aria-valuenow={Math.min(100, Math.round((stage.done / stage.total) * 100))} aria-valuemin={0} aria-valuemax={100}>
                        <div className="stage-fill" style={{ width: `${Math.min(100, Math.round((stage.done / stage.total) * 100))}%` }} />
                      </div>
                    )}
                  </>
                )}
              </div>
            </div>
          )}
          {prepared === null && failure !== null && (
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

      {(support === false || decodeFailed || resolution.error !== null) && !playingTranscoded && (
        <footer className="player-foot">
          {support === false &&
            (streams.some((stream) => codecSupported(mimeFor(stream))) ? (
              <Status tone="warn">这台机器的播放内核不支持这条流，从右上角换一条清晰度试试。</Status>
            ) : (
              // 探测是启动时的快照，装完扩展必须重启应用才会重新判定，文案不要把重启吞掉。
              <Status tone="warn">
                <button
                  type="button"
                  onClick={() =>
                    void openUrl("https://apps.microsoft.com/detail/9n4wgh0z6vhq").catch(() => undefined)
                  }
                >
                  安装 HEVC 视频扩展
                </button>{" "
                }，重启应用后就能播（这台机器缺 H.265 解码组件，所有清晰度都解不了）。
                {canTranscode && (
                  <>
                    {" 或"}
                    <button
                      type="button"
                      disabled={transcoding !== null}
                      onClick={transcoding !== null ? stopTranscode : startTranscode}
                    >
                      {transcoding !== null
                        ? `${transcodeStageLabel(transcoding.stage)} ${transcoding.percent}%（点取消）`
                        : "转成 H.264 再播"}
                    </button>
                    {transcoding === null && "（首次需下载转码工具，之后走本地缓存）。"}
                    {transcodeError !== null && ` 转码失败：${transcodeError}。`}
                  </>
                )}
              </Status>
            ))}
          {decodeFailed && support !== false && (
            <Status tone="warn">
              播放出错。换一条清晰度，或稍后重试。
              {canTranscode && (
                <>
                  <button
                    type="button"
                    disabled={transcoding !== null}
                    onClick={transcoding !== null ? stopTranscode : startTranscode}
                  >
                    {transcoding !== null
                      ? `${transcodeStageLabel(transcoding.stage)} ${transcoding.percent}%（点取消）`
                      : "转成 H.264 再播"}
                  </button>
                  {transcoding === null && "（首次需下载转码工具）。"}
                  {transcodeError !== null && ` 转码失败：${transcodeError}。`}
                </>
              )}
              还是不行？
              <button type="button" onClick={() => void openUrl(ISSUES_URL).catch(() => undefined)}>
                去反馈
              </button>{" "}
              或到官网
              <button type="button" onClick={() => void openUrl(FEEDBACK_URL).catch(() => undefined)}>
                填反馈表
              </button>
            </Status>
          )}
          {resolution.error !== null && (
            <Status tone="error">
              <button type="button" onClick={onBack}>
                返回
              </button>{" "}
              {resolution.error} · 反复这样可以去
              <button type="button" onClick={() => void openUrl(ISSUES_URL).catch(() => undefined)}>
                提交 issue
              </button>
              ，或到官网{" "}
              <button type="button" onClick={() => void openUrl(FEEDBACK_URL).catch(() => undefined)}>
                填反馈表
              </button>
            </Status>
          )}
        </footer>
      )}
    </section>
  );
}
