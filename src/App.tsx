/**
 * App shell: a header, four tabs, and one piece of routing state.
 *
 * There is no router library because there are a handful of views and a parameter
 * each. The rule the rest of the app depends on lives here: a view receives
 * business objects and an opaque `seriesId`/`vid`, and never assembles a request
 * URL.
 *
 * 顶栏只剩三个入口：首页（合并热播榜/真人剧/漫剧/AI 剧四个红果类目，Netflix 式
 * 横滚）、西饭短剧、「我的」（观看历史与收藏融合在一页）。河马短剧因暂不支持
 * 付费播放隐藏了入口（搜索结果也过滤掉 hema 开头的卡），数据源本身保留。红果四个
 * 类目不再是各自带分页的页签，而是
 * 首页里的一行行轨道；某一行点「查看全部」才进类目页（`all`）——进去先回到顶部，
 * 往下用「加载更多」 ajax 追加，不再放翻页按钮。
 *
 * 播放页是唯一占满整个窗口的路由：进了播放就没有顶栏、没有页脚，退出或 Esc
 * 回到进来的那一页。切换上一集/下一集只换 episode，`from` 保持不变。
 *
 * `category` 跟着路由走：播放器把它写进观看记录，首页顶部的「继续观看」才能按
 * 类目筛出红果的记录，第三方页签各自只看自己的历史。
 *
 * 外部指令（`--watch 关键词`，给 AI Agent 和脚本用，见 README）先落搜索结果页，
 * 路由上带一个 `autoPlay`：搜索页挑最匹配的一张卡进详情页，详情页再按历史决定
 * 从哪一集、哪一秒开播。人自己点搜索时没有这个标记，两段都不会自动发生。
 */

import { useCallback, useEffect, useRef, useState } from "react";
import type { Episode, LaunchCommand, SeriesCard } from "./ipc/client";
import { onLaunchCommand, takeLaunchCommand } from "./ipc/client";
import { syncFavoriteIds } from "./favorites";
import { useRefreshOnFocus } from "./hooks/useRefreshOnFocus";
import { HomeView } from "./views/HomeView";
import { CategoryView } from "./views/CategoryView";
import { BrowseView } from "./views/BrowseView";
import { SearchView } from "./views/SearchView";
import { SeriesView } from "./views/SeriesView";
import { PlayerView } from "./views/PlayerView";
import { RecentFavoritesView } from "./views/RecentFavoritesView";
import { WindowControls } from "./components/WindowControls";
import { Toaster } from "./components/Toast";
import { UpdatePill } from "./updater";
import { CursorFX } from "./components/CursorFX";
import "./App.css";

const TABS: { key: string; label: string }[] = [
  { key: "home", label: "首页" },
  // 河马暂不支持付费播放，先隐藏菜单入口；数据源与后端路由保留，放开时加回这一行即可。
  // { key: "hema", label: "河马短剧" },
  { key: "xifan", label: "西饭短剧" },
  { key: "history", label: "我的" },
];

/** 这两个页签保持独立目录页，同时也是搜索的落点：后端按同一个词表路由。 */
const THIRD_PARTY = new Set(["hema", "xifan"]);

const HOME: Route = { view: "home" };

// 「我的」这个路由 key 仍是 `history`：播放器、历史记录与 e2e 都拿它当类目标记，
// 只有顶栏文案与页面内容换了名字。
type Route =
  | { view: "home" }
  | { view: "all"; category: string; genre: string; page: number }
  | { view: "browse"; category: string; genre: string; page: number }
  | { view: "search"; keyword: string; page: number; category: string; autoPlay?: number }
  | { view: "series"; seriesId: string; category: string; from: Route; autoPlay?: number }
  | { view: "player"; episode: Episode; resumeAt: number; category: string; from: Route }
  | { view: "history" };

/** 当前路由带不带类目：首页/历史页没有，其余都带一个大类 key。 */
function categoryOf(route: Route): string {
  return route.view === "all" || route.view === "browse" || route.view === "search" || route.view === "series" || route.view === "player"
    ? route.category
    : "";
}

