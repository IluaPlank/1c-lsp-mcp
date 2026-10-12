#Requires -Version 5.1
<#
.SYNOPSIS
  Пишет config.toml и запускает lsp-skill-server (portable).
#>
param(
  [switch]$NoBrowser,
  [int]$HttpPort = 4000,
  [int]$McpPort = 9010
)

$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "Common.ps1")

$Root = Get-PortableRoot
Write-Host "1c-mcp portable: $Root"

$java = Find-Java
if (-not $java) {
  Write-Host ""
  Write-Host "ERROR: Java не найдена. Установите JDK 17+ и добавьте java в PATH." -ForegroundColor Red
  Write-Host "  https://adoptium.net/"
  exit 1
}
$jv = Test-JavaVersion $java
Write-Host "Java: $java (major=$($jv.Version))"
if (-not $jv.Ok) {
  Write-Host "ERROR: нужна Java 17+, сейчас: $($jv.Raw)" -ForegroundColor Red
  exit 1
}

$exe = Find-ServerExe $Root
if (-not $exe) {
  Write-Host "ERROR: не найден bin\lsp-skill-server.exe. Сначала запустите scripts\pack-portable.ps1" -ForegroundColor Red
  exit 1
}

$lsJar = Find-BundledJar $Root "bsl-language-server-*-exec.jar"
if (-not $lsJar) {
  Write-Host "ERROR: нет jars\bsl-language-server-*-exec.jar" -ForegroundColor Red
  exit 1
}
$ctxJar = Find-BundledJar $Root "mcp-bsl-context-*.jar"
if (-not $ctxJar) {
  Write-Host "WARN: нет mcp-bsl-context-*.jar — справка платформы (search/info) будет недоступна." -ForegroundColor Yellow
  $ctxJar = ""
}

$platform = Find-PlatformPath
if (-not $platform) {
  Write-Host "WARN: платформа 1С не найдена в 'C:\Program Files\1cv8'. Укажите platform_path вручную в config.toml позже." -ForegroundColor Yellow
  $platform = ""
} else {
  Write-Host "Platform: $platform"
}

$configPath = Write-AppConfig -JarPath $lsJar -BslContextJarPath $ctxJar -PlatformPath $platform -HttpPort $HttpPort -McpPort $McpPort
Write-Host "Config: $configPath"

$base = "http://127.0.0.1:$HttpPort"
if (Test-ServerUp $base) {
  Write-Host "Сервер уже запущен: $base"
} else {
  # Stop stale processes pointing elsewhere
  Get-Process lsp-skill-server -ErrorAction SilentlyContinue | ForEach-Object {
    Write-Host "Останавливаю старый процесс PID $($_.Id)"
    Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue
  }
  Start-Sleep -Milliseconds 400
  Clear-StaleRuntimeLock

  $logOut = Join-Path $Root "server.out.log"
  $logErr = Join-Path $Root "server.err.log"
  Write-Host "Запуск: $exe"
  Start-Process -FilePath $exe -WorkingDirectory (Split-Path $exe -Parent) `
    -RedirectStandardOutput $logOut -RedirectStandardError $logErr -WindowStyle Hidden | Out-Null

  if (-not (Wait-ServerUp $base 45)) {
    Write-Host "ERROR: сервер не ответил на $base" -ForegroundColor Red
    if (Test-Path $logErr) { Get-Content $logErr -Tail 40 }
    exit 1
  }
  Write-Host "OK: UI $base"
  Write-Host "OK: MCP http://127.0.0.1:$McpPort/mcp"
}

if (-not $NoBrowser) {
  Start-Process "$base/"
}

Write-Host ""
Write-Host "Дальше: запустите 01-CONNECT-PROJECT.cmd чтобы подключить конфигурацию к Cursor."
