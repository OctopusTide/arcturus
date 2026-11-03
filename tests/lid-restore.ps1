# Restores the lid-close setting saved by lid-test.ps1 (lid-journal.json).
# Safe to run any time; does nothing if there is no journal.
$journal = Join-Path $PSScriptRoot 'lid-journal.json'
$ErrorActionPreference = 'Stop'
if (-not (Test-Path $journal)) { 'nothing to restore'; return }
$j = Get-Content $journal -Raw | ConvertFrom-Json
powercfg /setacvalueindex $j.scheme SUB_BUTTONS LIDACTION $j.ac
if ($LASTEXITCODE -ne 0) { throw 'could not restore AC lid action; recovery journal retained' }
powercfg /setdcvalueindex $j.scheme SUB_BUTTONS LIDACTION $j.dc
if ($LASTEXITCODE -ne 0) { throw 'could not restore battery lid action; recovery journal retained' }
powercfg /setactive $j.scheme
if ($LASTEXITCODE -ne 0) { throw 'could not reactivate power scheme; recovery journal retained' }
Remove-Item -LiteralPath $journal
"restored lid action: plugged in = $($j.ac), on battery = $($j.dc)  (0 nothing, 1 sleep, 2 hibernate, 3 shut down)"
