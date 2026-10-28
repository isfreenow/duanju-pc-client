// 桌面端界面端到端检查：真实 Tauri 窗口里走一遍列表、搜索、播放、续播、历史。
//
//   node scripts/e2e-desktop.cjs                # 自己拉起 pnpm tauri dev，跑完关掉
//   node scripts/e2e-desktop.cjs --attach       # 连已经在跑的窗口（需 9222 调试端口）
//   node scripts/e2e-desktop.cjs --keep         # 跑完不关窗口
//   node scripts/e2e-desktop.cjs --quick        # 播放用最小清晰度，省流量
//   node scripts/e2e-desktop.cjs --only=list|play|third|history|guard
//
// 为什么不用浏览器测试框架：这套界面的核心断言（取源 → 网页侧处理 → `<video>` 出画）
// 只有 Tauri 自带的 WebView2 才同时具备「能调 Rust command」和「有 HEVC 硬解」两个
// 条件，普通 Chromium 页面缺前者。
//
// 场景之间是有状态的：二（红果播放）与三（第三方源）都会往本机历史里写，四（最近与
// 收藏页与首页最新播放行）读它们，最后清空。所以单跑 --only=history 之前先播一集，
// 否则没有记录可读（要么先跑 play，或直接 --only=play,history）。
//
// 隐私红线：本脚本任何输出（含失败信息）都不得出现媒体地址或内容密钥，只打印
// 数量、尺寸、耗时和像素统计。

const { spawn, execSync } = require("node:child_process");
const net = require("node:net");
const fs = require("node:fs");
const path = require("node:path");

// 调试端口不一定轮得到我们：本机常驻的 Chrome（浏览器自动化）经常先占掉 9222，
// 那时 WebView2 会静默放弃绑定，脚本就连到了别人的标签页（页面能挂载，但没有
// Tauri 运行时，症状是「导航后应用没挂载」）。所以默认端口被占就往上找一个空闲的。
const PREFERRED_PORT = Number(process.env.HG_CDP_PORT ?? 9222);
const APP_ORIGIN = process.env.HG_APP_ORIGIN ?? "http://localhost:1420";

function portIsFree(port) {
  return new Promise((resolve) => {
    const probe = net.createServer();
    probe.once("error", () => resolve(false));
    probe.once("listening", () => probe.close(() => resolve(true)));
    probe.listen(port, "127.0.0.1");
  });
}

async function pickCdpPort() {
  for (let offset = 0; offset < 30; offset += 1) {
    if (await portIsFree(PREFERRED_PORT + offset)) return PREFERRED_PORT + offset;
  }
  return PREFERRED_PORT;
}

// 自己拉起窗口时才需要挑空闲端口；--attach 由调用方用 HG_CDP_PORT 指明。
let CDP_PORT = PREFERRED_PORT;
const FLAGS = new Set(process.argv.slice(2).map((arg) => arg.replace(/^-+/, "").toLowerCase()));
const QUICK = FLAGS.has("quick");
const ATTACH = FLAGS.has("attach");
const KEEP = FLAGS.has("keep");
// --shots：把关键画面存成 PNG，横竖屏遮挡、滚动条这类问题看图比看数字准。
const SHOTS = FLAGS.has("shots");
const SHOT_DIR = path.join(process.cwd(), ".tmp-verify", "shots");
// --only=… 单独跑一段（逗号分隔可多段），调试时不用等全套。
const ONLY = (process.argv.find((arg) => arg.startsWith("--only")) ?? "").split("=")[1]?.toLowerCase() ?? "";
const ONLY_SET = ONLY ? new Set(ONLY.split(",").map((name) => name.trim()).filter(Boolean)) : null;
const wanted = (name) => ONLY_SET === null || ONLY_SET.has(name);

const TIMEOUTS = {
  cards: 60_000,
  episodes: 60_000,
  streams: 90_000,
  download: 200_000,
  decode: 40_000,
  // 第三方源是直连播放，不等下载；但首包 + 起播还是要给点余量。
  direct: 120_000,
};

const results = [];
let currentGroup = "未分组";

function check(name, condition, detail) {
  const text = detail === undefined ? "" : typeof detail === "string" ? detail : JSON.stringify(detail);
  results.push({ group: currentGroup, name, ok: Boolean(condition), detail: text.slice(0, 400) });
  const mark = condition ? "  ok " : " FAIL";
  console.log(`${mark} [${currentGroup}] ${name}${text ? ` — ${text}` : ""}`);
  return Boolean(condition);
}

// 横竖屏是两套摆法，所以断言写成「方向 → 应有形态」，不把上游哪天换片源方向写死：
// 画面必须完整落在舞台里、不出窗口、页面不因此多出滚动条；竖屏还要有虚化背景，
// 窗口够宽时还要有右侧分集栏（横屏不给，宽度是画面的）。
function checkLayout(label, fit) {
  if (!fit) {
    check(`${label}：画面摆法`, false, "没量到几何信息");
    return;
  }
  const wantSide = fit.orientation === "竖屏" && fit.w >= 940;
  const ok =
    fit.inside === true &&
    fit.inWindow === true &&
    fit.pageScroll <= 1 &&
    fit.side === wantSide &&
    (fit.orientation !== "竖屏" || fit.backdrop === true);
  check(
    `${label}：${fit.orientation ?? "?"}画面的摆法`,
    ok,
    `编码 ${fit.coded}｜画面 ${fit.box} 舞台 ${fit.stage}｜${fit.stageFill}｜侧栏 ${fit.side ? `${fit.sideRows} 格，当前 ${fit.currentRow ?? "-"}` : "无"
    }｜背景 ${fit.backdrop ? "有" : "无"}｜视口 ${fit.viewport}｜文档超出 ${fit.pageScroll}px`,
  );
}

// 「没有窗口滚动条」要在每个会滚动的页各量一次：首页改对了不代表详情页也对。
function checkNoWindowScroll(label, s) {
  if (!s) {
    check(`${label}：不靠窗口滚动条`, false, "没量到滚动信息");
    return;
  }
  check(
    `${label}：不靠窗口滚动条（右侧/底部都不该出现）`,
    s.pageScroll <= 1 && s.sideScroll <= 1 && s.bodyScroll <= 1,
    `竖向多出 ${s.pageScroll}px、body ${s.bodyScroll}px、横向多出 ${s.sideScroll}px，overflow ${s.bar}，内容区 ${s.scroller}`,
  );
}

// ── CDP 驱动 ────────────────────────────────────────────────────────────────

async function listTargets() {
  for (const host of ["127.0.0.1", "localhost"]) {
    try {
      const list = await (await fetch(`http://${host}:${CDP_PORT}/json/list`, { signal: AbortSignal.timeout(2000) })).json();
      if (Array.isArray(list)) return list.filter((t) => t.type === "page" && t.webSocketDebuggerUrl);
    } catch {
      /* 端口还没起来 */
    }
  }
  return null;
}

// 冷启动时窗口经常停在 about:blank，或者 Tauri 会多开一个隐藏 target。
// 与其猜哪个是真的，不如逐个连上去导航一次、看 #root 有没有挂起来。
async function attachAppTarget(deadlineMs, devLog) {
  const deadline = Date.now() + deadlineMs;
  let lastNote = "还没有 target";
  for (; ;) {
    const targets = await listTargets();
    if (targets === null) lastNote = `调试端口 ${CDP_PORT} 无响应（窗口没起来？dev 日志有没有报错？）`;
    else if (targets.length === 0) lastNote = "没有 page target";
    for (const target of targets ?? []) {
      const client = connect(target.webSocketDebuggerUrl);
      try {
        await client.ready;
        await client.send("Runtime.enable");
        await client.send("Page.enable");
        await client.send("Page.navigate", { url: APP_ORIGIN });
        if (await waitRootMounted(client, 45_000)) {
          console.log(`· 已连接窗口：${target.title || "(无标题)"} @ ${APP_ORIGIN}`);
          return { client, target };
        }
        lastNote = `target ${target.id} 导航后应用没挂载（#root 空或没有 Tauri 运行时）`;
      } catch (error) {
        lastNote = `target ${target.id} 连接失败：${String(error?.message ?? error).slice(0, 60)}`;
      }
      client.close();
    }
    if (Date.now() > deadline) {
      if (devLog.length > 0) console.log("  dev 日志最后几行：" + devLog.map((l) => `    ${l}`).join(" ; "));
      throw new Error(`连不上应用窗口（${lastNote}）`);
    }
    await new Promise((r) => setTimeout(r, 2000));
  }
}

async function waitRootMounted(client, ms) {
  const deadline = Date.now() + ms;
  for (; ;) {
    const boot = await client.send("Runtime.evaluate", {
      expression: "Boolean(document.getElementById('root')?.firstElementChild && window.__TAURI_INTERNALS__?.invoke)",
      returnByValue: true,
    });
    if (boot.result?.result?.value === true) return true;
    if (Date.now() > deadline) return false;
    await new Promise((r) => setTimeout(r, 700));
  }
}

