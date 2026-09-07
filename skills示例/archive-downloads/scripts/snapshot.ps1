param([string]$Source = "")
# 只读快照:列出来源目录的文件概况(Windows),不做任何改动。
# 用法:.\snapshot.ps1 [目录]   缺省 %USERPROFILE%\Downloads
$ErrorActionPreference = 'SilentlyContinue'

if (-not $Source) { $Source = Join-Path $env:USERPROFILE 'Downloads' }
if (-not (Test-Path -LiteralPath $Source)) {
    Write-Output "目录不存在: $Source"
    exit 0
}

Write-Output "== 来源目录: $Source =="
Write-Output "== 最近下载(时间倒序,最多 30 条) =="
Get-ChildItem -LiteralPath $Source -File | Sort-Object LastWriteTime -Descending |
    Select-Object -First 30 |
    ForEach-Object { '{0:yyyy-MM-dd HH:mm}  {1,12:N0}  {2}' -f $_.LastWriteTime, $_.Length, $_.Name }

Write-Output "== 压缩包统计 =="
$n = 0
foreach ($t in @('*.zip', '*.tar', '*.tgz', '*.tar.gz', '*.tar.bz2', '*.7z', '*.rar')) {
    $n += @(Get-ChildItem -LiteralPath $Source -File -Filter $t).Count
}
Write-Output "压缩包: $n 个"
