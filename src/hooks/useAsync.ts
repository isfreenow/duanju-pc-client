/**
 * One small loading hook.
 *
 * Every view in this app is "fetch a page of business data, render it", and the
 * parts that are easy to get wrong are the ones this hides: a slow response that
 * lands after the user moved on, and an error that must not leave a spinner
 * turning forever.
 *
 * 两个可选行为服务于本轮的健壮性目标：
 * - `keepStale`：刷新失败时保留屏幕上已有的那一份数据，只把错误标出来，而不是
 *   清空回骨架屏。列表页「失败保留旧片单」就是靠它——网络抖一下不该把用户已经
 *   看到的片单抹掉。
 * - `cacheKey`：接会话级片单缓存（`sessionCache`）。挂载时若命中，先画旧结果、
 *   后台静默重取（revalidate），切回来不闪骨架屏。缓存只存业务片单，不碰播放地址。
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { cacheGet, cacheSet } from "../sessionCache";

export interface AsyncState<T> {
  readonly data: T | null;
  readonly loading: boolean;
  readonly error: string | null;
  readonly retry: () => void;
}

export interface AsyncOptions {
  /** 失败时保留上一次的数据（不清空），只置 error。 */
  readonly keepStale?: boolean;
  /** 会话片单缓存键；命中则先画旧数据并后台重取。 */
  readonly cacheKey?: string;
}

export function useAsync<T>(
  factory: () => Promise<T>,
  deps: readonly unknown[],
  options: AsyncOptions = {},
): AsyncState<T> {
  const seeded = useRef<Seed<T>>(computeSeed<T>(options.cacheKey));
  const [data, setData] = useState<T | null>(seeded.current.data);
  const [loading, setLoading] = useState(!seeded.current.hit);
  const [error, setError] = useState<string | null>(null);
  const [attempt, setAttempt] = useState(0);

  // Bumped for every request; a response is only accepted while it is still the
  // newest one, so a slow page cannot overwrite the view the user moved on.
  const running = useRef(0);
  const mounted = useRef(true);
  const factoryRef = useRef(factory);
  factoryRef.current = factory;
  // 最新一份 data / options 用 ref 读，避免把它们塞进依赖里反复重取。
  const dataRef = useRef<T | null>(data);
  dataRef.current = data;
  const optionsRef = useRef(options);
  optionsRef.current = options;

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  useEffect(() => {
    const id = running.current + 1;
    running.current = id;
    // 屏幕上已经有东西（缓存命中或上一次结果）就不倒回骨架屏，静默重取。
    if (dataRef.current === null) setLoading(true);
    setError(null);
    const cacheKey = optionsRef.current.cacheKey;
    factoryRef
      .current()
      .then((result) => {
        if (!mounted.current || running.current !== id) return;
        setData(result);
        if (cacheKey !== undefined) cacheSet(cacheKey, result);
        setLoading(false);
      })
      .catch((rejected: unknown) => {
        if (!mounted.current || running.current !== id) return;
        setLoading(false);
        setError(rejected instanceof Error ? rejected.message : String(rejected));
        // keepStale 或本来就有数据：留着屏幕上的那份，只报错误；否则清回空。
        const keep = optionsRef.current.keepStale === true || dataRef.current !== null;
        if (!keep) setData(null);
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [...deps, attempt]);

  const retry = useCallback(() => setAttempt((value) => value + 1), []);

  return { data, loading, error, retry };
}

interface Seed<T> {
  data: T | null;
  hit: boolean;
}

/** 读取会话缓存作为初值；`hit` 决定挂载时要不要骨架屏。 */
function computeSeed<T>(cacheKey: string | undefined): Seed<T> {
  if (cacheKey === undefined) return { data: null, hit: false };
  const hit = cacheGet<T>(cacheKey);
  return hit === undefined ? { data: null, hit: false } : { data: hit, hit: true };
}
