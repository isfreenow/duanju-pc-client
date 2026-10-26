# 清掉本应用留下的僵尸 WebView2 进程。
#
# 强杀（taskkill /T /F）桌面进程后，WebView2 的浏览器/渲染/GPU 子进程有时会继续
# 占着这个应用的用户数据目录，下一次启动就会在创建 webview 时失败：
#   Failed to setup app: failed to create webview: WebView2 error:
#   HRESULT(0x8007139F)「组或资源未处于正确状态」
# 端到端脚本每次拉起窗口前先跑这个，避免把环境问题误报成代码问题。
#
# 只按命令行里是否含本应用的 exe 名匹配，不会碰别的程序用的 WebView2。
#
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts/kill-stale-webview.ps1

param([string]$ExeName = "hongguo-desktop-client.exe")

$targets = @(Get-CimInstance Win32_Process -Filter "Name='msedgewebview2.exe'" |
  Where-Object { $_.CommandLine -match [regex]::Escape($ExeName) })

foreach ($process in $targets) {
  Stop-Process -Id $process.ProcessId -Force -ErrorAction SilentlyContinue
}

Start-Sleep -Milliseconds 800
Write-Output ("已清理 {0} 个 {1} 的残留 WebView2 进程" -f $targets.Count, $ExeName)
