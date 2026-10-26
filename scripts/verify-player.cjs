/**
 * Offline check of the browser decryptor against a real recorded episode.
 *
 * `src/player/*.ts` is written for the webview, so this harness transpiles those
 * modules with the project's own TypeScript and runs them in Node — WebCrypto and
 * `setTimeout` exist in both, so the code under test is literally the shipped
 * code, not a mirror of it.
 *
 * Inputs (both git-ignored, both produced by the Rust dev test):
 *   fixtures/ep01.mp4       one whole encrypted episode
 *   fixtures/ep01.mp4.keyhex  its content key, 32 hex chars
 *
 * Refresh them with:
 *   cd src-tauri && HG_FIXTURE=../fixtures/ep01.mp4 \
 *     cargo test --test inspect_container -- --ignored --nocapture
 *
 * Run: node scripts/verify-player.cjs [fixture.mp4]
 */

const fs = require("node:fs");
const path = require("node:path");
const { execFileSync } = require("node:child_process");
const ts = require("typescript");

const root = path.resolve(__dirname, "..");
const work = path.join(root, ".tmp-verify");

function fail(message) {
  throw new Error(message);
}

function check(condition, message) {
  if (!condition) fail(message);
}

// The player modules are ES modules with extensionless imports; CommonJS output
// lets Node resolve them without a bundler.
function transpilePlayer() {
  fs.rmSync(work, { recursive: true, force: true });
  fs.mkdirSync(work, { recursive: true });
  fs.writeFileSync(path.join(work, "package.json"), JSON.stringify({ type: "commonjs" }));
  for (const name of ["mp4", "cenc"]) {
    const source = fs.readFileSync(path.join(root, "src", "player", `${name}.ts`), "utf8");
    const out = ts.transpileModule(source, {
      compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2020 },
      fileName: `${name}.ts`,
    }).outputText;
    fs.writeFileSync(path.join(work, `${name}.js`), out);
  }
  return {
    mp4: require(path.join(work, "mp4.js")),
    cenc: require(path.join(work, "cenc.js")),
  };
}

