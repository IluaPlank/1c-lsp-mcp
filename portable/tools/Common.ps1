#Requires -Version 5.1
$ErrorActionPreference = "Stop"

function Get-PortableRoot {
  # tools/ -> portable root
  return (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
}

function Get-ConfigPath {
  $dir = Join-Path $env:APPDATA "Infostart\LspSkill\config"
  New-Item -ItemType Directory -Force -Path $dir | Out-Null
  return (Join-Path $dir "config.toml")
}

function Get-RuntimePath {
  return (Join-Path $env:LOCALAPPDATA "Infostart\LspSkill\data\runtime.json")
}

function Clear-StaleRuntimeLock {
  $runtime = Get-RuntimePath
  if (-not (Test-Path $runtime)) { return }
  $ownerPid = $null
  $ownerName = $null
  try {
    $meta = Get-Content $runtime -Raw -Encoding UTF8 | ConvertFrom-Json
    if ($meta.pid) { $ownerPid = [int]$meta.pid }
  } catch {
    Remove-Item $runtime -Force
    Write-Host "Удалён повреждённый runtime.json"
    return
  }
  if ($ownerPid) {
    $owner = Get-Process -Id $ownerPid -ErrorAction SilentlyContinue
    if ($owner) { $ownerName = $owner.ProcessName }
  }
  if ($ownerName -eq "lsp-skill-server") { return }
  $who = if ($ownerName) { "$ownerName" } else { "процесс уже завершён" }
  Write-Host "Сбрасываю устаревший runtime.json (pid $ownerPid = $who)"
  Remove-Item $runtime -Force
}

function Find-Java {
  $cmd = Get-Command java -ErrorAction SilentlyContinue
  if ($cmd) { return $cmd.Source }
  $candidates = @(
    "$env:JAVA_HOME\bin\java.exe",
    "C:\Program Files\Java\*\bin\java.exe",
    "C:\Program Files\Eclipse Adoptium\*\bin\java.exe",
    "C:\Program Files\Microsoft\jdk-*\bin\java.exe"
  )
  foreach ($pattern in $candidates) {
    $hit = Get-Item $pattern -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($hit) { return $hit.FullName }
  }
  return $null
}

function Test-JavaVersion([string]$JavaExe) {
  # java -version пишет в stderr. При ErrorActionPreference=Stop PowerShell
  # превращает это в NativeCommandError и обрывает скрипт.
  $prev = $ErrorActionPreference
  $ErrorActionPreference = "Continue"
  try {
    $out = & $JavaExe -version 2>&1 | ForEach-Object { $_.ToString() } | Out-String
  } catch {
    $out = $_.Exception.Message
  } finally {
    $ErrorActionPreference = $prev
  }
  if ($out -match 'version "(\d+)') {
    $major = [int]$Matches[1]
    if ($major -eq 1 -and $out -match 'version "1\.(\d+)') {
      $major = [int]$Matches[1]
    }
    return @{ Ok = ($major -ge 17); Version = $major; Raw = $out.Trim() }
  }
  return @{ Ok = $false; Version = 0; Raw = $out.Trim() }
}

function Find-PlatformPath {
  $base = "C:\Program Files\1cv8"
  if (-not (Test-Path $base)) { return $null }
  $dirs = Get-ChildItem $base -Directory -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -match '^\d+\.\d+' } |
    Sort-Object Name -Descending
  foreach ($d in $dirs) {
    # Prefer full platform tree that looks like 8.3.x.x
    if (Test-Path (Join-Path $d.FullName "bin\1cv8.exe")) {
      return $d.FullName
    }
    return $d.FullName
  }
  return $null
}

function Find-BundledJar([string]$Root, [string]$Pattern) {
  $jarsDir = Join-Path $Root "jars"
  $hit = Get-ChildItem $jarsDir -Filter $Pattern -ErrorAction SilentlyContinue | Select-Object -First 1
  if ($hit) { return $hit.FullName }
  return $null
}

function Find-ServerExe([string]$Root) {
  $candidates = @(
    (Join-Path $Root "bin\lsp-skill-server.exe"),
    (Join-Path $Root "..\target\release\lsp-skill-server.exe"),
    (Join-Path $Root "..\target\debug\lsp-skill-server.exe")
  )
  foreach ($c in $candidates) {
    $resolved = $null
    try { $resolved = (Resolve-Path $c -ErrorAction Stop).Path } catch { }
    if ($resolved -and (Test-Path $resolved)) { return $resolved }
  }
  return $null
}

function Write-AppConfig {
  param(
    [string]$JarPath,
    [string]$BslContextJarPath,
    [string]$PlatformPath,
    [int]$HttpPort = 4000,
    [int]$McpPort = 9010
  )
  $configPath = Get-ConfigPath
  $jar = ($JarPath -replace '\\', '/')
  $ctx = ($BslContextJarPath -replace '\\', '/')
  $plat = if ($PlatformPath) { ($PlatformPath -replace '\\', '/') } else { "" }

  @"
jar_path = "$jar"
listen_host = "127.0.0.1"
http_port = $HttpPort
log_level = "info"

mcp_enabled = true
mcp_port = $McpPort

use_toon_format = false

platform_path = "$plat"
bsl_context_jar_path = "$ctx"
"@ | Set-Content -Path $configPath -Encoding UTF8
  return $configPath
}

