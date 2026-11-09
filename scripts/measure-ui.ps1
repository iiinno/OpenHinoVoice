#Requires -Version 7.2
[CmdletBinding()]
param(
    [ValidateSet('Resident', 'Settings')][string]$Mode = 'Resident',
    [string]$BaselineExecutable,
    [string]$CandidateExecutable,
    [string]$BaselineRevision,
    [string]$CandidateRevision,
    [string]$BaselineFeatures = 'unspecified',
    [string]$CandidateFeatures = 'unspecified',
    [ValidateSet('Absent', 'Present', 'Either')][string]$BaselineUia = 'Either',
    [ValidateSet('Absent', 'Present', 'Either')][string]$CandidateUia = 'Either',
    [ValidateSet('standard', 'soft', 'minimal', 'rich')][string]$BaselineHudStyle = 'standard',
    [ValidateSet('standard', 'soft', 'minimal', 'rich')][string]$CandidateHudStyle = 'standard',
    [ValidateRange(3, 10)][int]$Repetitions = 3,
    [ValidateRange(30, 60)][int]$DurationSeconds = 30,
    [ValidateRange(100, 5000)][int]$IntervalMs = 1000,
    [ValidateRange(1000, 10000)][int]$WarmupMs = 2000,
    [string]$OutputDir,
    [switch]$CompileOnly
)
$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -cne 'true') {
    throw 'このハーネスのコンパイル・測定はGitHub Actions専用です。利用者のPCでは実行しません。'
}
$repo = Split-Path -Parent $PSScriptRoot
$runId = 'perf-{0}-{1}' -f [DateTime]::UtcNow.ToString('yyyyMMddTHHmmss'), [Guid]::NewGuid().ToString('N').Substring(0, 8)
if (-not $OutputDir) { $OutputDir = Join-Path $repo "target/perf/$runId" }
$OutputDir = [IO.Path]::GetFullPath($OutputDir)
New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$resultPath = Join-Path $OutputDir 'result.json'
$result = [ordered]@{
    schemaVersion = 1; runId = $runId; mode = $Mode; status = 'running'
    startedAt = [DateTime]::UtcNow.ToString('o'); finishedAt = $null
    compile = 'not_run'; measurement = 'not_run'; harnessSha256 = $null
    repetitions = $Repetitions; durationSeconds = $DurationSeconds; intervalMs = $IntervalMs; warmupMs = $WarmupMs
    measurementScope = 'owned process on a separate inactive desktop; repeated samples after observed HWND responsiveness'
    desktopSwitched = $false; audioRequested = $false; microphoneRequested = $false
    limitations = @(
        '測定完了は性能予算の合格を意味しない',
        '別desktopでトレイが使えず常駐が起動しない場合は失敗として記録する',
        'SettingsのUIAなし・ありは同じソース/ビルド条件のexeを明示する。ソース同一性はharnessから推定しない',
        'CPU背景負荷・キャッシュ・電源状態は操作しない',
        '実マイク・HUD稼働・入力遅延・貼付・実画面品質は測定しない'
    )
}
function Save-Result { $result | ConvertTo-Json -Depth 30 | Set-Content -LiteralPath $resultPath -Encoding utf8 }
function Run-Hidden([string]$File, [string[]]$Arguments, [int]$TimeoutMs) {
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $File; $info.WorkingDirectory = $repo
    $info.UseShellExecute = $false; $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
    foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) }
    $process = [Diagnostics.Process]::new(); $process.StartInfo = $info
    $started = $false
    try {
        $started = $process.Start()
        if (-not $started) { throw "開始できません: $File" }
        $stdout = $process.StandardOutput.ReadToEndAsync(); $stderr = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit($TimeoutMs)) { throw "期限を超えました: $File" }
        # Controller owns a kill-on-close Job, so a forced exit also reclaims only its descendants.
        return [pscustomobject]@{ exitCode = $process.ExitCode; stdout = $stdout.GetAwaiter().GetResult(); stderr = $stderr.GetAwaiter().GetResult() }
    } finally {
        if ($started -and -not $process.HasExited) { $process.Kill($true); $process.WaitForExit(10000) | Out-Null }
        $process.Dispose()
    }
}
function Artifact([string]$Label, [string]$Path, [string]$Revision, [string]$Features, [string]$Uia, [string]$HudStyle) {
    if (-not $Path -or -not [IO.Path]::IsPathFullyQualified($Path) -or -not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "$Label のビルド済みexe絶対パスを明示してください。"
    }
    $Path = [IO.Path]::GetFullPath($Path)
    if ([IO.Path]::GetExtension($Path) -ne '.exe') { throw "$Label はexeではありません。" }
    return [ordered]@{
        label = $Label; path = $Path; sha256 = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash
        bytes = (Get-Item -LiteralPath $Path).Length; revision = $Revision; features = $Features
        uiaExpected = $Uia; hudStyle = $HudStyle
    }
}
function Distribution([object[]]$Values) {
    $sorted = @($Values | ForEach-Object { [double]$_ } | Sort-Object)
    if ($sorted.Count -eq 0) { return $null }
    $middle = [int][Math]::Floor($sorted.Count / 2)
    $median = if ($sorted.Count % 2) { $sorted[$middle] } else { ($sorted[$middle - 1] + $sorted[$middle]) / 2 }
    return [ordered]@{ count = $sorted.Count; median = $median; min = $sorted[0]; max = $sorted[-1] }
}
$exitCode = 1
try {
    Save-Result
    $source = Join-Path $PSScriptRoot 'perf-harness.cs'
    $result.harnessSha256 = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash
    $framework = Join-Path $env:WINDIR 'Microsoft.NET/Framework64/v4.0.30319'
    $compiler = Join-Path $framework 'csc.exe'
    $helper = Join-Path $OutputDir 'perf-harness.exe'
    $references = @('UIAutomationClient.dll', 'UIAutomationTypes.dll', 'WindowsBase.dll') | ForEach-Object {
        '/reference:' + (Join-Path $framework "WPF/$_")
    }
    $arguments = @('/nologo', '/target:exe', '/platform:x64', "/out:$helper", '/reference:System.Web.Extensions.dll') + $references + @($source)
    $compile = Run-Hidden $compiler $arguments 30000
    [IO.File]::WriteAllText((Join-Path $OutputDir 'compile.log'), $compile.stdout + $compile.stderr)
    if ($compile.exitCode -ne 0) { $result.compile = 'failed'; throw '性能harnessのコンパイルに失敗しました。compile.logを確認してください。' }
    $result.compile = 'passed'
    if ($CompileOnly) {
        $result.status = 'compiled_only'; $exitCode = 0
    } else {
        $artifacts = @(
            (Artifact 'baseline' $BaselineExecutable $BaselineRevision $BaselineFeatures $BaselineUia $BaselineHudStyle)
            (Artifact 'candidate' $CandidateExecutable $CandidateRevision $CandidateFeatures $CandidateUia $CandidateHudStyle)
        )
        $result.artifacts = $artifacts
        $timeoutMs = $Repetitions * 2 * ($DurationSeconds * 1000 + $WarmupMs + 40000) + 60000
        $plan = [ordered]@{
            mode = $Mode; namespaceValue = $runId; output = $OutputDir; repetitions = $Repetitions
            durationSeconds = $DurationSeconds; intervalMs = $IntervalMs; warmupMs = $WarmupMs
            timeoutMs = $timeoutMs; artifacts = $artifacts
        }
        $planPath = Join-Path $OutputDir 'plan.json'
        $plan | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $planPath -Encoding utf8
        $result.measurement = 'running'; Save-Result
        $probe = Run-Hidden $helper @('--controller', $planPath) ($timeoutMs + 20000)
        [IO.File]::WriteAllText((Join-Path $OutputDir 'harness.log'), $probe.stdout + $probe.stderr)
        foreach ($name in @('controller-result', 'worker-result')) {
            $path = Join-Path $OutputDir "$name.json"
            if (Test-Path -LiteralPath $path) { $result[$name] = Get-Content -LiteralPath $path -Raw | ConvertFrom-Json }
        }
        if ($probe.exitCode -ne 0 -or $result['controller-result'].status -ne 'measured' -or $result['worker-result'].status -ne 'measured') {
            $result.measurement = 'failed'
            throw '性能測定が成立しませんでした。phase、起動/終了、UIA条件を確認してください。'
        }
        $groups = [ordered]@{}
        foreach ($artifact in $artifacts) {
            $rows = @($result['worker-result'].cases | Where-Object { $_.artifact -eq $artifact.label -and $_.status -eq 'measured' })
            if ($rows.Count -ne $Repetitions) { throw "$($artifact.label) の成功反復数が不足しています。" }
            $groups[$artifact.label] = [ordered]@{
                startupToResponsiveWindowMs = Distribution @($rows | ForEach-Object { $_.startupToResponsiveWindowMs })
                beforeUiaPeakPrivateBytes = Distribution @($rows | ForEach-Object { $_.beforeUia.peakPrivateBytes })
                beforeUiaPeakWorkingSetBytes = Distribution @($rows | ForEach-Object { $_.beforeUia.peakWorkingSetBytes })
                beforeUiaCpuMs = Distribution @($rows | ForEach-Object { $_.beforeUia.cpuMs })
                closeMs = Distribution @($rows | ForEach-Object { $_.closeMs })
            }
            if ($Mode -eq 'Settings') {
                $groups[$artifact.label].afterUiaPeakPrivateBytes = Distribution @($rows | ForEach-Object { $_.afterUia.peakPrivateBytes })
                $groups[$artifact.label].afterUiaPeakWorkingSetBytes = Distribution @($rows | ForEach-Object { $_.afterUia.peakWorkingSetBytes })
                $groups[$artifact.label].afterUiaCpuMs = Distribution @($rows | ForEach-Object { $_.afterUia.cpuMs })
            }
        }
        $privateKey = if ($Mode -eq 'Settings') { 'afterUiaPeakPrivateBytes' } else { 'beforeUiaPeakPrivateBytes' }
        $privateDelta = $groups.candidate[$privateKey].median - $groups.baseline[$privateKey].median
        $exeDelta = $artifacts[1].bytes - $artifacts[0].bytes
        $result.summary = $groups
        $result.difference = [ordered]@{
            medianPeakPrivateBytes = $privateDelta; executableBytes = $exeDelta
            startupToResponsiveWindowMs = $groups.candidate.startupToResponsiveWindowMs.median - $groups.baseline.startupToResponsiveWindowMs.median
            scope = if ($Mode -eq 'Settings') { 'after the explicit UIA probe; expected feature-off absence is not accessibility success' } else { 'inactive desktop resident idle' }
        }
        if ($Mode -eq 'Resident') {
            $result.proposedBudget = [ordered]@{ privateIncreaseBytes = 1MB; privateExceeded = $privateDelta -gt 1MB }
        } elseif ($BaselineUia -eq 'Absent' -and $CandidateUia -eq 'Present') {
            $result.proposedBudget = [ordered]@{
                privateIncreaseBytes = 4MB; privateExceeded = $privateDelta -gt 4MB
                executableIncreaseBytes = 1MB; executableExceeded = $exeDelta -gt 1MB
                requires = 'same-source/profile builds differing only in official accessibility feature; supplied by caller'
            }
        }
        $result.status = 'measured'; $result.measurement = 'measured'; $exitCode = 0
    }
} catch {
    $result.status = 'failed'; $result.error = $_.Exception.Message
} finally {
    $result.finishedAt = [DateTime]::UtcNow.ToString('o')
    Save-Result
    Write-Output $resultPath
}
exit $exitCode
