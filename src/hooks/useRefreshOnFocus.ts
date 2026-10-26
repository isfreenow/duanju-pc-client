/**
 * 窗口不关也要能换血。
 *
 * 桌面程序一旦挂着不关，各视图的取数都停在挂载那一刻：后端有 120s 目录缓存，
 * 前端却一次 mount 只发一次请求，挂机几小时后看到的还是早上的榜单。这个 hook
 * 监听窗口重新聚焦 / 页面重新可见，返回一个自增的刷新计数，数据视图把它放进
 * 取数依赖里，回到窗口就重新拉一次。
 */

import { useEffect, useState } from "react";

export function useRefreshOnFocus(): number {
  const [key, setKey] = useState(0);
  useEffect(() => {
    const bump = () => setKey((value) => value + 1);
    const onVisible = () => {
      if (document.visibilityState === "visible") bump();
    };
    window.addEventListener("focus", bump);
    document.addEventListener("visibilitychange", onVisible);
    return () => {
      window.removeEventListener("focus", bump);
      document.removeEventListener("visibilitychange", onVisible);
    };
  }, []);
  return key;
}
