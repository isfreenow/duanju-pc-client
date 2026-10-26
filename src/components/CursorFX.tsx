/**
 * 红晕光环光标：取代系统指针的一层 fixed 覆盖物。
 *
 * 结构上只有四个常驻节点（光标本体 / 光环 / 辉光 / 尾迹容器）加按需生成的涟漪，
 * 全部走 transform，位置由单个 rAF 更新；状态判定靠 pointerover 委托 +
 * closest()，不为每张卡片挂监听。定制只发生在本窗口内（DOM 覆盖层，不碰
 * 系统光标），整窗包括播放画面都用自绘形状，见 App.css 的 cursor: none。
 */

import { useEffect, useRef } from "react";

/** 判定顺序即优先级：先文本、再拖动、最后普通可点。 */
const TEXT_SEL = "input, textarea, [contenteditable='true']";
const GRAB_SEL = ".rail.dragging, .recent-row.dragging";
const HOT_SEL = "button, a, input, .card, .episode, .side-row, .chip, .tab, .see-all, .crumb-home, .crumb-series";

type RingState = "idle" | "hot" | "text" | "grab";

export function CursorFX() {
    const rootRef = useRef<HTMLDivElement>(null);
    const dotRef = useRef<HTMLSpanElement>(null);
    const ringRef = useRef<HTMLSpanElement>(null);
    const glowRef = useRef<HTMLSpanElement>(null);
    const trailRef = useRef<HTMLDivElement>(null);
    const ripplesRef = useRef<HTMLDivElement>(null);

    useEffect(() => {
        const root = rootRef.current!;
        const dot = dotRef.current!;
        const ring = ringRef.current!;
        const glow = glowRef.current!;
        const trailHost = trailRef.current!;
        const ripples = ripplesRef.current!;

        const reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)");
        const x = { pointer: 0, ring: 0 };
        const y = { pointer: 0, ring: 0 };
        let lastMoveX = 0;
        let lastMoveY = 0;
        let state: RingState = "idle";
        let visible = false;
        let alive = true;
        let raf = 0;
        let idleTimer = 0;
        let trailClock = 0;

        const setState = (next: RingState) => {
            if (next !== state) {
                state = next;
                root.dataset.state = next;
            }
        };

        const classify = (target: EventTarget | null): RingState => {
            const el = target instanceof Element ? target : target instanceof Node ? target.parentElement : null;
            if (!el) return "idle";
            if (el.closest(GRAB_SEL)) return "grab";
            if (el.closest(TEXT_SEL)) return "text";
            if (el.closest(HOT_SEL)) return "hot";
            return "idle";
        };

        const wake = () => {
            if (!visible) {
                visible = true;
                root.dataset.hidden = "0";
            }
            window.clearTimeout(idleTimer);
            // 手停下来后整层淡出：静止的光环比没有光环更碍眼。
            idleTimer = window.setTimeout(() => {
                visible = false;
                root.dataset.hidden = "1";
            }, 2400);
        };

        const onPointerMove = (event: PointerEvent) => {
            if (event.pointerType !== "mouse") return;
            x.pointer = event.clientX;
            y.pointer = event.clientY;
            if (!visible) {
                // 第一次出现时不让环从屏幕另一头追过来。
                x.ring = x.pointer;
                y.ring = y.pointer;
            }
            wake();
        };

        const onPointerOver = (event: PointerEvent) => {
            if (event.pointerType !== "mouse") return;
            setState(classify(event.target));
        };

        const onPointerDown = (event: PointerEvent) => {
            if (event.pointerType !== "mouse" || event.button !== 0) return;
            setState(classify(event.target));
            root.dataset.pressed = "1";
            window.addEventListener("pointerup", releasePress, { once: true, passive: true });
            if (reduceMotion.matches) return;
            const ripple = document.createElement("span");
            ripple.className = "cursor-ripple";
            ripple.style.left = `${event.clientX}px`;
            ripple.style.top = `${event.clientY}px`;
            ripple.addEventListener("animationend", () => ripple.remove(), { once: true });
            ripples.appendChild(ripple);
        };

        const releasePress = () => {
            root.dataset.pressed = "0";
        };

        const onWindowBlur = () => {
            root.dataset.hidden = "1";
            root.dataset.pressed = "0";
            visible = false;
        };

        const frame = (now: number) => {
            if (!alive) return;
            const follow = reduceMotion.matches ? 1 : 0.2;
            x.ring += (x.pointer - x.ring) * follow;
            y.ring += (y.pointer - y.ring) * follow;

            dot.style.transform = `translate3d(${x.pointer}px, ${y.pointer}px, 0)`;
            ring.style.transform = `translate3d(${x.ring}px, ${y.ring}px, 0)`;
            glow.style.transform = `translate3d(${x.pointer}px, ${y.pointer}px, 0)`;

            const dx = x.pointer - lastMoveX;
            const dy = y.pointer - lastMoveY;
            lastMoveX = x.pointer;
            lastMoveY = y.pointer;
            const speed = Math.hypot(dx, dy);

            // 辉光和尾迹只在你「正在挥动」时存在，停下来就收回去。
            const active = Math.min(1, speed / 18);
            glow.style.opacity = reduceMotion.matches ? "0" : `${0.35 + active * 0.65}`;
            if (!reduceMotion.matches && speed > 7 && trailHost.childElementCount < 18 && now - trailClock > 24) {
                trailClock = now;
                const mote = document.createElement("span");
                mote.className = "cursor-mote";
                mote.style.left = `${x.pointer + (Math.random() - 0.5) * 14}px`;
                mote.style.top = `${y.pointer + (Math.random() - 0.5) * 14}px`;
                mote.addEventListener("animationend", () => mote.remove(), { once: true });
                trailHost.appendChild(mote);
            }

            raf = requestAnimationFrame(frame);
        };
        raf = requestAnimationFrame(frame);

        window.addEventListener("pointermove", onPointerMove, { passive: true });
        window.addEventListener("pointerover", onPointerOver, { passive: true });
        window.addEventListener("pointerdown", onPointerDown, { passive: true });
        window.addEventListener("blur", onWindowBlur);
        document.addEventListener("pointerleave", onWindowBlur);

        return () => {
            alive = false;
            cancelAnimationFrame(raf);
            window.clearTimeout(idleTimer);
            window.removeEventListener("pointermove", onPointerMove);
            window.removeEventListener("pointerover", onPointerOver);
            window.removeEventListener("pointerdown", onPointerDown);
            window.removeEventListener("blur", onWindowBlur);
            document.removeEventListener("pointerleave", onWindowBlur);
        };
    }, []);

    return (
        <div ref={rootRef} className="cursor-fx" data-state="idle" data-hidden="1" data-pressed="0" aria-hidden="true">
            <span ref={glowRef} className="cursor-glow" />
            <div ref={trailRef} className="cursor-trail" />
            <span ref={ringRef} className="cursor-ring" />
            {/* 光标本体：同一锚点上叠四个形状，data-state 切换哪个可见（见 App.css）。 */}
            <span ref={dotRef} className="cursor-shape">
                <span className="cursor-shape-arrow">
                    <svg viewBox="0 0 18 18" width="18" height="18" aria-hidden="true">
                        <path d="M3.5 2.2v12.9l3.4-3.3 2 4.7 2.5-1.1-2-4.6h4.9z" />
                    </svg>
                </span>
                <span className="cursor-shape-hand">
                    <svg viewBox="0 0 24 24" width="20" height="20" aria-hidden="true">
                        <path d="M9 11.5V5.75a1.25 1.25 0 0 1 2.5 0v5.25h.75V4.25a1.25 1.25 0 0 1 2.5 0v6.75h.75V5.75a1.25 1.25 0 0 1 2.5 0v6.75h.75V7.75a1.25 1.25 0 0 1 2.5 0v7.75c0 3.2-2.3 5.5-5.6 5.5h-2.9c-2.1 0-3.4-1-4.5-2.6l-2.7-4.2a1.35 1.35 0 0 1 2.1-1.6L9 14.75z" />
                    </svg>
                </span>
                <span className="cursor-shape-ibeam" />
                <span className="cursor-shape-grab">
                    <svg viewBox="0 0 24 24" width="20" height="20" aria-hidden="true">
                        <path d="M23 12.5c0-1.2-1-2.2-2.2-2.2-.3-1.1-1.3-2-2.5-2-.6 0-1.1.2-1.6.5-.4-.3-.9-.5-1.4-.5-1 0-1.9.6-2.3 1.5-.4-.1-.8-.2-1.2-.2-1.4 0-2.6 1.2-2.6 2.6v.8L6.4 12c-.9-.9-2.4-.9-3.3 0-.9.9-.9 2.4 0 3.3l4.7 4.7c1.2 1.2 2.8 1.9 4.5 1.9h5.3c2.9 0 5.4-2.4 5.4-5.4zM10.8 12.2H9.4v-1.1h1.4zm2.8 0h-1.4v-1.1h1.4zm2.8 0h-1.4v-1.1h1.4z" />
                    </svg>
                </span>
            </span>
            <div ref={ripplesRef} className="cursor-ripples" />
        </div>
    );
}
