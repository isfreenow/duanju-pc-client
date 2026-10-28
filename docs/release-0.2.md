# 0.2.0 发布说明（相对 0.1.6 的增量）

这一轮的三条主线：网络层健壮性一次独立改造、追剧闭环（预取 / 检测 / 角标）、
播不出来的机器也能播（兼容转码按需下载工具）。以下按提交面逐项说明。

## 网络层健壮性（一次独立改造，不夹带功能）

- **`src-tauri/src/api/netguard.rs`（新增）**：SSRF 校验与「代理 socket 隔离」。
  - `classify_address` / `validate_addresses`：保留段（环回、私网、链路本地等）
    一律拒连；Fake-IP 段（198.18.0.0/15）不当错误，而是升级信号——换 DoH 重解。
  - `route_for`：官网这一跳直接走 DoH（Cloudflare / Google 两路竞速，答案必须
    全是公网才收下），其余 host 先系统解析、撞到全 Fake-IP 再升级 DoH。校验后
    的地址钉进独立的 pinned client 池（按「主机 + 地址集合」分组），不给请求
    沿用默认池的机会，这就是 socket 隔离。
  - `check_hop`：重定向逐跳校验协议 / 凭据 / 地址，最多 5 跳，与参考实现的
    `maxRedirects: 5` 同量级。
  - `DUANJU_PROXY` 显式代理：设了就走代理池并**不再**对目标做本地 IP 校验
    ——解析发生在代理远端，本地看不到真实地址，信任边界放在代理上是诚实做法。
- **错误语义分离**：`ApiError` 增加 `recovery_hint`（这一类错误用户下一步该
  干什么），`CommandError` 增加 `hint` 并随 IPC 透出；前端 `client.ts` 抛类型化
  `IpcError{code, hint}`，`errorWithHint` 把提示并进一行文案。

## 分集页：双路线竞速 + vid 级熔断

- `web.rs` 拉分集清单时网页接口与 App 接口两条路线并发竞速，先给出合法结果
  的胜出；一边挂了另一边照旧，全挂才报错。
- 熔断按 **vid 粒度**记在 `PlayCache`：某一条流的源地址反复拉不动（403 / 超时
  链），只熔断这一条 vid，不再连带整部剧的缓存。

## 搜索：标题归一化 + 季数感知排序

- **`src-tauri/src/api/rank.rs`（新增，纯函数）**：`normalize_search_text`
  （全半角 / 空白 / 标点归一）、`season_title` / `family_base`（《XX 第二季》
  与《XX2》识别出同一基础名）、`score_match`（同名 > 包含 > 前缀的匹配分级）、
  `find_sequels`（同族续作候选）、`rank_order`（按匹配分 + 季数稳定排序）。
- `app_search.rs` / 官网搜索统一经 `run_search` 接入这套排序。

## 功能增量

- **后续两集预取**：命令 `prefetch_play` 把「签接口 + 取回源地址 + 解 spade」
  提前跑完，结果只进 Rust 侧会话缓存；`PlayerView` 进入一集后对接下来两集静默
  预取（河马付费集不预取），真正切集时秒开。`drop_prefetch` 供播放器在探测到
  某条流拉不动时作废对应缓存。
- **追剧检测（新集 + 续作）**：`history.rs` 每条记录新增 `known_total` /
  `acknowledged_total` / `last_checked_at`；确认只往前走。命令
  `check_series_update`（单部，24h 节流回缓存）、`check_updates`（批量上限 20）、
  `acknowledge_series`（点开即清角标）。「我的」页卡片左上角出现
  「新增 N 集」/「续作 N 部」角标，点卡片即就地清除。续作检测只对红果做，
  河马/西饭只报新集。
- **会话级片单缓存 + 失败保留旧片单**：前端新增 `src/sessionCache.ts`
  （模块级 Map，48 键、命中续期、只存业务片单——播放地址与密钥仍只在 Rust）；
  `useAsync` 增加 `keepStale` / `cacheKey` 两个选项。首页四类目、类目页、
  详情页分集与推荐池、播放器分集栏、搜索第一页全部接入：刷新失败留着屏幕上
  的旧片单，切回已看过的页先画旧结果再静默重取，不闪骨架屏。
- **播放控制补全**：倍速档位（0.5x–2x，`,`/`.` 键升降，切集重置回 1x）、
  画中画小窗（内核不支持则不出按钮）、空格/k 播放暂停、←/→ 五秒跳转、
  m 静音、n/p 上下集。
- **兼容转码（HEVC → H.264）**：`src-tauri/src/transcode.rs`。转码工具
  **首次需要时按需下载**（zip 解包取最浅层可执行文件，带 sha 校验与看门狗），
  不打进安装包。命令 `transcode_status` / `prepare_transcode` /
  `transcode_episode` / `cancel_transcode`，进度经 `transcode-progress` 事件
  推给播放器（工具下载 / 取片 / 编码三段，可取消；离开播放页自动取消）。
  产物落缓存目录，走 asset 协议（`Cargo.toml` 启用 `protocol-asset`，
  `tauri.conf.json` scope 限定 `$APPDATA/**`）+ `convertFileSrc` 播放。
  入口只在「这台机器解不了这条流」时出现。

## 工程机制

- 命令注册表：`lib.rs` 的 `invoke_handler` 本轮新增 10 条命令（prefetch /
  drop / 追剧检测 3 条 / 转码 4 条 / flush 保持），DTO 一律
  `#[serde(rename_all = "camelCase")]`，前端 `client.ts` 镜像同名接口。
- `scripts/check-leaks.cjs`：「硬编码来源」规则增加本项目自有 origin 的白名单
  （官网 / issue 页 / 微软商店 / 统计 loader），一行里混入任何其它 http(s)
  地址仍照常报；红果协议词汇各规则保持零容忍。

## 验证

- `cargo test --lib`：**84 passed / 0 failed**（含 netguard 纯函数、rank
  归一与季数、history 确认单调性、transcode 解包与真实子进程捕获等）。
- `pnpm run check`：泄露门禁 + `tsc --noEmit` + `vite build` 全部通过。
- 未做运行时验证的：预取/转码/角标的真机 e2e 走查（转码依赖现场下载
  FFmpeg，角标依赖远端有新集数据），属后续回归项。
