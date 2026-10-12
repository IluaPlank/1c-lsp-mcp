#Requires -Version 5.1
<#
.SYNOPSIS
  Мастер: создать/запустить проект 1С и прописать x-project-id в <проект>\.cursor\mcp.json.
#>
param(
  [string]$Name,
  [string]$RootPath,
  [string]$ProjectRootPath,
  [switch]$SkipWait,
  [int]$McpPort = 9010
)

$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "Common.ps1")

# Ensure server
$base = Get-ServerBaseUrl
if (-not (Test-ServerUp $base)) {
  Write-Host "Сервер не запущен — стартую..."
  & (Join-Path $PSScriptRoot "Start-Server.ps1") -NoBrowser -McpPort $McpPort
  $base = Get-ServerBaseUrl
}
if (-not (Test-ServerUp $base)) {
  throw "Сервер недоступен. Сначала 00-START.cmd"
}
Write-Host "API: $base"

if (-not $RootPath) {
  Write-Host ""
  Write-Host "Выберите папку КОНФИГУРАЦИИ (каталог с исходниками BSL, обычно 'конфигурация'):"
  $RootPath = Select-Folder "Папка конфигурации (BSL root / root_path)"
  if (-not $RootPath) { throw "Отменено" }
}
if (-not (Test-Path $RootPath)) { throw "Не найден root_path: $RootPath" }

if (-not $ProjectRootPath) {
  $guess = Split-Path $RootPath -Parent
  Write-Host ""
  Write-Host "Выберите корень проекта (project_root_path). От него считаются relative file_path."
  Write-Host "Обычно родитель папки конфигурации: $guess"
  $ProjectRootPath = Select-Folder "Корень проекта (project_root_path)"
  if (-not $ProjectRootPath) {
    $ProjectRootPath = $guess
    Write-Host "Использую: $ProjectRootPath"
  }
}
if (-not (Test-Path $ProjectRootPath)) { throw "Не найден project_root_path: $ProjectRootPath" }

if (-not $Name) {
  $Name = Split-Path $ProjectRootPath -Leaf
  Write-Host "Имя проекта: $Name"
}

# Reuse existing project with same root_path if present
$projects = Invoke-RestMethod "$base/api/projects"
$existing = $projects | Where-Object {
  $_.root_path -eq $RootPath -or
  (($_.root_path -replace '/', '\') -eq ($RootPath -replace '/', '\'))
} | Select-Object -First 1

if ($existing) {
  $projectId = $existing.id
  Write-Host "Проект уже есть: $projectId ($($existing.name))"
} else {
  $body = @{
    name              = $Name
    root_path         = $RootPath
    project_root_path = $ProjectRootPath
    jvm_args          = ""
    bsl_config        = ""
    debug             = $false
  } | ConvertTo-Json
  $created = Invoke-RestMethod -Method Post -Uri "$base/api/projects" `
    -ContentType "application/json; charset=utf-8" -Body ([System.Text.Encoding]::UTF8.GetBytes($body))
  $projectId = $created.id
  Write-Host "Создан проект: $projectId"
}

# Start if needed
$snap = Invoke-RestMethod "$base/api/projects/$projectId"
$st = $snap.status.status
if ($st -eq "stopped" -or $st -eq "error") {
  Write-Host "Запуск LSP..."
  try {
    $null = Invoke-RestMethod -Method Post -Uri "$base/api/projects/$projectId/start"
  } catch {
    # "already started" etc.
    Write-Host "start: $($_.Exception.Message)"
  }
}

if (-not $SkipWait) {
  Write-Host "Ожидание индексации (ready)..."
  for ($i = 1; $i -le 120; $i++) {
    Start-Sleep -Seconds 3
    $snap = Invoke-RestMethod "$base/api/projects/$projectId"
    $st = $snap.status.status
    $pct = $snap.progress.percentage
    $msg = $snap.progress.message
    Write-Host ("[{0}] {1} {2}% {3}" -f $i, $st, $pct, $msg)
    if ($st -eq "ready") { break }
    if ($st -eq "error") {
      throw "Ошибка LSP: $($snap.status.error)"
    }
  }
}

$mcpPath = Update-ProjectMcpJson -ProjectRootPath $ProjectRootPath -ProjectId $projectId -McpPort $McpPort
$skillPath = Install-ProjectSkill -ProjectRootPath $ProjectRootPath
Write-Host ""
Write-Host "Готово." -ForegroundColor Green
Write-Host "  project-id : $projectId"
Write-Host "  root_path  : $RootPath"
Write-Host "  project_root_path: $ProjectRootPath"
Write-Host "  mcp.json   : $mcpPath"
if ($skillPath) { Write-Host "  skill      : $skillPath" }
Write-Host ""
Write-Host "Откройте в Cursor именно папку проекта: $ProjectRootPath"
Write-Host "Settings → MCP → Reload у сервера '1c'."
Write-Host "Проверка: tool status должен вернуть ready."
Write-Host "Агенту: skill 1c-mcp (диагностика/навигация/влияние/платформа)."
Write-Host "(Глобальный %USERPROFILE%\.cursor\mcp.json не трогаем — привязка только в проекте.)"

# Keep window open when double-clicked
if ($Host.Name -eq "ConsoleHost") {
  Write-Host ""
  Write-Host "Нажмите Enter чтобы закрыть..."
  [void][Console]::ReadLine()
}
