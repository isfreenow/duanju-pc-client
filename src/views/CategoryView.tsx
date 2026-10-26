/**
 * 「查看全部」页：从首页某一行的「查看全部 ›」进来，铺开这个大类目下的所有子类目
 * （题材 chips），并停在进入时所在的那个题材上。
 *
 * 进来先把内容区滚回顶部（首页那行往往已经往下滚了一段）。往下不用翻页按钮，也
 * 不用手动点「加载更多」：滚到底附近就自动用 ajax 取下一页、往当前网格后面追加，
 * 已加载的卡片不清空。来源对榜单/目录页都只给当前页有没有内容，凑不出可靠的总页数，
 * 所以取到空（或去重后没新增）就自动停住、显示「没有更多了」。
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { browseList, type CatalogPage, type SeriesCard } from "../ipc/client";
import { GenreChips, SeriesGrid, Skeleton, Status } from "../components/Cards";

export function CategoryView({
    category,
    genre,
    page = 1,
    refresh,
    onChange,
    onPick,
    onBack,
}: {
    category: string;
    genre: string;
    page?: number;
    refresh: number;
    onChange: (next: { genre?: string; page?: number }) => void;
    onPick: (card: SeriesCard) => void;
    onBack: () => void;
}) {
    const [head, setHead] = useState<CatalogPage | null>(null);
    const [items, setItems] = useState<SeriesCard[]>([]);
    const [loading, setLoading] = useState(true);
    const [loadingMore, setLoadingMore] = useState(false);
    const [error, setError] = useState<string | null>(null);
    const [moreError, setMoreError] = useState<string | null>(null);
    const [hasMore, setHasMore] = useState(true);
    // 已经取到第几页的下一页号；失败不前进，重试还从这一页接着取。
    const nextPage = useRef(page);
    // 已上屏的剧 id 集：来源分页偶尔会重叠，追加时靠它去重，去重后没新增就当作到底。
    const loadedIds = useRef<Set<string>>(new Set());
    // 一次只允许一个「加载更多」在飞，避免 sentinel 反复命中造成并发取同一页。
    const busy = useRef(false);
    const mounted = useRef(true);
    const sectionRef = useRef<HTMLElement>(null);
    const sentinelRef = useRef<HTMLDivElement>(null);

    // 取首页：本组件按 `category:genre` 挂了 key，换题材会整块重挂，这里只管挂载即取第一页。
    // `force` 只有窗口聚焦刷新那一次传 true，打穿后端目录缓存；重试等普通重取照走缓存。
    const loadFirst = useCallback((force = false) => {
        setLoading(true);
        setError(null);
        setMoreError(null);
        setItems([]);
        setHead(null);
        setHasMore(true);
        nextPage.current = page;
        browseList(category, genre, page, force)
            .then((result) => {
                if (!mounted.current) return;
                setHead(result);
                setItems(result.results);
                loadedIds.current = new Set(result.results.map((card) => card.seriesId));
                nextPage.current = result.page + 1;
                setHasMore(result.emptyMessage === undefined && result.results.length > 0);
                setLoading(false);
            })
            .catch((rejected: unknown) => {
                if (!mounted.current) return;
                setError(rejected instanceof Error ? rejected.message : String(rejected));
                setLoading(false);
            });
    }, [category, genre, page]);

    useEffect(() => {
        mounted.current = true;
        loadFirst();
        return () => {
            mounted.current = false;
        };
    }, [loadFirst]);

    // 窗口重新聚焦/可见（refresh 自增）时回到顶部重取第一页，并绕过后端目录缓存。
    useEffect(() => {
        if (refresh === 0) return;
        loadFirst(true);
    }, [refresh, loadFirst]);

    // 进入本页 / 换题材后把内容区滚回顶部，让用户从网格开头看起。
    useEffect(() => {
        sectionRef.current?.scrollIntoView({ block: "start", inline: "nearest" });
    }, [category, genre]);

    const loadMore = useCallback(() => {
        if (busy.current) return;
        busy.current = true;
        setLoadingMore(true);
        browseList(category, genre, nextPage.current)
            .then((result) => {
                busy.current = false;
                if (!mounted.current) return;
                nextPage.current = result.page + 1;
                setHead((prev) => (prev ? { ...prev, page: result.page } : prev));
                const fresh = result.results.filter((card) => !loadedIds.current.has(card.seriesId));
                fresh.forEach((card) => loadedIds.current.add(card.seriesId));
                if (fresh.length > 0) setItems((prev) => [...prev, ...fresh]);
                // 去重后没有新内容，或本页就是空/不支持 → 认为到底，别再自动翻。
                setHasMore(result.emptyMessage === undefined && fresh.length > 0);
                setLoadingMore(false);
            })
            .catch((rejected: unknown) => {
                busy.current = false;
                if (!mounted.current) return;
                setMoreError(rejected instanceof Error ? rejected.message : String(rejected));
                setHasMore(false);
                setLoadingMore(false);
            });
    }, [category, genre]);

    // 到底部附近自动取下一页；换题材/首次加载完成/追加后 items 变化都会重新挂观察。
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

    // 加载更多失败后的重试：清掉错误、恢复 hasMore，再从上次那页接着取（nextPage 未前进）。
    const retryMore = useCallback(() => {
        setMoreError(null);
        setHasMore(true);
        loadMore();
    }, [loadMore]);

    return (
        <section className="view" ref={sectionRef}>
            <div className="category-top">
                <header className="view-head crumbs">
                    <button type="button" className="crumb-home" onClick={onBack}>
                        首页
                    </button>
                    <span className="crumb-sep" aria-hidden="true">›</span>
                    <h2>{head === null ? "" : head.pageTitle}</h2>
                    {head !== null && head.unsupported === true && (
                        <span className="tag tag-warn">该分类内容形态暂不支持</span>
                    )}
                </header>
                {head !== null && (
                    <GenreChips genres={head.genres} value={genre} onChange={(slug) => onChange({ genre: slug, page: 1 })} />
                )}
            </div>

            {loading && head === null && <Skeleton cards={12} rows={2} />}
            {error !== null && (
                <Status tone="error">
                    {error} ·{" "}
                    <button type="button" onClick={() => loadFirst()}>
                        重试
                    </button>
                </Status>
            )}

            {head !== null && (
                <>
                    {head.emptyMessage !== undefined ? (
                        <Status tone="warn">{head.emptyMessage}</Status>
                    ) : items.length === 0 ? (
                        <Status tone="warn">这一页没有内容，换个题材看看。</Status>
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
                    )}
                </>
            )}
        </section>
    );
}
