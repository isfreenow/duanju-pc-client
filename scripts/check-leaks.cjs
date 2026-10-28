/**
 * Leak gate.
 *
 * The project's one structural promise is that the Red Fruit protocol lives in
 * Rust: signing, device identity, endpoints, headers, key derivation. The webview
 * is allowed exactly two per-play values — the resolved media address and its
 * content key — and only for the episode on screen. That is a convention unless
 * something checks it on every build, so this script greps the whole front-end
 * tree for the Rust-side vocabulary and for hardcoded origins, and fails loudly.
 *
 * It scans source only (`src`, `index.html`), never `src-tauri`, where these words
 * are the point of the code.
 *
 * Run: node scripts/check-leaks.cjs
 */

const fs = require("node:fs");
const path = require("node:path");

const root = path.resolve(__dirname, "..");
const targets = ["src", "index.html"];

/** Each rule is [label, pattern]; a match means the boundary moved. */
const RULES = [
  ["App/官网主机名", /(fqnovel|snssdk|hongguoduanju|qznovelvod|douyinvod|youlishipin|kuaikaw)/i],
  ["签名算法名", /(gorgon|khronos|argus|ladon|helios|medusa|abogus)/i],
  ["签名请求头", /x-(gorgon|khronos|argus|ladon|helios|medusa)/i],
  ["设备身份参数", /\b(device_id|cdid|iid|verify_ticket|_rticket|install_id|app_id)\b/],
  ["spade 密钥派生", /(spade|derive_content_key|decrypt_spade)/i],
  ["签名相关动词", /\bsign(ature|ing|ed)?\b/i],
  ["站点 Cookie", /(ttwid|passport_(aid|userid|token)|sessionid)\b/],
  ["硬编码来源", /https?:\/\//i],
];

/**
 * Comments are skipped: Vite strips them from the bundle, and this module's own
 * doc comments have to name the vocabulary in order to forbid it. Everything that
 * actually executes must be clean.
 */
const IGNORE_LINES = [/^\s*(\*|\/\/)\s/, /^\s*\/\*/];

/**
 * 本项目自己拥有或运营的公开链接：官网、issue 页、微软商店的 HEVC 扩展、统计 loader。
 * 「硬编码来源」这条规则要拦的是把红果内容从哪拉这件事透到前端；这些链接是
 * UI 文案与外部商店入口，不泄露任何协议细节，所以按(origin 粒度)放行。
 */
const ALLOWED_URLS =
  /(duanju\.isfreenow\.com|github\.com\/isfreenow\/duanju-pc-client|apps\.microsoft\.com\/detail|data\.autolinks\.site)/i;

function* walk(entry) {
  const stat = fs.statSync(entry);
  if (stat.isDirectory()) {
    for (const child of fs.readdirSync(entry)) {
      if (child === "node_modules" || child.startsWith(".")) continue;
      yield* walk(path.join(entry, child));
    }
    return;
  }
  if (/\.(ts|tsx|css|html|js)$/.test(entry)) yield entry;
}

const findings = [];
for (const target of targets) {
  const absolute = path.join(root, target);
  if (!fs.existsSync(absolute)) continue;
  for (const file of walk(absolute)) {
    const lines = fs.readFileSync(file, "utf8").split(/\r?\n/);
    lines.forEach((line, index) => {
      if (IGNORE_LINES.some((pattern) => pattern.test(line))) return;
      for (const [label, pattern] of RULES) {
        if (!pattern.test(line)) continue;
        // 只对「硬编码来源」开口子：命中的链接全部属于自家 origin 才算豁免，
        // 一行里混进任何别的 http(s) 地址仍照常报。其余规则零容忍。
        if (label === "硬编码来源") {
          const urls = line.match(/https?:\/\/\S+/g) ?? [];
          if (urls.length > 0 && urls.every((url) => ALLOWED_URLS.test(url))) continue;
        }
        findings.push(`${path.relative(root, file)}:${index + 1}  ${label}  ${line.trim()}`);
      }
    });
  }
}

if (findings.length > 0) {
  console.error(`发现 ${findings.length} 处前端越界引用（红果协议细节只能在 Rust 侧）：\n`);
  for (const finding of findings) console.error(`  ${finding}`);
  console.error("\n把这些内容移回 src-tauri，或说明为什么它属于业务 DTO。");
  process.exitCode = 1;
} else {
  const files = [...walkTargets()].length;
  console.log(`OK 未发现协议泄露（扫描 ${targets.join(", ")}，${files} 个文件）`);
}

function* walkTargets() {
  for (const target of targets) {
    const absolute = path.join(root, target);
    if (fs.existsSync(absolute)) yield* walk(absolute);
  }
}
