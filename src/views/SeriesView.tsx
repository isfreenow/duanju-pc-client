/**
 * One series: a Netflix-style detail hero plus the full episode list.
 *
 * The episode list and the metadata come from two different pages of the site and
 * load independently, so a slow detail page never blocks picking an episode.
 *
 * 续播提示读的是本机历史（Rust 已经判过「看完没看完」），所以这里不比较秒数，
 * 只把结果说出来：看到第几集、第几秒，点一下接着放。
 *
 * 来源接口会标明哪些集在网页上取不到地址（目前只有河马的付费集），这些格在列表上
 * 直接标「付费」并禁用；红果和西饭的付费集能直接取出地址，不标也不禁用。其余各集
 * 能不能取出地址，仍是播放器 resolve 时的事。
 * 底部的「相关推荐」用来源类目第一页的目录按标签重合度筛选，不新起任何接口。
 *
 * 收藏存在本机（Rust 侧的 favorites.json），详情页的☆/★就是它的开关；
 * 点亮后「我的」页会把这部剧排在最前面。点下去有三重反馈：星星弹一下、
 * 文案当场翻面（不等落盘回来）、右下角一条自己消失的提示；失败时星星与文案回退，不假装成功。
 *
 * `autoPlay` 是外部指令（`--watch` / `--series`）一路接过来的最后一棒：分集列表到手、
 * 并且历史那一发落定之后，自己点一下主按钮——看过就续播那一集那一秒，没看过就从
 * 第一集开头。两个条件缺一个都不动，免得把看到一半的剧放回开头。
 */

import { useEffect, useRef, useState } from "react";
import { toast } from "../components/Toast";
import { setFavoriteId, useFavoriteIds } from "../favorites";
import { browseList, getPublicDetails, getSeriesEpisodes, latestProgress, toggleFavorite, type Episode, type SeriesCard } from "../ipc/client";
import { clock, episodeOf, SeriesGrid, Skeleton, Status } from "../components/Cards";
import { useAsync } from "../hooks/useAsync";

function count(value: number | undefined, label: string): string | null {
  if (value === undefined || value <= 0) return null;
  return `${label} ${value >= 10000 ? `${(value / 10000).toFixed(1)} 万` : Math.round(value)}`;
}

const RELATED_LIMIT = 12;

/** 用目录卡片给当前这部找标签：优先它自己那条，退化到共性最多的那条。 */
function tagsOf(cards: SeriesCard[], seriesId: string): string[] {
  const own = cards.find((card) => card.seriesId === seriesId);
  if (own !== undefined && own.tags.length > 0) return own.tags;
  const tally = new Map<string, number>();
  for (const card of cards) {
    for (const tag of card.tags) tally.set(tag, (tally.get(tag) ?? 0) + 1);
  }
  const best = [...tally.entries()].sort((a, b) => b[1] - a[1])[0];
  return best === undefined ? [] : [best[0]];
}

