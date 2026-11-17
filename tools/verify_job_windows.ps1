# Job Object 验证（纯 ASCII）
#   A：超时路径——先证明子进程起来了，再证明整棵树被收掉
#   B：正常退出路径——start /b 留下的后台子进程不许活过命令
$ErrorActionPreference = 'Continue'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$repo = 'X:\dsh_ws\harness-v2'
$exe  = Join-Path $repo 'out\win\ui.exe'
$failed = 0

function Test-TimeoutCase($ws) {
  $sc = Join-Path $ws 'sc.json'
  Write-Host "`n=== A: timeout kills the tree ==="
  if (-not (Test-Path $sc)) { Write-Host "missing $sc"; $script:failed++; return }

  $before = @(Get-Process ping -ErrorAction SilentlyContinue).Count
  $p = Start-Process -FilePath $exe -ArgumentList @('run','--task','t','--mock',$sc,'--workspace',$ws,'--yes') -NoNewWindow -PassThru -RedirectStandardOutput (Join-Path $ws 'out.txt') -RedirectStandardError (Join-Path $ws 'err.txt')

  Start-Sleep -Milliseconds 2500
  $mid = @(Get-Process ping -ErrorAction SilentlyContinue).Count
  Write-Host "ping during run=$mid (before=$before)"

  $p.WaitForExit(30000) | Out-Null
  Start-Sleep -Milliseconds 1200
  $after = @(Get-Process ping -ErrorAction SilentlyContinue).Count
  Write-Host "ping after exit=$after"

  if ($mid -le $before) {
    Write-Host "FAIL child never started - cannot prove reclamation"
    $script:failed++
    return
  }
  if ($after -gt $before) {
    Write-Host "FAIL leftover after timeout"
    Get-Process ping -ErrorAction SilentlyContinue | Stop-Process -Force
    $script:failed++
    return
  }
  Write-Host "PASS child ran ($mid) and the whole tree was reclaimed"
}

function Test-OrphanCase($ws) {
  $sc = Join-Path $ws 'sc.json'
  Write-Host "`n=== B: normal exit must not orphan (start /b) ==="
  if (-not (Test-Path $sc)) { Write-Host "missing $sc"; $script:failed++; return }

  $before = @(Get-Process ping -ErrorAction SilentlyContinue).Count
  & $exe run --task t --mock $sc --workspace $ws --yes 2>&1 | Out-Null
  Start-Sleep -Milliseconds 900
  $after = @(Get-Process ping -ErrorAction SilentlyContinue).Count
  Write-Host "ping before=$before after=$after"
  if ($after -gt $before) {
    Write-Host "FAIL orphan survived normal exit"
    Get-Process ping -ErrorAction SilentlyContinue | Stop-Process -Force
    $script:failed++
    return
  }
  Write-Host "PASS no orphan (job terminated on completion)"
}

Test-TimeoutCase (Join-Path $repo 'out\win-jobtest-a')
Test-OrphanCase  (Join-Path $repo 'out\win-jobtest-b')

Write-Host "`n--- result ---"
if ($failed -eq 0) { Write-Host "ALL PASS"; exit 0 } else { Write-Host "$failed FAILED"; exit 1 }
