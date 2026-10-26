/**
 * 收藏清单在前端的一份镜像快照。
 *
 * 卡片满屏都是「这部剧收藏过没有」要问，逐页各拉一次 `list_favorites` 既重复又
 * 会各自不一致，所以这里存一份 seriesId 集合：App 挂载和窗口聚焦刷新时拉全量，
 * 详情页点亮/取消、「我的」页页内 ✕ 取消时直接改这一份，改完订阅方立刻重绘。
 */

import { useSyncExternalStore } from "react";
import { listFavorites } from "./ipc/client";

let ids = new Set<string>();
const listeners = new Set<() => void>();

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** 当前收藏过的剧编号集合。 */
export function useFavoriteIds(): Set<string> {
  return useSyncExternalStore(subscribe, () => ids);
}

/** 拉一次全量收藏替换快照；拉失败保持原样，不把已知的收藏抹掉。 */
export async function syncFavoriteIds(): Promise<void> {
  const rows = await listFavorites().catch(() => null);
  if (rows === null) return;
  ids = new Set(rows.map((row) => row.seriesId));
  for (const listener of listeners) listener();
}

/** 收藏动作确认后同步集合，不等下一次全量拉取。 */
export function setFavoriteId(seriesId: string, favorited: boolean): void {
  if (ids.has(seriesId) === favorited) return;
  ids = new Set(ids);
  if (favorited) ids.add(seriesId);
  else ids.delete(seriesId);
  for (const listener of listeners) listener();
}
