/**
 * 会话级片单缓存：把「已经拉到的一次列表结果」留在渲染进程内存里。
 *
 * 为什么要有这一层：首页→类目页→详情页→播放器→返回，同一片单会被反复拉。
 * 每次都对来源发一次签名请求，既慢（骨架屏一闪）又把来源打病。这里让「切回去」
 * 直接命中上一次的结果，先画出来再静默更新（revalidate）。
 *
 * 边界：**只存业务片单（标题、封面、集号、分类页），绝不碰播放地址与密钥**——
 * 那两个短期凭据只在 Rust 侧的 `PlayCache` 里，不落这一层。所以这块内存被读走
 * 也换不来一条能播的地址。
 *
 * 淘汰按条数上限，命中即续期；不做 TTL，因为「一次运行里片单不会自己变旧」，
 * 需要强制换新时视图走 `forceRefresh` 打穿后端缓存即可。
 */

const MAX_KEYS = 48;

interface Row {
    value: unknown;
    touched: number;
}

const rows = new Map<string, Row>();

export function cacheGet<T>(key: string): T | undefined {
    const row = rows.get(key);
    if (row === undefined) return undefined;
    // 命中即续期，让最近看过的片单留在窗口里。
    row.touched = Date.now();
    return row.value as T;
}

export function cacheSet<T>(key: string, value: T): void {
    rows.set(key, { value, touched: Date.now() });
    if (rows.size <= MAX_KEYS) return;
    // 超量先淘汰最久没被命中的那一条。
    let oldestKey: string | undefined;
    let oldest = Number.POSITIVE_INFINITY;
    for (const [candidate, row] of rows) {
        if (row.touched < oldest) {
            oldest = row.touched;
            oldestKey = candidate;
        }
    }
    if (oldestKey !== undefined) rows.delete(oldestKey);
}

export function cacheClear(): void {
    rows.clear();
}
