/**
 * Search results.
 *
 * The command behind this defaults to the website endpoint — measured 2026-10-26,
 * the signed App search answers 200 with an empty body, so the cursor it returns
 * is carried along but only ever used if a later build wants to try that route
 * again. The UI shows which channel answered so a silent fallback is visible.
 *
 * `category` 是顶栏当前页签：西饭时搜索走自己来源，其余都走官网。
 *
 * 河马暂不支持付费播放：菜单入口已隐藏，结果里再过滤掉 hema 开头的卡兜底，
 * 不让搜出来的河马剧点进详情页后被整页付费集拦死。
 *
 * 和类目页一样不做翻页按钮：滚到底部附近自动取下一页并往网格后面追加，已加载的
 * 结果不清空。取到空页、或去重后没有新增、或来源明确说没有更多，就停住显示
 * 「没有更多了」。换关键词/换页签时从第一页重新来。
 *
 * `autoPlay` 是外部指令（`--watch`）带进来的：第一页到手后不等人点，直接挑标题
 * 最贴关键词的那张卡交给 `onPick`。只发生一次，失败或没结果就停在结果页。
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { searchSeries, type SearchResult, type SeriesCard } from "../ipc/client";
import { SeriesGrid, Skeleton, Status } from "../components/Cards";

/** 河马暂不支持付费播放，搜索结果不透出；系列编号以 `hema:` 开头自解释来源。 */
const isHemaCard = (card: SeriesCard) => card.seriesId.startsWith("hema:");

/** 比标题和关键词前先拉掉空白与常见标点，否则「吃械入宋」对不上「《吃械入宋》」。 */
function flatten(value: string): string {
  return value.toLowerCase().replace(/[\s《》【】()（）·、,，.。!！?？:：;；\-_]/g, "");
}

/** 第一条不一定是最想要的那条：完全同名优先，其次互为包含，都不中就回到第一条。 */
function bestMatch(cards: SeriesCard[], keyword: string): SeriesCard {
  const want = flatten(keyword);
  return (
    cards.find((card) => flatten(card.seriesTitle) === want) ??
    cards.find((card) => {
      const have = flatten(card.seriesTitle);
      return have.includes(want) || (have.length > 0 && want.includes(have));
    }) ??
    cards[0]
  );
}

