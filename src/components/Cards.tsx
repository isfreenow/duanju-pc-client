/**
 * Card grid plus the pager that sits under every list view.
 *
 * The grid is shared by browse, search and ranking results because all three hand
 * back the same `SeriesCard`; the only difference is which labels the site chose
 * to fill in. 卡面上的快速收藏 ♡ 也只在 `CardTile` 里实现一次，所有网格共享。
 *
 * `clock` and `episodeOf` live here too because three views show the same
 * "看到哪儿了" line: 首页的最近播放行、详情页的继续播放条、「我的」页。
 */

import { useCallback, useEffect, useMemo, useRef, useState, type MouseEvent as ReactMouseEvent, type ReactNode } from "react";
import { toggleFavorite, removeHistory, type Episode, type Genre, type ProgressRow, type SeriesCard } from "../ipc/client";
import { setFavoriteId, useFavoriteIds } from "../favorites";
import { toast } from "./Toast";

/**
 * 鼠标在横向轨道上长按拖动即可左右滚动（Netflix 式）。只在轨道确实能横向滚时
 * 才接管。抢 pointer 必须等真的拖出位移再下手：down 时就 setPointerCapture 会把
 * 兼容鼠标事件改道到轨道，原生 click 落在容器而不是卡片，普通点卡片全部失灵
 * （页内合成 click 测不出这条，只能真鼠标复现）。拖动产生位移后吞掉紧随的
 * click，免得拖完误进详情。屏蔽图片原生拖拽（dragstart），否则从封面上起拖
 * 会被浏览器拿去拖图。返回回调 ref：继续观看行是异步拉到历史才渲染出容器，
 * 回调 ref 能在节点真正挂载时才挂监听。
 */
function useDragHorizontalScroll<T extends HTMLElement>() {
  const detach = useRef<(() => void) | null>(null);
  const setRef = useCallback((el: T | null) => {
    detach.current?.();
    detach.current = null;
    if (el === null) return;
    let startX = 0;
    let startScroll = 0;
    let pointerId = -1;
    let moved = false;
    const onPointerDown = (event: PointerEvent) => {
      if (event.button !== 0 || el.scrollWidth - el.clientWidth <= 0) return;
      pointerId = event.pointerId;
      startX = event.clientX;
      startScroll = el.scrollLeft;
      moved = false;
    };
    const onPointerMove = (event: PointerEvent) => {
      if (event.pointerId !== pointerId) return;
      const dx = event.clientX - startX;
      if (!moved) {
        if (Math.abs(dx) <= 4) return;
        moved = true;
        // 确认是拖动了才抢 pointer，从这一刻起指针离开轨道也能继续跟着滚。
        el.classList.add("dragging");
        el.setPointerCapture(pointerId);
      }
      el.scrollLeft = startScroll - dx;
      event.preventDefault();
    };
    const endDrag = (event: PointerEvent) => {
      if (event.pointerId !== pointerId) return;
      if (el.hasPointerCapture?.(pointerId)) el.releasePointerCapture(pointerId);
      el.classList.remove("dragging");
      pointerId = -1;
    };
    // 拖动过（moved）则拦下这次 click，不让卡片 onClick 生效。
    const onClickCapture = (event: MouseEvent) => {
      if (moved) {
        event.stopPropagation();
        event.preventDefault();
        moved = false;
      }
    };
    const blockDragStart = (event: Event) => event.preventDefault();
    el.addEventListener("pointerdown", onPointerDown);
    el.addEventListener("pointermove", onPointerMove, { passive: false });
    el.addEventListener("pointerup", endDrag);
    el.addEventListener("pointercancel", endDrag);
    el.addEventListener("click", onClickCapture, true);
    el.addEventListener("dragstart", blockDragStart);
    detach.current = () => {
      el.removeEventListener("pointerdown", onPointerDown);
      el.removeEventListener("pointermove", onPointerMove);
      el.removeEventListener("pointerup", endDrag);
      el.removeEventListener("pointercancel", endDrag);
      el.removeEventListener("click", onClickCapture, true);
      el.removeEventListener("dragstart", blockDragStart);
      el.classList.remove("dragging");
    };
  }, []);
  useEffect(() => () => detach.current?.(), []);
  return setRef;
}

