param([Parameter(Mandatory=$true)][string]$Log)
$ErrorActionPreference='Stop'
$rows=@(Get-Content -LiteralPath $Log | ForEach-Object {
    if ($_ -match 'gap=(\d+)s\s+temp=([\d.-]*)C\s+battery=([\d.]*)%') {
        [pscustomobject]@{gap=[int]$Matches[1];temp=if($Matches[2]){[double]$Matches[2]}else{$null};battery=$Matches[3]}
    }
})
if ($rows.Count -eq 0) {throw 'no heartbeat records found'}
[pscustomobject]@{samples=$rows.Count;max_gap_seconds=($rows.gap | Measure-Object -Maximum).Maximum;
    suspicious_gaps=@($rows|Where-Object gap -gt 15).Count;maximum_temperature_c=($rows.temp|Measure-Object -Maximum).Maximum;
    result=if(@($rows|Where-Object gap -gt 15).Count){'heartbeat gaps detected; inspect sleep/resume events'}else{'no heartbeat gap over 15 seconds'}} | ConvertTo-Json