export function SearchView({
  keyword,
  category,
  autoPlay = 0,
  onPick,
}: {
  keyword: string;
  /** 顶栏页签 key；空串表示官网。 */
  category: string;
  /** 外部指令带进来的「替用户点一下第一条」，带的是指令序号；0 就是没有人下过。 */
  autoPlay?: number;
  onPick: (card: SeriesCard) => void;
}) {
  const [head, setHead] = useState<SearchResult | null>(null);
  const [items, setItems] = useState<SeriesCard[]>([]);
  const [loading, setLoading] = useState(true);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [moreError, setMoreError] = useState<string | null>(null);
  const [hasMore, setHasMore] = useState(true);
  // 已经取到第几页的下一页号；失败不前进，重试还从这一页接着取。
  const nextPage = useRef(2);
  // 已上屏的剧 id 集：来源分页偶尔会重叠，追加时靠它去重，去重后没新增就当作到底。
  const loadedIds = useRef<Set<string>>(new Set());
  // 一次只允许一个「加载更多」在飞，避免 sentinel 反复命中造成并发取同一页。
  const busy = useRef(false);
  const mounted = useRef(true);
  // 自动选中只做一次；`onPick` 在上层是内联箭头函数，放进依赖会让第一页无限重取，
  // 所以拿 ref 转手。
  const autoPick = useRef(autoPlay > 0);
  const pickRef = useRef(onPick);
  pickRef.current = onPick;

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  // 取第一页：换关键词或换页签时整块重取，已追加的后面页丢弃。
  const loadFirst = useCallback(() => {
    if (keyword.trim() === "") return;
    setLoading(true);
    setError(null);
    setMoreError(null);
    setItems([]);
    setHead(null);
    setHasMore(true);
    searchSeries(keyword, 1, undefined, category)
      .then((result) => {
        if (!mounted.current) return;
        const visible = result.results.filter((card) => !isHemaCard(card));
        setHead(result);
        setItems(visible);
        loadedIds.current = new Set(visible.map((card) => card.seriesId));
        nextPage.current = result.page + 1;
        setHasMore(result.hasMore && visible.length > 0);
        setLoading(false);
        if (autoPick.current && visible.length > 0) {
          autoPick.current = false;
          pickRef.current(bestMatch(visible, keyword));
        }
      })
      .catch((rejected: unknown) => {
        if (!mounted.current) return;
        setError(rejected instanceof Error ? rejected.message : String(rejected));
        setLoading(false);
      });
  }, [keyword, category]);

  useEffect(() => {
    loadFirst();
  }, [loadFirst]);

  const loadMore = useCallback(() => {
    if (busy.current) return;
    busy.current = true;
    setLoadingMore(true);
    searchSeries(keyword, nextPage.current, undefined, category)
      .then((result) => {
        busy.current = false;
        if (!mounted.current) return;
        nextPage.current = result.page + 1;
        setHead((prev) => (prev ? { ...prev, page: result.page } : prev));
        const fresh = result.results.filter(
          (card) => !isHemaCard(card) && !loadedIds.current.has(card.seriesId),
        );
        fresh.forEach((card) => loadedIds.current.add(card.seriesId));
        if (fresh.length > 0) setItems((prev) => [...prev, ...fresh]);
        // 去重后没有新内容，或来源说没有更多 → 认为到底，别再自动翻。
        setHasMore(result.hasMore && fresh.length > 0);
        setLoadingMore(false);
      })
      .catch((rejected: unknown) => {
        busy.current = false;
        if (!mounted.current) return;
        setMoreError(rejected instanceof Error ? rejected.message : String(rejected));
        setHasMore(false);
        setLoadingMore(false);
      });
  }, [keyword, category]);

  // 到底部附近自动取下一页；换关键词/首次加载完成/追加后 items 变化都会重新挂观察。
  useEffect(() => {
    if (loading || moreError !== null || !hasMore) return;
    const el = sentinelRef.current;
    if (el === null || typeof IntersectionObserver === "undefined") return;
    const io = new IntersectionObserver(
      (entries) => {
        if (entries.some((entry) => entry.isIntersecting)) loadMore();
      },
      { rootMargin: "400px 0px" },
    );
    io.observe(el);
    return () => io.disconnect();
  }, [loading, hasMore, moreError, items.length, loadMore]);

  const sentinelRef = useRef<HTMLDivElement>(null);

  // 加载更多失败后的重试：清掉错误、恢复 hasMore，再从上次那页接着取（nextPage 未前进）。
  const retryMore = useCallback(() => {
    setMoreError(null);
    setHasMore(true);
    loadMore();
  }, [loadMore]);

  if (keyword.trim() === "") return <Status>输入剧名开始搜索。</Status>;

  return (
    <section className="view">
      <header className="view-head">
        <h2>“{keyword}” 的搜索结果</h2>
        {head !== null && head.note !== undefined && <span className="view-sub">{head.note}</span>}
      </header>

      {loading && head === null && <Skeleton cards={12} rows={1} />}
      {error !== null && (
        <Status tone="error">
          {error} ·{" "}
          <button type="button" onClick={loadFirst}>
            重试
          </button>
        </Status>
      )}

      {head !== null && (
        items.length === 0 ? (
          <Status tone="warn">没有结果，换个关键词试试。</Status>
        ) : (
          <>
            <SeriesGrid cards={items} onPick={onPick} />
            <div className="load-more">
              {moreError !== null ? (
                <Status tone="warn">
                  {moreError} ·{" "}
                  <button type="button" onClick={retryMore}>
                    重试
                  </button>
                </Status>
              ) : hasMore ? (
                <>
                  <div ref={sentinelRef} className="load-more-sentinel" />
                  {loadingMore && <span className="load-more-end">加载中…</span>}
                </>
              ) : (
                <span className="load-more-end">没有更多了</span>
              )}
            </div>
          </>
        )
      )}
    </section>
  );
}