/** 秒 → `分:秒`；不做小时位，短剧一集就几十秒到几分钟。 */
export function clock(seconds: number): string {
  const total = Math.max(0, Math.floor(seconds));
  const mm = Math.floor(total / 60);
  const ss = String(total % 60).padStart(2, "0");
  return `${mm}:${ss}`;
}

/**
 * 一条历史够拼出一个 Episode：播放器只要标识，不要地址。
 *
 * `title` 用「第 N 集」而不是原分集标题，因为历史里没有标题字段，也不值得为它
 * 再多取一次分集列表。`category` 从调用页透传，重播也要记在同一类目下。
 */
export function episodeOf(row: ProgressRow, category = ""): Episode {
  return {
    vid: row.vid,
    vidIndex: row.vidIndex,
    title: `第 ${row.vidIndex} 集`,
    seriesId: row.seriesId,
    seriesTitle: row.seriesTitle,
    cover: row.cover,
    category,
  };
}

/**
 * 封面图：未加载/加载中/无封面/加载失败时，都先铺一层「模糊遮罩」占位（毛玻璃
 * 渐变 + 被高斯模糊的剧名首字 + 扫光），图片到位后再淡入盖住它。图片本身 opacity
 * 从 0 过渡到 1，占位层始终在底下，所以加载完成是交叉淡化而不是闪一下。
 * `className` 决定用哪套封面尺寸（cover 网格卡 / recent-cover 继续观看卡）；
 * `children` 是浮在图上的角标/进度条。
 */
export function Cover({
  src,
  alt,
  title,
  className = "cover",
  children,
}: {
  src: string;
  alt: string;
  title: string;
  className?: string;
  children?: ReactNode;
}) {
  const [loaded, setLoaded] = useState(false);
  const [failed, setFailed] = useState(false);
  const showImg = src.length > 0 && !failed;
  return (
    <span className={className}>
      <span className="cover-mask" aria-hidden="true">
        <span className="cover-shimmer" />
        <span className="cover-mask-text">{title.slice(0, 2)}</span>
      </span>
      {showImg && (
        <img
          src={src}
          alt={alt}
          loading="lazy"
          referrerPolicy="no-referrer"
          className={loaded ? "loaded" : undefined}
          onLoad={() => setLoaded(true)}
          onError={() => setFailed(true)}
        />
      )}
      {children}
    </span>
  );
}

/** 一部剧的海报卡片：竖版封面 + 标题 + 集数/热度 + 标签。网格与横向行共用。 */
function CardTile({ card, onPick }: { card: SeriesCard; onPick: (card: SeriesCard) => void }) {
  const favoriteIds = useFavoriteIds();
  const favorited = favoriteIds.has(card.seriesId);
  // 快速收藏与详情页同款乐观链：先翻快照 + toast，IPC 回话不一致或报错再纠偏。
  // 卡片上没有类目信息，记空串——与 ContinueRail 重播时的行为一致。
  const quickToggle = async (event: ReactMouseEvent) => {
    event.stopPropagation();
    const next = !favorited;
    setFavoriteId(card.seriesId, next);
    toast(next ? `已收藏《${card.seriesTitle}》` : `已取消收藏《${card.seriesTitle}》`);
    try {
      const confirmed = await toggleFavorite({ seriesId: card.seriesId, seriesTitle: card.seriesTitle, cover: card.cover });
      setFavoriteId(card.seriesId, confirmed);
      if (confirmed !== next) toast(next ? "收藏没存上，已回到原样" : "取消没存上，仍是收藏");
    } catch {
      setFavoriteId(card.seriesId, !next);
      toast("没存上，已回到原样");
    }
  };
  return (
    <button type="button" className="card" onClick={() => onPick(card)}>
      <Cover src={card.cover} alt={card.seriesTitle} title={card.seriesTitle}>
        {card.rank !== undefined && <span className="badge rank">{card.rank}</span>}
        {card.episodeCount > 0 && <span className="badge count">{card.episodeCount} 集</span>}
        {favorited && <span className="badge faved">★ 已收藏</span>}
        <span
          className={favorited ? "fav-toggle faved" : "fav-toggle"}
          role="button"
          aria-label={favorited ? `取消收藏 ${card.seriesTitle}` : `收藏 ${card.seriesTitle}`}
          title={favorited ? "取消收藏" : "收藏"}
          onClick={quickToggle}
        >
          {favorited ? "♥" : "♡"}
        </span>
      </Cover>
      <span className="card-body">
        <span className="title">{card.seriesTitle}</span>
        <span className="meta">
          {card.episodeLabel.length > 0 ? card.episodeLabel : card.completionStatus === "completed" ? "已完结" : ""}
          {card.heat !== undefined && card.heat.length > 0 ? ` · ${card.heat}` : ""}
        </span>
        {card.tags.length > 0 && (
          <span className="tags">
            {card.tags.slice(0, 3).map((tag) => (
              <span key={tag} className="tag">
                {tag}
              </span>
            ))}
          </span>
        )}
      </span>
    </button>
  );
}

