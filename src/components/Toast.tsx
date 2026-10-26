/**
 * 一条极短的页内提示（toast）：模块级 `toast(text)` 让任何视图都能直接喊一声，
 * 不需要往上抬一层状态；`<Toaster />` 挂在 App 外壳里消费它。
 *
 * 收藏这类「点了就该有个交代」的动作，按钮自己的文案变化在窗口另一端可能被
 * 忽略，一条会自己消失的提示比再摆个弹窗便宜。同时只留最后一条，免得连点刷屏。
 */

import { useEffect, useRef, useState } from "react";

export interface ToastItem {
  id: number;
  text: string;
}

let queue: ToastItem[] = [];
const listeners = new Set<(items: ToastItem[]) => void>();

export function toast(text: string): void {
  queue = [...queue, { id: Date.now() + Math.random(), text }].slice(-1);
  for (const listener of listeners) listener(queue);
}

export function Toaster() {
  const [items, setItems] = useState<ToastItem[]>(queue);
  // 已排定过期时间的条：新条进来只给没排过的排队，否则每次追加都会把
  // 旧条的计时器推倒重来，一条能挂好几百秒。
  const scheduled = useRef(new Set<number>());

  useEffect(() => {
    listeners.add(setItems);
    return () => {
      listeners.delete(setItems);
    };
  }, []);

  useEffect(() => {
    const fresh = items.filter((item) => !scheduled.current.has(item.id));
    if (fresh.length === 0) return;
    for (const item of fresh) scheduled.current.add(item.id);
    const timers = fresh.map((item) =>
      setTimeout(() => {
        scheduled.current.delete(item.id);
        queue = queue.filter((row) => row.id !== item.id);
        for (const listener of listeners) listener(queue);
      }, 1_800),
    );
    return () => timers.forEach(clearTimeout);
  }, [items]);

  if (items.length === 0) return null;
  return (
    <div className="toaster" role="status" aria-live="polite">
      {items.map((item) => (
        <span key={item.id} className="toast">
          {item.text}
        </span>
      ))}
    </div>
  );
}