function connect(webSocketUrl) {
  const socket = new WebSocket(webSocketUrl);
  const pending = new Map();
  let messageId = 0;
  const listeners = new Set();

  socket.addEventListener("message", (event) => {
    const message = JSON.parse(event.data);
    const handler = pending.get(message.id);
    if (handler) {
      pending.delete(message.id);
      handler(message);
    }
    for (const listener of listeners) listener(message);
  });

  return {
    ready: new Promise((resolve, reject) => {
      socket.addEventListener("open", resolve);
      socket.addEventListener("error", () => reject(new Error("WebSocket 连接失败")));
    }),
    send(method, params = {}) {
      const id = ++messageId;
      return new Promise((resolve) => {
        pending.set(id, resolve);
        socket.send(JSON.stringify({ id, method, params }));
      });
    },
    onMessage(listener) {
      listeners.add(listener);
    },
    close() {
      try {
        socket.close();
      } catch {
        /* 已经关了 */
      }
    },
  };
}

function toError(details) {
  const info = details?.exceptionDetails ?? {};
  const description = info.exception?.description;
  const firstLine = description ? String(description).split("\n")[0] : "";
  const where = info.lineNumber !== undefined ? `@表达式第 ${Number(info.lineNumber) + 2} 行` : "";
  const text = info.text ?? "未知异常";
  const message = [firstLine || text, where].filter(Boolean).join(" ");
  return new Error(description ? message : `${message}｜${JSON.stringify(info).slice(0, 300)}`);
}

/** 超时之后还能拿到一个明确的错误，而不是让 Promise 永远悬着。 */
function withDeadline(promise, ms, message) {
  let timer;
  const guard = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(message)), ms);
  });
  return Promise.race([promise, guard]).finally(() => clearTimeout(timer));
}

// 失败的那一刻页面到底停在哪：只读 DOM，不 await，所以页面卡住时它自己不会卡住。
const SNAPSHOT = `(() => {
  const t = (sel) => { const e = document.querySelector(sel); return e ? e.textContent.replace(/\\s+/g, " ").trim().slice(0, 48) : null; };
  const all = (sel) => Array.from(document.querySelectorAll(sel)).map((e) => e.textContent.replace(/\\s+/g, " ").trim().slice(0, 40));
  const parts = [];
  parts.push("页签=" + (t("button.tab.current") ?? "?"));
  parts.push("标题=" + (t(".view-head h2") ?? t("h2") ?? "?"));
  parts.push("卡片=" + document.querySelectorAll("button.card").length);
  parts.push("选集=" + document.querySelectorAll("button.episode").length);
  parts.push("播放器=" + (document.querySelector("section.player-shell") ? "在" : "无"));
  const status = all("p[class^=status]");
  if (status.length) parts.push("状态条=" + status.join("｜"));
  const foot = all(".player-foot .view-sub");
  if (foot.length) parts.push("播放脚=" + foot.join("｜"));
  const stage = t(".player-stage");
  if (stage) parts.push("舞台=" + stage);
  const video = document.querySelector("section.player-shell video");
  if (video) parts.push("video=rs" + video.readyState + "/" + video.videoWidth + "x" + video.videoHeight + "/err" + (video.error ? video.error.code : 0) + "/t" + Math.round(video.currentTime));
  return parts.join(" · ");
})()`;

/** 端口 1420 上是不是已经有人应答（上一次 --keep 留下的 dev server）。 */
async function devServerOccupied() {
  try {
    const response = await fetch(APP_ORIGIN, { signal: AbortSignal.timeout(2500) });
    return `HTTP ${response.status}`;
  } catch {
    return null;
  }
}

