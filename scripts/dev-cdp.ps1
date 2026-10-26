# 本地调试用：带 CDP 调试端口拉起桌面端（e2e 脚本的独立手动版）。
# 用法：powershell -File scripts/dev-cdp.ps1 [-Port 9333]
param([int]$Port = 9333)
$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=$Port"
Set-Location (Split-Path -Parent $PSScriptRoot)
pnpm tauri dev