async function main() {
  const { mp4, cenc } = transpilePlayer();

  const fixture = path.resolve(process.argv[2] ?? path.join(root, "fixtures", "ep01.mp4"));
  const keyFile = [`${fixture}.keyhex`, `${fixture}.key`].find((candidate) => fs.existsSync(candidate)) ?? `${fixture}.keyhex`;
  check(fs.existsSync(fixture), `缺少样本 ${fixture}`);
  check(fs.existsSync(keyFile), `缺少密钥 ${keyFile}（Vite 拒发 .key 后缀，所以用 .keyhex）`);

  const bytes = new Uint8Array(fs.readFileSync(fixture));
  const key = mp4.hexToBytes(fs.readFileSync(keyFile, "utf8").trim());

  // The counter block and the pattern helper are where CENC semantics could
  // silently drift, so pin them before touching real bytes.
  const iv = Uint8Array.from([0xa1, 0x90, 0x8d, 0x10, 0x5d, 0xde, 0xe5, 0x77]);
  check(
    Buffer.from(cenc.counterBlock(iv)).toString("hex") === "a1908d105ddee5770000000000000000",
    "counter block 应为 IV‖BE64(0)",
  );
  check(JSON.stringify(cenc.protectedRanges(1000, 0, 0)) === "[[0,1000]]", "整样本加密的范围不对");
  check(JSON.stringify(cenc.protectedRanges(10, 2, 1)) === "[[1,2],[4,2],[7,2]]", "跳样模式范围不对");

  const plan = mp4.planCencFile(bytes);
  console.log(`容器 ${mp4.describePlan(plan)}，KID ${plan.keyId}，moov 重写 ${plan.shift} 字节`);

  const totalSamples = plan.tracks.reduce((sum, track) => sum + track.samples.length, 0);
  check(totalSamples > 0, "没有样本");
  for (const track of plan.tracks) {
    check(track.samples.length === track.ivs.length, `${track.kind} 轨道 IV 数量与样本不符`);
    for (const sample of track.samples) {
      check(sample.offset + sample.size <= bytes.length, "样本越界");
    }
  }

  // Metadata-only pass: after the rewrite the container must no longer advertise a
  // protection scheme, so planning it a second time should find nothing to decrypt.
  const skeleton = mp4.buildContainer(plan);
  let stillEncrypted = false;
  try {
    mp4.planCencFile(skeleton);
    stillEncrypted = true;
  } catch (error) {
    check(/没有发现加密样本/.test(error.message), `重写后解析结果异常：${error.message}`);
  }
  check(!stillEncrypted, "重写后的容器仍被判定为加密");

  let lastReport = 0;
  const started = Date.now();
  const plain = await cenc.decryptCencFile(plan, key, (progress) => {
    if (progress.done - lastReport >= 6_000_000) {
      lastReport = progress.done;
      console.log(`  解密 ${(progress.done / 1048576).toFixed(1)} / ${(progress.total / 1048576).toFixed(1)} MB`);
    }
  });
  const elapsed = Date.now() - started;

  check(plain.length === bytes.length + plan.shift, "输出长度与计划不符");
  const output = path.join(work, `${path.basename(fixture, ".mp4")}.plain.mp4`);
  fs.writeFileSync(output, Buffer.from(plain));
  console.log(`解密 ${totalSamples} 个样本、${(plain.length / 1048576).toFixed(1)}MB 用时 ${elapsed}ms → ${output}`);

  // The first video sample must now read as real HEVC: a 4-byte length, then a NAL
  // header whose type is one of the parameter-set / slice types.
  const video = plan.tracks.find((track) => track.kind === "video");
  check(video !== undefined, "没有视频轨道");
  const first = video.samples[0];
  const at = mp4.outputOffset(plan, first.offset);
  const nalLength = new DataView(plain.buffer, plain.byteOffset + at).getUint32(0);
  const nalType = (plain[at + 4] >> 1) & 0x3f;
  console.log(`首个视频样本：NAL 长度 ${nalLength}，类型 ${nalType}`);
  check(nalLength > 0 && nalLength <= first.size - 4, `NAL 长度不合理：${nalLength}`);
  check([1, 2, 5, 32, 33, 34, 39, 40].includes(nalType), `NAL 类型不像 HEVC：${nalType}`);

  // A decrypted container must still describe itself as encrypted if the rewrite
  // failed; assert the opposite so a silent no-op cannot pass.
  const head = Buffer.from(plain.subarray(0, Math.min(plain.length, 400_000))).toString("latin1");
  check(!head.includes("encv") && !head.includes("senc"), "输出里仍留着加密声明");

  if (process.env.HG_FFPROBE !== "0") {
    try {
      const info = execFileSync(
        "ffprobe",
        [
          "-v",
          "error",
          "-show_entries",
          "stream=codec_type,codec_name:format=format_name,duration",
          "-of",
          "default=noprint_wrappers=1",
          output,
        ],
        { encoding: "utf8" },
      );
      console.log(`ffprobe:\n${info.trim()}`);
      check(/codec_name=hevc/.test(info), "ffprobe 没看到 hevc");
      check(/codec_name=aac/.test(info), "ffprobe 没看到 aac");
    } catch (error) {
      const message = String(error.message ?? error).split("\n")[0];
      if (message.includes("spawnSync") || message.includes("ENOENT")) {
        console.log(`ffprobe 不可用，跳过外部校验：${message}`);
      } else {
        check(false, `ffprobe 校验失败：${message}`);
      }
    }
  }

  console.log("OK 解密链路验证通过");
}

main().catch((error) => {
  console.error(`FAIL ${error.message}`);
  process.exitCode = 1;
});
