# 只读垃圾/缓存扫描(Windows),输出 CAND|标签|大小|路径,不做任何删除。
$ErrorActionPreference = 'SilentlyContinue'

function Get-Size([string]$p) {
    if (Test-Path -LiteralPath $p) {
        $s = (Get-ChildItem -LiteralPath $p -Recurse -File | Measure-Object -Property Length -Sum).Sum
        if ($null -eq $s) { $s = 0 }
        if ($s -ge 1GB) { '{0:N1}G' -f ($s / 1GB) }
        elseif ($s -ge 1MB) { '{0:N0}M' -f ($s / 1MB) }
        else { '{0:N0}K' -f ($s / 1KB) }
    } else { '(无)' }
}

Write-Output '== 候选清理项(只读扫描,不会删除任何东西)=='
$user = $env:USERPROFILE
$loc = $env:LOCALAPPDATA

Write-Output ("CAND|用户临时文件 %TEMP%|" + (Get-Size $env:TEMP) + "|" + $env:TEMP)
Write-Output "CAND|回收站(系统)|按需用 Clear-RecycleBin|RecycleBin"
Write-Output ("CAND|npm 缓存|" + (Get-Size (Join-Path $loc 'npm-cache')) + "|" + (Join-Path $loc 'npm-cache'))
Write-Output ("CAND|pip 缓存|" + (Get-Size (Join-Path $loc 'pip\Cache')) + "|" + (Join-Path $loc 'pip\Cache'))
Write-Output ("CAND|gradle 缓存|" + (Get-Size (Join-Path $user '.gradle\caches')) + "|" + (Join-Path $user '.gradle\caches'))
Write-Output ("CAND|cargo registry 缓存|" + (Get-Size (Join-Path $user '.cargo\registry\cache')) + "|" + (Join-Path $user '.cargo\registry\cache'))
Write-Output ("CAND|浏览器缓存(Chrome)|" + (Get-Size (Join-Path $loc 'Google\Chrome\User Data\Default\Cache')) + "|" + (Join-Path $loc 'Google\Chrome\User Data\Default\Cache'))

# %TEMP% 中超过 7 天的旧文件(仅统计,清理需用户点名)
$cut = (Get-Date).AddDays(-7)
$old = @(Get-ChildItem -LiteralPath $env:TEMP -File | Where-Object { $_.LastWriteTime -lt $cut })
$oldSize = ($old | Measure-Object -Property Length -Sum).Sum
if ($null -eq $oldSize) { $oldSize = 0 }
$sz = if ($oldSize -gt 0) { '{0:N0}K' -f ($oldSize / 1KB) } else { '(无)' }
Write-Output ("CAND|%TEMP% 旧文件(>7 天," + $old.Count + " 个)|" + $sz + "|TEMP-old")

Write-Output '== 扫描完成:请把 CAND 行整理成编号清单,等用户挑选后再执行 =='
