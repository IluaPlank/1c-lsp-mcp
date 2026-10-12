#Requires -Version 5.1
param(
  [string]$BslLsJar = "",
  [string]$BslContextJar = "",
  [string]$OutDir = "",
  [switch]$Zip,
  [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "..")
$CargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
if (Test-Path $CargoBin) { $env:Path = "$CargoBin;$env:Path" }

if (-not $OutDir) {
  $OutDir = Join-Path $RepoRoot "dist\1c-mcp-portable"
}

function Resolve-DefaultJar([string]$Hint, [string[]]$SearchRoots, [string]$Filter) {
  if ($Hint -and (Test-Path $Hint)) { return (Resolve-Path $Hint).Path }
  foreach ($root in $SearchRoots) {
    if (-not (Test-Path $root)) { continue }
    $hit = Get-ChildItem $root -Recurse -Filter $Filter -ErrorAction SilentlyContinue |
      Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if ($hit) { return $hit.FullName }
  }
  return $null
}

$search = @(
  (Join-Path $RepoRoot "jars"),
  (Join-Path $RepoRoot "..")
)

$BslLsJar = Resolve-DefaultJar $BslLsJar $search "bsl-language-server-*-exec.jar"
$BslContextJar = Resolve-DefaultJar $BslContextJar $search "mcp-bsl-context-*.jar"

if (-not $BslLsJar) {
  throw "bsl-language-server-*-exec.jar not found. Pass -BslLsJar"
}
if (-not $BslContextJar) {
  Write-Warning "mcp-bsl-context-*.jar not found - package without platform help tools"
}

if (-not $SkipBuild) {
  Write-Host "==> cargo build -p lsp-skill-server --release"
  Push-Location $RepoRoot
  try {
    cargo build -p lsp-skill-server --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
  } finally {
    Pop-Location
  }
}

$exe = Join-Path $RepoRoot "target\release\lsp-skill-server.exe"
if (-not (Test-Path $exe)) {
  $exe = Join-Path $RepoRoot "target\debug\lsp-skill-server.exe"
}
if (-not (Test-Path $exe)) {
  throw "lsp-skill-server.exe not found"
}

Write-Host "==> OutDir: $OutDir"
if (Test-Path $OutDir) {
  Remove-Item $OutDir -Recurse -Force
}
New-Item -ItemType Directory -Force -Path (Join-Path $OutDir "bin") | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $OutDir "jars") | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $OutDir "tools") | Out-Null

Copy-Item $exe (Join-Path $OutDir "bin\lsp-skill-server.exe") -Force
Copy-Item $BslLsJar (Join-Path $OutDir "jars\$(Split-Path $BslLsJar -Leaf)") -Force
if ($BslContextJar) {
  Copy-Item $BslContextJar (Join-Path $OutDir "jars\$(Split-Path $BslContextJar -Leaf)") -Force
}

$portableSrc = Join-Path $RepoRoot "portable"
Copy-Item (Join-Path $portableSrc "00-START.cmd") $OutDir -Force
Copy-Item (Join-Path $portableSrc "01-CONNECT-PROJECT.cmd") $OutDir -Force
Copy-Item (Join-Path $portableSrc "02-STOP.cmd") $OutDir -Force
Copy-Item (Join-Path $portableSrc "tools\*") (Join-Path $OutDir "tools") -Force

$skillSrc = Join-Path $RepoRoot "skills\1c-mcp\SKILL.md"
if (-not (Test-Path $skillSrc)) {
  throw "skills\1c-mcp\SKILL.md not found"
}
$skillDest = Join-Path $OutDir "skills\1c-mcp"
New-Item -ItemType Directory -Force -Path $skillDest | Out-Null
Copy-Item $skillSrc (Join-Path $skillDest "SKILL.md") -Force

$lsName = Split-Path $BslLsJar -Leaf
$ctxLine = if ($BslContextJar) { "  jars\$(Split-Path $BslContextJar -Leaf)" } else { "  (bsl-context missing)" }

$readme = @"
1c-mcp portable
===============

Requirements:
  - Windows 10/11 x64
  - Java 17+ in PATH (https://adoptium.net/)
  - 1C platform in C:\Program Files\1cv8\... (for search/info)
  - Cursor

Install (2 clicks):
  1. Copy this folder to the new PC
  2. Double-click 00-START.cmd
  3. Double-click 01-CONNECT-PROJECT.cmd
     - pick configuration folder (BSL root)
     - pick project root
     - script writes <project>\.cursor\mcp.json
     - copies skill 1c-mcp into <project>\.cursor\skills\

Switch project: run 01-CONNECT-PROJECT.cmd again
Stop: 02-STOP.cmd

UI:  http://127.0.0.1:4000
MCP: http://127.0.0.1:9010/mcp
Config: %APPDATA%\Infostart\LspSkill\config\config.toml

JARs:
  jars\$lsName
$ctxLine
"@
$utf8Bom = New-Object System.Text.UTF8Encoding $true
[System.IO.File]::WriteAllText((Join-Path $OutDir "README.txt"), $readme, $utf8Bom)

$size = (Get-ChildItem $OutDir -Recurse -File | Measure-Object Length -Sum).Sum
Write-Host ("==> Done: {0:N1} MB" -f ($size / 1MB))
Write-Host "    $OutDir"

if ($Zip) {
  $zipPath = "$OutDir.zip"
  if (Test-Path $zipPath) { Remove-Item $zipPath -Force }
  Write-Host "==> Zip: $zipPath"
  Compress-Archive -Path $OutDir -DestinationPath $zipPath -CompressionLevel Optimal
  Write-Host ("    {0:N1} MB" -f ((Get-Item $zipPath).Length / 1MB))
}

Write-Host ""
Write-Host "Copy the folder (or zip) to another PC and run 00-START.cmd"