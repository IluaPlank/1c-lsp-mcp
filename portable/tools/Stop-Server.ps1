#Requires -Version 5.1
$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "Common.ps1")

Get-Process lsp-skill-server -ErrorAction SilentlyContinue | ForEach-Object {
  Write-Host "Stop PID $($_.Id)"
  Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue
}

# Optional: leave Java LSP children; they usually exit with parent.
# Kill orphaned bsl-language-server java if still around.
Get-CimInstance Win32_Process -Filter "Name='java.exe'" -ErrorAction SilentlyContinue |
  Where-Object { $_.CommandLine -like '*bsl-language-server*' } |
  ForEach-Object {
    Write-Host "Stop java LSP PID $($_.ProcessId)"
    Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue
  }

Write-Host "Остановлено."
