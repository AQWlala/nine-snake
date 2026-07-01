# nine-snake startup diagnostics
# Usage: powershell -ExecutionPolicy Bypass -File diagnose.ps1

$ErrorActionPreference = "Continue"
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$OutputEncoding = [System.Text.Encoding]::UTF8

Write-Host "=== nine-snake diagnostics ===" -ForegroundColor Cyan
Write-Host ""

# 1. Find exe
Write-Host "[1] Check install path..." -ForegroundColor Yellow
$installPaths = @(
    "$env:LOCALAPPDATA\nine-snake\nine-snake.exe",
    "$env:ProgramFiles\nine-snake\nine-snake.exe",
    "${env:ProgramFiles(x86)}\nine-snake\nine-snake.exe"
)
$exePath = $null
foreach ($p in $installPaths) {
    if (Test-Path $p) {
        $exePath = $p
        Write-Host "  Found: $p" -ForegroundColor Green
        break
    }
}
if (-not $exePath) {
    Write-Host "  nine-snake.exe NOT found in default paths" -ForegroundColor Red
    Write-Host "  Please locate it manually and set `$exePath" -ForegroundColor Red
} else {
    $ver = (Get-Item $exePath).VersionInfo
    Write-Host "  Version: $($ver.ProductVersion)" -ForegroundColor Gray
    Write-Host "  Size: $((Get-Item $exePath).Length) bytes" -ForegroundColor Gray
}

# 2. WebView2
Write-Host ""
Write-Host "[2] Check WebView2 runtime..." -ForegroundColor Yellow
$wbv2 = Get-ItemProperty "HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}" -ErrorAction SilentlyContinue
if ($wbv2 -and $wbv2.pv) {
    Write-Host "  WebView2 installed (machine): $($wbv2.pv)" -ForegroundColor Green
} else {
    $wbv2user = Get-ItemProperty "HKCU:\Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}" -ErrorAction SilentlyContinue
    if ($wbv2user -and $wbv2user.pv) {
        Write-Host "  WebView2 installed (user): $($wbv2user.pv)" -ForegroundColor Green
    } else {
        Write-Host "  WebView2 NOT FOUND! Tauri apps require WebView2." -ForegroundColor Red
        Write-Host "  Download: https://developer.microsoft.com/microsoft-edge/webview2/" -ForegroundColor Red
    }
}

# 3. Log files
Write-Host ""
Write-Host "[3] Check log files..." -ForegroundColor Yellow
$logDir = "$env:LOCALAPPDATA\nine-snake\logs"
if (Test-Path $logDir) {
    Write-Host "  Log dir exists: $logDir" -ForegroundColor Green
    $panicLog = Join-Path $logDir "nine-snake-panic.log"
    if (Test-Path $panicLog) {
        Write-Host "  === PANIC LOG (last 20 lines) ===" -ForegroundColor Red
        Get-Content $panicLog -Tail 20 -Encoding UTF8
        Write-Host "  === END PANIC LOG ===" -ForegroundColor Red
    } else {
        Write-Host "  No panic log (app did not panic, or panicked before hook install)" -ForegroundColor Yellow
    }
    $appLogs = Get-ChildItem $logDir -Filter "nine-snake.log.*" -ErrorAction SilentlyContinue
    if ($appLogs) {
        $latest = $appLogs | Sort-Object LastWriteTime -Descending | Select-Object -First 1
        Write-Host "  Latest app log: $($latest.Name)" -ForegroundColor Green
        Write-Host "  === APP LOG (last 30 lines) ===" -ForegroundColor Cyan
        Get-Content $latest.FullName -Tail 30 -Encoding UTF8
        Write-Host "  === END APP LOG ===" -ForegroundColor Cyan
    } else {
        Write-Host "  No app log file" -ForegroundColor Yellow
    }
} else {
    Write-Host "  Log dir NOT exists: $logDir" -ForegroundColor Yellow
    Write-Host "  App did not reach init_tracing() step" -ForegroundColor Yellow
}

# 4. App data dir
Write-Host ""
Write-Host "[4] Check app data dir..." -ForegroundColor Yellow
$appDataDir = "$env:LOCALAPPDATA\com.nine-snake.desktop"
if (Test-Path $appDataDir) {
    Write-Host "  App data dir exists: $appDataDir" -ForegroundColor Green
    $files = Get-ChildItem $appDataDir -Recurse -File -ErrorAction SilentlyContinue
    if ($files) {
        foreach ($f in $files) {
            Write-Host "    $($f.Name) ($($f.Length) bytes)" -ForegroundColor Gray
        }
    } else {
        Write-Host "  (dir exists but empty)" -ForegroundColor Yellow
    }
} else {
    Write-Host "  App data dir NOT exists: $appDataDir" -ForegroundColor Yellow
    Write-Host "  App did not reach setup phase" -ForegroundColor Yellow
}

# 5. Run exe from command line to capture stderr
if ($exePath) {
    Write-Host ""
    Write-Host "[5] Run exe from CLI (kill after 10s)..." -ForegroundColor Yellow
    Write-Host "  Path: $exePath" -ForegroundColor Gray
    $stdoutFile = "$env:TEMP\nine-snake-stdout.txt"
    $stderrFile = "$env:TEMP\nine-snake-stderr.txt"
    Remove-Item $stdoutFile -ErrorAction SilentlyContinue
    Remove-Item $stderrFile -ErrorAction SilentlyContinue
    $proc = Start-Process -FilePath $exePath -PassThru -NoNewWindow -RedirectStandardOutput $stdoutFile -RedirectStandardError $stderrFile
    Start-Sleep -Seconds 10
    if (-not $proc.HasExited) {
        Write-Host "  Process still running (PID=$($proc.Id)), killing..." -ForegroundColor Yellow
        Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
    } else {
        Write-Host "  Process EXITED, exit code: $($proc.ExitCode)" -ForegroundColor Red
    }

    Write-Host ""
    Write-Host "  === STDOUT ===" -ForegroundColor Cyan
    if ((Test-Path $stdoutFile) -and (Get-Content $stdoutFile -Raw)) {
        Get-Content $stdoutFile -Encoding UTF8
    } else {
        Write-Host "  (empty)" -ForegroundColor Gray
    }

    Write-Host ""
    Write-Host "  === STDERR ===" -ForegroundColor Cyan
    if ((Test-Path $stderrFile) -and (Get-Content $stderrFile -Raw)) {
        Get-Content $stderrFile -Encoding UTF8
    } else {
        Write-Host "  (empty)" -ForegroundColor Gray
    }
}

Write-Host ""
Write-Host "=== Diagnostics complete ===" -ForegroundColor Cyan
Write-Host "Please paste ALL output above back to me" -ForegroundColor Green
