---
name: duanju-search-play
description: 在本地短剧桌面客户端（红果/西饭短剧）里搜索剧名并按 1、2、3 序号列出结果，再按用户回复的序号唤起客户端从第一集或上次进度开播。当用户说「搜索短剧XX」「我要看短剧XX」「第2个」「就3」这类只给剧名或序号的请求，或要求用 Agent 驱动该客户端播放时使用。
---

# 短剧搜索与按序号开播

程序认三个命令行入口，Agent 不需要操作界面：

- `--search 关键词`：只查一次列表，打到 stdout 就退出，**不开窗口**
- `--series <ID>`：唤起窗口，直接进这一部的详情页并开播（有历史就续播）
- `--watch 关键词`：唤起窗口，程序自己搜、自己挑最匹配的一条开播

## 工作流

1. 解析可执行文件路径（见下）
2. 搜索：`& $APP --search 持械入宋 | Out-String`
3. 把输出原样列给用户（序号 + 剧名，别改顺序、别吞掉 ID），等用户选
4. 用户回序号 → 取那一行的 ID：`& $APP --series 7671966197985856536`
5. 用户一上来就给了剧名、没要列表 → 跳过第 2 步直接 `& $APP --watch 剧名`

打包后的产品名是中文（短剧随心看），exe 文件名由 bundler 从产品名推导、不保证稳定，
所以安装版优先读 NSIS 写进注册表卸载项的 InstallLocation（产品名与 exe 名都无需知道），
命中不到再回退旧的英文名安装。下面后两条是仓库内的相对路径（在本仓库里用）：

```powershell
$reg = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\com.administrator.hongguo-desktop-client',
  'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\com.administrator.hongguo-desktop-client'
$installed = ''
foreach ($k in $reg) { if (-not $installed -and (Test-Path $k)) {
  $installed = (Get-ItemProperty $k).InstallLocation } }
$APP = @($env:DUANJU_EXE,
  $(if ($installed) { (Get-ChildItem $installed -Filter *.exe -File |
      Where-Object { $_.Name -ne 'uninstall.exe' } | Select-Object -First 1).FullName }),
  "$env:LOCALAPPDATA\hongguo-desktop-client\hongguo-desktop-client.exe",
  "src-tauri\target\release\hongguo-desktop-client.exe",
  "src-tauri\target\debug\hongguo-desktop-client.exe") |
  Where-Object { $_ -and (Test-Path $_) } | Select-Object -First 1
```

## 输出形状

```
持械入宋 · 10 条结果，显示前 10 条
 1  持械入宋：第一季  ID 7671966197985856536  100集
 2  持械入宋：第十季  ID 7697209269287914521  100集
```

序号只在这次列表里有效，所以下一条指令必须带 ID 而不是序号。列表默认 10 条，
`--limit 3` 收窄；搜西饭那一类加 `--category xifan`。

## 坑

- **节目已经开着不用管**：`--series` / `--watch` 由 single-instance 转交给在跑的那个
  实例（暂存指令 + 广播事件 + 窗口置顶），不要先杀进程。
- **乱码**：先执行 `[Console]::OutputEncoding = [Text.Encoding]::UTF8` 再取输出。
- **退出码非 0**：`搜索失败：SOURCE · 来源暂不可用（HTTP 502）` 属上游瞬时错误，
  重试一次；仍失败就如实告诉用户来源在抽风，不要换成别的剧。
- **零匹配仍返回列表**：关键词对不上时站点会给推荐位（标题跟关键词无关），
  这种列表要在回话里点明「没搜到精确匹配的《XX》」。
- **debug 构建**：`target\debug` 那个 exe 的前端在 `localhost:1420`，得先有
  `pnpm tauri dev`（或 `scripts/dev-cdp.ps1`）在跑，否则只有空窗口。开发态传不进
  额外参数时改用环境变量：`$env:DUANJU_WATCH` / `DUANJU_SERIES`。
- 播放失败时程序自己停在详情页或搜索页，不会乱点，Agent 不需要兜底重试。