export function SeriesView({
  seriesId,
  category,
  refresh,
  autoPlay = 0,
  onPlay,
  onBack,
  onPick,
}: {
  seriesId: string;
  /** 进来时所在的顶栏页签，用来取同类目目录做推荐；空串走热播榜。 */
  category: string;
  /** 窗口聚焦刷新计数：回到窗口时重取列表/详情/进度，不闪骨架屏。 */
  refresh: number;
  /** 外部指令带进来的「替用户点一下继续播放」，带的是指令序号；0 就是没有人下过。 */
  autoPlay?: number;
  onPlay: (episode: Episode, resumeAt?: number) => void;
  onBack: () => void;
  onPick: (card: SeriesCard) => void;
}) {
  const list = useAsync(() => getSeriesEpisodes(seriesId), [seriesId, refresh]);
  const details = useAsync(() => getPublicDetails(seriesId), [seriesId, refresh]);
  const progress = useAsync(() => latestProgress(seriesId), [seriesId, refresh]);
  const pool = useAsync(() => browseList(category === "history" ? "hot-drama" : category, "", 1), [category, refresh]);
  // 收藏状态只有一个来源：点亮/取消都是就地改那份快照，点下去当场翻面，不等 IPC 回来。
  const favoriteIds = useFavoriteIds();
  const favorited = favoriteIds.has(seriesId);
  // 每次点一下自增，给星星当 key：重挂才重跑动画，进页面不会自己弹。
  const [starPops, setStarPops] = useState(0);

  // 自动开播只发生一次，而且必须等列表与历史两个都落定。
  const pendingAutoPlay = useRef(autoPlay > 0);
  useEffect(() => {
    if (!pendingAutoPlay.current || list.loading || progress.loading) return;
    pendingAutoPlay.current = false;
    const seen = progress.data;
    if (seen !== null) {
      onPlay(episodeOf(seen, category), seen.finished ? 0 : seen.seconds);
      return;
    }
    const first = list.data?.episodes[0];
    if (first !== undefined) onPlay(first, 0);
  }, [list.data, list.loading, progress.data, progress.loading, category, onPlay]);

  const switchFavorite = async () => {
    const data = list.data;
    const next = !favorited;
    const said = next ? "已加入收藏 · 在「我的」置顶" : "已取消收藏";
    setFavoriteId(seriesId, next);
    setStarPops((value) => value + 1);
    toast(said);
    try {
      const confirmed = await toggleFavorite({
        seriesId,
        seriesTitle: data?.seriesTitle ?? "",
        cover: data?.cover ?? "",
        category,
      });
      setFavoriteId(seriesId, confirmed);
      // 服务端结果与乐观状态不一致才补一句，成功不啰叨第二遍。
      if (confirmed !== next) toast(next ? "收藏没生效，已回退" : "取消没生效，仍是收藏");
    } catch {
      setFavoriteId(seriesId, !next);
      toast("收藏没存上：" + (next ? "加入失败" : "取消失败"));
    }
  };

  if (list.loading && list.data === null) return <Skeleton cards={10} rows={3} />;
  if (list.error !== null) {
    return (
      <Status tone="error">
        <button type="button" onClick={onBack}>
          返回
        </button>{" "}
        {list.error}
      </Status>
    );
  }
  const data = list.data;
  if (data === null) return null;

  const meta = details.data;
  const social = meta === null ? [] : [
    count(meta.likeCount, "点赞"),
    count(meta.favoriteCount, "收藏"),
    meta.rating !== undefined && meta.rating > 0 ? `评分 ${meta.rating.toFixed(1)}` : null,
    count(meta.ratingCount, "人评"),
  ].filter((value): value is string => value !== null);

  // 看完的就别再催「继续播放」了，给一个「再看一遍」；没看过的行不出现。
  const seen = progress.data;

  // 相关推荐：按标签重合度排序，不足就用同类目里的其它条目补位。
  const poolCards = (pool.data?.results ?? []).filter((card) => card.seriesId !== seriesId);
  const tags = tagsOf(pool.data?.results ?? [], seriesId);
  const scored = poolCards
    .map((card) => ({ card, hits: card.tags.filter((tag) => tags.includes(tag)).length }))
    .sort((a, b) => b.hits - a.hits);
  const related = scored.slice(0, RELATED_LIMIT).map((row) => row.card);

  return (
    <section className="view series-view">
      <div className="category-top">
        <header className="view-head crumbs">
          <button type="button" className="crumb-home" onClick={onBack}>
            首页
          </button>
          <span className="crumb-sep" aria-hidden="true">›</span>
          <h2>{data.seriesTitle}</h2>
        </header>
      </div>
      <header className="series-hero">
        <div className="series-id">
          {data.cover.length > 0 && <img className="series-cover" src={data.cover} alt={data.seriesTitle} />}
          <div className="series-text">
            <p className="view-sub">
              共 {data.total} 集
              {meta !== null && meta.episodeLabel.length > 0 && ` · ${meta.episodeLabel}`}
            </p>
            {social.length > 0 && <p className="series-social">{social.join(" · ")}</p>}
            <div className="series-actions">
              {seen !== null ? (
                <button
                  type="button"
                  className="primary-action"
                  onClick={() => onPlay(episodeOf(seen, category), seen.finished ? 0 : seen.seconds)}
                >
                  {seen.finished ? "再看一遍" : `继续播放 · 第 ${seen.vidIndex} 集`}
                </button>
              ) : (
                data.episodes.length > 0 && (
                  <button type="button" className="primary-action" onClick={() => onPlay(data.episodes[0], 0)}>
                    播放
                  </button>
                )
              )}
              <button
                type="button"
                className={favorited ? "favorite-action on" : "favorite-action"}
                aria-pressed={favorited}
                onClick={switchFavorite}
              >
                <span key={starPops} className="star" aria-hidden="true">
                  {favorited ? "★" : "☆"}
                </span>
                {favorited ? "已收藏" : "收藏"}
              </button>
            </div>
            {seen !== null && !seen.finished && seen.duration > 0 && (
              <span className="resume-bar" aria-hidden="true">
                <span
                  className="resume-fill"
                  style={{ width: `${Math.min(100, Math.round((seen.seconds / seen.duration) * 100))}%` }}
                />
              </span>
            )}
          </div>
        </div>
      </header>

      {list.loading && <Skeleton cards={6} rows={2} />}

      <section className="episode-section" aria-label="分集列表">
        <h3>选集</h3>
        <div className="episodes">
          {data.episodes.map((episode) => {
            // 红果（官网）和西饭的付费集都能直接取出地址播放，只有河马取不到，才标「付费」并禁用。
            const locked = data.source === "hema" && episode.webAvailable === false;
            return (
              <button
                key={episode.vid}
                type="button"
                className={
                  [locked ? "episode locked" : "episode", seen !== null && seen.vid === episode.vid ? "watched" : ""]
                    .filter(Boolean)
                    .join(" ")
                }
                disabled={locked}
                title={locked ? `第 ${episode.vidIndex} 集为付费集，暂不支持在此播放` : undefined}
                onClick={() => onPlay(episode, 0)}
              >
                <span className="episode-no">{episode.vidIndex}</span>
                {locked && <span className="episode-pay">付费</span>}
                {seen !== null && seen.vid === episode.vid && (
                  <span className="episode-mark">{seen.finished ? "已看完" : clock(seen.seconds)}</span>
                )}
              </button>
            );
          })}
        </div>
      </section>

      {related.length > 0 && (
        <section className="related" aria-label="相关推荐">
          <h3>为你推荐</h3>
          <SeriesGrid cards={related} onPick={onPick} />
        </section>
      )}

      {details.error !== null && <Status tone="warn">详情未加载：{details.error}</Status>}
    </section>
  );
}
