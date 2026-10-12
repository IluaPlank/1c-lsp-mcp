#Requires -Version 5.1
<#
.SYNOPSIS
  Быстрая установка для разработки ИЛИ сборка portable-пакета.

.PARAMETER Portable
  Собрать dist\1c-mcp-portable (то же что scripts\pack-portable.ps1)

.PARAMETER Zip
  Вместе с -Portable создать zip

.PARAMETER PlatformPath / BslLsJar / BslContextJar / McpPort / SkipBuild
  Как раньше — для локального config.toml при разработке
#>
param(
  [switch]$Portable,
  [switch]$Zip,
  [string]$PlatformPath = "",
  [string]$BslLsJar = "",
  [string]$BslContextJar = "",
  [int]$McpPort = 9010,
  [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent $MyInvocation.MyCommand.Path

if ($Portable) {
  $pack = Join-Path $Root "scripts\pack-portable.ps1"
  $args = @()
  if ($BslLsJar) { $args += @("-BslLsJar", $BslLsJar) }
  if ($BslContextJar) { $args += @("-BslContextJar", $BslContextJar) }
  if ($SkipBuild) { $args += "-SkipBuild" }
  if ($Zip) { $args += "-Zip" }
  & $pack @args
  Write-Host ""
  Write-Host "Дальше: docs\PORTABLE.md"
  exit 0
}

$CargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
if (Test-Path $CargoBin) { $env:Path = "$CargoBin;$env:Path" }

Write-Host "==> Java"
$java = Get-Command java -ErrorAction SilentlyContinue
if (-not $java) { throw "java не найден в PATH. Установите JDK 17+." }
java -version 2>&1 | Select-Object -First 3

if (-not $SkipBuild) {
  Write-Host "==> cargo build -p lsp-skill-server --release"
  Push-Location $Root
  try { cargo build -p lsp-skill-server --release } finally { Pop-Location }
}

$exe = Join-Path $Root "target\release\lsp-skill-server.exe"
if (-not (Test-Path $exe)) { $exe = Join-Path $Root "target\debug\lsp-skill-server.exe" }
if (-not (Test-Path $exe)) { throw "Бинарник не найден." }

function Resolve-Jar([string]$Hint, [string]$Filter) {
  if ($Hint) {
    if (-not (Test-Path $Hint)) { throw "Файл не найден: $Hint" }
    return (Resolve-Path $Hint).Path
  }
  $roots = @(
    (Join-Path $Root "jars"),
    (Join-Path $Root "..")
  )
  foreach ($dir in $roots) {
    if (-not (Test-Path $dir)) { continue }
    $hit = Get-ChildItem $dir -Recurse -Filter $Filter -ErrorAction SilentlyContinue |
      Sort-Object LastWriteTime -Descending |
      Select-Object -First 1
    if ($hit) { return $hit.FullName }
  }
  return $null
}

$BslLsJar = Resolve-Jar $BslLsJar "bsl-language-server-*-exec.jar"
if (-not $BslLsJar) {
  throw "Не найден bsl-language-server-*-exec.jar. Положите его в jars\ или передайте -BslLsJar."
}
$BslContextJar = Resolve-Jar $BslContextJar "mcp-bsl-context-*.jar"
if (-not $BslContextJar) {
  Write-Warning "mcp-bsl-context-*.jar не найден. Справка платформы будет выключена. Передайте -BslContextJar, чтобы включить."
  $BslContextJar = ""
}

if (-not $PlatformPath) {
  $base = "C:\Program Files\1cv8"
  if (Test-Path $base) {
    $PlatformPath = (Get-ChildItem $base -Directory | Sort-Object Name -Descending | Select-Object -First 1).FullName
  }
}

$configDir = Join-Path $env:APPDATA "Infostart\LspSkill\config"
New-Item -ItemType Directory -Force -Path $configDir | Out-Null
$configPath = Join-Path $configDir "config.toml"
$jarPath = ($BslLsJar -replace '\\', '/')
$platform = ($PlatformPath -replace '\\', '/')
$ctxJar = ($BslContextJar -replace '\\', '/')

@"
jar_path = "$jarPath"
listen_host = "127.0.0.1"
http_port = 4000
log_level = "info"
mcp_enabled = true
mcp_port = $McpPort
platform_path = "$platform"
bsl_context_jar_path = "$ctxJar"
use_toon_format = false
"@ | Set-Content -Path $configPath -Encoding UTF8

Write-Host "==> config: $configPath"
Write-Host ""
Write-Host "Локальный запуск: & '$exe'"
Write-Host "Или portable-пакет для другого ПК:"
Write-Host "  .\install.ps1 -Portable -Zip"
Write-Host "Документация: docs\PORTABLE.md"
Write-Host ""
Write-Host "Cursor mcp.json:"
Write-Host @"
{
  "mcpServers": {
    "1c": {
      "url": "http://127.0.0.1:$McpPort/mcp",
      "headers": { "x-project-id": "<PROJECT_ID>" }
    }
  }
}
"@