export function SeriesGrid({
  cards,
  onPick,
}: {
  cards: SeriesCard[];
  onPick: (card: SeriesCard) => void;
}) {
  return (
    <div className="grid">
      {cards.map((card) => (
        <CardTile key={card.seriesId} card={card} onPick={onPick} />
      ))}
    </div>
  );
}

/** 首页一行的横向海报轨道：右侧溢出横向滚动，不画滚动条。 */
export function PosterRail({
  cards,
  onPick,
}: {
  cards: SeriesCard[];
  onPick: (card: SeriesCard) => void;
}) {
  const ref = useDragHorizontalScroll<HTMLDivElement>();
  return (
    <div className="rail" ref={ref}>
      {cards.map((card) => (
        <CardTile key={card.seriesId} card={card} onPick={onPick} />
      ))}
    </div>
  );
}

/**
 * 顶部「继续观看」轨道：与正常剧集卡同比例的竖版海报 + 底部进度线。
 * `categoryOf` 决定重播时记回哪个类目（首页用记录自己的类目，第三方页签用当前页签）。
 * 悬停卡片右上角出 ✕ 删除观看记录，与「我的」页最近观看同一套乐观链：先隐卡 +
 * toast，存失败再放回并说明。
 */
export function ContinueRail({
  rows,
  categoryOf,
  onPlay,
  title = "继续观看",
}: {
  rows: ProgressRow[];
  categoryOf: (row: ProgressRow) => string;
  onPlay: (episode: Episode, resumeAt?: number) => void;
  title?: string;
}) {
  const ref = useDragHorizontalScroll<HTMLDivElement>();
  const favoriteIds = useFavoriteIds();
  // 按 seriesId 隐：remove_history 也是整部删，同部剧的多条记录一起消失才对得上。
  const [removed, setRemoved] = useState<Set<string>>(() => new Set());
  const visibleRows = useMemo(() => rows.filter((row) => !removed.has(row.seriesId)), [rows, removed]);
  if (visibleRows.length === 0) return null;
  const removeWatched = async (row: ProgressRow) => {
    setRemoved((prev) => new Set(prev).add(row.seriesId));
    toast(`已删除《${row.seriesTitle}》的观看记录`);
    try {
      await removeHistory(row.seriesId);
    } catch {
      setRemoved((prev) => {
        const next = new Set(prev);
        next.delete(row.seriesId);
        return next;
      });
      toast("删除没存上，已回到原样");
    }
  };
  return (
    <section className="recent" aria-label={title}>
      <header className="recent-head">
        <h3>{title}</h3>
      </header>
      <div className="recent-row" ref={ref}>
        {visibleRows.map((row) => (
          <button
            key={`${row.seriesId}-${row.vid}`}
            type="button"
            className="recent-card"
            onClick={() => onPlay(episodeOf(row, categoryOf(row)), row.finished ? 0 : row.seconds)}
          >
            <Cover src={row.cover} alt={row.seriesTitle} title={row.seriesTitle} className="recent-cover">
              <span
                className="card-remove"
                role="button"
                aria-label={`删除 ${row.seriesTitle} 的观看记录`}
                title="删除观看记录"
                onClick={(event) => {
                  event.stopPropagation();
                  void removeWatched(row);
                }}
              >
                ✕
              </span>
              {favoriteIds.has(row.seriesId) && <span className="badge faved">★ 已收藏</span>}
              {!row.finished && row.duration > 0 && (
                <span className="recent-progress" aria-hidden="true">
                  <span
                    className="recent-progress-fill"
                    style={{ width: `${Math.min(100, Math.round((row.seconds / row.duration) * 100))}%` }}
                  />
                </span>
              )}
            </Cover>
            <span className="recent-body">
              <span className="title">{row.seriesTitle}</span>
              <span className="meta">{row.finished ? "已看完" : `第 ${row.vidIndex} 集 · ${clock(row.seconds)}`}</span>
            </span>
          </button>
        ))}
      </div>
    </section>
  );
}

