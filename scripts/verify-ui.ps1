#Requires -Version 7.2
[CmdletBinding()]
param(
    [string]$Executable,
    [string]$OutputDir,
    [switch]$CompileOnly
)
$ErrorActionPreference = 'Stop'
if ($env:GITHUB_ACTIONS -ne 'true') {
    throw 'UIハーネスのコンパイルと実行はGitHub Actionsで行ってください。'
}
$repo = Split-Path -Parent $PSScriptRoot
$runId = 'ui-{0}-{1}' -f [DateTime]::UtcNow.ToString('yyyyMMddTHHmmss'), [Guid]::NewGuid().ToString('N').Substring(0, 8)
if (-not $OutputDir) { $OutputDir = Join-Path $repo "target/qa/$runId" }
$OutputDir = [IO.Path]::GetFullPath($OutputDir)
$Executable = if ($Executable) { [IO.Path]::GetFullPath($Executable) } else { $null }
New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$resultPath = Join-Path $OutputDir 'result.json'
$result = [ordered]@{
    schemaVersion = 1; mode = 'HiddenDesktopUI'; runId = $runId
    status = 'running'; startedAt = [DateTime]::UtcNow.ToString('o'); finishedAt = $null
    scope = 'settings UIA + client capture at 600x420 and 720x520 logical pixels'
    executable = $Executable; executableSha256 = $null; harnessSha256 = $null
    compile = 'not_run'; ui = 'not_run'; sourceRevision = $null
    inputDesktopSwitched = $false; audioRequested = $false; microphoneRequested = $false
    limitations = @('画像の見た目は別途確認が必要', 'WindowsのDPIと高コントラスト設定は変更しない', 'キーボード操作・音・ASR・貼付・外部APIは未実施')
}
function Save-Result { $result | ConvertTo-Json -Depth 16 | Set-Content -LiteralPath $resultPath -Encoding utf8 }
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
        return [pscustomobject]@{ exitCode = $process.ExitCode; stdout = $stdout.GetAwaiter().GetResult(); stderr = $stderr.GetAwaiter().GetResult() }
    } finally {
        if ($started -and -not $process.HasExited) { $process.Kill($true); $process.WaitForExit() }
        $process.Dispose()
    }
}
$exitCode = 1
try {
    Save-Result
    if ($Executable) {
        if (-not (Test-Path -LiteralPath $Executable -PathType Leaf)) { throw '検証するビルド済みexeがありません。' }
        $result.executableSha256 = (Get-FileHash -LiteralPath $Executable -Algorithm SHA256).Hash
    } elseif (-not $CompileOnly) {
        throw 'UI検証には -Executable でビルド済みexeを指定してください。'
    }
    $source = Join-Path $PSScriptRoot 'ui-harness.cs'
    $result.harnessSha256 = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash
    $framework = Join-Path $env:WINDIR 'Microsoft.NET/Framework64/v4.0.30319'
    $compiler = Join-Path $framework 'csc.exe'
    $helper = Join-Path $OutputDir 'ui-harness.exe'
    $references = @('UIAutomationClient.dll', 'UIAutomationTypes.dll', 'WindowsBase.dll') | ForEach-Object { '/reference:' + (Join-Path $framework "WPF/$_") }
    $arguments = @('/nologo', '/target:exe', '/platform:x64', "/out:$helper", '/reference:System.Drawing.dll', '/reference:System.Web.Extensions.dll') + $references + @($source)
    $compile = Run-Hidden $compiler $arguments 30000
    [IO.File]::WriteAllText((Join-Path $OutputDir 'compile.log'), $compile.stdout + $compile.stderr)
    if ($compile.exitCode -ne 0) { $result.compile = 'failed'; throw 'UI harnessのコンパイルに失敗しました。compile.logを確認してください。' }
    $result.compile = 'passed'
    $git = Run-Hidden 'git' @('rev-parse', 'HEAD') 10000
    if ($git.exitCode -eq 0) { $result.sourceRevision = $git.stdout.Trim() }
    if ($CompileOnly) {
        $result.status = 'compiled_only'
        $exitCode = 0
    } else {
        $result.ui = 'running'; Save-Result
        $ui = Run-Hidden $helper @('--controller', $Executable, $OutputDir, $runId) 110000
        [IO.File]::WriteAllText((Join-Path $OutputDir 'harness.log'), $ui.stdout + $ui.stderr)
        foreach ($name in @('controller-result', 'worker-result')) {
            $path = Join-Path $OutputDir "$name.json"
            if (Test-Path -LiteralPath $path) { $result[$name] = Get-Content -LiteralPath $path -Raw | ConvertFrom-Json }
        }
        if ($ui.exitCode -ne 0 -or $result['controller-result'].status -ne 'passed' -or $result['worker-result'].status -ne 'passed') {
            $result.ui = 'failed'; throw '別desktopでのUI検証が失敗しました。phaseとエラー、画像を確認してください。'
        }
        $result.ui = 'passed'; $result.status = 'passed'; $exitCode = 0
    }
} catch {
    $result.status = 'failed'; $result.error = $_.Exception.Message
} finally {
    $result.finishedAt = [DateTime]::UtcNow.ToString('o')
    Save-Result
    Write-Output $resultPath
}
exit $exitCode
