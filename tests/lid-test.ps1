# Lid-close test for away-guard step 1.
# Sets "lid close while plugged in" to Do nothing, keeps the system awake, and logs a
# heartbeat every 10 s (temperature, battery, internet). Close the lid during the run.
# A gap much longer than ~12 s in the log means the machine slept.
# The original setting is saved to lid-journal.json before any change and restored at
# the end. If this window is killed, run lid-restore.ps1.
param([ValidateRange(1,120)][int]$Minutes = 20, [ValidateRange(45,95)][int]$MaxTempC = 85)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'lid-native.ps1')

$journal = Join-Path $PSScriptRoot 'lid-journal.json'
$log = Join-Path $PSScriptRoot ('lid-test-' + (Get-Date -Format 'yyyyMMdd-HHmmss') + '.log')

# 1. Journal first: the true original survives a crash of an earlier run
if (Test-Path -LiteralPath $journal) { throw 'A previous lid test still needs restoration. Run tests/lid-restore.ps1 first.' }
$original = [Arcturus.LidNative]::Read()
$scheme = $original[0]
$record = @{ scheme = $scheme; ac = [int]$original[1]; dc = [int]$original[2]; savedAt = (Get-Date -Format s) } | ConvertTo-Json
# CreateNew also refuses a concurrent test without overwriting its recovery record.
$stream = [System.IO.File]::Open($journal, [System.IO.FileMode]::CreateNew, [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
try {
  $bytes = [System.Text.Encoding]::ASCII.GetBytes($record)
  $stream.Write($bytes, 0, $bytes.Length)
  $stream.Flush()
} finally {
  $stream.Dispose()
}

$ping = New-Object System.Net.NetworkInformation.Ping
if ((Get-CimInstance Win32_Battery | Select-Object -First 1).BatteryStatus -ne 2) {
  'WARNING: not plugged in. Only the plugged-in setting is changed, so closing the lid now will still sleep.'
}

try {
  powercfg /setacvalueindex $scheme SUB_BUTTONS LIDACTION 0
  if ($LASTEXITCODE -ne 0) { throw "powercfg failed with exit code $LASTEXITCODE" }
  powercfg /setactive $scheme
  if ($LASTEXITCODE -ne 0) { throw "powercfg activation failed with exit code $LASTEXITCODE" }
  if ([Arcturus.LidNative]::SetThreadExecutionState(0x80000001) -eq 0) { throw 'keep-awake request failed' }
  "Lid (plugged in) set to Do nothing for $Minutes min. Close the lid now. Log: $log"

  # 2. Heartbeat
  $end = (Get-Date).AddMinutes($Minutes)
  $last = Get-Date
  while ((Get-Date) -lt $end) {
    $now = Get-Date
    $gap = [int]($now - $last).TotalSeconds
    $last = $now
    $tz = Get-CimInstance Win32_PerfFormattedData_Counters_ThermalZoneInformation | Select-Object -First 1
    $tempC = $null
    if ($tz) { $tempC = [math]::Round($tz.HighPrecisionTemperature / 10 - 273.15, 1) }
    $bat = Get-CimInstance Win32_Battery | Select-Object -First 1
    try { $net = $ping.Send('1.1.1.1', 1500).Status } catch { $net = 'Error' }
    $line = '{0}  gap={1}s  temp={2}C  battery={3}%  plugged={4}  net={5}' -f $now.ToString('HH:mm:ss'), $gap, $tempC, $bat.EstimatedChargeRemaining, ($bat.BatteryStatus -eq 2), $net
    Add-Content $log $line
    $line
    if ($tempC -and $tempC -ge $MaxTempC) { Add-Content $log "STOP: temperature $tempC C >= $MaxTempC C"; break }
    Start-Sleep -Seconds 10
  }
} finally {
  # 3. Restore
  [void][Arcturus.LidNative]::SetThreadExecutionState(0x80000000)
  $ping.Dispose()
  & (Join-Path $PSScriptRoot 'lid-restore.ps1')
}