function Get-ServerBaseUrl {
  $runtime = Get-RuntimePath
  if (Test-Path $runtime) {
    try {
      $j = Get-Content $runtime -Raw -Encoding UTF8 | ConvertFrom-Json
      if ($j.connect_url) { return $j.connect_url.TrimEnd('/') }
      if ($j.port) { return "http://127.0.0.1:$($j.port)" }
    } catch { }
  }
  return "http://127.0.0.1:4000"
}

function Test-ServerUp([string]$BaseUrl, [int]$TimeoutSec = 2) {
  try {
    $null = Invoke-RestMethod -Uri "$BaseUrl/api/settings" -TimeoutSec $TimeoutSec
    return $true
  } catch {
    return $false
  }
}

function Wait-ServerUp([string]$BaseUrl, [int]$TimeoutSec = 30) {
  $deadline = (Get-Date).AddSeconds($TimeoutSec)
  while ((Get-Date) -lt $deadline) {
    if (Test-ServerUp $BaseUrl) { return $true }
    Start-Sleep -Milliseconds 500
  }
  return $false
}

function Install-ProjectSkill {
  param(
    [Parameter(Mandatory)][string]$ProjectRootPath
  )
  # Canonical skill lives next to portable tools: ../skills/1c-mcp/SKILL.md
  $portableRoot = Split-Path $PSScriptRoot -Parent
  $src = Join-Path $portableRoot "skills\1c-mcp\SKILL.md"
  if (-not (Test-Path $src)) {
    # Dev checkout: repo skills/
    $repoSkill = Join-Path (Split-Path $portableRoot -Parent) "skills\1c-mcp\SKILL.md"
    if (Test-Path $repoSkill) { $src = $repoSkill }
  }
  if (-not (Test-Path $src)) {
    Write-Warning "Skill 1c-mcp не найден рядом с portable — пропуск"
    return $null
  }
  $destDir = Join-Path $ProjectRootPath ".cursor\skills\1c-mcp"
  New-Item -ItemType Directory -Force -Path $destDir | Out-Null
  $dest = Join-Path $destDir "SKILL.md"
  Copy-Item $src $dest -Force
  return $dest
}

function Update-ProjectMcpJson {
  param(
    [Parameter(Mandatory)][string]$ProjectRootPath,
    [Parameter(Mandatory)][string]$ProjectId,
    [int]$McpPort = 9010
  )
  # Привязка к проекту — только в <project>\.cursor\mcp.json (не в %USERPROFILE%\.cursor).
  $mcpDir = Join-Path $ProjectRootPath ".cursor"
  New-Item -ItemType Directory -Force -Path $mcpDir | Out-Null
  $mcpPath = Join-Path $mcpDir "mcp.json"

  $doc = $null
  if (Test-Path $mcpPath) {
    $raw = Get-Content $mcpPath -Raw -Encoding UTF8
    if ($raw.Trim()) {
      $doc = $raw | ConvertFrom-Json
    }
  }
  if (-not $doc) {
    $doc = [pscustomobject]@{ mcpServers = [pscustomobject]@{} }
  }
  if (-not $doc.mcpServers) {
    $doc | Add-Member -NotePropertyName mcpServers -NotePropertyValue ([pscustomobject]@{}) -Force
  }

  $entry = [pscustomobject]@{
    url     = "http://127.0.0.1:$McpPort/mcp"
    headers = [pscustomobject]@{ "x-project-id" = $ProjectId }
  }

  # Preserve other project-local servers; overwrite/add "1c"
  $servers = @{}
  foreach ($p in $doc.mcpServers.PSObject.Properties) {
    $servers[$p.Name] = $p.Value
  }
  $servers["1c"] = $entry

  $mcpServers = [pscustomobject]@{}
  foreach ($k in ($servers.Keys | Sort-Object)) {
    $mcpServers | Add-Member -NotePropertyName $k -NotePropertyValue $servers[$k]
  }
  $out = [pscustomobject]@{ mcpServers = $mcpServers }
  $json = $out | ConvertTo-Json -Depth 8
  [System.IO.File]::WriteAllText($mcpPath, $json + "`n", (New-Object System.Text.UTF8Encoding $false))
  return $mcpPath
}

function Select-Folder([string]$Description) {
  Add-Type -AssemblyName System.Windows.Forms | Out-Null
  $dlg = New-Object System.Windows.Forms.FolderBrowserDialog
  $dlg.Description = $Description
  $dlg.ShowNewFolderButton = $false
  if ($dlg.ShowDialog() -ne [System.Windows.Forms.DialogResult]::OK) {
    return $null
  }
  return $dlg.SelectedPath
}