export default function App() {
  const [route, setRoute] = useState<Route>(HOME);
  const [tab, setTab] = useState("home");
  const [draft, setDraft] = useState("");
  // 窗口重新聚焦/可见时自增：数据视图把它放进取数依赖里，挂机不关窗也能换血。
  const refresh = useRefreshOnFocus();
  // 收藏快照在全应用只拉这一份，卡片据此标「★ 已收藏」；窗口回来时跟数据一起换新。
  useEffect(() => {
    void syncFavoriteIds();
  }, [refresh]);

  const openTab = (key: string) => {
    setTab(key);
    if (key === "home") setRoute({ view: "home" });
    else if (key === "history") setRoute({ view: "history" });
    else setRoute({ view: "browse", category: key, genre: "", page: 1 });
  };

  /** 首页某行「查看全部」→ 类目页；顶栏仍高亮首页。 */
  const openAll = (category: string, genre: string) => setRoute({ view: "all", category, genre, page: 1 });

  const submitSearch = () => {
    const keyword = draft.trim();
    if (keyword.length === 0) return;
    setRoute({ view: "search", keyword, page: 1, category: THIRD_PARTY.has(tab) ? tab : "" });
  };

  const openSeries = (card: SeriesCard, category: string, autoPlay = 0) =>
    setRoute({ view: "series", seriesId: card.seriesId, category, from: route, autoPlay });

  /** 历史页只有编号，没有整张卡片，所以另开一个入口（类目由记录自己带）。 */
  const openSeriesById = (seriesId: string) => setRoute({ view: "series", seriesId, category: "", from: route });

  /** `resumeAt` 秒 > 0 就是「继续播放」进来的一次，播放器会在元数据就绪后跳过去。 */
  const openEpisode = (episode: Episode, resumeAt = 0) =>
    setRoute((previous) => ({
      view: "player",
      episode,
      resumeAt,
      category: previous.view === "player" ? previous.category : categoryOf(previous),
      from: previous.view === "player" ? previous.from : previous,
    }));

  const back = () => {
    if (route.view === "player" || route.view === "series") setRoute(route.from);
    else if (route.view === "all" || route.view === "history") setRoute({ view: "home" });
  };

  /**
   * 一条外部指令换成路由：只负责送到搜索/详情，往下接力开播由那两页自己做。
   *
   * `autoPlay` 带的是指令序号而不是一个布尔：同一个关键词、同一部剧被再下一次指
   * 令点到时，序号变了才能把那一页重挂一次重新走完整条链（布尔会让重挂失效，
   * 表现为「再喊一遍同一个剧名没反应」）。
   */
  const launchSeq = useRef(0);
  const applyLaunch = useCallback((command: LaunchCommand) => {
    setTab("home");
    launchSeq.current += 1;
    const seq = launchSeq.current;
    if (command.action === "watch") {
      setDraft(command.keyword);
      setRoute({ view: "search", keyword: command.keyword, page: 1, category: "", autoPlay: seq });
      return;
    }
    setRoute((previous) => ({
      view: "series",
      seriesId: command.seriesId,
      category: "",
      from: previous,
      autoPlay: seq,
    }));
  }, []);

  // 冷启动：指令在 Rust 里等着，取一次就清零。已在跑时再启动：只认事件。
  useEffect(() => {
    let alive = true;
    let dispose: (() => void) | undefined;
    takeLaunchCommand()
      .then((command) => {
        if (alive && command !== null) applyLaunch(command);
      })
      .catch(() => { });
    onLaunchCommand((command) => {
      if (alive) applyLaunch(command);
    })
      .then((stop) => {
        if (alive) dispose = stop;
        else stop();
      })
      .catch(() => { });
    return () => {
      alive = false;
      dispose?.();
    };
  }, [applyLaunch]);

  // 播放器整窗接管：不在 main 里，也不带顶栏页脚。
  if (route.view === "player") {
    return (
      <>
        <PlayerView
          episode={route.episode}
          resumeAt={route.resumeAt}
          category={route.category}
          onBack={back}
          onHome={() => openTab("home")}
          onSwitch={openEpisode}
        />
        <Toaster />
        <UpdatePill />
        <CursorFX />
      </>
    );
  }

  return (
    <div className="app">
      <header className="top" data-tauri-drag-region>
        <button type="button" className="brand" onClick={() => openTab("home")}>
          短剧随心看
        </button>
        <nav className="tabs">
          {TABS.map((item) => (
            <button
              key={item.key}
              type="button"
              className={tab === item.key ? "tab current" : "tab"}
              onClick={() => openTab(item.key)}
            >
              {item.label}
            </button>
          ))}
        </nav>
        <form
          className="search"
          onSubmit={(event) => {
            event.preventDefault();
            submitSearch();
          }}
        >
          <input
            value={draft}
            placeholder="搜索剧名"
            aria-label="搜索剧名"
            onChange={(event) => setDraft(event.target.value)}
          />
          <button type="submit">搜索</button>
        </form>
        <WindowControls />
      </header>

      <main className="main">
        {route.view === "home" && <HomeView refresh={refresh} onPick={openSeries} onPlay={openEpisode} onSeeAll={openAll} />}
        {route.view === "all" && (
          <CategoryView
            key={`${route.category}:${route.genre}`}
            category={route.category}
            genre={route.genre}
            page={route.page}
            refresh={refresh}
            onChange={(next) =>
              setRoute({
                view: "all",
                category: route.category,
                genre: next.genre ?? route.genre,
                page: next.page ?? route.page,
              })
            }
            onPick={(card) => openSeries(card, route.category)}
            onBack={back}
          />
        )}
        {route.view === "browse" && (
          <BrowseView
            // 换大类时整块重挂：新目录在来的路上，此刻屏幕上的还是上一类的卡片。
            key={route.category}
            category={route.category}
            genre={route.genre}
            page={route.page}
            refresh={refresh}
            onChange={(next) =>
              setRoute({
                view: "browse",
                category: next.category ?? route.category,
                genre: next.genre ?? route.genre,
                page: next.page ?? route.page,
              })
            }
            onPick={(card) => openSeries(card, route.category)}
            onPlay={openEpisode}
          />
        )}
        {route.view === "search" && (
          <SearchView
            // 序号进 key：同一关键词被再下一次指令点到时才能重挂、重新走完整条链。
            key={`${route.keyword}:${route.autoPlay ?? 0}`}
            keyword={route.keyword}
            category={route.category}
            autoPlay={route.autoPlay ?? 0}
            onPick={(card) => openSeries(card, route.category, route.autoPlay ?? 0)}
          />
        )}
        {route.view === "series" && (
          <SeriesView
            key={`${route.seriesId}:${route.autoPlay ?? 0}`}
            seriesId={route.seriesId}
            category={route.category}
            refresh={refresh}
            autoPlay={route.autoPlay ?? 0}
            onPlay={openEpisode}
            onBack={back}
            onPick={(card) => openSeries(card, route.category)}
          />
        )}
        {route.view === "history" && (
          <RecentFavoritesView refresh={refresh} onPick={openSeriesById} />
        )}
      </main>
      {/* 页内提示：收藏这类即时动作的回馈，不抖弹窗。 */}
      <Toaster />
      <UpdatePill />
      <CursorFX />
    </div>
  );
}
