/**
 * 「我的」页：观看历史与收藏融合在同一个页面。
 *
 * 收藏在前（「最近收藏」先展示一截，点「查看全部」才展开完整清单），观看记录
 * 紧随其后；已收藏的剧不再重复出现在观看记录里。数据全在本机的两个 JSON
 * 文件里（history.json / favorites.json），所以这个页面离线也能打开，也不会
 * 把任何观看记录发出去。「清空历史」只删历史文件，收藏不受影响。
 *
 * 卡片与普通剧集卡同款，封面上只标「看到第几集 / 完播 / 收藏」；点卡片进详情页，
 * 续播由详情页的「继续播放 / 再看一遍」按钮承接，列表里不再摆播放动作；
 * 收藏卡右上角有个 ✕，在本页就能直接取消收藏：卡片当场缩小消失，右下角同步
 * 给一条提示，存失败再把原样端回来并说明。✕ 自己也有按下缩小与悬停变红的反馈。
 */

import { useState } from "react";
import {
  clearHistory,
  listFavorites,
  listHistory,
  toggleFavorite,
  type FavoriteEntry,
  type ProgressRow,
} from "../ipc/client";
import { Cover, Skeleton, Status } from "../components/Cards";
import { setFavoriteId } from "../favorites";
import { toast } from "../components/Toast";
import { useAsync } from "../hooks/useAsync";

/** 折叠态只摆这么多收藏，剩下的点「查看全部」展开。 */
const FAVORITES_PREVIEW = 12;

/** 相对时间：列表里「昨天看过」比一串时间戳有用。 */
function ago(updatedAt: number): string {
  const minutes = Math.max(0, Math.floor((Date.now() - updatedAt) / 60_000));
  if (minutes < 1) return "刚刚";
  if (minutes < 60) return `${minutes} 分钟前`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} 小时前`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days} 天前`;
  return new Date(updatedAt).toLocaleDateString("zh-CN");
}

export function RecentFavoritesView({
  refresh,
  onPick,
}: {
  refresh?: number;
  onPick: (seriesId: string) => void;
}) {
  const history = useAsync(() => listHistory(), [refresh]);
  const favorites = useAsync(() => listFavorites(), [refresh]);
  // 清空后本地状态立刻归零，否则得等 IPC 回来才能确认界面已经干净。
  const [cleared, setCleared] = useState(false);
  const [showAllFavorites, setShowAllFavorites] = useState(false);
  // 取消收藏即时生效在本地列表上，不等两个 IPC 往返。
  const [favRows, setFavRows] = useState<FavoriteEntry[] | null>(null);

  const loaded = favRows ?? favorites.data ?? [];
  const favList = [...loaded].sort((a, b) => b.updatedAt - a.updatedAt);
  const seenIds = new Set(favList.map((row) => row.seriesId));
  const watched = cleared ? [] : (history.data ?? []).filter((row) => !seenIds.has(row.seriesId));
  const shown = showAllFavorites || favList.length <= FAVORITES_PREVIEW
    ? favList
    : favList.slice(0, FAVORITES_PREVIEW);
  const loading = (history.loading && history.data === null) || (favorites.loading && favorites.data === null && favRows === null);
  const error = history.error ?? favorites.error;
  const retry = () => {
    setCleared(false);
    history.retry();
    favorites.retry();
  };

  const removeFavorite = async (seriesId: string) => {
    const base = favList;
    const entry = base.find((row) => row.seriesId === seriesId);
    const said = `已取消收藏《${entry?.seriesTitle ?? ""}》`;
    setFavRows(base.filter((row) => row.seriesId !== seriesId));
    setFavoriteId(seriesId, false);
    toast(said);
    try {
      await toggleFavorite({
        seriesId,
        seriesTitle: entry?.seriesTitle ?? "",
        cover: entry?.cover ?? "",
        category: entry?.category ?? "",
      });
    } catch {
      // 以 Rust 侧为准再拉一次：失败就回到原样，成功但本地快照过时也能对齐。
      const rows = await listFavorites().catch(() => base);
      setFavRows(rows);
      setFavoriteId(seriesId, rows.some((row) => row.seriesId === seriesId));
      toast("取消没存上，已回到原样");
    }
  };

  const subtitle = [
    favList.length > 0 ? `${favList.length} 部收藏` : null,
    watched.length > 0 ? `${watched.length} 部最近看过` : null,
  ].filter(Boolean).join(" · ");

  return (
    <section className="view">
      <header className="view-head">
        <h2>我的</h2>
        <span className="view-sub">
          {subtitle}
          {subtitle.length > 0 && " · "}
          <button type="button" className="link" onClick={retry}>
            刷新
          </button>
        </span>
      </header>

      {error !== null && (
        <Status tone="error">
          {error} ·{" "}
          <button type="button" onClick={retry}>
            重试
          </button>
        </Status>
      )}

      {favList.length === 0 && watched.length === 0 && error === null && (
        loading ? (
          <Skeleton cards={6} rows={1} />
        ) : (
          <Status>空空如也</Status>
        )
      )}

      {favList.length > 0 && (
        <section className="favorite-section" aria-label="收藏">
          <h3>最近收藏</h3>
          <div className="grid">
            {shown.map((row) => (
              <button key={`fav-${row.seriesId}`} type="button" className="card" onClick={() => onPick(row.seriesId)}>
                <span
                  className="fav-remove"
                  role="button"
                  aria-label={`取消收藏 ${row.seriesTitle}`}
                  title="取消收藏"
                  onClick={(event) => {
                    event.stopPropagation();
                    void removeFavorite(row.seriesId);
                  }}
                >
                  ✕
                </span>
                <Cover src={row.cover} alt={row.seriesTitle} title={row.seriesTitle}>
                  <span className="badge count">已收藏</span>
                </Cover>
                <span className="card-body">
                  <span className="title">{row.seriesTitle}</span>
                  <span className="meta">{ago(row.updatedAt)}收藏</span>
                </span>
              </button>
            ))}
          </div>
          {favList.length > FAVORITES_PREVIEW && !showAllFavorites && (
            <button type="button" className="link" onClick={() => setShowAllFavorites(true)}>
              查看全部 {favList.length} 部收藏
            </button>
          )}
        </section>
      )}

      {watched.length > 0 && (
        <>
          <section className="history-section" aria-label="最近观看">
            {favList.length > 0 && <h3>最近观看</h3>}
            <div className="grid history-grid">
              {watched.map((row: ProgressRow) => (
                <button key={`${row.seriesId}-${row.vid}`} type="button" className="card" onClick={() => onPick(row.seriesId)}>
                  <Cover src={row.cover} alt={row.seriesTitle} title={row.seriesTitle}>
                    <span className="badge count">{row.finished ? "完播" : `看到第 ${row.vidIndex} 集`}</span>
                  </Cover>
                  <span className="card-body">
                    <span className="title">{row.seriesTitle}</span>
                    <span className="meta">{ago(row.updatedAt)}</span>
                  </span>
                </button>
              ))}
            </div>
          </section>
          <p className="history-foot">
            <button
              type="button"
              className="danger"
              onClick={async () => {
                await clearHistory().catch(() => undefined);
                setCleared(true);
              }}
            >
              清空历史
            </button>
            <span className="view-sub">收藏不受清空影响。</span>
          </p>
        </>
      )}

      {favList.length > 0 && watched.length === 0 && (
        <p className="history-foot">
          <span className="view-sub">收藏里的剧都还没看过，点卡片进去就能播；取消收藏点卡片右上角的 ✕。</span>
        </p>
      )}
    </section>
  );
}
