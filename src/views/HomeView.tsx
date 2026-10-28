/**
 * 首页：把热播榜 / 真人剧 / 漫剧 / AI 剧四个红果类目合成一页，Netflix 式排版。
 *
 * 顶部一条「继续观看」是这台机器自己的记录，跨四个类目聚合（只认红果的类目，
 * 河马/西饭的历史留给它们自己的页签）。往下每个类目铺开成若干横向行：类目本身
 * 一行，它的每个题材（爱情、年代…）再各占一行——全部题材都成行，横向滚动。
 * 首页不放分页按钮，每行右侧的「查看全部」才进带分页的类目页。
 *
 * 行是懒加载的：先各取一次类目首页读出题材清单，题材行滚到附近（IntersectionObserver
 * 提前 300px）才发那一行第一页的请求，免得一进首页就打几十个并发。类目级和榜单
 * 请求失败各自退化，不整页崩。
 */

import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { browseList, listHistory, type Episode, type SeriesCard } from "../ipc/client";
import { ContinueRail, PosterRail, Status } from "../components/Cards";
import { useAsync } from "../hooks/useAsync";

const RECENT_LIMIT = 10;

/** 首页合并的四个红果大类：key 是路由/记录用的类目，label 是行标题前缀。 */
export const ROWS: { category: string; label: string }[] = [
    { category: "hot-drama", label: "热播短剧" },
    { category: "real-drama", label: "真人剧" },
    { category: "comic-drama", label: "漫剧" },
    { category: "ai-drama", label: "AI 剧" },
];

const REDGUO = new Set(ROWS.map((row) => row.category));

/** 类目 key → 中文名：详情页面包屑要显示「来自哪一类」。 */
export function categoryLabel(category: string): string {
    if (category === "xifan") return "西饭短剧";
    if (category === "hema") return "河马短剧";
    return ROWS.find((row) => row.category === category)?.label ?? "";
}

/** 一行的外壳：标题在左、「查看全部 ›」在右，下面接轨道内容。第三方页签的横向行也复用。 */
export function RailSection({
    title,
    onSeeAll,
    children,
}: {
    title: string;
    onSeeAll: () => void;
    children: ReactNode;
}) {
    return (
        <section className="home-row">
            <header className="home-row-head">
                <h3>{title}</h3>
                <button type="button" className="see-all" onClick={onSeeAll}>
                    查看全部 ›
                </button>
            </header>
            {children}
        </section>
    );
}

/**
 * 一个题材行：进视口才发第一页请求。`active` 一旦翻真就断开观察器，请求只发一次；
 * 之后靠后端 120s 目录缓存复用。请求失败按「没有内容」退化，不整页崩。第三方页签
 * （河马/西饭）的横向行同样复用，标题前缀由调用方给。
 */
export function GenreRail({
    category,
    genre,
    title,
    refresh,
    onPick,
    onSeeAll,
}: {
    category: string;
    genre: string;
    title: string;
    refresh: number;
    onPick: (card: SeriesCard, category: string) => void;
    onSeeAll: (category: string, genre: string) => void;
}) {
    const ref = useRef<HTMLElement>(null);
    const [active, setActive] = useState(false);

    useEffect(() => {
        if (active) return;
        const el = ref.current;
        if (el === null || typeof IntersectionObserver === "undefined") {
            setActive(true);
            return;
        }
        const io = new IntersectionObserver(
            (entries) => {
                if (entries.some((entry) => entry.isIntersecting)) {
                    setActive(true);
                    io.disconnect();
                }
            },
            { rootMargin: "300px" },
        );
        io.observe(el);
        return () => io.disconnect();
    }, [active]);

    const state = useAsync(
        () =>
            active
                ? browseList(category, genre, 1).catch(() => null)
                : Promise.resolve(null),
        [active, category, genre, refresh],
        { cacheKey: `browse:${category}:${genre}:1`, keepStale: true },
    );
    const page = state.data;

    return (
        <section className="home-row" ref={ref}>
            <header className="home-row-head">
                <h3>{title}</h3>
                <button type="button" className="see-all" onClick={() => onSeeAll(category, genre)}>
                    查看全部 ›
                </button>
            </header>
            {page !== null && page.results.length > 0 ? (
                <PosterRail cards={page.results} onPick={(card) => onPick(card, category)} />
            ) : state.loading ? (
                <p className="view-sub rail-hint">加载中…</p>
            ) : (
                <p className="view-sub rail-hint">这一类暂时没有内容。</p>
            )}
        </section>
    );
}

export function HomeView({
    refresh,
    onPick,
    onPlay,
    onSeeAll,
}: {
    refresh: number;
    onPick: (card: SeriesCard, category: string) => void;
    onPlay: (episode: Episode, resumeAt?: number) => void;
    onSeeAll: (category: string, genre: string) => void;
}) {
    // 一次取四个类目的首页：既给「总览行」出内容，又读出各类目的题材清单。
    // cacheKey 固定不带 refresh：先画上一次缓存的片单，refresh 只留在依赖里驱动后台
    // 静默重取（revalidate），聚焦/返回才不倒回骨架屏。整组请求失败时 keepStale 留旧片单。
    const meta = useAsync(
        () => Promise.all(ROWS.map((row) => browseList(row.category, "", 1).catch(() => null))),
        [refresh],
        { cacheKey: "home:meta", keepStale: true },
    );
    const recent = useAsync(() => listHistory(""), [refresh], {
        cacheKey: "home:recent",
        keepStale: true,
    });
    const recentRows = useMemo(
        () => (recent.data ?? []).filter((row) => REDGUO.has(row.category)).slice(0, RECENT_LIMIT),
        [recent.data],
    );

    const pages = meta.data;
    const allFailed = pages !== null && pages.every((page) => page === null);

    return (
        <section className="view home">
            <ContinueRail rows={recentRows} categoryOf={(row) => row.category} onPlay={onPlay} />

            {meta.error !== null && (
                <Status tone="error">
                    {meta.error} ·{" "}
                    <button type="button" onClick={meta.retry}>
                        重试
                    </button>
                </Status>
            )}
            {allFailed && meta.error === null && (
                <Status tone="warn">
                    暂时没有加载到内容。
                    <button type="button" className="link" onClick={meta.retry}>
                        重试
                    </button>
                </Status>
            )}
            {pages === null && meta.error === null && <Status>正在加载首页…</Status>}

            {pages !== null &&
                ROWS.map((row, index) => {
                    const page = pages[index];
                    if (page === null) return null;
                    return (
                        <div key={row.category} className="home-group">
                            <RailSection title={row.label} onSeeAll={() => onSeeAll(row.category, "")}>
                                {page.results.length > 0 ? (
                                    <PosterRail cards={page.results} onPick={(card) => onPick(card, row.category)} />
                                ) : (
                                    <p className="view-sub rail-hint">这一类暂时没有内容。</p>
                                )}
                            </RailSection>
                            {page.genres.map((item) => (
                                <GenreRail
                                    key={item.slug}
                                    category={row.category}
                                    genre={item.slug}
                                    title={`${row.label} · ${item.label}`}
                                    refresh={refresh}
                                    onPick={onPick}
                                    onSeeAll={onSeeAll}
                                />
                            ))}
                        </div>
                    );
                })}
        </section>
    );
}
