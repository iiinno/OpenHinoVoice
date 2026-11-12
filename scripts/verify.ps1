#Requires -Version 7.2
[CmdletBinding()]
param(
    [ValidateSet('Fast', 'Full', 'Live')][string]$Mode = 'Fast',
    [string]$Filter,
    [ValidateSet('Segmented', 'Worker', 'Realtime')][string]$LiveTest = 'Segmented',
    [string[]]$Corpus = @('short'),
    [ValidateSet('Fresh', 'Reuse')][string]$RealtimeWorkerMode = 'Fresh',
    [ValidateRange(1, 10)][int]$RealtimeRepetitions = 1,
    [string]$CorpusDir,
    [string]$SpeechExe,
    [string]$ModelPath,
    [string]$OutputDir,
    [string]$Cargo = 'cargo',
    [string]$TargetDir,
    [switch]$AllowLocalBuild
)

$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true' -and -not $AllowLocalBuild) {
    throw 'Rustのビルド・テストはGitHub Actionsで実行してください。ローカル実行は利用者の明示許可と -AllowLocalBuild が必要です。'
}
$repo = Split-Path -Parent $PSScriptRoot
$runId = '{0}-{1}' -f [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssfffZ'), [Guid]::NewGuid().ToString('N').Substring(0, 8)
if (-not $OutputDir) { $OutputDir = Join-Path $repo "target/qa/$runId" }
$OutputDir = [IO.Path]::GetFullPath($OutputDir)
New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$resultPath = Join-Path $OutputDir 'result.json'
$checks = [Collections.Generic.List[object]]::new()
$result = [ordered]@{
    schemaVersion = 1
    runId = $runId
    mode = $Mode
    liveTest = $(if ($Mode -eq 'Live') { $LiveTest } else { $null })
    scope = $(if ($Filter) { 'filtered' } else { $Mode.ToLowerInvariant() })
    startedAt = [DateTime]::UtcNow.ToString('o')
    finishedAt = $null
    revision = $null
    dirty = $null
    sourceSha256 = $null
    sourceFileCount = 0
    status = 'running'
    checks = $checks
    tests = [ordered]@{ passed = 0; failed = 0; ignored = 0 }
    ignored = @()
    manualPrerequisites = @(
        '実マイクの録音と切断',
        '対象アプリの本文とクリップボード復元',
        'UIAとDPI・複数モニター',
        '実クラウドAPIと複数のローカルLLM'
    )
    isolation = [ordered]@{ appData = 'appdata'; localAppData = 'localappdata'; namespace = $runId }
}

function Save-Result {
    $result | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $resultPath -Encoding utf8
}

function Run-Command([string]$Executable, [string[]]$Arguments, [string]$LiveLogPrefix) {
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $Executable
    $info.WorkingDirectory = $repo
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $info.RedirectStandardOutput = $true
    $info.RedirectStandardError = $true
    $info.StandardOutputEncoding = [Text.Encoding]::UTF8
    $info.StandardErrorEncoding = [Text.Encoding]::UTF8
    foreach ($argument in $Arguments) { $info.ArgumentList.Add($argument) }
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $info
    $started = $false
    $stdoutFile = $null
    $stderrFile = $null
    try {
        $started = $process.Start()
        if (-not $started) { throw "プロセスを開始できません: $Executable" }
        if ($LiveLogPrefix) {
            $stdoutFile = [IO.FileStream]::new("$LiveLogPrefix.stdout.log", [IO.FileMode]::Create, [IO.FileAccess]::Write, [IO.FileShare]::Read, 1, [IO.FileOptions]::Asynchronous)
            $stderrFile = [IO.FileStream]::new("$LiveLogPrefix.stderr.log", [IO.FileMode]::Create, [IO.FileAccess]::Write, [IO.FileShare]::Read, 1, [IO.FileOptions]::Asynchronous)
            $stdout = $process.StandardOutput.BaseStream.CopyToAsync($stdoutFile)
            $stderr = $process.StandardError.BaseStream.CopyToAsync($stderrFile)
        } else {
            $stdout = $process.StandardOutput.ReadToEndAsync()
            $stderr = $process.StandardError.ReadToEndAsync()
        }
        while (-not $process.WaitForExit(200)) { }
        $process.WaitForExit()
        if ($LiveLogPrefix) {
            $stdout.GetAwaiter().GetResult()
            $stderr.GetAwaiter().GetResult()
            $stdoutFile.Dispose(); $stdoutFile = $null
            $stderrFile.Dispose(); $stderrFile = $null
            return [pscustomobject]@{ exitCode = $process.ExitCode; stdout = [IO.File]::ReadAllText("$LiveLogPrefix.stdout.log"); stderr = [IO.File]::ReadAllText("$LiveLogPrefix.stderr.log") }
        }
        return [pscustomobject]@{ exitCode = $process.ExitCode; stdout = $stdout.GetAwaiter().GetResult(); stderr = $stderr.GetAwaiter().GetResult() }
    } finally {
        if ($started -and -not $process.HasExited) {
            # この呼び出しが作ったプロセスだけを回収する。
            $process.Kill($true)
            $process.WaitForExit()
        }
        if ($stdoutFile) { $stdoutFile.Dispose() }
        if ($stderrFile) { $stderrFile.Dispose() }
        $process.Dispose()
    }
}

function Invoke-Check([object]$Check, [string]$Executable, [string[]]$Arguments) {
    $Check.status = 'running'
    Save-Result
    $watch = [Diagnostics.Stopwatch]::StartNew()
    try {
        $outcome = Run-Command $Executable $Arguments (Join-Path $OutputDir $Check.name)
        $Check.exitCode = $outcome.exitCode
        $text = $outcome.stdout + $outcome.stderr
        [IO.File]::WriteAllText((Join-Path $OutputDir $Check.log), $text)
        $Check.status = $(if ($outcome.exitCode -eq 0) { 'passed' } else { 'failed' })
        if ($Check.name -eq 'tests') {
            foreach ($match in [regex]::Matches($text, 'test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;')) {
                $result.tests.passed += [int]$match.Groups[1].Value
                $result.tests.failed += [int]$match.Groups[2].Value
                $result.tests.ignored += [int]$match.Groups[3].Value
            }
            $result.ignored = @([regex]::Matches($text, '(?m)^test (.+?) \.\.\. ignored(?:, ([^\r\n]+))?\r?$') | ForEach-Object {
                [ordered]@{ name = $_.Groups[1].Value; reason = $_.Groups[2].Value }
            })
            if ($outcome.exitCode -eq 0 -and ($result.tests.passed + $result.tests.failed) -eq 0) {
                $Check.status = 'failed'
                $Check.validationError = '実行されたテストが0件です。フィルターとテスト名を確認してください。'
            }
        }
    } catch {
        $Check.status = 'failed'
        $Check.validationError = $_.Exception.Message
        [IO.File]::WriteAllText((Join-Path $OutputDir $Check.log), $_.Exception.Message)
    } finally {
        $Check.durationMs = $watch.ElapsedMilliseconds
        Save-Result
    }
    Write-Host ("{0}: {1} ({2} ms)" -f $Check.name, $Check.status, $Check.durationMs)
    if ($Check.status -ne 'passed') {
        $logPath = Join-Path $OutputDir $Check.log
        if (Test-Path -LiteralPath $logPath) {
            Get-Content -LiteralPath $logPath -Tail 100 | ForEach-Object { Write-Host $_ }
        }
        throw "$($Check.name) が失敗しました。$($Check.log)を確認してください。"
    }
}

$savedEnvironment = @{}
$environmentNames = @('APPDATA', 'LOCALAPPDATA', 'CARGO_TARGET_DIR', 'OHV_TEST_NAMESPACE', 'OHV_REVISION', 'JV_CORPUS_DIR', 'JV_PERF_OUTPUT_DIR', 'JV_PERF_CASES', 'JV_SPEECH_EXE', 'JV_MODEL', 'JV_REALTIME_WORKER_MODE', 'JV_REALTIME_REPETITIONS')
foreach ($name in $environmentNames) { $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
$exitCode = 1
try {
    Save-Result
    $revision = Run-Command 'git' @('rev-parse', 'HEAD')
    if ($revision.exitCode -eq 0) { $result.revision = $revision.stdout.Trim() }
    $status = Run-Command 'git' @('status', '--porcelain')
    if ($status.exitCode -eq 0) { $result.dirty = -not [string]::IsNullOrWhiteSpace($status.stdout) }
    $sources = Run-Command 'git' @('ls-files', '-z', '--cached', '--others', '--exclude-standard', '--', 'src', 'tests', 'examples', 'scripts', 'assets', 'Cargo.toml', 'Cargo.lock', 'build.rs')
    if ($sources.exitCode -eq 0) {
        $sourceDigests = @($sources.stdout.Split([char]0, [StringSplitOptions]::RemoveEmptyEntries) | Sort-Object -Unique | ForEach-Object {
            $sourcePath = Join-Path $repo $_
            if (Test-Path -LiteralPath $sourcePath -PathType Leaf) {
                "$_`t$((Get-FileHash -LiteralPath $sourcePath -Algorithm SHA256).Hash)"
            }
        })
        if ($sourceDigests.Count -gt 0) {
            $result.sourceFileCount = $sourceDigests.Count
            $result.sourceSha256 = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData([Text.Encoding]::UTF8.GetBytes(($sourceDigests -join "`n"))))
        }
    }
    $env:OHV_REVISION = $result.revision
    if ($TargetDir) { $env:CARGO_TARGET_DIR = [IO.Path]::GetFullPath($TargetDir) }
    $env:OHV_TEST_NAMESPACE = $runId
    $env:APPDATA = Join-Path $OutputDir 'appdata'
    $env:LOCALAPPDATA = Join-Path $OutputDir 'localappdata'
    New-Item -ItemType Directory -Force -Path $env:APPDATA, $env:LOCALAPPDATA | Out-Null
    $commands = @()
    if ($Mode -eq 'Live') {
        if ($Filter) { throw 'LiveではFilterを指定せず、LiveTestで対象を選んでください。' }
        if (-not $SpeechExe -or -not (Test-Path -LiteralPath $SpeechExe -PathType Leaf)) { throw 'Liveには既存speech.exeのフルパスをSpeechExeで指定してください。' }
        if (-not $ModelPath -or -not (Test-Path -LiteralPath $ModelPath -PathType Leaf)) { throw 'Liveには取得済みモデルファイルのフルパスをModelPathで指定してください。' }
        if (-not [IO.Path]::IsPathFullyQualified($SpeechExe) -or -not [IO.Path]::IsPathFullyQualified($ModelPath)) { throw 'speechとモデルには絶対パスを指定してください。' }
        if (-not $CorpusDir) { $CorpusDir = Join-Path $repo 'tests/corpus' }
        $CorpusDir = [IO.Path]::GetFullPath($CorpusDir)
        if ($Corpus.Count -eq 0) { throw 'Corpusには測定する音声を1つ以上指定してください。' }
        $inputs = foreach ($case in $Corpus) {
            if ($case -notin @('short', '1min', '5min', '30min')) { throw "不明なCorpusです: $case" }
            foreach ($extension in @('wav', 'ref.txt')) {
                $path = Join-Path $CorpusDir "$case.$extension"
                if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "必要なcorpusがありません: $case.$extension" }
                [ordered]@{ name = "$case.$extension"; bytes = (Get-Item -LiteralPath $path).Length; sha256 = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash }
            }
        }
        $result.inputs = @($inputs)
        $result.model = [ordered]@{ file = [IO.Path]::GetFileName($ModelPath); bytes = (Get-Item -LiteralPath $ModelPath).Length; sha256 = (Get-FileHash -LiteralPath $ModelPath -Algorithm SHA256).Hash }
        $env:JV_SPEECH_EXE = [IO.Path]::GetFullPath($SpeechExe)
        $env:JV_MODEL = [IO.Path]::GetFullPath($ModelPath)
        $env:JV_CORPUS_DIR = $CorpusDir
        $env:JV_PERF_CASES = $Corpus -join ','
        $env:JV_PERF_OUTPUT_DIR = Join-Path $OutputDir 'perf'
        $env:JV_REALTIME_WORKER_MODE = $RealtimeWorkerMode.ToLowerInvariant()
        $env:JV_REALTIME_REPETITIONS = $RealtimeRepetitions.ToString()
        $result.realtime = [ordered]@{ workerMode = $env:JV_REALTIME_WORKER_MODE; repetitions = $RealtimeRepetitions }
        $testName = switch ($LiveTest) {
            'Segmented' { 'live_segmented_latency_and_accuracy_on_corpus' }
            'Worker' { 'live_worker_latency_and_accuracy_on_corpus' }
            'Realtime' { 'live_realtime_session_on_corpus' }
        }
        $testTarget = if ($LiveTest -eq 'Realtime') { 'realtime_live' } else { 'perf_live' }
        $commands += @{ name = 'tests'; arguments = @('test', '--release', '--test', $testTarget, '--locked', '--offline', '--', '--ignored', '--exact', $testName, '--nocapture') }
    } else {
        $commands += @{ name = 'format'; arguments = @('fmt', '--all', '--', '--check') }
        if ($Mode -eq 'Full') { $commands += @{ name = 'clippy'; arguments = @('clippy', '--all-targets', '--locked', '--offline', '--', '-D', 'warnings') } }
        $testArguments = @('test', '--all-targets', '--locked', '--offline')
        if ($Filter) { $testArguments += @('--', $Filter) }
        $commands += @{ name = 'tests'; arguments = $testArguments }
        if ($Mode -eq 'Full') { $commands += @{ name = 'release'; arguments = @('build', '--release', '--locked', '--offline') } }
    }
    foreach ($command in $commands) {
        $checks.Add([ordered]@{ name = $command.name; status = 'not_run'; durationMs = 0; exitCode = $null; log = "$($command.name).log"; validationError = $null })
    }
    for ($i = 0; $i -lt $commands.Count; $i++) { Invoke-Check $checks[$i] $Cargo $commands[$i].arguments }
    $result.status = 'passed'
    $exitCode = 0
} catch {
    $result.status = 'failed'
    $result.error = $_.Exception.Message
} finally {
    foreach ($name in $environmentNames) { [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], 'Process') }
    $result.finishedAt = [DateTime]::UtcNow.ToString('o')
    Save-Result
    Write-Output $resultPath
}
exit $exitCode
