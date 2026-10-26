/**
 * Catalogue and ranking pages.
 *
 * All three catalogues come from one Rust command — the tab key is the routing
 * key — so this view only has to keep three pieces of state in the URL-free app
 * shell: which tab, which genre chip, which page.
 *
 * 最前面那一行是这台机器自己的记录，而且只限当前类目（Rust 侧按页签过滤），
 * 全部类目合起来的历史在专门的「我的」页。
 *
 * 河马/西饭没选题材时改成首页那套横向滚动排版：总览一行 + 每个题材一行（懒加载，
 * 复用 HomeView 的 GenreRail）；行尾「查看全部」才落到带题材条和分页的网格页。
 * 红果类目维持网格加分页不变。
 */

import { browseList, listHistory, type Episode, type Genre, type SeriesCard } from "../ipc/client";
import { ContinueRail, Pager, PosterRail, SeriesGrid, Skeleton, Status } from "../components/Cards";
import { GenreRail, RailSection } from "./HomeView";
import { useAsync } from "../hooks/useAsync";

const RECENT_LIMIT = 10;

/** 这两个第三方页签用首页式横向轨道排版。 */
const RAIL_TABS = new Set(["hema", "xifan"]);

export function BrowseView({
  category,
  genre,
  page,
  refresh,
  onChange,
  onPick,
  onPlay,
}: {
  category: string;
  genre: string;
  page: number;
  refresh: number;
  onChange: (next: { category?: string; genre?: string; page?: number }) => void;
  onPick: (card: SeriesCard) => void;
  onPlay: (episode: Episode, resumeAt?: number) => void;
}) {
  const state = useAsync(() => browseList(category, genre, page), [category, genre, page, refresh]);
  const recent = useAsync(() => listHistory(category), [category, refresh]);
  const data = state.data;

  if (state.loading && data === null) return <Skeleton cards={12} rows={2} />;
  if (state.error !== null) {
    return (
      <>
        <Status tone="error">
          {state.error} ·{" "}
          <button type="button" onClick={state.retry}>
            重试
          </button>
        </Status>
      </>
    );
  }
  if (data === null) return null;

  const genres: Genre[] = data.genres;
  const rows = (recent.data ?? []).slice(0, RECENT_LIMIT);

  if (RAIL_TABS.has(category) && genre === "") {
    return (
      <section className="view home">
        <ContinueRail rows={rows} categoryOf={() => category} onPlay={onPlay} />

        {data.unsupported === true && (
          <header className="view-head">
            <h2>{data.pageTitle}</h2>
            <span className="tag tag-warn">该分类内容形态暂不支持</span>
          </header>
        )}

        {data.emptyMessage !== undefined ? (
          <Status tone="warn">{data.emptyMessage}</Status>
        ) : (
          <div className="home-group">
            <RailSection title={data.pageTitle} onSeeAll={() => onChange({ genre: "", page: 1 })}>
              <PosterRail cards={data.results} onPick={onPick} />
            </RailSection>
            {genres.map((item) => (
              <GenreRail
                key={item.slug}
                category={category}
                genre={item.slug}
                title={`${data.pageTitle} · ${item.label}`}
                refresh={refresh}
                onPick={onPick}
                onSeeAll={(_category, nextGenre) => onChange({ genre: nextGenre, page: 1 })}
              />
            ))}
          </div>
        )}
      </section>
    );
  }

  return (
    <section className="view">
      <ContinueRail rows={rows} categoryOf={() => category} onPlay={onPlay} />

      <header className="view-head">
        <h2>{data.pageTitle}</h2>
        {data.unsupported === true && <span className="tag tag-warn">该分类内容形态暂不支持</span>}
      </header>

      {genres.length > 0 && (
        <div className="chips">
          <button type="button" className={genre === "" ? "chip current" : "chip"} onClick={() => onChange({ genre: "", page: 1 })}>
            全部
          </button>
          {genres.map((item) => (
            <button
              key={item.slug}
              type="button"
              className={genre === item.slug ? "chip current" : "chip"}
              onClick={() => onChange({ genre: item.slug, page: 1 })}
            >
              {item.label}
            </button>
          ))}
        </div>
      )}

      {data.emptyMessage !== undefined ? (
        <Status tone="warn">{data.emptyMessage}</Status>
      ) : (
        <>
          <SeriesGrid cards={data.results} onPick={onPick} />
          <Pager page={data.page} totalPages={data.totalPages} onChange={(next) => onChange({ page: next })} />
        </>
      )}
    </section>
  );
}