/** 题材筛选条：目录页/榜单页共用，`genres` 为空（如榜单）时整块不渲染。 */
export function GenreChips({
  genres,
  value,
  onChange,
}: {
  genres: Genre[];
  value: string;
  onChange: (slug: string) => void;
}) {
  if (genres.length === 0) return null;
  return (
    <div className="chips">
      <button type="button" className={value === "" ? "chip current" : "chip"} onClick={() => onChange("")}>
        全部
      </button>
      {genres.map((item) => (
        <button
          key={item.slug}
          type="button"
          className={value === item.slug ? "chip current" : "chip"}
          onClick={() => onChange(item.slug)}
        >
          {item.label}
        </button>
      ))}
    </div>
  );
}

export function Pager({
  page,
  totalPages,
  onChange,
}: {
  page: number;
  totalPages: number;
  onChange: (page: number) => void;
}) {
  if (totalPages <= 1) return null;
  const first = Math.max(1, Math.min(page - 2, totalPages - 4));
  const window = Array.from({ length: Math.min(5, totalPages) }, (_, index) => first + index).filter(
    (value) => value >= 1 && value <= totalPages,
  );
  return (
    <nav className="pager" aria-label="分页">
      <button type="button" disabled={page <= 1} onClick={() => onChange(page - 1)}>
        上一页
      </button>
      {window.map((value) => (
        <button
          key={value}
          type="button"
          className={value === page ? "current" : undefined}
          disabled={value === page}
          onClick={() => onChange(value)}
        >
          {value}
        </button>
      ))}
      <span className="pager-total">
        {page} / {totalPages}
      </span>
      <button type="button" disabled={page >= totalPages} onClick={() => onChange(page + 1)}>
        下一页
      </button>
    </nav>
  );
}

export function Status({ tone = "info", children }: { tone?: "info" | "warn" | "error"; children: ReactNode }) {
  return <p className={`status status-${tone}`}>{children}</p>;
}

/**
 * 骨架屏：形状和真布局一致（海报格 + 行标题），加载完成前屏幕不是「一句话」，
 * 而是已经在描述结果长什么样。`rows` 是几组标题+网格。
 */
export function Skeleton({ cards = 12, rows = 1 }: { cards?: number; rows?: number }) {
  return (
    <div className="skeleton" aria-hidden="true">
      {Array.from({ length: rows }, (_, rowIndex) => (
        <section key={rowIndex} className="skeleton-row">
          <span className="skeleton-line skeleton-title" />
          <div className="grid">
            {Array.from({ length: rowIndex === 0 ? cards : Math.max(6, Math.round(cards / 2)) }, (_, index) => (
              <div key={index} className="skeleton-card">
                <span className="skeleton-cover" />
                <span className="skeleton-line" />
                <span className="skeleton-line skeleton-short" />
              </div>
            ))}
          </div>
        </section>
      ))}
    </div>
  );
}