async function main() {
  let appProcess = null;
  const devLog = [];

  if (!ATTACH) {
    const occupied = await devServerOccupied();
    if (occupied !== null) {
      // 复用旧 dev server 会让「有页面但没有 Tauri 运行时」变成半小时的排查，
      // 所以宁可在这里停下，也不要跑一半再失败。
      console.log(
        `· ${APP_ORIGIN} 已经有服务在应答（${occupied}）——多半是上一次 --keep 留下的 dev server。\n` +
        `  想复用它就加 --attach（还需要窗口自己开着调试端口），或者先把残留的 pnpm/cargo/应用进程停掉。`,
      );
      process.exitCode = 2;
      return;
    }
    // 上一次强杀留下的 WebView2 子进程会占住本应用的用户数据目录，新建 webview 直接
    // 失败在 HRESULT 0x8007139F；那看起来像代码问题，其实是环境残留。
    try {
      execSync("powershell -NoProfile -ExecutionPolicy Bypass -File scripts/kill-stale-webview.ps1", { stdio: "ignore" });
    } catch {
      /* 没有 PowerShell 就跳过，让后面的连接失败信息说话 */
    }
    CDP_PORT = await pickCdpPort();
    if (CDP_PORT !== PREFERRED_PORT) {
      console.log(`· 调试端口 ${PREFERRED_PORT} 已被别的进程占着（多半是常驻 Chrome），改用 ${CDP_PORT}`);
    }
    console.log(`· 拉起桌面窗口（pnpm tauri dev，CDP 端口 ${CDP_PORT}）`);
    // 三个禁用节流开关不是可选的：窗口被遮挡或失焦时 WebView2 会把 setTimeout 降到分钟级，
    // 而断言全靠轮询等待——被冻住的页面「什么都没做错」，只是永远不返回，最后 CDP
    // 超时会带着一个空的 exceptionDetails 回来，看起来像代码 bug。
    const webviewArgs = [
      `--remote-debugging-port=${CDP_PORT}`,
      "--disable-background-timer-throttling",
      "--disable-backgrounding-occluded-windows",
      "--disable-renderer-backgrounding",
    ].join(" ");
    appProcess = spawn("pnpm tauri dev", {
      cwd: process.cwd(),
      shell: true,
      env: { ...process.env, WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: webviewArgs },
      stdio: ["ignore", "pipe", "pipe"],
    });
    const record = (chunk, prefix) => {
      for (const raw of String(chunk).split(new RegExp(String.fromCharCode(13) + "?" + String.fromCharCode(10)))) {
        const line = raw.trim();
        if (!line) continue;
        devLog.push(`${prefix}${line.slice(0, 160)}`);
        if (devLog.length > 30) devLog.shift();
        if (process.env.HG_VERBOSE) console.log(`  [dev] ${line.slice(0, 160)}`);
        else if (/error|Error|panic|already in use/.test(line)) console.log(`  [dev!] ${line.slice(0, 200)}`);
      }
    };
    appProcess.stdout.on("data", (chunk) => record(chunk, ""));
    appProcess.stderr.on("data", (chunk) => record(chunk, "! "));
    appProcess.on("exit", (code) => {
      if (code !== 0 && code !== null) console.log(`  [dev] 进程退出，code=${code}`);
    });
    // 断言之前的任何异常（连不上窗口、注入失败）都不能把 cargo/vite/应用留在后台，
    // 否则下一次运行先撞上 1420 端口，还得手动清进程。
    process.on("exit", () => {
      if (KEEP) return;
      try {
        execSync(`taskkill /PID ${appProcess.pid} /T /F`, { stdio: "ignore" });
      } catch {
        /* 进程树已经没了 */
      }
      // 顺手把自己应用的 WebView2 子进程带走，别给下一次留 0x8007139F。
      try {
        execSync("powershell -NoProfile -ExecutionPolicy Bypass -File scripts/kill-stale-webview.ps1", { stdio: "ignore" });
      } catch {
        /* 没有 PowerShell 就算了 */
      }
    });
  }

  const { client } = await attachAppTarget(ATTACH ? 20_000 : 300_000, devLog);

  // 每个场景都从界面重新开始导航，避免上一轮遗留的 route 影响断言；
  // 工具函数必须在这之后注入（导航会清掉 window 上的东西）。
  const helpers = `
window.__H = {
  q: (sel) => document.querySelector(sel),
  all: (sel) => Array.from(document.querySelectorAll(sel)),
  // wait() 把回调的返回值当「就绪信号」，所以「等到 N 个元素」必须返回数组本身，
  // 返回布尔值会让调用方拿到 true 再去 [0]。
  many: (sel, min = 1) => {
    const found = Array.from(document.querySelectorAll(sel));
    return found.length >= min ? found : null;
  },
  text: (sel) => (document.querySelector(sel)?.textContent ?? "").replace(/\\s+/g, " ").trim(),
  // 接口字段里可能夹不可见字符；比对前剥掉非常规可打印字符，真不相等时
  // 打印第一个差异码点。注意：这里用字符串拼正则再 new RegExp，因为整个 helpers
  // 就坐在 node 模板字符串里，正则字面量里的 \\uXXXX 会被模板先吃一道变成字母序列。
  _invis: (() => {
    const ranges = [[0, 31], [127, 159], [173, 173], [847, 847], [1564, 1564], [6158, 6158], [8203, 8207], [8288, 8303], [8232, 8233], [65279, 65279]];
    return new RegExp("[" + ranges.map((r) => String.fromCharCode(r[0]) + "-" + String.fromCharCode(r[1])).join("") + "]+", "g");
  })(),
  strip: (s) => String(s ?? "").replace(window.__H._invis, ""),
  // 取字符串开头的连续数字（集号）。刻意不用正则：helpers 与 snippet 都坐在 node
  // 模板字符串里，\d 会被模板求值吃掉一道反斜杠送到页面成字母 d，正则静默失效。
  // charCodeAt 48..57 就是 '0'..'9'，零转义、零歧义。
  digitPrefix(s) {
    const t = String(s ?? "").trim();
    let k = 0;
    while (k < t.length && t.charCodeAt(k) >= 48 && t.charCodeAt(k) <= 57) k += 1;
    return t.slice(0, k);
  },
  sameSeries(title, series) {
    const a = window.__H.strip(title);
    const b = window.__H.strip(series);
    if (a.startsWith(b)) return { ok: true };
    let i = 0;
    while (i < b.length && a[i] === b[i]) i += 1;
    const cp = (s, k) => (s[k] ? s.codePointAt(k).toString(16) : "-");
    return { ok: false, at: i, titleCp: cp(a, i), seriesCp: cp(b, i), raw: JSON.stringify({ title: String(title), series: String(series) }).slice(0, 160) };
  },
  tab: (label) => Array.from(document.querySelectorAll("button.tab")).find((b) => b.textContent.trim() === label) ?? null,
  wait(fn, ms, label) {
    const deadline = Date.now() + ms;
    return new Promise((resolve, reject) => {
      const tick = () => {
        let value = null;
        try { value = fn(); } catch (e) { value = null; }
        if (value) return resolve(value);
        if (Date.now() > deadline) return reject(new Error("超时：" + label + "（" + ms + "ms）"));
        setTimeout(tick, 120);
      };
      tick();
    });
  },
  // 取样 64×36 的亮度过渡与离散度。黑屏/冻帧的 spread 会趋近 0。
  // 跨域直连的片源会画脏 canvas，getImageData 抛 SecurityError——那不是黑屏，
  // 所以如实标 blocked，让调用方改用时间轴断言。
  pixels(video) {
    const canvas = document.createElement("canvas");
    canvas.width = 64; canvas.height = 36;
    const ctx = canvas.getContext("2d");
    if (ctx === null) throw new Error("没有 2D 画布，无法取样");
    ctx.drawImage(video, 0, 0, 64, 36);
    let data;
    try { data = ctx.getImageData(0, 0, 64, 36).data; }
    catch (e) { return { mean: -1, spread: -1, blocked: true }; }
    let sum = 0, sq = 0;
    for (let i = 0; i < data.length; i += 4) {
      const luma = 0.2126 * data[i] + 0.7152 * data[i + 1] + 0.0722 * data[i + 2];
      sum += luma; sq += luma * luma;
    }
    const n = data.length / 4;
    const mean = sum / n;
    return { mean: Math.round(mean * 10) / 10, spread: Math.round(Math.sqrt(sq / n - mean * mean) * 10) / 10 };
  },
  // React 受控输入必须走原生 setter，否则 onChange 不触发。
  type(sel, value) {
    const input = document.querySelector(sel);
    const setter = Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype, "value").set;
    setter.call(input, value);
    input.dispatchEvent(new Event("input", { bubbles: true }));
    return input;
  },
  blobGone: (url) => fetch(url).then((r) => ({ alive: true, status: r.status }), () => ({ alive: false })),
  srcKind: (video) => ((video.currentSrc || video.src) || "").startsWith("blob:") ? "本地文件" : "来源直连",
  // 「像桌面程序」这条是可量的：文档高度不该超过视口，横向也是。
  // 每个会滚动的页都量一次，否则只在首页改对了、详情页漏出去也看不见。
  windowScroll() {
    return {
      pageScroll: document.documentElement.scrollHeight - window.innerHeight,
      sideScroll: document.documentElement.scrollWidth - window.innerWidth,
      bodyScroll: document.body.scrollHeight - window.innerHeight,
      bar: getComputedStyle(document.documentElement).overflowY + "/" + getComputedStyle(document.body).overflowY,
      scroller: (() => { const m = document.querySelector(".main"); return m ? m.scrollHeight - m.clientHeight + "px可滚" : "无内容区"; })(),
    };
  },
  // 横竖屏分别怎么摆，光看 videoWidth/Height 不够（旋转元数据会让「画面」和「编码尺寸」
  // 不一致），所以量的是几何：画面盒子必须在舞台里，且窗口自己不能被撑出页面滚动条。
  fit(video) {
    const stage = video.closest(".player-stage") ?? video.parentElement;
    const box = video.getBoundingClientRect();
    const stageBox = stage.getBoundingClientRect();
    const r = (n) => Math.round(n);
    return {
      coded: video.videoWidth + "x" + video.videoHeight,
      orientation: video.videoHeight > video.videoWidth ? "竖屏" : "横屏",
      box: r(box.width) + "x" + r(box.height),
      stage: r(stageBox.width) + "x" + r(stageBox.height),
      inside:
        box.left >= stageBox.left - 2 &&
        box.top >= stageBox.top - 2 &&
        box.right <= stageBox.right + 2 &&
        box.bottom <= stageBox.bottom + 2,
      inWindow: box.top >= -2 && box.bottom <= window.innerHeight + 2 && box.left >= -2 && box.right <= window.innerWidth + 2,
      pageScroll: document.documentElement.scrollHeight - window.innerHeight,
      // 舞台里除了画面不许有别的可见块（侧栏另算，它不该抢画面的高度）
      stageFill: (() => {
        const s = getComputedStyle(video);
        return s.objectFit + "/" + s.width + "x" + s.height;
      })(),
      side: Boolean(document.querySelector(".player-side")),
      backdrop: Boolean(document.querySelector(".stage-backdrop")),
      sideRows: document.querySelectorAll(".side-row").length,
      currentRow: (() => { const c = document.querySelector(".side-row.current"); return c ? c.textContent.trim() : null; })(),
      w: window.innerWidth,
      viewport: window.innerWidth + "x" + window.innerHeight,
    };
  },
  // React 把一次点击引发的重排放进之后的任务里，所以「点完页签立刻找卡片」会命中上
  // 一个页签留下的 DOM。先让出一个宏任务，等界面真的换了再开始轮询。
  settle(ms = 350) {
    return new Promise((resolve) => setTimeout(resolve, ms));
  },
  // 回到首页：品牌按钮把顶栏页签复位到「首页」，首页一出横排卡片即算到位。
  home() {
    document.querySelector("button.brand")?.click();
    return window.__H.settle().then(() => window.__H.wait(() => window.__H.many("button.card"), 60_000, "回到榜单"));
  },
  player() {
    return window.__H.wait(() => document.querySelector("section.player-shell"), 90_000, "播放器外壳");
  },
  exitPlayer() {
    // 面包屑的剧名段才是「退出到详情页」；首页段会直接跳首页，不能用在续播断言前。
    document.querySelector(".player-bar button.crumb-series")?.click();
    return window.__H.wait(() => document.querySelector("section.player-shell") === null, 20_000, "退出播放器");
  },
};
return typeof window.__H.type === "function";
`;
  const injected = await client.send("Runtime.evaluate", { expression: `(async () => { ${helpers} })()`, awaitPromise: true, returnByValue: true });
  if (injected.result?.result?.value !== true) throw new Error("页面工具函数注入失败：" + JSON.stringify(injected.result?.exceptionDetails?.text ?? injected.result?.result));

  async function run(name, expression, timeoutMs) {
    const started = Date.now();
    // 页面上的等待总和必须明显小于这里的预算：CDP 的 timeout 先到期的时候，WebView2
    // 只回一个空的 exceptionDetails，日志里就成了没法定位的「未知异常｜{}」。
    const guardMs = timeoutMs + 25_000;
    // 页内抛出的异常一律转成返回值：轮询等待超时（「超时：来源目录」）和被测代码里的
    // TypeError 都是这样，包起来之后失败信息直接说清是哪一步、什么错。
    // 注意这层 try 兜不住语法错误——而 snippet 里的语法错误几乎都来自同一件事：
    // 模板字符串会把单反斜杠吃掉（/2\s*/ 送到页面变成 /2s*/，正则里剩下的 / 提前闭合
    // 字面量），现场就是「已等 0s + 未知异常｜{}」。所以 snippet 里的正则一律写两遍反斜杠。
    const wrapped = `(async () => { try { ${expression} } catch (e) { return { __inPage: String(e && e.message || e), __at: String(e && e.stack || "").split(String.fromCharCode(10)).slice(1, 3).join(" ← ").slice(0, 120) }; } })()`;
    let response;
    try {
      response = await withDeadline(
        client.send("Runtime.evaluate", {
          expression: wrapped,
          awaitPromise: true,
          returnByValue: true,
          timeout: timeoutMs,
        }),
        guardMs,
        `CDP 在 ${Math.round(guardMs / 1000)} 秒内没有回包`,
      );
    } catch (error) {
      throw await withContext(`${error?.message ?? error}`, started, timeoutMs);
    }
    if (response.result?.exceptionDetails) throw await withContext(toError(response).message, started, timeoutMs);
    if (response.error || response.result === undefined) {
      throw await withContext("CDP 返回异常：" + JSON.stringify(response).slice(0, 400), started, timeoutMs);
    }
    const value = response.result?.result?.value;
    if (value && typeof value === "object" && value.__inPage) {
      throw await withContext(`页面内异常：${value.__inPage}（${value.__at || "无栈"}）`, started, timeoutMs);
    }
    return value;
  }

  // 把「跑了多久」和「页面此刻停在哪」附在错误信息后面，一次失败就能看出是应用慢、
  // 应用报错，还是这段断言本身写错了。
  async function withContext(message, started, timeoutMs) {
    const elapsed = Math.round((Date.now() - started) / 1000);
    let state = "读不到页面状态";
    try {
      const shot = await withDeadline(client.send("Runtime.evaluate", { expression: SNAPSHOT, returnByValue: true }), 8_000, "快照超时");
      state = String(shot.result?.result?.value ?? "快照为空");
    } catch {
      /* 页面可能整个卡死了，那就只报时间 */
    }
    return new Error(`${message}｜已等 ${elapsed}s / 预算 ${Math.round(timeoutMs / 1000)}s｜此刻：${state}`);
  }

  // --shots：几何断言能说「盒子在不在舞台里」，说不了「画面看着对不对」。横竖屏遮挡
  // 这类问题必须看图，所以把关键帧留在 .tmp-verify/shots/（已在 .gitignore 里）。
  async function capture(name) {
    if (!SHOTS) return;
    try {
      const image = await withDeadline(client.send("Page.captureScreenshot", { format: "png" }), 15_000, "截图超时");
      const data = image.result?.data;
      if (!data) {
        console.log(`  [shot] ${name} 没拿到图：${JSON.stringify(image).slice(0, 160)}`);
        return;
      }
      fs.mkdirSync(SHOT_DIR, { recursive: true });
      const file = path.join(SHOT_DIR, `${name}.png`);
      fs.writeFileSync(file, Buffer.from(data, "base64"));
      console.log(`  [shot] ${name} → ${file}`);
    } catch (error) {
      console.log(`  [shot] ${name} 失败：${String(error?.message ?? error)}`);
    }
  }

  // ══ 场景一：首页（Netflix 式横排轨道）与搜索 ════════════════════════════
  currentGroup = "首页";
  if (wanted("list")) try {
    const list = await run(
      "list",
      `
      const out = {};
      await window.__H.wait(() => window.__H.many("button.card"), ${TIMEOUTS.cards}, "首页横排卡片");
      const cards = window.__H.all("button.card");
      out.cardCount = cards.length;
      out.tabs = window.__H.all("button.tab").map((b) => b.textContent.trim());
      out.first = (window.__H.all(".rail .card")[0] ?? cards[0]).textContent.replace(/\\s+/g, " ").trim().slice(0, 60);
      out.brand = window.__H.text("button.brand");
      out.rows = window.__H.all(".home-row").length;
      out.seeAll = window.__H.all(".see-all").length;
      out.hasPager = Boolean(document.querySelector("nav.pager"));
      // 首页很长（几十个题材行），下面的封面靠懒加载、本来就没进 DOM 可见区，
      // 所以只量顶部这一屏能解码的卡，不用全量卡片数当分母（否则必假阴性）。
      const visible = window.__H.all("button.card").slice(0, 16).flatMap((c) => Array.from(c.querySelectorAll("img")));
      await window.__H.wait(() => visible.filter((img) => img.complete && img.naturalWidth > 0).length >= Math.ceil(visible.length * 0.6), 15_000, "首屏封面加载")
        .catch(() => {});
      out.images = visible.filter((img) => img.complete && img.naturalWidth > 0).length;
      out.imgTotal = visible.length;
      out.imageProtocols = Array.from(new Set(visible.map((i) => { try { return new URL(i.src).protocol; } catch { return "坏"; } })));
      out.emptyNotice = window.__H.all("section.home p[class^=status]").map((p) => p.textContent.trim());
      Object.assign(out, window.__H.windowScroll());
      return out;
      `,
      TIMEOUTS.cards + 20_000,
    );
    check("首页渲染横向海报轨道", list.cardCount >= 10, `共 ${list.cardCount} 张`);
    check("顶栏四个入口（首页 + 两来源 + 历史）", list.tabs.length === 4, list.tabs.join("/"));
    check("品牌名是短剧随心看", list.brand === "短剧随心看", list.brand);
    check("首页是多行类目轨道", list.rows >= 3, `${list.rows} 行`);
    check("每行右侧都有「查看全部」", list.seeAll >= list.rows && list.rows >= 3, `${list.seeAll} 个入口 / ${list.rows} 行`);
    check("首页本身没有分页按钮", list.hasPager === false, `nav.pager 存在=${list.hasPager}`);
    check("首张卡有标题与集数", /热度|集|完结|更新/.test(list.first), list.first);
    check("封面图成功加载", list.images >= Math.ceil(list.imgTotal * 0.6), `首屏 ${list.images}/${list.imgTotal} 张已解码，协议 ${list.imageProtocols.join(",")}`);
    check("首页没有错误条", list.emptyNotice.length === 0, list.emptyNotice.join("；"));
    checkNoWindowScroll("首页", list);
    await capture("1-home");
  } catch (error) {
    check("首页加载", false, String(error?.message ?? error));
  }

  if (wanted("list")) try {
    const paging = await run(
      "paging",
      `
      const out = {};
      const seeAll = window.__H.all(".see-all")[0];
      if (!seeAll) return { error: "首页没有「查看全部」" };
      seeAll.click();
      await window.__H.wait(() => window.__H.many(".grid button.card"), ${TIMEOUTS.cards} + 60_000, "类目页网格").catch(() => null);
      out.pageTitle = window.__H.text(".view-head h2");
      out.gridCount = window.__H.all(".grid button.card").length;
      out.pager = window.__H.text("nav.pager");
      out.revealsTotal = /\\d+\\s*\\/\\s*\\d+/.test(out.pager ?? "");
      const before = window.__H.all(".grid button.card")[0]?.textContent.replace(/\\s+/g, " ").trim().slice(0, 40) ?? "";
      const next = window.__H.all("nav.pager button").find((b) => b.textContent.includes("下一页"));
      if (!next || next.disabled) return { ...out, error: "下一页不可用" };
      next.click();
      // 类目页只报「第 N 页」，翻页断言盯着页码而不是总页数——总页数本就不该出现。
      const moved = await window.__H.wait(() => {
        const pager = window.__H.text("nav.pager");
        return /第\\s*2\\s*页/.test(pager) ? pager : null;
      }, 90_000, "翻页到第 2 页").catch(() => null);
      out.pagerAfter = window.__H.text("nav.pager");
      out.moved = moved !== null;
      out.countAfter = window.__H.all(".grid button.card").length;
      out.after = window.__H.all(".grid button.card")[0]?.textContent.replace(/\\s+/g, " ").trim().slice(0, 40) ?? "";
      out.firstChanged = out.after !== "" && out.after !== before;
      out.status = window.__H.all("section.view p[class^=status]").map((p) => p.textContent.trim());
      return out;
      `,
      TIMEOUTS.cards + 90_000,
    );
    check("「查看全部」进入网格类目页", !paging.error && paging.gridCount >= 10, `《${paging.pageTitle ?? ""}》${paging.gridCount ?? 0} 张${paging.error ? " · " + paging.error : ""}`);
    check("类目页标题来自接口", typeof paging.pageTitle === "string" && paging.pageTitle.length > 4, paging.pageTitle);
    check("类目页只给上/下一页、不报总页数", /第\s*1\s*页/.test(paging.pager ?? "") && paging.revealsTotal === false, `${paging.pager}｜含总页数=${paging.revealsTotal}`);
    check("翻到第 2 页", !paging.error && paging.moved === true, `${paging.pagerAfter}${(paging.status ?? []).length ? " · 状态条：" + paging.status.join("｜") : ""}`);
    check("第 2 页仍是整页数据", paging.countAfter >= 10, `${paging.countAfter} 张，首卡「${paging.after}」${paging.firstChanged ? "（与第 1 页不同）" : "（与第 1 页开头撞名）"}`);
  } catch (error) {
    check("查看全部翻页", false, String(error?.message ?? error));
  }

  if (wanted("list")) try {
    const search = await run(
      "search",
      `
      const out = {};
      window.__H.type("form input", "玄幻");
      document.querySelector("form").requestSubmit();
      await window.__H.wait(() => window.__H.text("h2").includes("的搜索结果"), ${TIMEOUTS.cards}, "搜索标题");
      out.head = window.__H.text("h2");
      out.sub = window.__H.text("header .view-sub");
      await window.__H.wait(() => window.__H.many("button.card"), ${TIMEOUTS.cards}, "搜索结果卡片");
      out.count = window.__H.all("button.card").length;
      window.__H.all("button.card")[0].click();
      const eps = await window.__H.wait(() => window.__H.many("button.episode"), ${TIMEOUTS.episodes}, "搜索结果选集");
      out.episodeCount = eps.length;
      out.seriesHead = window.__H.text("h2");
      out.cover = Boolean(window.__H.q("img.series-cover"));
      // 详情页是最长的一页（一部剧上千集），窗口滚动条最容易在这里漏出来。
      Object.assign(out, window.__H.windowScroll());
      return out;
      `,
      TIMEOUTS.cards * 2 + TIMEOUTS.episodes + 30_000,
    );
    check("搜索走通并出结果", search.count >= 1, `${search.head}（命中数量不再上界面，只看卡片数）`);
    check("搜索词正确回显", (search.head ?? "").includes("玄幻"), search.head);
    check("搜索结果可进入详情", search.episodeCount >= 1 && search.seriesHead.length > 1, `《${search.seriesHead}》${search.episodeCount} 集，封面 ${search.cover ? "有" : "无"}`);
    checkNoWindowScroll("详情页（上千集）", search);
    await capture("1b-series");
  } catch (error) {
    check("搜索", false, String(error?.message ?? error));
  }

  // ══ 场景二：播放页（红果，含全屏外壳 / 换集 / 续播）════════════════════
  currentGroup = "播放页";
  let played = null;
  if (wanted("play")) try {
    const entry = await run(
      "entry",
      `
      const out = {};
      await window.__H.home();
      window.__H.all("button.card")[0].click();
      const eps = await window.__H.wait(() => window.__H.many("button.episode"), ${TIMEOUTS.episodes}, "选集网格");
      out.series = window.__H.text("h2");
      out.episodes = eps.length;
      // 集号从 .episode-no 子元素取：看过的格子会拼上时间标记（「1」+「0:45」），
      // 整块 textContent 的开头数字会被粘成「10」。
      out.pickedNo = window.__H.digitPrefix(eps[0].querySelector(".episode-no")?.textContent ?? eps[0].textContent);
      eps[0].click();
      const shell = await window.__H.player();
      const box = shell.getBoundingClientRect();
      out.coversWindow = Math.round(box.width) >= window.innerWidth - 2 && Math.round(box.height) >= window.innerHeight - 2;
      out.chromeGone = !document.querySelector("header.top") && !document.querySelector("footer.foot");
      out.title = window.__H.text("h2.player-title");
      // 点开的剧 / 集号必须就是标题里那一个：播放器还开着上一部时会在这里露出来。
      out.match = window.__H.sameSeries(out.title, out.series);
      out.rightSeries = (out.series ?? "").length > 0 && out.match.ok === true;
      // text() 已把连续空白归一为单空格，剧名/集号比对一律走 strip + includes，
      // 不再把带反斜杠的正则塞进模板字符串（\s → 字母 s、\d → 字母 d 的转义链坑）。
      out.rightEpisode = out.pickedNo.length > 0 && window.__H.strip(out.title ?? "").includes("第 " + out.pickedNo + " 集");
      // 集号比对失败时需要的现场：原始标题 JSON（控制字符可见）。
      out.epNoDebug = { title: JSON.stringify(out.title ?? ""), pickedNo: out.pickedNo };
      out.nav = window.__H.all(".player-nav button").filter((b) => !b.closest(".quality")).map((b) => b.textContent.trim() + (b.disabled ? "(禁用)" : ""));
      out.naughty = (shell.innerText.match(/解密|加密|密钥|帧|codec|HEVC|hvc1|AES/gi) ?? []);
      out.exit = window.__H.text(".player-bar button.back");
      out.crumbs = window.__H.all(".player-bar .crumbs button").map((b) => b.textContent.trim());
      return out;
      `,
      TIMEOUTS.episodes + TIMEOUTS.streams + 20_000,
    );
    check("进入播放页（占满窗口，页面框架让位）", entry.coversWindow === true && entry.chromeGone === true, `覆盖窗口 ${entry.coversWindow}，顶栏/页脚已隐藏 ${entry.chromeGone}`);
    check("标题是剧名 + 集号", /第\s*\d+\s*集/.test(entry.title ?? ""), entry.title);
    if (!(entry.rightSeries === true && entry.rightEpisode === true)) {
      // 断言红了但信息不够时，把比对用的原始字段整份端出来（控制字符在 JSON 里可见）。
      console.log(`  [entry-dump] ${JSON.stringify(entry.series)}|${JSON.stringify(entry.title)}|${JSON.stringify(entry.pickedNo)}|match=${JSON.stringify(entry.match)}|rightE=${entry.rightEpisode}|dbg=${JSON.stringify(entry.epNoDebug)}`);
    }
    check("播的就是刚点开的那一部那一集", entry.rightSeries === true && entry.rightEpisode === true, `详情页 h2=「${entry.series}」第 ${entry.pickedNo} 集 → 播放页「${entry.title}」${entry.match && entry.match.ok !== true ? "｜差异 @" + entry.match.at + " 码点 " + entry.match.titleCp + " vs " + entry.match.seriesCp + " 原文 " + entry.match.raw : ""}`);
    check("面包屑头与前后集按钮都在", entry.nav.length === 2 && /首页/.test(entry.exit ?? "") && (entry.crumbs ?? []).length === 2, `${entry.crumbs?.join(" › ")} · ${entry.nav.join("/")}`);
    check("界面不讲加解密", (entry.naughty ?? []).length === 0, (entry.naughty ?? []).join("、") || "无相关字样");

    played = await run(
      "play",
      `
      const out = {};
      // --quick 用清晰度菜单里的最低一档省流量；只有一路流时没有菜单，直接用默认选中项。
      if (${QUICK} && window.__H.q("button.quality-toggle")) {
        window.__H.q("button.quality-toggle").click();
        const items = window.__H.all("button.quality-item");
        if (items.length > 1) items[items.length - 1].click();
      }
      const started = Date.now();
      // 等不到视频就直接带出屏幕上写了什么——上游取源失败时，状态条里那句
      // 「APP_EMPTY 接口返回空响应」比 200 秒的超时栈有用得多。
      const video = await window.__H.wait(() => window.__H.q("section.player-shell video"), ${TIMEOUTS.download}, "视频元素").catch((e) => {
        const foot = window.__H.all(".player-foot .status, .player-foot .view-sub").map((s) => s.textContent.trim()).join(" ｜ ");
        throw new Error(e.message + "｜状态条=" + (foot || "(空)"));
      });
      out.prepareMs = Date.now() - started;
      out.srcKind = window.__H.srcKind(video);
      await window.__H.wait(() => video.readyState >= 2 && video.videoWidth > 0, ${TIMEOUTS.decode}, "解码元数据");
      // 脚注（续播提示/告警）要在元数据到了之后采：元数据前 .player-foot 本来就
      // 没有东西，先采会把正常的首帧空窗误报成「没提示」。
      out.ready = window.__H.all(".player-foot .view-sub").map((s) => s.textContent.replace(/\\s+/g, " ").trim());
      out.shape = video.videoWidth + "x" + video.videoHeight;
      out.duration = Math.round((video.duration || 0) * 100) / 100;
      try { await video.play(); } catch (e) { out.playRejected = String(e?.name ?? e); }
      await window.__H.wait(() => video.currentTime > 0.3, 20_000, "时间轴推进").catch((e) => { out.warn = String(e.message); });
      out.at = Math.round(video.currentTime * 100) / 100;
      out.early = window.__H.pixels(video);
      if (out.duration > 10) {
        video.currentTime = Math.min(45, out.duration - 1);
        await window.__H.wait(() => video.readyState >= 2, 20_000, "seek 后缓冲").catch(() => {});
        await new Promise((r) => setTimeout(r, 900));
        out.seekedTo = Math.round(video.currentTime * 100) / 100;
        out.seeked = window.__H.pixels(video);
      }
      out.paused = video.paused;
      out.readyState = video.readyState;
      out.videoError = video.error ? "code " + video.error.code : null;
      out.warnBar = window.__H.all("p[class^=status]").map((p) => p.textContent.trim());
      out.firstSrc = video.currentSrc || video.src;
      out.videosInDom = window.__H.all("video").length;
      out.fit = window.__H.fit(video);
      return out;
      `,
      TIMEOUTS.download + TIMEOUTS.decode + 90_000,
    );
    const frame = played.seeked ?? played.early;
    // 从头播第一集时脚注本来就是空的（只有续播/到尾页才写东西），这里只验证取材
    // 通道本身没抛错；真正的提示文案由后面的「续播」用例去断言。
    check("取源后就绪", Array.isArray(played.ready) && played.srcKind, `${played.srcKind}｜脚注：${(played.ready ?? []).join(" ｜ ") || "（从头播，无提示，正常）"}`);
    check("画面尺寸有效", /^\d{3,4}x\d{3,4}$/.test(played.shape ?? "") && played.shape !== "0x0", played.shape);
    check("时长读出正常", played.duration > 5, `${played.duration}s`);
    check("时间轴在推进（非冻帧）", played.at > 0.3 && played.paused === false, `at=${played.at}s paused=${played.paused} readyState=${played.readyState} 用时 ${(played.prepareMs / 1000).toFixed(1)}s`);
    check("取样不是黑屏", frame && (frame.blocked === true || frame.spread > 1), `首段 ${played.early?.spread} → seek 后 ${played.seeked?.spread ?? "-"}（均值 ${frame?.mean ?? "-"}）`);
    check("视频元素无解码错误", played.videoError === null, played.videoError ?? "无");
    check("播放页无告警条", (played.warnBar ?? []).length === 0, (played.warnBar ?? []).join("；"));
    check("同时只有一个视频元素", played.videosInDom === 1, `${played.videosInDom} 个`);
    checkLayout("红果", played.fit);
    check("窗口右侧没有页面滚动条", played.fit?.pageScroll <= 1, `文档比视口高 ${played.fit?.pageScroll}px`);
    await capture("2-player-hongguo");

    // 换一集：用播放器自己的「下一集」，本地文件必须被回收。
    const switching = await run(
      "switch",
      `
      const out = { before: window.__H.all("video").length };
      const stale = ${JSON.stringify(played.firstSrc ?? "")};
      const before = window.__H.text("h2.player-title");
      const next = window.__H.all(".player-nav button").find((b) => b.textContent.includes("下一集") && !b.disabled);
      if (!next) return { error: "下一集不可用：" + before };
      next.click();
      await window.__H.wait(() => window.__H.text("h2.player-title") !== "" && window.__H.text("h2.player-title") !== before, ${TIMEOUTS.streams}, "标题换集");
      out.title = window.__H.text("h2.player-title");
      const video = await window.__H.wait(() => window.__H.q("section.player-shell video"), ${TIMEOUTS.download}, "第二集视频");
      await window.__H.wait(() => video.readyState >= 2 && video.videoWidth > 0, ${TIMEOUTS.decode}, "第二集元数据");
      out.shape = video.videoWidth + "x" + video.videoHeight;
      out.pixels = window.__H.pixels(video);
      out.srcKind = window.__H.srcKind(video);
      out.newSrc = (video.currentSrc || video.src) !== stale;
      out.stale = stale.startsWith("blob:") ? await window.__H.blobGone(stale) : { alive: false, skipped: "上一集是直连，没有本地文件" };
      out.videos = window.__H.all("video").length;
      out.prev = window.__H.all(".player-nav button").find((b) => b.textContent.includes("上一集"))?.disabled ?? null;
      return out;
      `,
      TIMEOUTS.streams + TIMEOUTS.download + TIMEOUTS.decode + 60_000,
    );
    check("下一集换集后重新出画", !switching.error && /^\d{3,4}x\d{3,4}$/.test(switching.shape ?? ""), `${switching.title ?? switching.error} · ${switching.shape} · ${switching.srcKind}`);
    check("上一集的本地文件已回收", switching.stale?.alive === false, switching.stale?.skipped ?? (switching.stale?.alive ? "仍可取到，说明没释放" : "已释放"));
    check("换集后仍只有一个视频元素，且能回上一集", switching.videos === 1 && switching.prev === false, `${switching.videos} 个，上一集禁用=${switching.prev}`);

    // 退出 → 详情页必须给出「继续播放」，点回去要真的落在上次那个位置。
    const resuming = await run(
      "resume",
      `
      const out = {};
      const at = (() => { const v = window.__H.q("section.player-shell video"); return v ? Math.round(v.currentTime) : 0; })();
      out.watchedSeconds = at;
      await window.__H.exitPlayer();
      out.head = window.__H.text("h2");
      out.resumeText = window.__H.text(".series-actions");
      out.resumeButton = Boolean(window.__H.q("button.primary-action"));
      out.mark = window.__H.text(".episode-mark");
      out.watched = window.__H.all("button.episode.watched").length;
      if (!out.resumeButton) return out;
      window.__H.q("button.primary-action").click();
      await window.__H.player();
      const video = await window.__H.wait(() => window.__H.q("section.player-shell video"), ${TIMEOUTS.download}, "续播视频");
      await window.__H.wait(() => video.readyState >= 2 && video.videoWidth > 0, ${TIMEOUTS.decode}, "续播元数据");
      out.resumedTo = Math.round(video.currentTime * 100) / 100;
      out.note = window.__H.all(".player-foot .view-sub").map((s) => s.textContent.trim());
      out.title = window.__H.text("h2.player-title");
      return out;
      `,
      TIMEOUTS.download + TIMEOUTS.decode + 40_000,
    );
    check("退出播放回到详情页", (resuming.head ?? "").length > 1 && resuming.resumeButton === true, `${resuming.head} · ${resuming.resumeText}`);
    check("详情页提示可继续播放（含集号与时间点）", /继续播放|再看一遍/.test(resuming.resumeText ?? "") && /第\s*\d+\s*集/.test(resuming.resumeText ?? ""), resuming.resumeText);
    check("看过的集在选集网格里有标记", resuming.watched >= 1 && (resuming.mark ?? "").length > 0, `${resuming.watched} 集有标记，位置 ${resuming.mark}`);
    check("继续播放真的接上上次的位置", resuming.resumedTo > 10 || resuming.resumeButton !== true, `上次 ${resuming.watchedSeconds}s → 续播落在 ${resuming.resumedTo}s（${(resuming.note ?? []).join(" ｜ ")}）`);

  } catch (error) {
    check("播放链路", false, String(error?.message ?? error));
  } finally {
    // 收尾必须在 finally 里：这一段一旦中途失败，播放器会开着留给下一段，于是下一段
    // 的「回到榜单」永远等不到卡片——一处上游抖动就会串成三处失败。清场本身失败不算用例。
    await run("leave", `await window.__H.exitPlayer().catch(() => {}); await window.__H.home(); return true;`, TIMEOUTS.cards + 20_000)
      .catch((error) => console.log(`  !! 清场失败（不算断言）：${String(error?.message ?? error)}`));
  }

  // ══ 场景三：另两个播放源（直连明文，不需要本地处理）══════════════════
  currentGroup = "第三方源";
  // 一段式跑完「切页签 → 进详情 → 出画」要等 5 分钟，中间任何一步慢一点，CDP 就先超时。
  // 拆成三次求用，每次的预算只覆盖自己那一步，失败信息也直接说明卡在哪一步。
  const SRC = { cards: 120_000, episodes: 120_000, video: 150_000 };
  async function thirdPartySource(label, tabName) {
    let stage = "回到榜单首页";
    try {
      await run(`${label}·home`, `await window.__H.home(); return true;`, TIMEOUTS.cards + 20_000);

      stage = "目录";
      const open = await run(
        `${label}·目录`,
        `
        const tab = window.__H.tab(${JSON.stringify(tabName)});
        if (!tab) return { error: "顶栏没有「" + ${JSON.stringify(tabName)} + "」" };
        const headBefore = window.__H.text(".view-head h2");
        tab.click();
        // 换页签会重挂列表，所以这里等到「新目录真的铺出来」，不会拿到上一类的残留卡片。
        await window.__H.settle();
        await window.__H.wait(() => window.__H.many("button.card", 5), ${SRC.cards}, "来源目录");
        return {
          cards: window.__H.all("button.card").length,
          head: window.__H.text(".view-head h2"),
          headBefore,
          chips: window.__H.all("button.chip").length,
          currentTab: window.__H.text("button.tab.current"),
        };
        `,
        SRC.cards + 30_000,
      );
      if (open.error) {
        check(`${label}：整条链路`, false, `${stage} — ${open.error}`);
        return;
      }
      check(
        `${label}：目录与题材`,
        open.cards >= 5 &&
        open.chips >= 1 &&
        open.currentTab === tabName &&
        /红果/.test(open.head ?? "") === false &&
        open.head !== open.headBefore,
        `${open.currentTab} → ${open.head}，${open.cards} 张卡 / ${open.chips} 个题材（切换前是「${open.headBefore}」）`,
      );

      stage = "分集";
      const detail = await run(
        `${label}·分集`,
        `
        window.__H.all("button.card")[0].click();
        const eps = await window.__H.wait(() => window.__H.many("button.episode"), ${SRC.episodes}, "来源选集");
        return {
          episodes: eps.length,
          series: window.__H.text("h2"),
          total: window.__H.text(".series-hero .view-sub"),
          notice: window.__H.all("section.view p[class^=status]").map((p) => p.textContent.trim()),
        };
        `,
        SRC.episodes + 30_000,
      );
      check(`${label}：分集与详情`, detail.episodes >= 1 && /\d+\s*集/.test(detail.total ?? ""), `${detail.series} · ${detail.total}${detail.notice?.length ? " · " + detail.notice.join("｜") : ""}`);

      stage = "出画";
      const play = await run(
        `${label}·播放`,
        `
        const out = {};
        const ep = window.__H.all("button.episode")[0];
        // 点开的是哪一集，标题就得写到哪一集；播的是哪一部，标题就得是那一部的名字。
        // 少了这条对照，上一轮「红果播放器没关掉」串进来的另一部剧会被当成西饭的成绩通过。
        const wantSeries = window.__H.text("h2");
        // 同上：集号取 .episode-no，避开看过的格子上拼接的时间标记。
        out.pickedNo = window.__H.digitPrefix(ep.querySelector(".episode-no")?.textContent ?? ep.textContent);
        ep.click();
        await window.__H.player();
        const video = await window.__H.wait(() => window.__H.q("section.player-shell video"), ${SRC.video}, "来源视频").catch(() => null);
        if (video === null) {
          // 空手回来最难查：把屏幕上真的写了什么一起带出来，别只说「没出画」。
          return {
            error:
              "没出画｜在播放页=" + Boolean(window.__H.q("section.player-shell")) +
              "｜舞台：" + (window.__H.text(".player-stage") || "(空)") +
              "｜脚部：" + (window.__H.all(".player-foot .view-sub, .player-foot .status").map((s) => s.textContent.trim()).join(" ｜ ") || "(空)") +
              "｜标题：" + (window.__H.text("h2.player-title") || "(空)"),
          };
        }
        await window.__H.wait(() => video.readyState >= 2 && video.videoWidth > 0, ${TIMEOUTS.decode}, "来源元数据");
        out.shape = video.videoWidth + "x" + video.videoHeight;
        out.duration = Math.round((video.duration || 0) * 100) / 100;
        out.srcKind = window.__H.srcKind(video);
        out.ready = window.__H.all(".player-foot .view-sub").map((s) => s.textContent.replace(/\\s+/g, " ").trim());
        out.title = window.__H.text("h2.player-title");
        out.wantSeries = wantSeries;
        out.match = window.__H.sameSeries(out.title, wantSeries);
        out.rightSeries = wantSeries.length > 0 && out.match.ok === true;
        out.rightEpisode = out.pickedNo.length > 0 && window.__H.strip(out.title ?? "").includes("第 " + out.pickedNo + " 集");
        try { await video.play(); } catch (e) { out.playRejected = String(e?.name ?? e); }
        await window.__H.wait(() => video.currentTime > 0.3, 20_000, "来源时间轴").catch((e) => { out.warn = String(e.message); });
        out.at = Math.round(video.currentTime * 100) / 100;
        out.pixels = window.__H.pixels(video);
        out.videoError = video.error ? "code " + video.error.code : null;
        out.fit = window.__H.fit(video);
        return out;
        `,
        SRC.video + TIMEOUTS.decode + 60_000,
      );
      if (play.error) {
        check(`${label}：整条链路`, false, `${stage} — ${play.error}`);
        await capture(`3-player-${tabName}-FAIL`);
        return;
      }
      check(`${label}：播的就是刚点开的那一部那一集`, play.rightSeries === true && play.rightEpisode === true, `详情页《${play.wantSeries}》第 ${play.pickedNo} 集 → 播放页「${play.title}」${play.match && play.match.ok !== true ? "｜差异 @" + play.match.at + " 码点 " + play.match.titleCp + " vs " + play.match.seriesCp + " 原文 " + play.match.raw : ""}`);
      check(`${label}：直接播放（不走本地文件）`, play.srcKind === "来源直连", `${play.title} · ${play.shape} · ${play.duration}s · ${(play.ready ?? []).join(" ｜ ")}`);
      check(`${label}：时间轴推进、无解码错误`, play.at > 0.3 && play.videoError === null, `at=${play.at}s，取样 spread ${play.pixels?.blocked ? "跨域不可读" : play.pixels?.spread}${play.warn ? ` · ${play.warn}` : ""}`);
      checkLayout(label, play.fit);
      await capture(`3-player-${tabName}`);
      // 竖屏时右侧有分集栏：它得真的能换集，而且付费集不能是「看着能点」的。
      if (play.fit?.side === true) {
        stage = "分集栏换集";
        const rail = await run(
          `${label}·分集栏`,
          `
          const out = {};
          const before = window.__H.text("h2.player-title");
          const rows = window.__H.all(".side-row");
          out.rows = rows.length;
          out.locked = window.__H.all(".side-row.locked").length;
          out.currentRows = window.__H.all(".side-row.current").length;
          out.currentText = window.__H.text(".side-row.current");
          const target = rows.find((b) => !b.disabled);
          if (!target) return { error: "分集栏里没有可点的集（全被锁了？）", rows: out.rows };
          out.picked = target.textContent.trim();
          target.click();
          await window.__H.settle();
          const video = await window.__H
            .wait(() => { const v = window.__H.q("section.player-shell video"); return v && v.readyState >= 2 && v.videoWidth > 0 ? v : null; }, ${SRC.video}, "分集栏换集后出画")
            .catch(() => null);
          if (video === null) return { ...out, error: "点了分集栏但没出画" };
          out.title = window.__H.text("h2.player-title");
          out.changed = out.title !== before;
          out.shape = video.videoWidth + "x" + video.videoHeight;
          out.nowCurrent = window.__H.text(".side-row.current");
          out.stuck = video.paused;
          return out;
          `,
          SRC.video + TIMEOUTS.decode + 40_000,
        );
        if (rail.error) check(`${label}：竖屏分集栏`, false, `${stage} — ${rail.error}`);
        else {
          check(
            `${label}：分集栏只高亮当前集，锁着的置灰`,
            rail.currentRows === 1 && rail.rows >= 2,
            `${rail.rows} 格，当前「${rail.currentText}」，置灰 ${rail.locked} 格`,
          );
          check(
            `${label}：分集栏点一下直接换集`,
            rail.changed === true && new RegExp(`第\\s*${rail.picked}\\s*集`).test(rail.title ?? "") && rail.nowCurrent === rail.picked,
            `点「${rail.picked}」→ ${rail.title} · ${rail.shape}`,
          );
        }
      }
      await run(`${label}·退出`, `await window.__H.exitPlayer(); return true;`, 30_000);
    } catch (error) {
      check(`${label}：整条链路`, false, `卡在「${stage}」— ${String(error?.message ?? error)}`);
    }
  }
  if (wanted("third")) {
    // 河马短剧入口已隐藏（暂不支持付费播放），这里只跑西饭。
    await thirdPartySource("西饭短剧", "西饭短剧");
  }

  // ══ 场景四：首页继续观看行（按类目）+ 「我的」页（最近与收藏融合）═══════════
  currentGroup = "观看记录";
  if (wanted("history")) try {
    const home = await run(
      "home-recent",
      `
      const out = {};
      await window.__H.home();
      out.railHead = window.__H.text(".recent-head h3");
      // 继续观看行是异步列表：榜单卡片先铺出来、历史行还在路上，所以要等而不是切。
      await window.__H.wait(() => window.__H.many("button.recent-card"), 15_000, "本类目继续观看行").catch(() => null);
      out.rail = window.__H.all("button.recent-card").length;
      out.first = window.__H.all("button.recent-card")[0]?.textContent.replace(/\\s+/g, " ").trim().slice(0, 60) ?? "";
      out.meta = window.__H.all(".recent-card .meta").map((m) => m.textContent.trim());
      if (out.rail > 0) {
        window.__H.all("button.recent-card")[0].click();
        await window.__H.player();
        out.title = window.__H.text("h2.player-title");
        await window.__H.exitPlayer();
      }
      // 类目隔离：首页只列红果的记录，西饭那一行不会串到这里。
      out.noCrossCategory = out.meta.every((m) => !/西饭|烤猪蹄|潮汐/.test(m));
      return out;
      `,
      TIMEOUTS.cards + TIMEOUTS.direct + 60_000,
    );
    check("首页最前面有本类目的继续观看行", home.rail >= 1 && /继续观看/.test(home.railHead ?? ""), `${home.railHead}：${home.rail} 部，首条「${home.first}」`);
    if (home.rail >= 1) {
      check("继续观看行点得进播放", (home.title ?? "").length > 3, home.title ?? "");
    }
    check("继续观看行不串其它类目的记录", home.noCrossCategory === true, (home.meta ?? []).join(" ｜ ") || "无");
    if (home.rail === 0) {
      // 红果取源抖动时本类目没记录可记（上游接口时好时坏），但西饭那一类必须能
      // 单独成行——类目隔离的另一半。降级断言，不让上游故障伪装成产品缺陷。
      // （河马页签已隐藏，隔离断言改用西饭。）
      const xifanRail = await run(
        "xifan-recent",
        `
        const tab = window.__H.tab("西饭短剧");
        if (!tab) return { error: "顶栏没有西饭短剧" };
        tab.click();
        await window.__H.settle();
        await window.__H.wait(() => window.__H.many("button.recent-card"), 30_000, "西饭的继续观看行").catch(() => null);
        return { rail: window.__H.all("button.recent-card").length, first: window.__H.all("button.recent-card")[0]?.textContent.replace(/\\s+/g, " ").trim().slice(0, 60) ?? "" };
        `,
        TIMEOUTS.cards + 40_000,
      );
      check("西饭页签有自己的继续观看行", xifanRail.rail >= 1, `${xifanRail.rail ?? 0} 部，首条「${xifanRail.first ?? xifanRail.error}」`);
      await run("recent-leave", `await window.__H.home(); return true;`, TIMEOUTS.cards + 20_000).catch(() => { });
    }

    const page = await run(
      "history-page",
      `
      const out = {};
      const tab = window.__H.tab("我的");
      if (!tab) return { error: "顶栏没有我的" };
      tab.click();
      await window.__H.wait(() => window.__H.many(".history-grid button.card"), 30_000, "观看记录卡片");
      out.rows = window.__H.all(".history-grid button.card").length;
      out.head = window.__H.text(".view-head h2");
      out.sub = window.__H.text("header .view-sub");
      out.favSection = window.__H.many(".favorite-section");
      out.firstRow = window.__H.text(".history-grid .card");
      Object.assign(out, window.__H.windowScroll());
      // 点卡片进详情，续播由详情页的按钮承接；顺手把这部剧收藏上
      window.__H.all(".history-grid button.card")[0].click();
      await window.__H.wait(() => window.__H.many("button.episode"), ${TIMEOUTS.episodes}, "历史进入详情");
      out.seriesHead = window.__H.text("h2");
      out.resume = window.__H.text(".series-actions");
      const favBtn = window.__H.q("button.favorite-action");
      if (!favBtn) return { ...out, error: "详情页没有收藏按钮" };
      favBtn.click();
      await window.__H.wait(() => /已收藏/.test(window.__H.text("button.favorite-action")), 15_000, "收藏点亮");
      out.favorited = window.__H.text("button.favorite-action");
      out.favToast = window.__H.text(".toaster .toast") ?? (await window.__H.wait(() => window.__H.q(".toaster .toast"), 5_000, "收藏提示").then((el) => el.textContent.trim()).catch(() => ""));
      // 回融合页：收藏区点亮，再清空历史
      const back = window.__H.tab("我的");
      back.click();
      await window.__H.wait(() => window.__H.many(".favorite-section .card"), ${TIMEOUTS.episodes}, "收藏区");
      out.favCards = window.__H.all(".favorite-section .card").length;
      out.favBadge = window.__H.text(".favorite-section .card .badge");
      const danger = window.__H.q("button.danger");
      if (!danger) return { ...out, error: "融合页没有清空历史按钮" };
      danger.click();
      await window.__H.wait(() => !window.__H.q("button.danger"), 20_000, "清空历史").catch(() => null);
      out.afterClear = window.__H.many(".history-grid") + "|" + window.__H.all(".favorite-section .card").length;
      out.favStill = window.__H.all(".favorite-section .card").length;
      // 收尾：取消收藏，不给后续跑测留垃圾
      window.__H.q(".favorite-section .card .card-remove").click();
      out.removeToast = window.__H.text(".toaster .toast") ?? (await window.__H.wait(() => window.__H.q(".toaster .toast"), 5_000, "取消提示").then((el) => el.textContent.trim()).catch(() => ""));
      await window.__H.wait(() => window.__H.all(".favorite-section .card").length === out.favStill - 1, 15_000, "取消收藏").catch(() => null);
      out.favAfterRemove = window.__H.all(".favorite-section .card").length;
      return out;
      `,
      TIMEOUTS.episodes + 120_000,
    );
    if (page.error) check("我的页", false, page.error);
    else {
      check("我的页列出观看过的部", page.rows >= 1 && /我的/.test(page.head ?? ""), `${page.rows} 张卡 · ${page.sub}`);
      check("卡片标出看到第几集或完播", /看到第\s*\d+\s*集|完播/.test(page.firstRow ?? ""), page.firstRow?.slice(0, 90));
      checkNoWindowScroll("融合页", page);
      check("点卡片进详情并提示续播动作", page.rows >= 1 && (page.seriesHead ?? "").length > 1 && /继续播放|再看一遍/.test(page.resume ?? ""), `${page.seriesHead} · ${page.resume}`);
      check("详情页点亮收藏后融合页收藏区置顶", /已收藏/.test(page.favorited ?? "") && page.favCards >= 1 && /收藏/.test(page.favBadge ?? ""), `${page.favCards} 部收藏 · ${page.favBadge}`);
      check("清空历史只删记录、收藏不受影响", page.favStill === page.favCards && /收藏不受清空影响/.test(page.sub ?? ""), `收藏 ${page.favStill} 部`);
      check("取消收藏即时从收藏区移走", page.favAfterRemove === page.favStill - 1, `收藏区 ${page.favAfterRemove} 张`);
      check("收藏按钮点了有反馈：文案翻面 + 右下角提示", /已收藏/.test(page.favorited ?? "") && /已加入收藏/.test(page.favToast ?? ""), `${page.favorited} · ${page.favToast}`);
      check("页内 ✕ 取消收藏也给一条提示", /已取消收藏/.test(page.removeToast ?? ""), page.removeToast);
    }
  } catch (error) {
    check("观看记录", false, String(error?.message ?? error));
  }

  // ══ 场景五：边界与防泄露 ═══════════════════════════════════════════════
  currentGroup = "边界";
  if (wanted("guard")) try {
    const guard = await run(
      "guard",
      `
      const out = {};
      // 播放页必须能优雅失败：给一个不可能的 vid 让后端返回业务错误。
      const invoke = window.__TAURI_INTERNALS__.invoke;
      try {
        await invoke("resolve_play", { vid: "0000000000", seriesId: "0" });
        out.resolve = "本该失败";
      } catch (e) {
        out.resolveCode = String(e?.code ?? "");
        out.resolveMessage = String(e?.message ?? e).slice(0, 60);
      }
      // 非法编号必须被拒，而不是播出一段不知道是什么的东西。
      for (const bad of ["hema:1:2:3", "xifan:nope:1:2:3"]) {
        try { await invoke("resolve_play", { vid: bad }); out["reject_" + bad] = "本该失败"; }
        catch (e) { out["reject_" + bad] = String(e?.message ?? e).slice(0, 30); }
      }
      // 搜索空结果 / 上游 5xx 必须显示为状态条，而不是白屏。
      await window.__H.home();
      window.__H.type("form input", "这个关键词一定查不到zzzqqq");
      document.querySelector("form").requestSubmit();
      await window.__H.wait(() => {
        const head = window.__H.text("h2");
        const cards = window.__H.all("button.card").length;
        const statuses = window.__H.all("p[class^=status]").length;
        return head.includes("的搜索结果") && (cards > 0 || statuses > 0) ? true : null;
      }, 45_000, "搜索空态/错误态").catch(() => {});
      out.searchStatus = window.__H.all("p[class^=status]").map((p) => p.textContent.trim());
      out.searchCards = window.__H.all("button.card").length;
      out.searchHead = window.__H.text("h2");
      out.view = window.__H.q("section.view")?.className ?? "（没有 view 容器）";
      out.rootMounted = (document.getElementById("root")?.childElementCount ?? 0) > 0;
      out.bodyChars = (document.body.innerText ?? "").length;
      // 界面上不能出现来源域名/协议词汇。
      out.forbidden = ((document.body.innerText ?? "").match(/fqnovel|snssdk|hongguoduanju|kuaikaw|youlishipin|spade|gorgon|x-licepay|signature/gi) ?? []);
      out.docTitle = document.title;
      return out;
      `,
      TIMEOUTS.cards + 90_000,
    );
    check("非法 vid 被后端拒绝", guard.resolveCode !== "" || /片源|失败|错误|不存在|Business|NO_PLAYABLE/.test(guard.resolveMessage ?? ""), `${guard.resolveCode} ${guard.resolveMessage ?? ""}`);
    check("第三方编号格式校验", Object.keys(guard).every((key) => !key.startsWith("reject_") || guard[key] !== "本该失败"), Object.keys(guard).filter((k) => k.startsWith("reject_")).map((k) => `${k.slice(7)}→${guard[k]}`).join(" ｜ "));
    check("查不到的词不白屏", guard.rootMounted === true && (guard.searchCards > 0 || guard.searchStatus.length > 0), `卡片 ${guard.searchCards}，状态条 ${(guard.searchStatus ?? []).join("｜") || "无"}，页头「${guard.searchHead}」，容器 ${guard.view}，正文 ${guard.bodyChars} 字`);
    check("页面文案不含来源域名与协议词汇", (guard.forbidden ?? []).length === 0, (guard.forbidden ?? []).join("、") || "无");
    check("文档标题是短剧随心看", guard.docTitle === "短剧随心看", guard.docTitle);
    check("Tauri 运行时可调用", true, "command 正常返回/报错");
  } catch (error) {
    check("边界处理", false, String(error?.message ?? error));
  }

  client.close();
  if (appProcess && !KEEP) {
    console.log("· 关闭桌面窗口");
    // shell:true 起的是包装进程，kill 只掉它会把 cargo / tauri / 应用留在后台，
    // 下次 pnpm tauri dev 直接撞 1420 端口。按进程树强杀。
    try {
      execSync(`taskkill /PID ${appProcess.pid} /T /F`, { stdio: "ignore" });
    } catch {
      appProcess.kill("SIGTERM");
    }
    await new Promise((r) => setTimeout(r, 2500));
  }

  const failed = results.filter((r) => !r.ok);
  console.log("");
  console.log(`共 ${results.length} 项断言，通过 ${results.length - failed.length}，失败 ${failed.length}`);
  for (const failure of failed) console.log(`  FAIL [${failure.group}] ${failure.name} — ${failure.detail}`);
  process.exitCode = failed.length > 0 ? 1 : 0;
}

main().catch((error) => {
  console.error(`脚本失败：${String(error?.stack ?? error).split("\n").slice(0, 3).join(" | ")}`);
  process.exitCode = 2;
});
