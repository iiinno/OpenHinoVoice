#Requires -Version 7.2
[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidateRange(1, 2147483647)][int]$ProcessId,
    [ValidateRange(1, 3600)][int]$DurationSeconds = 60,
    [ValidateRange(100, 10000)][int]$IntervalMs = 1000,
    [string]$OutputPath
)

$ErrorActionPreference = 'Stop'
if (-not $OutputPath) {
    $repo = Split-Path -Parent $PSScriptRoot
    $stamp = [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssfffZ')
    $OutputPath = Join-Path $repo "target/qa/process-$ProcessId-$stamp.json"
}
$OutputPath = [IO.Path]::GetFullPath($OutputPath)
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $OutputPath) | Out-Null
$samples = [Collections.Generic.List[object]]::new()
$result = [ordered]@{
    schemaVersion = 1
    processId = $ProcessId
    status = 'running'
    scope = 'single_process'
    requestedDurationSeconds = $DurationSeconds
    intervalMs = $IntervalMs
    startedAt = [DateTime]::UtcNow.ToString('o')
    samples = $samples
}
$exitCode = 1
$process = $null
try {
    $process = Get-Process -Id $ProcessId -ErrorAction Stop
    $startTime = $process.StartTime
    $result.processStartedAt = $startTime.ToUniversalTime().ToString('o')
    $result.processName = $process.ProcessName
    $result.exeBytes = if ($process.Path) { (Get-Item -LiteralPath $process.Path).Length } else { $null }
    $initialCpu = $process.TotalProcessorTime.TotalMilliseconds
    $watch = [Diagnostics.Stopwatch]::StartNew()
    do {
        $process.Refresh()
        if ($process.HasExited -or $process.StartTime -ne $startTime) { throw '指定したプロセスが測定中に終了しました。' }
        $samples.Add([pscustomobject][ordered]@{
            elapsedMs = $watch.ElapsedMilliseconds
            cpuMs = $process.TotalProcessorTime.TotalMilliseconds - $initialCpu
            privateBytes = $process.PrivateMemorySize64
            workingSetBytes = $process.WorkingSet64
        })
        if ($watch.Elapsed.TotalSeconds -ge $DurationSeconds) { break }
        $remaining = [Math]::Ceiling(($DurationSeconds - $watch.Elapsed.TotalSeconds) * 1000)
        Start-Sleep -Milliseconds ([int][Math]::Min($IntervalMs, $remaining))
    } while ($true)
    $result.elapsedMs = $watch.ElapsedMilliseconds
    $result.cpuMs = $samples[-1].cpuMs
    $result.cpuSecondsPerMinute = $result.cpuMs / $result.elapsedMs * 60
    $result.peakPrivateBytes = ($samples | Measure-Object -Property privateBytes -Maximum).Maximum
    $result.peakWorkingSetBytes = ($samples | Measure-Object -Property workingSetBytes -Maximum).Maximum
    $result.status = 'passed'
    $exitCode = 0
} catch {
    $result.status = 'failed'
    $result.error = $_.Exception.Message
} finally {
    if ($process) { $process.Dispose() }
    $result.finishedAt = [DateTime]::UtcNow.ToString('o')
    $result | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $OutputPath -Encoding utf8
    Write-Output $OutputPath
}
exit $exitCode
